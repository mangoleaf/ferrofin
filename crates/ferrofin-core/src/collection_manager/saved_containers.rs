//! The playlists an older Ferrofin saved with a container as one entry and
//! no media type, brought to what a create or add stores today (gl-583).
//! [`crate::adoption_repairs::expand_saved_playlist_containers`] runs it once
//! at boot.

use std::collections::HashMap;

use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_traits::error::ServiceError;
use uuid::Uuid;

use super::{Entry, FerrofinPlaylistManager, LINKED_CHILD_MANUAL, read_entries, write_entries_in};
use crate::db_error::db_err;
use crate::item_data;
use crate::item_type_lookup::kind_from_type_name;

/// What one playlist owes: its entries with each container expanded, and the
/// media type it lacks. Built by
/// [`FerrofinPlaylistManager::saved_container_plan`], written by
/// [`FerrofinPlaylistManager::apply_saved_container_plan`].
#[derive(Debug)]
pub(crate) struct SavedContainerPlan {
    playlist: Uuid,
    /// The entries as read; the write goes ahead only while they are still
    /// stored.
    entries: Vec<Entry>,
    /// The entries to store instead, when a container entry expands.
    expanded: Option<Vec<Entry>>,
    /// The media type to stamp, when the playlist has none.
    media_type: Option<MediaType>,
}

impl SavedContainerPlan {
    /// The playlist the plan is for.
    pub(crate) fn playlist(&self) -> Uuid {
        self.playlist
    }

    /// Whether the plan replaces container entries.
    pub(crate) fn expands(&self) -> bool {
        self.expanded.is_some()
    }

    /// Whether the plan stamps a media type.
    pub(crate) fn stamps(&self) -> bool {
        self.media_type.is_some()
    }
}

/// The kinds `playlist_items` expands rather than storing them as one entry:
/// a music genre or artist, and every folder.
fn expands_on_add(kind: BaseItemKind) -> bool {
    matches!(kind, BaseItemKind::MusicGenre | BaseItemKind::MusicArtist)
        || crate::kinds::is_folder(kind)
}

impl FerrofinPlaylistManager {
    /// Whether a playlist stored at `path` is one a create made: its folder is
    /// in the playlists folder `CreatePlaylist` makes folders in. A playlist
    /// the scan found in a library (a playlist file, a `[playlist]` folder, a
    /// folder with a `playlist.xml`) takes its entries and media type from
    /// what is on disk.
    pub(crate) fn in_playlists_folder(&self, path: &str) -> bool {
        let folder = format!("{}/playlists", self.paths.data_path());
        let path = self.virtual_paths.expand(path);
        let path = std::path::Path::new(&path);
        path.starts_with(&folder) && path != std::path::Path::new(&folder)
    }

    /// What `playlist` owes, or `None` when it is already in today's shape.
    ///
    /// Each manual entry naming a container becomes, in its place, what an
    /// add of it stores now (`Playlist.GetPlaylistItems`, through
    /// `playlist_items`), as `owner` sees it; a nil `owner` (none recorded)
    /// sees everything, and so does an owner since deleted, as for an add.
    /// A container that gives nothing keeps its entry, and so does an entry
    /// naming the playlist itself (an update drops that id, as upstream's
    /// clear does). A playlist with no media type gets the one
    /// `PlaylistManager.CreatePlaylist` infers (`infer_media_type`) from the
    /// entries as stored, which stand for the ids it was created with.
    pub(crate) async fn saved_container_plan(
        &self,
        playlist: Uuid,
        owner: Uuid,
    ) -> Result<Option<SavedContainerPlan>, ServiceError> {
        let Some(row) = self.library_manager.get_item_by_id(playlist).await? else {
            return Ok(None);
        };
        let entries = read_entries(self.db.pool(), &guid_to_db(playlist)).await?;
        let ids: Vec<Uuid> = entries
            .iter()
            .filter_map(|(child, _)| Uuid::parse_str(child).ok())
            .collect();
        let kinds: HashMap<Uuid, Option<BaseItemKind>> = self
            .items
            .retrieve_items(&ids)
            .await?
            .iter()
            .filter_map(|row| {
                Some((
                    Uuid::parse_str(&row.id).ok()?,
                    kind_from_type_name(&row.type_),
                ))
            })
            .collect();
        let mut expanded = Vec::with_capacity(entries.len());
        let mut expands = false;
        for entry in &entries {
            let container = Uuid::parse_str(&entry.0).ok().filter(|id| {
                *id != playlist
                    && entry.1 == i64::from(LINKED_CHILD_MANUAL)
                    && kinds.get(id).copied().flatten().is_some_and(expands_on_add)
            });
            if let Some(id) = container {
                let members = self.playlist_items(&[id], owner).await?;
                if !members.is_empty() {
                    expanded.extend(
                        members
                            .into_iter()
                            .map(|member| (guid_to_db(member), i64::from(LINKED_CHILD_MANUAL))),
                    );
                    expands = true;
                    continue;
                }
            }
            expanded.push(entry.clone());
        }
        let media_type = if crate::dto_service::parse_media_type(row.media_type.as_deref())
            == MediaType::Unknown
        {
            let existing: Vec<Uuid> = ids
                .into_iter()
                .filter(|id| kinds.contains_key(id))
                .collect();
            Some(self.infer_media_type(&existing).await?)
        } else {
            None
        };
        if !expands && media_type.is_none() {
            return Ok(None);
        }
        Ok(Some(SavedContainerPlan {
            playlist,
            entries,
            expanded: expands.then_some(expanded),
            media_type,
        }))
    }

    /// Writes `plan` in one `BEGIN IMMEDIATE` transaction, the entries and the
    /// media type together (in the column and in `Data`, as a create does).
    /// `false`, writing nothing, when the entries are no longer the ones the
    /// plan read: the previous version, still serving during a rolling
    /// update, changed them. Expanded entries re-rate the playlist from its
    /// new members and queue its refresh (its collage), as an add does.
    pub(crate) async fn apply_saved_container_plan(
        &self,
        plan: &SavedContainerPlan,
    ) -> Result<bool, ServiceError> {
        let pid = guid_to_db(plan.playlist);
        let edit = self.entry_edits.lock().await;
        let mut tx = self
            .db
            .writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_err)?;
        if read_entries(&mut *tx, &pid).await? != plan.entries {
            return Ok(false);
        }
        if let Some(expanded) = &plan.expanded {
            write_entries_in(&mut tx, &pid, expanded).await?;
        }
        if let Some(media_type) = plan.media_type {
            let value = serde_json::to_value(media_type)
                .map_err(|e| ServiceError::Backend(format!("serialize MediaType: {e}")))?;
            crate::item_persistence_service::stamp_playlist_in(
                &mut tx,
                plan.playlist,
                value.as_str(),
                None,
            )
            .await?;
            item_data::set_data_key_in(&mut tx, plan.playlist, "PlaylistMediaType", value).await?;
        }
        tx.commit().await.map_err(db_err)?;
        if plan.expands() {
            if let Err(err) = self.update_rating_to_items(plan.playlist).await {
                tracing::warn!(%err, item_id = %plan.playlist, "could not re-rate a playlist from its expanded entries");
            }
            // The refresh an add queues (collage, rating), as `AddToPlaylistInternal`
            // does, after the edit guard is released, as every other edit queues it.
            drop(edit);
            if let Err(err) = super::queue_refresh(
                self.providers.as_deref(),
                plan.playlist,
                super::force_save_refresh(),
            )
            .await
            {
                tracing::warn!(%err, item_id = %plan.playlist, "could not queue a refresh of a playlist with expanded entries");
            }
        }
        Ok(true)
    }
}

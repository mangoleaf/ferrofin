//! [`FerrofinMusicManager`] — the concrete [`MusicManager`].
//!
//! Port of `Emby.Server.Implementations.Library.MusicManager`. The C# manager
//! builds an "instant mix" — a shuffled playlist of songs related to a seed (a
//! song, album, artist, playlist, folder, or genre set). It leans on the
//! `MusicArtist`/`MusicAlbum`/`Audio` object tree; at this seam the seed is a
//! [`Uuid`], the songs are [`BaseItemEntity`] audio rows served by the injected
//! [`ItemRepository`], and the kind is recovered from the stored CLR type name
//! ([`kind_from_type_name`]) so `GetInstantMixFromItem`'s `is` chain ports
//! one-for-one.
//!
//! There is no similarity scorer to port: every C# arm funnels into
//! `GetInstantMixFromGenreIds`, which is a plain `GenreIds` query ordered
//! `Random` and capped at 200.

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::entities::users::UserEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::{MusicManager, UserManager};
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::ItemRepository;

use crate::item_type_lookup::kind_from_type_name;

/// The maximum number of songs an instant mix returns (the 200-item cap in C#
/// `MusicManager.GetInstantMixFromGenreIds`).
const INSTANT_MIX_LIMIT: i32 = 200;

/// The concrete music manager.
#[derive(Clone)]
pub struct FerrofinMusicManager {
    items: Arc<dyn ItemRepository>,
    users: Option<Arc<dyn UserManager>>,
}

impl std::fmt::Debug for FerrofinMusicManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinMusicManager")
            .finish_non_exhaustive()
    }
}

/// The display genres of a row (its `Genres` column, pipe-split).
fn row_genres(row: &BaseItemEntity) -> Vec<String> {
    row.genres
        .as_deref()
        .map(|g| {
            g.split('|')
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// C# `Jellyfin.Extensions.DistinctNames` — distinct, case-insensitive, first
/// spelling wins, input order preserved.
fn distinct_names(names: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|n| seen.insert(n.to_lowercase()))
        .collect()
}

impl FerrofinMusicManager {
    /// Creates a music manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self { items, users: None }
    }

    /// Attaches the user seam so a mix is scoped to the caller's libraries —
    /// C# builds every instant-mix query as `new InternalItemsQuery(user)`.
    /// Without it (unit-test composition) the mix is unscoped.
    #[must_use]
    pub fn with_users(mut self, users: Arc<dyn UserManager>) -> Self {
        self.users = Some(users);
        self
    }

    /// Resolves the query's `user` row, when both a user seam and an id exist.
    async fn user_row(&self, user_id: Option<Uuid>) -> Result<Option<UserEntity>, ServiceError> {
        let (Some(users), Some(id)) = (self.users.as_ref(), user_id) else {
            return Ok(None);
        };
        users.get_user_by_id(id).await
    }

    /// Port of C# `GetInstantMixFromGenreIds`: audio items carrying any of
    /// `genre_ids`, shuffled and capped. An empty id list means no genre
    /// predicate — the same "whole audio library" answer the C# gives when no
    /// genre resolves.
    async fn mix_from_genre_ids(
        &self,
        genre_ids: &[Uuid],
        user: Option<UserEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let query = InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            genre_ids: genre_ids.to_vec(),
            user,
            recursive: true,
            limit: Some(INSTANT_MIX_LIMIT),
            order_by: vec![(ItemSortBy::Random, SortOrder::Ascending)],
            ..Default::default()
        };
        self.items.get_item_list(&query).await
    }

    /// The `MusicGenre` item ids for `genres` — C#
    /// `genres.DistinctNames().Select(i => _libraryManager.GetMusicGenre(i).Id)`,
    /// resolved here by the same cleaned-name match the by-name lookup uses. A
    /// name with no materialized row drops out (the C# `catch` arm).
    async fn music_genre_ids(&self, genres: &[String]) -> Result<Vec<Uuid>, ServiceError> {
        let names = distinct_names(genres.to_vec());
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .items
            .get_item_list(&InternalItemsQuery {
                names,
                include_item_types: vec![BaseItemKind::MusicGenre],
                ..InternalItemsQuery::default()
            })
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| Uuid::parse_str(&r.id).ok())
            .collect())
    }

    /// A mix seeded by a row's own display genres — the shared body of C#
    /// `GetInstantMixFromAlbum`/`GetInstantMixFromArtist`/`GetInstantMixFromPlaylist`.
    async fn mix_from_row_genres(
        &self,
        seed: &BaseItemEntity,
        user: Option<UserEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let genre_ids = self.music_genre_ids(&row_genres(seed)).await?;
        self.mix_from_genre_ids(&genre_ids, user).await
    }

    /// Port of C# `GetInstantMixFromFolder`: the distinct genres of the
    /// folder's recursive `Audio` children, unioned with the folder's own.
    async fn mix_from_folder(
        &self,
        seed: &BaseItemEntity,
        seed_id: Uuid,
        user: Option<UserEntity>,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let children = self
            .items
            .get_item_list(&InternalItemsQuery {
                ancestor_ids: vec![seed_id],
                include_item_types: vec![BaseItemKind::Audio],
                recursive: true,
                user: user.clone(),
                ..InternalItemsQuery::default()
            })
            .await?;
        let mut genres: Vec<String> = children.iter().flat_map(row_genres).collect();
        genres.extend(row_genres(seed));
        let genre_ids = self.music_genre_ids(&genres).await?;
        self.mix_from_genre_ids(&genre_ids, user).await
    }
}

#[async_trait]
impl MusicManager for FerrofinMusicManager {
    async fn get_instant_mix_from_item(
        &self,
        item_id: Uuid,
        user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // Port of `GetInstantMixFromItem`'s `is` chain, in the same order.
        // The seed's KIND decides how the mix is seeded — reading its `Genres`
        // column unconditionally is what made a MusicGenre seed (whose own
        // column is empty) return the entire audio library.
        let Some(seed) = self.items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        let user = self.user_row(user_id).await?;
        match kind_from_type_name(&seed.type_) {
            // A genre item stands for itself: filter on its own id.
            Some(BaseItemKind::MusicGenre) => self.mix_from_genre_ids(&[item_id], user).await,
            Some(BaseItemKind::Playlist | BaseItemKind::MusicAlbum | BaseItemKind::MusicArtist) => {
                self.mix_from_row_genres(&seed, user).await
            }
            Some(BaseItemKind::Audio) => {
                // `GetInstantMixFromSong` PREPENDS the seed and de-duplicates
                // it out of the tail — it does not drop it.
                let mut mix = self.mix_from_row_genres(&seed, user).await?;
                let seed_key = guid_to_db(item_id);
                mix.retain(|row| row.id != seed_key);
                mix.insert(0, seed);
                Ok(mix)
            }
            _ if seed.is_folder => self.mix_from_folder(&seed, item_id, user).await,
            // C# falls off the end of the chain with an empty list — never
            // with "every song in the library".
            _ => Ok(Vec::new()),
        }
    }

    async fn get_instant_mix_from_artist(
        &self,
        artist_id: Uuid,
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // `GetInstantMixFromArtist` is the MusicArtist arm of the dispatch.
        self.get_instant_mix_from_item(artist_id, user_id, dto_options)
            .await
    }

    async fn get_instant_mix_from_genres(
        &self,
        genres: &[String],
        user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let user = self.user_row(user_id).await?;
        let genre_ids = self.music_genre_ids(genres).await?;
        self.mix_from_genre_ids(&genre_ids, user).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item_repository::FerrofinItemRepository;
    use crate::item_type_lookup::ItemTypeLookup;
    use crate::test_support::{seed_item, seed_item_genre, seed_named_item, test_db};
    use ferrofin_db::Database;

    fn manager(db: &Database) -> FerrofinMusicManager {
        let lookup: Arc<dyn ferrofin_traits::persistence::ItemTypeLookup> =
            Arc::new(ItemTypeLookup::new());
        FerrofinMusicManager::new(Arc::new(FerrofinItemRepository::new(db.clone(), lookup)))
    }

    /// Writes the row's display `Genres` column — the seed the C# reads through
    /// `item.Genres` before resolving it to genre ids.
    async fn set_genres(db: &Database, id: Uuid, genres: &str) {
        sqlx::query(r#"UPDATE "BaseItems" SET "Genres" = ?2 WHERE "Id" = ?1"#)
            .bind(guid_to_db(id))
            .bind(genres)
            .execute(db.writer())
            .await
            .expect("set genres");
    }

    /// Seeds an audio row and attaches a genre both on the row and through
    /// `ItemValues` — the latter is what materializes the browsable
    /// `MusicGenre` row the mix query's `GenreIds` filter resolves against.
    async fn seed_song(db: &Database, id: Uuid, name: &str, genre: &str) {
        seed_named_item(db, id, BaseItemKind::Audio, name).await;
        set_genres(db, id, genre).await;
        seed_item_genre(db, id, genre).await;
    }

    #[tokio::test]
    async fn instant_mix_from_genres_returns_matching_songs() {
        let db = test_db().await;
        // Ids avoid 1 (the query translator's placeholder row id).
        seed_song(&db, Uuid::from_u128(0x101), "Song A", "Jazz").await;
        seed_song(&db, Uuid::from_u128(0x102), "Song B", "Jazz").await;
        // A non-audio item is excluded.
        seed_item(&db, Uuid::from_u128(0x103), BaseItemKind::Movie).await;
        let mgr = manager(&db);

        let mix = mgr
            .get_instant_mix_from_genres(&["Jazz".to_owned()], None, &DtoOptions::default())
            .await
            .expect("mix");
        assert_eq!(mix.len(), 2);
        assert!(mix.iter().all(|r| r.type_.contains("Audio")));
    }

    #[tokio::test]
    async fn instant_mix_from_song_prepends_the_seed() {
        let db = test_db().await;
        let seed = Uuid::from_u128(0x101);
        seed_song(&db, seed, "Seed", "Rock").await;
        seed_song(&db, Uuid::from_u128(0x102), "Other", "Rock").await;
        let mgr = manager(&db);

        let mix = mgr
            .get_instant_mix_from_item(seed, None, &DtoOptions::default())
            .await
            .expect("mix");
        // C# `GetInstantMixFromSong` returns `[item, ..rest.Where(id != item)]`.
        assert_eq!(mix.first().map(|r| r.id.clone()), Some(guid_to_db(seed)));
        assert_eq!(
            mix.iter().filter(|r| r.id == guid_to_db(seed)).count(),
            1,
            "the seed appears exactly once"
        );
        assert_eq!(mix.len(), 2);
    }

    #[tokio::test]
    async fn instant_mix_from_music_genre_filters_to_that_genre() {
        let db = test_db().await;
        seed_song(&db, Uuid::from_u128(0x101), "Rock A", "Rock").await;
        seed_song(&db, Uuid::from_u128(0x102), "Rock B", "Rock").await;
        seed_song(&db, Uuid::from_u128(0x103), "Jazz A", "Jazz").await;
        let mgr = manager(&db);
        // The browsable MusicGenre row the scanner materialized for "Rock".
        let rock = mgr
            .items
            .get_item_list(&InternalItemsQuery {
                names: vec!["Rock".to_owned()],
                include_item_types: vec![BaseItemKind::MusicGenre],
                ..InternalItemsQuery::default()
            })
            .await
            .expect("genre row");
        let rock_id =
            Uuid::parse_str(&rock.first().expect("a Rock MusicGenre row").id).expect("id");

        let mix = mgr
            .get_instant_mix_from_item(rock_id, None, &DtoOptions::default())
            .await
            .expect("mix");
        // The regression this guards: a MusicGenre row's own `Genres` column is
        // empty, so seeding from it returned the WHOLE audio library.
        let mut names: Vec<&str> = mix.iter().filter_map(|r| r.name.as_deref()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Rock A", "Rock B"]);
    }

    #[tokio::test]
    async fn instant_mix_from_an_unsupported_kind_is_empty() {
        let db = test_db().await;
        seed_song(&db, Uuid::from_u128(0x101), "Song A", "Jazz").await;
        let movie = Uuid::from_u128(0x103);
        seed_item(&db, movie, BaseItemKind::Movie).await;
        let mgr = manager(&db);

        // C# falls off the end of the `is` chain with an empty list.
        let mix = mgr
            .get_instant_mix_from_item(movie, None, &DtoOptions::default())
            .await
            .expect("mix");
        assert!(mix.is_empty(), "a non-music, non-folder seed mixes nothing");
    }

    #[tokio::test]
    async fn instant_mix_from_album_uses_the_album_genres() {
        let db = test_db().await;
        seed_song(&db, Uuid::from_u128(0x101), "Rock A", "Rock").await;
        seed_song(&db, Uuid::from_u128(0x102), "Jazz A", "Jazz").await;
        let album = Uuid::from_u128(0x104);
        seed_named_item(&db, album, BaseItemKind::MusicAlbum, "Album").await;
        set_genres(&db, album, "Rock").await;
        let mgr = manager(&db);

        let mix = mgr
            .get_instant_mix_from_item(album, None, &DtoOptions::default())
            .await
            .expect("mix");
        let names: Vec<&str> = mix.iter().filter_map(|r| r.name.as_deref()).collect();
        assert_eq!(names, vec!["Rock A"]);
    }
}

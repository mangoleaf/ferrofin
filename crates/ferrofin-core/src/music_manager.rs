//! [`FerrofinMusicManager`] — the concrete [`MusicManager`].
//!
//! Port of `Emby.Server.Implementations.Library.MusicManager` (the object-safe
//! subset). The C# manager builds an "instant mix" — a shuffled playlist of songs
//! related to a seed (a song, album, artist, or genre set). It leans on the
//! `MusicArtist`/`MusicAlbum`/`Audio` object tree and a genre-similarity scorer;
//! at this seam the seed is a [`Uuid`] (or genre names), the songs are
//! [`BaseItemEntity`] audio rows served by the injected [`ItemRepository`], and
//! the mix is "audio items sharing the seed's genres, newest-first, capped".
//!
//! The genre-weighted similarity ranking the C# code performs (Jaccard over genre
//! sets, then random tie-break) is simplified to a genre-overlap filter here; the
//! richer scorer is noted deferred. The cap mirrors C#
//! `MusicManager.GetInstantMixFromGenres` (200 items).

use std::sync::Arc;

use async_trait::async_trait;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::{BaseItemKind, MediaType};
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::library::MusicManager;
use ferrofin_traits::options::{DtoOptions, InternalItemsQuery};
use ferrofin_traits::persistence::ItemRepository;

/// The maximum number of songs an instant mix returns (C#
/// `InstantMixLimit` / the 200-item cap in `GetInstantMixFromGenres`).
const INSTANT_MIX_LIMIT: i32 = 200;

/// The concrete music manager.
#[derive(Clone)]
pub struct FerrofinMusicManager {
    items: Arc<dyn ItemRepository>,
}

impl std::fmt::Debug for FerrofinMusicManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinMusicManager")
            .finish_non_exhaustive()
    }
}

impl FerrofinMusicManager {
    /// Creates a music manager over the injected item repository.
    #[must_use]
    pub fn new(items: Arc<dyn ItemRepository>) -> Self {
        Self { items }
    }

    /// Builds the audio-item query for an instant mix scoped to `genres`
    /// (empty = any genre), newest-first and capped.
    ///
    /// Per-user visibility scoping (the C# `user` filter) is deferred here, as in
    /// the other library managers; the mix is genre-scoped over the whole audio
    /// library.
    fn instant_mix_query(genres: &[String]) -> InternalItemsQuery {
        InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            media_types: vec![MediaType::Audio],
            genres: genres.to_vec(),
            recursive: true,
            limit: Some(INSTANT_MIX_LIMIT),
            order_by: vec![(ItemSortBy::DateCreated, SortOrder::Descending)],
            ..Default::default()
        }
    }

    /// Reads the display genres of a seed item (its `Genres` column, pipe-split).
    async fn seed_genres(&self, seed_id: Uuid) -> Result<Vec<String>, ServiceError> {
        let Some(item) = self.items.retrieve_item(seed_id).await? else {
            return Ok(Vec::new());
        };
        Ok(item
            .genres
            .as_deref()
            .map(|g| {
                g.split('|')
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait]
impl MusicManager for FerrofinMusicManager {
    async fn get_instant_mix_from_item(
        &self,
        item_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let genres = self.seed_genres(item_id).await?;
        let query = Self::instant_mix_query(&genres);
        let mut mix = self.items.get_item_list(&query).await?;
        // The seed itself should not appear in its own mix. Compare in the
        // canonical stored GUID form — entity ids come back as stored TEXT.
        mix.retain(|row| row.id != guid_to_db(item_id));
        Ok(mix)
    }

    async fn get_instant_mix_from_artist(
        &self,
        artist_id: Uuid,
        user_id: Option<Uuid>,
        dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        // An artist seed uses the artist's genres, same as any other seed row.
        self.get_instant_mix_from_item(artist_id, user_id, dto_options)
            .await
    }

    async fn get_instant_mix_from_genres(
        &self,
        genres: &[String],
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let query = Self::instant_mix_query(genres);
        self.items.get_item_list(&query).await
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

    /// Seeds an audio row, sets its media type, and attaches a genre value (both
    /// on the row and through `ItemValues`, since the mix query filters on the
    /// latter).
    async fn seed_song(db: &Database, id: Uuid, name: &str, genre: &str) {
        seed_named_item(db, id, BaseItemKind::Audio, name).await;
        sqlx::query(
            r#"UPDATE "BaseItems" SET "MediaType" = 'Audio', "Genres" = ?2 WHERE "Id" = ?1"#,
        )
        .bind(guid_to_db(id))
        .bind(genre)
        .execute(db.writer())
        .await
        .expect("set song fields");
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
    async fn instant_mix_from_item_excludes_the_seed() {
        let db = test_db().await;
        let seed = Uuid::from_u128(0x101);
        seed_song(&db, seed, "Seed", "Rock").await;
        seed_song(&db, Uuid::from_u128(0x102), "Other", "Rock").await;
        let mgr = manager(&db);

        let mix = mgr
            .get_instant_mix_from_item(seed, None, &DtoOptions::default())
            .await
            .expect("mix");
        assert!(mix.iter().all(|r| r.id != guid_to_db(seed)));
        assert_eq!(mix.len(), 1);
    }
}

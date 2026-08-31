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
//! There is no genre-weighted similarity ranking to port: C#
//! `GetInstantMixFromGenreIds` is a plain `IncludeItemTypes = [Audio]`,
//! `GenreIds = …`, `Limit = 200`, `OrderBy = [(Random, Ascending)]` query, which
//! is what this builds. The cap mirrors that 200-item limit.

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

use crate::item_type_lookup::kind_from_type_name;

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
            // C# `GetInstantMixFromGenreIds`:
            // `OrderBy = [(ItemSortBy.Random, SortOrder.Ascending)]`.
            // A `DateCreated DESC` mix is not a mix — it hands back the same
            // newest-first list on every call, so "instant mix" never shuffled.
            order_by: vec![(ItemSortBy::Random, SortOrder::Ascending)],
            ..Default::default()
        }
    }

    /// The instant-mix query for an explicit set of `MusicGenre` **ids** — the
    /// shape C# `GetInstantMixFromGenreIds` is given directly when the seed is
    /// itself a `MusicGenre`.
    fn instant_mix_query_by_genre_ids(genre_ids: &[Uuid]) -> InternalItemsQuery {
        InternalItemsQuery {
            include_item_types: vec![BaseItemKind::Audio],
            media_types: vec![MediaType::Audio],
            genre_ids: genre_ids.to_vec(),
            recursive: true,
            limit: Some(INSTANT_MIX_LIMIT),
            order_by: vec![(ItemSortBy::Random, SortOrder::Ascending)],
            ..Default::default()
        }
    }

    /// The distinct genres of every `Audio` descendant of `folder_id`, unioned
    /// with the folder's own — C# `GetInstantMixFromFolder`, which concatenates
    /// `GetRecursiveChildren(..., IncludeItemTypes = [Audio]).SelectMany(i =>
    /// i.Genres)` with `item.Genres` and takes `DistinctNames()`.
    async fn folder_genres(
        &self,
        folder_id: Uuid,
        own: Vec<String>,
    ) -> Result<Vec<String>, ServiceError> {
        let children = self
            .items
            .get_item_list(&InternalItemsQuery {
                include_item_types: vec![BaseItemKind::Audio],
                media_types: vec![MediaType::Audio],
                parent_id: folder_id,
                recursive: true,
                ..Default::default()
            })
            .await?;
        let mut genres = Vec::new();
        for row in &children {
            genres.extend(split_genres(row.genres.as_deref()));
        }
        genres.extend(own);
        // `DistinctNames()` is case-insensitive on the name; keep first-seen order.
        let mut seen: Vec<String> = Vec::new();
        genres.retain(|g| {
            let lower = g.to_lowercase();
            if seen.contains(&lower) {
                false
            } else {
                seen.push(lower);
                true
            }
        });
        Ok(genres)
    }
}

/// Splits a `BaseItems."Genres"` cell into its display genre names.
fn split_genres(cell: Option<&str>) -> Vec<String> {
    cell.map(|g| {
        g.split('|')
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

#[async_trait]
impl MusicManager for FerrofinMusicManager {
    async fn get_instant_mix_from_item(
        &self,
        item_id: Uuid,
        _user_id: Option<Uuid>,
        _dto_options: &DtoOptions,
    ) -> Result<Vec<BaseItemEntity>, ServiceError> {
        let Some(seed) = self.items.retrieve_item(item_id).await? else {
            return Ok(Vec::new());
        };
        let kind = kind_from_type_name(&seed.type_);
        let own_genres = split_genres(seed.genres.as_deref());

        // Port of the C# `GetInstantMixFromItem` type ladder. Each arm differs;
        // the previous code ran the Playlist/Album/Artist arm for every kind,
        // which made a `MusicGenre` seed read its own (empty) `Genres` column
        // and return an unfiltered all-audio mix, and dropped the seed from a
        // song mix that upstream deliberately puts first.
        match kind {
            // `item is MusicGenre` => GetInstantMixFromGenreIds([item.Id]).
            Some(BaseItemKind::MusicGenre) => {
                let query = Self::instant_mix_query_by_genre_ids(&[item_id]);
                self.items.get_item_list(&query).await
            }
            // `item is Audio song` => [item, ..mix.Where(i => i.Id != item.Id)].
            // The seed leads its own mix; it is not dropped.
            Some(BaseItemKind::Audio | BaseItemKind::AudioBook) => {
                let query = Self::instant_mix_query(&own_genres);
                let mut mix = self.items.get_item_list(&query).await?;
                // Compare in the canonical stored GUID form — entity ids come
                // back as stored TEXT.
                mix.retain(|row| row.id != guid_to_db(item_id));
                mix.insert(0, seed);
                Ok(mix)
            }
            // Playlist / MusicAlbum / MusicArtist: the row's own genres.
            Some(BaseItemKind::Playlist | BaseItemKind::MusicAlbum | BaseItemKind::MusicArtist) => {
                let query = Self::instant_mix_query(&own_genres);
                self.items.get_item_list(&query).await
            }
            // `item is Folder folder` => the recursive Audio children's genres
            // unioned with the folder's own. Checked last, as in the C# ladder,
            // so the folder-ish music kinds above take their own arm first.
            _ if seed.is_folder => {
                let genres = self.folder_genres(item_id, own_genres).await?;
                let query = Self::instant_mix_query(&genres);
                self.items.get_item_list(&query).await
            }
            // C# falls off the end with `return new List<BaseItem>();`.
            _ => Ok(Vec::new()),
        }
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
    use crate::test_support::{
        seed_item, seed_item_genre, seed_named_item, set_clean_name, test_db,
    };
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

    /// C# `GetInstantMixFromSong`: `[item, .. mix.Where(i => i.Id != item.Id)]`
    /// — the seed LEADS its own mix and appears exactly once.
    #[tokio::test]
    async fn instant_mix_from_song_puts_the_seed_first_exactly_once() {
        let db = test_db().await;
        let seed = Uuid::from_u128(0x101);
        seed_song(&db, seed, "Seed", "Rock").await;
        seed_song(&db, Uuid::from_u128(0x102), "Other", "Rock").await;
        let mgr = manager(&db);

        let mix = mgr
            .get_instant_mix_from_item(seed, None, &DtoOptions::default())
            .await
            .expect("mix");
        assert_eq!(mix.len(), 2);
        assert_eq!(mix[0].id, guid_to_db(seed));
        assert_eq!(
            mix.iter().filter(|r| r.id == guid_to_db(seed)).count(),
            1,
            "the seed must not be duplicated"
        );
    }

    /// C# `GetInstantMixFromItem`: `if (item is MusicGenre) return
    /// GetInstantMixFromGenreIds([item.Id], …)` — the genre's OWN id seeds the
    /// filter. Reading the genre row's (empty) `Genres` column instead returned
    /// an unfiltered all-audio mix.
    #[tokio::test]
    async fn instant_mix_from_music_genre_filters_by_that_genre() {
        let db = test_db().await;
        seed_song(&db, Uuid::from_u128(0x201), "Jazz Song", "Jazz").await;
        seed_song(&db, Uuid::from_u128(0x202), "Rock Song", "Rock").await;
        // The by-name `MusicGenre` row the seed id points at; `genre_ids`
        // resolves it to its `CleanName` and matches the songs' `ItemValues`.
        let genre_id = Uuid::from_u128(0x203);
        seed_named_item(&db, genre_id, BaseItemKind::MusicGenre, "Jazz").await;
        set_clean_name(&db, genre_id, "Jazz").await;
        let mgr = manager(&db);

        let mix = mgr
            .get_instant_mix_from_item(genre_id, None, &DtoOptions::default())
            .await
            .expect("mix");
        let names: Vec<&str> = mix.iter().filter_map(|r| r.name.as_deref()).collect();
        assert_eq!(names, ["Jazz Song"], "only the seeded genre's songs");
    }

    /// A seed that is neither music nor a folder falls off the end of the C#
    /// ladder with an empty list.
    #[tokio::test]
    async fn instant_mix_from_a_movie_is_empty() {
        let db = test_db().await;
        let movie = Uuid::from_u128(0x301);
        seed_item(&db, movie, BaseItemKind::Movie).await;
        seed_song(&db, Uuid::from_u128(0x302), "Song", "Rock").await;
        let mgr = manager(&db);

        assert!(
            mgr.get_instant_mix_from_item(movie, None, &DtoOptions::default())
                .await
                .expect("mix")
                .is_empty()
        );
    }
}

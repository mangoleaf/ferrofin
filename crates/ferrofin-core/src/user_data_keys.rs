//! `BaseItem.GetUserDataKeys()`, ported as a free function over
//! [`BaseItemKind`].
//!
//! Every `UserData` row is keyed by an `(ItemId, UserId, CustomDataKey)`
//! triple, and **the key is not the item id**. Jellyfin derives a *list* of
//! keys from the item's metadata — a movie's TMDB id, a series' TVDB id, an
//! episode's series key plus its season/episode numbers — and
//! `UserDataManager.SaveUserData` writes **one row per key**. An adopted
//! database is therefore full of provider-keyed rows, and a server that writes
//! only under the item guid produces user data Jellyfin cannot see: the
//! favourite is stored, read back correctly by the server that wrote it, and
//! silently absent the moment the user swaps back. Measured on a real library:
//! 2,011 items already carrying more than one key.
//!
//! The C# lives on the class hierarchy (`BaseItem` → `Video` → `Episode`, and
//! `Folder` → `Series`/`Season`), which Ferrofin does not have; per the
//! architecture rules this is a free function over the kind, alongside
//! [`crate::kinds`].
//!
//! Ordering is load-bearing. The C# builds each list with `Insert(0, …)`, so
//! **the last provider inserted ends up first**, and `First()` is what a fresh
//! row is keyed by. The tables below are written in final order.
//!
//! | kind | keys, in order | C# |
//! |---|---|---|
//! | anything | `[id]` | `BaseItem.cs:1468` |
//! | `Video` family, no `ExtraType` | `[tmdb, imdb, id]` | `Video.cs:274` |
//! | `Video` family, `ExtraType` set | `[imdb-extra, tmdb-extra, id]` | `Video.cs:280` |
//! | `Series` | `[custom, tvdb, imdb, id]` | `TV/Series.cs:171` |
//! | `Season` | `[<series key>SSS…, id]` | `TV/Season.cs:103` |
//! | `Episode` | `[<series key>SSSEEE…, id]` | `TV/Episode.cs:158` |
//! | `Audio`, `AudioBook` | `[<artist>-<album>-PPPP-EEEE<name>, id]` | `Audio/Audio.cs:100` |
//! | `MusicAlbum` | `[MusicAlbum-MusicBrainzReleaseGroup-…, MusicAlbum-Musicbrainz-…, <artist>-<name>, id]` | `Audio/MusicAlbum.cs:99` |
//! | `MusicArtist` | `[Artist-Musicbrainz-…, Artist-<name>, id]` | `Audio/MusicArtist.cs:127` |
//! | `Genre`/`MusicGenre`/`Person`/`Studio` | `[<TypeName>-<name>, id]` | `Genre.cs:37`, `Person.cs:40`, … |
//! | `Year` | `[Year-<name>, id]` | `Year.cs:38` |
//! | `Program` | `[imdb/tmdb…, id]` or `[Program-<name><episodeTitle>, id]` | `LiveTv/LiveTvProgram.cs:165` |
//!
//! A **live TV channel** is keyed by its id alone. `LiveTvChannel.cs:91` would
//! prepend `TvChannel-<Name>`, but only when
//! `DisableLiveTvChannelUserDataName` is false — and that setting defaults to
//! **true** (`ServerConfiguration.cs:91`), so a stock server never writes that
//! key and would not read one. Emitting it would put a key Jellyfin ignores at
//! the head of the list.
//!
//! The `<name>` forms are **diacritic-stripped** (`RemoveDiacritics`), so
//! "Beyoncé" and "Beyonce" share a row. `Year` is the exception and is not.
//!
//! Three traps, all verified against `origin/release-10.11.z` rather than
//! inferred:
//!
//! - **An `Episode` gets no provider keys of its own.** `Episode` extends
//!   `Video`, but overrides `EnableDefaultVideoUserDataKeys => false`
//!   (`TV/Episode.cs:70`), so `Video`'s TMDB/IMDb branch is skipped entirely.
//!   Its keys come from the *series*.
//! - **An `Episode` drops the series' last key**, which is the series' own
//!   guid — `take--` when the series has more than one key. A `Season` keeps
//!   all of them. The two are otherwise near-identical, which is exactly why
//!   they are easy to conflate.
//! - **The extra branch orders its providers the opposite way** to the plain
//!   one. Both insert at 0, but the plain branch inserts imdb then tmdb (tmdb
//!   leads) while the extra branch inserts tmdb then imdb (**imdb** leads).
//!   Reading one and assuming the other is a silent mis-key.
//!
//! **Known omission:** `BaseItem.cs:1472` prepends `ExternalId` when
//! `SourceType == Channel`. The pinned 10.11.8 schema has no `SourceType`
//! column — it lives inside the `Data` blob — and Ferrofin has no channel
//! plugin that would set it, so there is nothing to read and no row that could
//! carry such a key. Stated rather than left invisible; if channel items ever
//! land, this is where their key belongs.

use ferrofin_model::data::BaseItemKind;
use ferrofin_util::string_extensions::remove_diacritics;
use uuid::Uuid;

use crate::kinds::is_video;

/// A provider id that is actually present.
///
/// `BaseItemProviders."ProviderValue"` is `NOT NULL` but can be `''`, and the
/// C# reads these through `TryGetProviderId`/`GetProviderId`, which treat an
/// empty string as absent (`string.IsNullOrEmpty`). Without this an empty
/// value becomes an empty `CustomDataKey` — a row that matches nothing and
/// collides with every other item's empty key.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// Whether [`user_data_keys`] reads any provider id for this kind.
///
/// Lets a caller skip the `BaseItemProviders` query for the kinds that cannot
/// use it — which is most of them, and notably `Episode`, whose keys come from
/// its series. Keep in step with the match in [`user_data_keys`].
#[must_use]
pub fn uses_provider_ids(kind: BaseItemKind) -> bool {
    match kind {
        BaseItemKind::Series
        | BaseItemKind::MusicAlbum
        | BaseItemKind::MusicArtist
        | BaseItemKind::Program
        | BaseItemKind::LiveTvProgram
        | BaseItemKind::TvProgram => true,
        // The Video family reads tmdb/imdb — except Episode, which is handled
        // by its own arm before the `is_video` one is ever reached.
        BaseItemKind::Episode => false,
        kind => is_video(kind),
    }
}

/// The item fields the key derivation reads.
///
/// A flat borrow rather than an entity, so the caller decides where the values
/// come from (a `BaseItems` row today, a scan-time struct tomorrow) and the
/// derivation stays a pure function that is trivial to test.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeySource<'a> {
    /// The item's own id.
    pub item_id: Uuid,
    /// What the item is.
    pub kind: BaseItemKind,
    /// `Tmdb` provider id, when the item carries one.
    pub tmdb: Option<&'a str>,
    /// `Imdb` provider id, when the item carries one.
    pub imdb: Option<&'a str>,
    /// `Tvdb` provider id — only a `Series` uses it.
    pub tvdb: Option<&'a str>,
    /// `Custom` provider id — only a `Series` uses it, and it outranks the rest.
    pub custom: Option<&'a str>,
    /// `MusicBrainzAlbum` provider id, for a `MusicAlbum`.
    pub musicbrainz_album: Option<&'a str>,
    /// `MusicBrainzReleaseGroup` provider id, for a `MusicAlbum`.
    pub musicbrainz_release_group: Option<&'a str>,
    /// `MusicBrainzArtist` provider id, for a `MusicArtist`.
    pub musicbrainz_artist: Option<&'a str>,
    /// The episode title, for a Live TV `Program` that is part of a series.
    pub episode_title: Option<&'a str>,
    /// Whether a Live TV `Program` belongs to a series, which picks between its
    /// two key shapes.
    pub is_series: bool,
    /// The episode/track number.
    pub index_number: Option<i64>,
    /// The season/disc number.
    pub parent_index_number: Option<i64>,
    /// The item's name, for the `Audio` composite key.
    pub name: Option<&'a str>,
    /// The track's album, for the `Audio` composite key.
    pub album: Option<&'a str>,
    /// The track's first album artist, for the `Audio` composite key.
    pub album_artist: Option<&'a str>,
    /// The extra kind (`trailer`, `behindthescenes`, …) when this is an extra,
    /// already lowercased as `ExtraType.ToString().ToLowerInvariant()` yields.
    pub extra_type: Option<&'a str>,
    /// Runtime ticks, which disambiguate two extras of the same kind.
    pub run_time_ticks: Option<i64>,
}

/// The item's `UserData` keys, most specific first.
///
/// `series` is the item's series for a `Season` or an `Episode`, and ignored
/// otherwise; pass `None` when it cannot be resolved, which degrades to the
/// item's own id exactly as the C# does when `Series` is null.
///
/// Never returns an empty vector — the item id is always present as the last
/// entry, so a caller can always key a fresh row.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one arm per C# override, each a few lines; splitting them into \
              helpers would scatter a table that is only readable as a whole"
)]
pub fn user_data_keys(item: &KeySource<'_>, series: Option<&KeySource<'_>>) -> Vec<String> {
    // `Guid.ToString()` with no format is "D": lowercase, hyphenated. This is
    // the string an adopted database already holds for guid-keyed rows, so the
    // formatting is a compatibility detail, not a style choice.
    let own = item.item_id.to_string();

    match item.kind {
        BaseItemKind::Season => {
            let mut keys =
                series_derived(series, &format!("{:03}", item.index_number.unwrap_or(0)));
            keys.push(own);
            keys
        }
        BaseItemKind::Episode => {
            // Both numbers required: without them there is no suffix to build
            // and the C# skips the whole branch.
            let (Some(season), Some(episode)) = (item.parent_index_number, item.index_number)
            else {
                return vec![own];
            };
            let mut keys =
                series_derived_dropping_own_id(series, &format!("{season:03}{episode:03}"));
            keys.push(own);
            keys
        }
        // `AudioBook : Audio` with no override of its own, so it takes the
        // same composite key — and it is the one kind whose resume state the
        // user-data manager special-cases, so losing its row is conspicuous.
        BaseItemKind::Audio | BaseItemKind::AudioBook => vec![audio_key(item), own],
        BaseItemKind::MusicAlbum => {
            let mut keys = Vec::with_capacity(4);
            // Insert(0, "<artist>-<name>"), then Musicbrainz, then
            // ReleaseGroup -> ReleaseGroup leads.
            if let Some(rg) = nonempty(item.musicbrainz_release_group) {
                keys.push(format!("MusicAlbum-MusicBrainzReleaseGroup-{rg}"));
            }
            if let Some(mb) = nonempty(item.musicbrainz_album) {
                keys.push(format!("MusicAlbum-Musicbrainz-{mb}"));
            }
            // NOT diacritic-stripped: the C# concatenates the raw strings here.
            // The name may be absent — upstream gates only on the artist, and a
            // null Name concatenates to "<artist>-".
            if let Some(artist) = nonempty(item.album_artist) {
                keys.push(format!("{artist}-{}", item.name.unwrap_or_default()));
            }
            keys.push(own);
            keys
        }
        BaseItemKind::MusicArtist => {
            // `InsertRange(0, [mbid?, "Artist-<name>"])` keeps that pair's own
            // order, so the MusicBrainz id leads and the name follows.
            let mut keys = Vec::with_capacity(3);
            if let Some(mb) = nonempty(item.musicbrainz_artist) {
                keys.push(format!("Artist-Musicbrainz-{mb}"));
            }
            keys.push(format!(
                "Artist-{}",
                remove_diacritics(item.name.unwrap_or_default())
            ));
            keys.push(own);
            keys
        }
        kind @ (BaseItemKind::Genre
        | BaseItemKind::MusicGenre
        | BaseItemKind::Person
        | BaseItemKind::Studio) => {
            // `GetType().Name + "-" + Name.RemoveDiacritics()`. The C# class
            // name is the kind's own name for these four, so `Debug` is the
            // type name — asserted per kind in the tests rather than assumed.
            vec![
                format!(
                    "{kind:?}-{}",
                    remove_diacritics(item.name.unwrap_or_default())
                ),
                own,
            ]
        }
        BaseItemKind::Year => {
            // The one name-keyed kind that does NOT strip diacritics — a year's
            // name is digits, so upstream never needed to.
            vec![format!("Year-{}", item.name.unwrap_or_default()), own]
        }
        // `LiveTvProgram` and `TvProgram` share one stored type name and
        // `kind_from_type_name` resolves it to `LiveTvProgram` (its table hits
        // that entry first), so all three are listed — a `Program`-only arm
        // was unreachable from the database.
        BaseItemKind::Program | BaseItemKind::LiveTvProgram | BaseItemKind::TvProgram => {
            let mut keys = Vec::with_capacity(3);
            if item.is_series {
                if let Some(episode_title) = nonempty(item.episode_title) {
                    keys.push(format!(
                        "Program-{}{episode_title}",
                        item.name.unwrap_or_default()
                    ));
                }
            } else {
                // Same order as the plain Video branch: imdb then tmdb inserted
                // at 0, so tmdb leads.
                if let Some(tmdb) = nonempty(item.tmdb) {
                    keys.push(tmdb.to_owned());
                }
                if let Some(imdb) = nonempty(item.imdb) {
                    keys.push(imdb.to_owned());
                }
            }
            keys.push(own);
            keys
        }
        BaseItemKind::Series => {
            let mut keys = Vec::with_capacity(4);
            // Insert(0, imdb), Insert(0, tvdb), Insert(0, custom) -> custom leads.
            if let Some(custom) = nonempty(item.custom) {
                keys.push(custom.to_owned());
            }
            if let Some(tvdb) = nonempty(item.tvdb) {
                keys.push(tvdb.to_owned());
            }
            if let Some(imdb) = nonempty(item.imdb) {
                keys.push(imdb.to_owned());
            }
            keys.push(own);
            keys
        }
        kind if is_video(kind) => {
            let mut keys = Vec::with_capacity(3);
            if let Some(extra) = item.extra_type {
                // OPPOSITE order to the plain branch below. Upstream inserts
                // tmdb at 0 and then imdb at 0, so IMDb ends up first here
                // while TMDB ends up first there. Not a typo — checked twice.
                if let Some(imdb) = nonempty(item.imdb) {
                    keys.push(extra_key(imdb, extra, item.run_time_ticks));
                }
                if let Some(tmdb) = nonempty(item.tmdb) {
                    keys.push(extra_key(tmdb, extra, item.run_time_ticks));
                }
            } else {
                // Insert(0, imdb) then Insert(0, tmdb) -> tmdb leads.
                if let Some(tmdb) = nonempty(item.tmdb) {
                    keys.push(tmdb.to_owned());
                }
                if let Some(imdb) = nonempty(item.imdb) {
                    keys.push(imdb.to_owned());
                }
            }
            keys.push(own);
            keys
        }
        _ => vec![own],
    }
}

/// Every series key with `suffix` appended — a `Season`'s derivation.
fn series_derived(series: Option<&KeySource<'_>>, suffix: &str) -> Vec<String> {
    let Some(series) = series else {
        return Vec::new();
    };
    user_data_keys(series, None)
        .into_iter()
        .map(|k| k + suffix)
        .collect()
}

/// The series keys with `suffix` appended, **dropping the last** when the
/// series has more than one — an `Episode`'s derivation.
///
/// That last key is the series' own guid. A `Season` keeps it; the asymmetry is
/// upstream's (`take--`), and reproducing it is what makes an adopted episode's
/// watch state land on the row Jellyfin reads.
fn series_derived_dropping_own_id(series: Option<&KeySource<'_>>, suffix: &str) -> Vec<String> {
    let Some(series) = series else {
        return Vec::new();
    };
    let mut keys = user_data_keys(series, None);
    if keys.len() > 1 {
        keys.pop();
    }
    keys.into_iter().map(|k| k + suffix).collect()
}

/// `Video.GetUserDataKey` — an extra's provider key, disambiguated by runtime
/// so two trailers of the same film do not share a row.
fn extra_key(provider_id: &str, extra_type: &str, run_time_ticks: Option<i64>) -> String {
    let mut key = format!("{provider_id}-{extra_type}");
    if let Some(ticks) = run_time_ticks {
        key.push('-');
        key.push_str(&ticks.to_string());
    }
    key
}

/// `Audio.GetUserDataKeys`' composite key: album artist, album, disc/track
/// numbers and the track name, so the same song keeps its play count across a
/// re-rip that changes the file's id.
fn audio_key(item: &KeySource<'_>) -> String {
    // The C# builds this inside-out; written here in the order it ends up.
    let mut key = String::new();
    if let Some(artist) = item.album_artist.filter(|a| !a.is_empty()) {
        key.push_str(artist);
        key.push('-');
    }
    if let Some(album) = item.album.filter(|a| !a.is_empty()) {
        key.push_str(album);
        key.push('-');
    }
    if let Some(disc) = item.parent_index_number {
        let _ = std::fmt::Write::write_fmt(&mut key, format_args!("{disc:04}-"));
    }
    if let Some(track) = item.index_number {
        let _ = std::fmt::Write::write_fmt(&mut key, format_args!("{track:04}"));
    }
    if let Some(name) = item.name {
        key.push_str(name);
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn movie<'a>(tmdb: Option<&'a str>, imdb: Option<&'a str>) -> KeySource<'a> {
        KeySource {
            item_id: id(1),
            kind: BaseItemKind::Movie,
            tmdb,
            imdb,
            ..KeySource::default()
        }
    }

    #[test]
    fn a_movie_leads_with_tmdb_then_imdb_then_its_own_id() {
        // The order is what `First()` keys a fresh row by, so it decides which
        // row Jellyfin and Ferrofin agree on. Insert(0, imdb) then
        // Insert(0, tmdb) in the C# means tmdb ends up first.
        let m = movie(Some("700391"), Some("tt12261776"));
        assert_eq!(
            user_data_keys(&m, None),
            vec![
                "700391".to_owned(),
                "tt12261776".to_owned(),
                id(1).to_string()
            ]
        );
    }

    #[test]
    fn a_movie_with_no_providers_is_keyed_by_its_id_alone() {
        assert_eq!(
            user_data_keys(&movie(None, None), None),
            vec![id(1).to_string()]
        );
    }

    #[test]
    fn a_movie_with_only_imdb_still_leads_with_imdb() {
        assert_eq!(
            user_data_keys(&movie(None, Some("tt1")), None),
            vec!["tt1".to_owned(), id(1).to_string()]
        );
    }

    #[test]
    fn a_series_ranks_custom_over_tvdb_over_imdb() {
        let s = KeySource {
            item_id: id(2),
            kind: BaseItemKind::Series,
            imdb: Some("tt99"),
            tvdb: Some("12345"),
            custom: Some("cust"),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&s, None),
            vec![
                "cust".to_owned(),
                "12345".to_owned(),
                "tt99".to_owned(),
                id(2).to_string()
            ]
        );
    }

    fn series<'a>() -> KeySource<'a> {
        KeySource {
            item_id: id(2),
            kind: BaseItemKind::Series,
            tvdb: Some("12345"),
            ..KeySource::default()
        }
    }

    #[test]
    fn a_season_suffixes_every_series_key_and_keeps_the_series_id() {
        let season = KeySource {
            item_id: id(3),
            kind: BaseItemKind::Season,
            index_number: Some(2),
            ..KeySource::default()
        };
        // Series keys are [tvdb, seriesId]; a Season keeps BOTH and suffixes
        // each with the zero-padded season number.
        assert_eq!(
            user_data_keys(&season, Some(&series())),
            vec![
                "12345002".to_owned(),
                format!("{}002", id(2)),
                id(3).to_string()
            ]
        );
    }

    #[test]
    fn an_episode_drops_the_series_own_id_where_a_season_keeps_it() {
        let ep = KeySource {
            item_id: id(4),
            kind: BaseItemKind::Episode,
            parent_index_number: Some(2),
            index_number: Some(7),
            // Provider ids on the episode itself are deliberately set: they
            // must be IGNORED, because Episode disables Video's branch.
            tmdb: Some("nope"),
            imdb: Some("alsonope"),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&ep, Some(&series())),
            vec!["12345002007".to_owned(), id(4).to_string()]
        );
    }

    #[test]
    fn an_episode_without_numbers_falls_back_to_its_own_id() {
        let ep = KeySource {
            item_id: id(4),
            kind: BaseItemKind::Episode,
            parent_index_number: Some(2),
            index_number: None,
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&ep, Some(&series())),
            vec![id(4).to_string()]
        );
    }

    #[test]
    fn an_episode_with_no_series_falls_back_to_its_own_id() {
        let ep = KeySource {
            item_id: id(4),
            kind: BaseItemKind::Episode,
            parent_index_number: Some(1),
            index_number: Some(1),
            ..KeySource::default()
        };
        assert_eq!(user_data_keys(&ep, None), vec![id(4).to_string()]);
    }

    #[test]
    fn an_episode_of_a_provider_less_series_still_derives_one_key() {
        // Series keys are [seriesId] alone, length 1, so `take--` does not
        // apply and the single key IS suffixed. The guard is `> 1`, not `>= 1`.
        let bare = KeySource {
            item_id: id(2),
            kind: BaseItemKind::Series,
            ..KeySource::default()
        };
        let ep = KeySource {
            item_id: id(4),
            kind: BaseItemKind::Episode,
            parent_index_number: Some(1),
            index_number: Some(3),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&ep, Some(&bare)),
            vec![format!("{}001003", id(2)), id(4).to_string()]
        );
    }

    #[test]
    fn a_track_is_keyed_by_artist_album_and_number() {
        let track = KeySource {
            item_id: id(5),
            kind: BaseItemKind::Audio,
            name: Some("Blue in Green"),
            album: Some("Kind of Blue"),
            album_artist: Some("Miles Davis"),
            parent_index_number: Some(1),
            index_number: Some(3),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&track, None),
            vec![
                "Miles Davis-Kind of Blue-0001-0003Blue in Green".to_owned(),
                id(5).to_string()
            ]
        );
    }

    #[test]
    fn an_extra_is_keyed_by_provider_kind_and_runtime() {
        // Two trailers for the same film share a TMDB id; the runtime is what
        // stops them sharing a watch state.
        let trailer = KeySource {
            item_id: id(6),
            kind: BaseItemKind::Trailer,
            tmdb: Some("700391"),
            extra_type: Some("trailer"),
            run_time_ticks: Some(1_200_000_000),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&trailer, None),
            vec!["700391-trailer-1200000000".to_owned(), id(6).to_string()]
        );

        // Without a runtime the disambiguating suffix is simply absent.
        let no_runtime = KeySource {
            run_time_ticks: None,
            ..trailer
        };
        assert_eq!(user_data_keys(&no_runtime, None)[0], "700391-trailer");
    }

    #[test]
    fn the_extra_branch_orders_its_providers_opposite_to_the_plain_one() {
        // Upstream inserts tmdb-then-imdb at position 0 for an extra, and
        // imdb-then-tmdb for a plain video — so IMDb leads for the extra and
        // TMDB leads for the film. Reading one branch and assuming the other
        // silently mis-keys every trailer.
        let plain = movie(Some("700391"), Some("tt12261776"));
        assert_eq!(user_data_keys(&plain, None)[0], "700391");

        let extra = KeySource {
            kind: BaseItemKind::Trailer,
            extra_type: Some("trailer"),
            ..plain
        };
        assert_eq!(
            user_data_keys(&extra, None),
            vec![
                "tt12261776-trailer".to_owned(),
                "700391-trailer".to_owned(),
                id(1).to_string()
            ]
        );
    }

    #[test]
    fn a_by_name_item_is_keyed_by_its_type_and_name() {
        // `GetType().Name + "-" + Name.RemoveDiacritics()`. Getting this wrong
        // means favouriting a person or genre writes a guid-only row Jellyfin
        // never reads — the same data loss as the movie case, on the kinds a
        // user is most likely to favourite by hand.
        for (kind, expected) in [
            (BaseItemKind::Genre, "Genre-Science Fiction"),
            (BaseItemKind::MusicGenre, "MusicGenre-Science Fiction"),
            (BaseItemKind::Person, "Person-Science Fiction"),
            (BaseItemKind::Studio, "Studio-Science Fiction"),
        ] {
            let item = KeySource {
                item_id: id(7),
                kind,
                name: Some("Science Fiction"),
                // Providers present and irrelevant: these kinds never read them.
                tmdb: Some("x"),
                imdb: Some("y"),
                ..KeySource::default()
            };
            assert_eq!(
                user_data_keys(&item, None),
                vec![expected.to_owned(), id(7).to_string()],
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_by_name_key_strips_diacritics_but_a_year_does_not() {
        // "Beyoncé" and "Beyonce" must share a row, which is the whole point of
        // RemoveDiacritics. `Year` is upstream's exception — its name is digits.
        let person = KeySource {
            item_id: id(7),
            kind: BaseItemKind::Person,
            name: Some("Beyoncé"),
            ..KeySource::default()
        };
        assert_eq!(user_data_keys(&person, None)[0], "Person-Beyonce");

        let year = KeySource {
            item_id: id(8),
            kind: BaseItemKind::Year,
            name: Some("1999"),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&year, None),
            vec!["Year-1999".to_owned(), id(8).to_string()]
        );
    }

    #[test]
    fn a_boxset_really_is_keyed_by_its_id_alone() {
        // No `GetUserDataKeys` override upstream, unlike its by-name neighbours.
        let item = KeySource {
            item_id: id(7),
            kind: BaseItemKind::BoxSet,
            name: Some("Trilogy"),
            tmdb: Some("x"),
            ..KeySource::default()
        };
        assert_eq!(user_data_keys(&item, None), vec![id(7).to_string()]);
    }

    #[test]
    fn an_audiobook_gets_the_same_composite_key_as_a_track() {
        // `AudioBook : Audio` with no override, and it is the one kind whose
        // resume position the user-data manager special-cases — so a lost row
        // here is a lost place in a 12-hour book.
        let book = KeySource {
            item_id: id(5),
            kind: BaseItemKind::AudioBook,
            name: Some("Chapter One"),
            album: Some("A Book"),
            album_artist: Some("An Author"),
            index_number: Some(1),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&book, None),
            vec![
                "An Author-A Book-0001Chapter One".to_owned(),
                id(5).to_string()
            ]
        );
    }

    #[test]
    fn a_music_album_ranks_release_group_over_musicbrainz_over_artist_name() {
        let album = KeySource {
            item_id: id(9),
            kind: BaseItemKind::MusicAlbum,
            name: Some("Kind of Blue"),
            album_artist: Some("Miles Davis"),
            musicbrainz_album: Some("mb-1"),
            musicbrainz_release_group: Some("rg-1"),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&album, None),
            vec![
                "MusicAlbum-MusicBrainzReleaseGroup-rg-1".to_owned(),
                "MusicAlbum-Musicbrainz-mb-1".to_owned(),
                "Miles Davis-Kind of Blue".to_owned(),
                id(9).to_string()
            ]
        );
    }

    #[test]
    fn a_music_artist_leads_with_musicbrainz_then_its_name() {
        let artist = KeySource {
            item_id: id(10),
            kind: BaseItemKind::MusicArtist,
            name: Some("Beyoncé"),
            musicbrainz_artist: Some("mbid"),
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&artist, None),
            vec![
                "Artist-Musicbrainz-mbid".to_owned(),
                "Artist-Beyonce".to_owned(),
                id(10).to_string()
            ]
        );
        // Without the MusicBrainz id the name key still leads — `InsertRange`
        // adds the pair together, so the name is never dropped.
        let plain = KeySource {
            musicbrainz_artist: None,
            ..artist
        };
        assert_eq!(user_data_keys(&plain, None)[0], "Artist-Beyonce");
    }

    #[test]
    fn a_live_tv_program_picks_a_shape_by_whether_it_is_a_series() {
        let film = KeySource {
            item_id: id(11),
            kind: BaseItemKind::Program,
            name: Some("Some Film"),
            tmdb: Some("700391"),
            imdb: Some("tt1"),
            is_series: false,
            ..KeySource::default()
        };
        assert_eq!(
            user_data_keys(&film, None),
            vec!["700391".to_owned(), "tt1".to_owned(), id(11).to_string()]
        );

        // A series episode is keyed by name+episode title instead, and its
        // provider ids are ignored entirely.
        let episode = KeySource {
            is_series: true,
            episode_title: Some("Pilot"),
            ..film
        };
        assert_eq!(
            user_data_keys(&episode, None),
            vec!["Program-Some FilmPilot".to_owned(), id(11).to_string()]
        );
    }

    #[test]
    fn a_tv_channel_is_keyed_by_its_id_alone() {
        // `LiveTvChannel.cs:91` would prepend `TvChannel-<Name>`, but only when
        // `DisableLiveTvChannelUserDataName` is false — and it defaults to
        // TRUE (`ServerConfiguration.cs:91`). A stock server never writes that
        // key, so emitting one would put a key Jellyfin ignores at the head of
        // the list. Both spellings of the kind resolve the same way.
        for kind in [BaseItemKind::TvChannel, BaseItemKind::LiveTvChannel] {
            let channel = KeySource {
                item_id: id(12),
                kind,
                name: Some("BBC One"),
                ..KeySource::default()
            };
            assert_eq!(
                user_data_keys(&channel, None),
                vec![id(12).to_string()],
                "{kind:?}"
            );
        }
    }

    #[test]
    fn a_live_tv_program_is_reachable_under_every_spelling_of_its_kind() {
        // `LiveTvProgram` and `TvProgram` share one stored type name, and
        // `kind_from_type_name` resolves it to `LiveTvProgram` — so an arm
        // matching only `Program` is dead code from the database's point of
        // view, and a program silently loses all its keys but its id.
        for kind in [
            BaseItemKind::Program,
            BaseItemKind::LiveTvProgram,
            BaseItemKind::TvProgram,
        ] {
            let film = KeySource {
                item_id: id(11),
                kind,
                tmdb: Some("700391"),
                ..KeySource::default()
            };
            assert_eq!(
                user_data_keys(&film, None),
                vec!["700391".to_owned(), id(11).to_string()],
                "{kind:?}"
            );
            assert!(uses_provider_ids(kind), "{kind:?} must fetch providers");
        }
    }

    #[test]
    fn an_empty_provider_value_is_not_a_key() {
        // `ProviderValue` is NOT NULL but can be ''. The C# reads these through
        // TryGetProviderId, which treats empty as absent; without the guard an
        // empty CustomDataKey collides across every item that has one.
        let m = KeySource {
            tmdb: Some(""),
            imdb: Some(""),
            ..movie(None, None)
        };
        assert_eq!(user_data_keys(&m, None), vec![id(1).to_string()]);
    }

    #[test]
    fn uses_provider_ids_matches_the_kinds_that_actually_read_them() {
        // The query-skipping optimisation is only safe while this agrees with
        // the match above; an Episode reading providers it then ignores is
        // wasted work, and a Series NOT reading them is a wrong key.
        for kind in [
            BaseItemKind::Movie,
            BaseItemKind::Series,
            BaseItemKind::MusicAlbum,
            BaseItemKind::MusicArtist,
            BaseItemKind::Program,
            BaseItemKind::Trailer,
        ] {
            assert!(uses_provider_ids(kind), "{kind:?} reads provider ids");
        }
        for kind in [
            BaseItemKind::Episode,
            BaseItemKind::Season,
            BaseItemKind::Audio,
            BaseItemKind::AudioBook,
            BaseItemKind::Genre,
            BaseItemKind::Person,
            BaseItemKind::Studio,
            BaseItemKind::Year,
            BaseItemKind::TvChannel,
            BaseItemKind::Folder,
            BaseItemKind::BoxSet,
        ] {
            assert!(!uses_provider_ids(kind), "{kind:?} ignores provider ids");
        }
    }

    #[test]
    fn the_item_id_is_always_the_last_key() {
        // Callers rely on this to key a fresh row when nothing matches.
        let cases = [
            movie(Some("a"), Some("b")),
            series(),
            KeySource {
                item_id: id(9),
                kind: BaseItemKind::Audio,
                name: Some("n"),
                ..KeySource::default()
            },
        ];
        for c in cases {
            let keys = user_data_keys(&c, None);
            assert_eq!(keys.last().unwrap(), &c.item_id.to_string(), "{:?}", c.kind);
        }
    }
}

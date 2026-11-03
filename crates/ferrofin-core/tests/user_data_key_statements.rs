//! An episode's or a season's user-data keys are resolved in one statement.
//!
//! Their keys derive from their series (`Episode.GetUserDataKeys`,
//! `Season.GetUserDataKeys`), which upstream finds in memory through
//! `Series`. The key read (`identity_ids`) used to take the items first and
//! their series in a second walk, so every user-data read and save of an
//! episode, and `PlaybackInfo`'s user data, cost two statements (gl-988). The
//! series a season or an episode names now comes in the same walk.
//!
//! This binary records, through a process-wide subscriber, the `sqlx::query`
//! events emitted inside one call's span (the SQLite worker enters the
//! caller's span), and counts the key reads.

use std::sync::{Arc, Mutex};

use ferrofin_core::{
    FerrofinServerApplicationPaths, FerrofinServerConfigurationManager, FerrofinUserDataManager,
};
use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::UpdateUserItemDataDto;
use ferrofin_traits::configuration::ServerConfigurationManager;
use ferrofin_traits::library::UserDataManager;
use tracing::Instrument as _;
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;

/// The span one recorded call runs in.
const SPAN: &str = "gl988_key_resolution";

/// The series' TVDB id, which leads its keys and so its children's.
const TVDB: &str = "series-tvdb";

/// The statements recorded inside [`SPAN`], tagged with the recording's id:
/// `cargo test` runs a binary's tests on threads of one process.
static STATEMENTS: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

/// Records the text of every `sqlx::query` event emitted inside [`SPAN`], with
/// the span's `recording` field.
struct Statements;

/// The `recording` field of a [`SPAN`].
struct Recording(u64);

impl<S> tracing_subscriber::Layer<S> for Statements
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if attrs.metadata().name() != SPAN {
            return;
        }
        let mut recording = RecordingField(None);
        attrs.record(&mut recording);
        if let (Some(span), Some(recording)) = (ctx.span(id), recording.0) {
            span.extensions_mut().insert(Recording(recording));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let Some(recording) = ctx.event_scope(event).and_then(|mut scope| {
            scope.find_map(|span| span.extensions().get::<Recording>().map(|r| r.0))
        }) else {
            return;
        };
        let mut text = StatementText(String::new());
        event.record(&mut text);
        STATEMENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((recording, text.0));
    }
}

/// Reads a span's `recording` field.
struct RecordingField(Option<u64>);

impl tracing::field::Visit for RecordingField {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "recording" {
            self.0 = Some(value);
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

/// sqlx's `summary` (the statement's start) and `db.statement` (the whole
/// statement, when the summary is cut short).
struct StatementText(String);

impl tracing::field::Visit for StatementText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if matches!(field.name(), "summary" | "db.statement") {
            self.0.push_str(value);
            self.0.push(' ');
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(field.name(), "summary" | "db.statement") {
            use std::fmt::Write as _;
            let _ = write!(self.0, "{value:?} ");
        }
    }
}

/// Installs [`Statements`] as the process's subscriber, once.
fn record_statements() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Statements))
            .expect("no other subscriber in this binary");
    });
}

/// Runs `call` inside a fresh [`SPAN`] and returns its output and how many key
/// reads (`identity_ids`) it ran.
async fn key_reads<T>(call: impl std::future::Future<Output = T>) -> (T, usize) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let recording = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let output = call.instrument(tracing::info_span!(SPAN, recording)).await;
    let reads = STATEMENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(id, statement)| *id == recording && statement.contains("identity_ids"))
        .count();
    (output, reads)
}

/// Inserts the minimal `Users` row the user-data foreign key needs.
async fn seed_user(db: &Database, id: Uuid) {
    sqlx::query(
        r#"INSERT INTO "Users"
           ("Id", "AuthenticationProviderId", "DisplayCollectionsView",
            "DisplayMissingEpisodes", "EnableAutoLogin", "EnableLocalPassword",
            "EnableNextEpisodeAutoPlay", "EnableUserPreferenceAccess",
            "HidePlayedInLatest", "InternalId", "InvalidLoginAttemptCount",
            "MaxActiveSessions", "MustUpdatePassword",
            "PasswordResetProviderId", "PlayDefaultAudioTrack",
            "RememberAudioSelections", "RememberSubtitleSelections",
            "RowVersion", "SubtitleMode", "SyncPlayAccess", "Username", "NormalizedUsername")
           VALUES (?1, '', 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, '', 1, 1, 1, 0, 0, 0, 'viewer', 'VIEWER')"#,
    )
    .bind(guid_to_db(id))
    .execute(db.writer())
    .await
    .expect("insert user");
}

/// Inserts an item of `kind` under `parent`, naming `series`, numbered
/// `parent_index`/`index`.
async fn seed_item(
    db: &Database,
    id: Uuid,
    kind: BaseItemKind,
    parent: Option<Uuid>,
    series: Option<Uuid>,
    numbers: (Option<i32>, Option<i32>),
) {
    sqlx::query(
        r#"INSERT INTO "BaseItems"
           ("Id", "Type", "IsFolder", "IsInMixedFolder", "IsLocked", "IsMovie",
            "IsRepeat", "IsSeries", "IsVirtualItem", "Name", "ParentId", "SeriesId",
            "ParentIndexNumber", "IndexNumber")
           VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, 0, ?2, ?3, ?4, ?5, ?6)"#,
    )
    .bind(guid_to_db(id))
    .bind(kind.stored_type_name())
    .bind(parent.map(guid_to_db))
    .bind(series.map(guid_to_db))
    .bind(numbers.0)
    .bind(numbers.1)
    .execute(db.writer())
    .await
    .expect("insert item");
}

/// A series with a TVDB id, its season 1, and that season's episode 2, the
/// season and the episode both naming the series.
struct Show {
    db: Database,
    manager: FerrofinUserDataManager,
    user: Uuid,
    season: Uuid,
    episode: Uuid,
    _root: tempfile::TempDir,
}

async fn show() -> Show {
    let root = tempfile::tempdir().expect("tempdir");
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root.path().join("program"),
        root.path().join("log"),
        root.path().join("config"),
        root.path().join("cache"),
        root.path().join("web"),
    ));
    let config = Arc::new(
        FerrofinServerConfigurationManager::load(paths)
            .await
            .expect("configuration"),
    );
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let user = Uuid::new_v4();
    seed_user(&db, user).await;
    let [series, season, episode] = [(); 3].map(|()| Uuid::new_v4());
    seed_item(&db, series, BaseItemKind::Series, None, None, (None, None)).await;
    seed_item(
        &db,
        season,
        BaseItemKind::Season,
        Some(series),
        Some(series),
        (None, Some(1)),
    )
    .await;
    seed_item(
        &db,
        episode,
        BaseItemKind::Episode,
        Some(season),
        Some(series),
        (Some(1), Some(2)),
    )
    .await;
    sqlx::query(
        r#"INSERT INTO "BaseItemProviders" ("ItemId", "ProviderId", "ProviderValue")
           VALUES (?1, 'Tvdb', ?2)"#,
    )
    .bind(guid_to_db(series))
    .bind(TVDB)
    .execute(db.writer())
    .await
    .expect("insert provider id");
    let manager =
        FerrofinUserDataManager::new(db.clone(), config as Arc<dyn ServerConfigurationManager>);
    Show {
        db,
        manager,
        user,
        season,
        episode,
        _root: root,
    }
}

/// An episode's user data, read alone (`GetUserData`, as `/UserItems/{id}/UserData`
/// and every play-state handler read it), resolves its keys and its series in
/// one read.
#[tokio::test]
async fn an_episode_s_user_data_reads_its_series_in_the_same_key_read() {
    record_statements();
    let show = show().await;

    let (dto, reads) = key_reads(show.manager.get_user_data_dto(show.episode, show.user)).await;

    let dto = dto.expect("user data").expect("a row");
    assert_eq!(
        dto.key,
        format!("{TVDB}001002"),
        "the series leads the keys"
    );
    assert_eq!(reads, 1, "one key read for the episode and its series");
}

/// A save on an episode (`SaveUserData`) writes every key its series gives it,
/// after one key read inside its transaction.
#[tokio::test]
async fn an_episode_s_save_reads_its_series_in_the_same_key_read() {
    record_statements();
    let show = show().await;
    let favourite = UpdateUserItemDataDto {
        is_favorite: Some(true),
        ..UpdateUserItemDataDto::default()
    };

    let (saved, reads) = key_reads(show.manager.save_user_data(
        show.user,
        show.episode,
        &favourite,
    ))
    .await;

    saved.expect("save");
    assert_eq!(reads, 1, "one key read for the episode and its series");
    let mut keys: Vec<String> = sqlx::query_scalar(
        r#"SELECT "CustomDataKey" FROM "UserData" WHERE "ItemId" = ?1 AND "IsFavorite""#,
    )
    .bind(guid_to_db(show.episode))
    .fetch_all(show.db.pool())
    .await
    .expect("user data rows");
    keys.sort();
    let mut expected = vec![format!("{TVDB}001002"), show.episode.to_string()];
    expected.sort();
    assert_eq!(keys, expected, "the series' key and the episode's own");
}

/// A batch of a season and an episode (`GetUserDataBatch`, as `PlaybackInfo`
/// and a version group's user data take it) reads their series with them.
#[tokio::test]
async fn a_season_and_an_episode_read_their_series_in_the_same_key_read() {
    record_statements();
    let show = show().await;

    let (dtos, reads) = key_reads(
        show.manager
            .get_user_data_dtos(&[show.season, show.episode], show.user),
    )
    .await;

    let dtos = dtos.expect("user data");
    assert_eq!(dtos[&show.season].key, format!("{TVDB}001"));
    assert_eq!(dtos[&show.episode].key, format!("{TVDB}001002"));
    assert_eq!(reads, 1, "one key read for the batch and its series");
}

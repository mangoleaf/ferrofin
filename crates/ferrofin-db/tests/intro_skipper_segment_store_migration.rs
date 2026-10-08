//! Migration 0037 seeds the Intro Skipper's segment tier from the segments
//! already published (Ferrofin's `IntroSkipper` rows and, on an adopted
//! Jellyfin database, the plugin's MD5-provider rows) and moves Ferrofin's rows
//! onto Jellyfin's provider id.

use std::borrow::Cow;

use sqlx::migrate::Migrator;

const JELLYFIN_ID: &str = "b0338b450421c081992860f1d02f261f";

#[tokio::test]
async fn published_segments_seed_the_tier_and_take_jellyfins_provider_id() {
    // `Database` registers the SQL functions earlier migrations call.
    let db = ferrofin_db::Database::connect_in_memory()
        .await
        .expect("database");
    let pool = db.pool().clone();
    let full = sqlx::migrate!("./migrations");
    let before = Migrator {
        migrations: Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < 37)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate!("./migrations")
    };
    before.run(&pool).await.expect("migrations through 0036");

    // Intro (5) and Outro (4) by Ferrofin, a Commercial (1) by the real plugin,
    // and another provider's Intro that must stay untouched.
    for (id, provider, kind, start, end) in [
        (
            "00000000-0000-0000-0000-00000000000A",
            "IntroSkipper",
            5,
            0_i64,
            300_000_000_i64,
        ),
        (
            "00000000-0000-0000-0000-00000000000B",
            "IntroSkipper",
            4,
            12_000_000_000,
            13_000_000_000,
        ),
        (
            "00000000-0000-0000-0000-00000000000C",
            JELLYFIN_ID,
            1,
            6_000_000_000,
            6_300_000_000,
        ),
        (
            "00000000-0000-0000-0000-00000000000D",
            "SomeOtherProvider",
            5,
            0,
            100_000_000,
        ),
    ] {
        sqlx::query(
            r#"INSERT INTO "MediaSegments"
               ("Id", "EndTicks", "ItemId", "SegmentProviderId", "StartTicks", "Type")
               VALUES (?1, ?2, '0000000A-AAAA-AAAA-AAAA-AAAAAAAAAAAA', ?3, ?4, ?5)"#,
        )
        .bind(id)
        .bind(end)
        .bind(provider)
        .bind(start)
        .bind(kind)
        .execute(&pool)
        .await
        .expect("seed segment");
    }

    full.run(&pool).await.expect("migration 0037");

    let tier: Vec<(String, i32, f64, f64, bool)> = sqlx::query_as(
        r#"SELECT "ItemId", "Type", "Start", "End", "IsUserProvided"
           FROM "FerrofinIntroSkipperSegments" ORDER BY "Start""#,
    )
    .fetch_all(&pool)
    .await
    .expect("tier");
    let item = "0000000a-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    assert_eq!(
        tier,
        [
            (item.to_owned(), 0, 0.0, 30.0, false),
            (item.to_owned(), 4, 600.0, 630.0, false),
            (item.to_owned(), 1, 1200.0, 1300.0, false),
        ]
    );

    let providers: Vec<(String,)> =
        sqlx::query_as(r#"SELECT "SegmentProviderId" FROM "MediaSegments" ORDER BY "Id""#)
            .fetch_all(&pool)
            .await
            .expect("providers");
    let providers: Vec<&str> = providers.iter().map(|(p,)| p.as_str()).collect();
    assert_eq!(
        providers,
        [JELLYFIN_ID, JELLYFIN_ID, JELLYFIN_ID, "SomeOtherProvider"]
    );
}

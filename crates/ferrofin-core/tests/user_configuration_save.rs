//! `UpdateConfigurationAsync` saves the `Users` row and its four list
//! preferences in one `SaveChanges`: a save that fails part-way leaves none of
//! it. The failure is forced with a trigger, which is raw SQL, so this lives
//! outside `crates/*/src` (`crates/ferrofin-db/tests/sql_boundary.rs`).

use ferrofin_core::FerrofinUserManager;
use ferrofin_db::Database;
use ferrofin_db::enums::PreferenceKind;
use ferrofin_model::configuration::UserConfiguration;
use ferrofin_traits::library::UserManager;
use uuid::Uuid;

#[tokio::test]
async fn a_configuration_save_that_fails_part_way_saves_nothing() {
    let db = Database::connect_in_memory().await.expect("connect");
    db.run_migrations().await.expect("migrations");
    let mgr = FerrofinUserManager::new(db.clone());
    let user = mgr.create_user("alice").await.expect("create");
    let id = Uuid::parse_str(&user.id).expect("id");
    let before = mgr
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .configuration;

    // A preference write after the `Users` UPDATE and the first two
    // preferences fails.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        r#"CREATE TRIGGER "FailMyMediaExcludes" BEFORE UPDATE ON "Preferences"
           WHEN NEW."Kind" = {kind} BEGIN SELECT RAISE(ABORT, 'forced'); END;
           CREATE TRIGGER "FailMyMediaExcludesInsert" BEFORE INSERT ON "Preferences"
           WHEN NEW."Kind" = {kind} BEGIN SELECT RAISE(ABORT, 'forced'); END;"#,
        kind = i32::from(PreferenceKind::MyMediaExcludes)
    )))
    .execute(db.writer())
    .await
    .expect("trigger");

    let config = UserConfiguration {
        audio_language_preference: Some("fra".to_owned()),
        grouped_folders: vec![Uuid::new_v4()],
        my_media_excludes: vec![Uuid::new_v4()],
        ..before.clone().expect("configuration")
    };
    mgr.update_configuration(id, &config)
        .await
        .expect_err("the forced failure surfaces");

    // A fresh manager, so the stored rows are read, not a cached DTO.
    let fresh = FerrofinUserManager::new(db);
    let user = fresh
        .get_user_by_id(id)
        .await
        .expect("get")
        .expect("present");
    let after = fresh
        .get_user_dto(&user, None)
        .await
        .expect("dto")
        .configuration;
    assert_eq!(after, before);
}

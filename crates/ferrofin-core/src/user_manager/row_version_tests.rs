//! `RowVersion`, EF's concurrency token on `Users`, `Permissions` and
//! `Preferences`, through each account flow. Upstream's
//! `JellyfinDbContext.HandleConcurrencyToken` adds 1 to every entity in the
//! `Modified` state when it saves: a new row starts at 0, and each save that
//! modifies a row bumps it once. `ExecuteUpdateAsync` (a sign-in's dates and
//! counter) bypasses it.

use super::*;

type Rows = Vec<(i32, i64)>;

async fn versions(mgr: &FerrofinUserManager, user_id: &str) -> (i64, Rows, Rows) {
    user_repository::row_versions(mgr.db.pool(), user_id)
        .await
        .expect("row versions")
}

/// `rows` with each of `kinds` one higher.
fn bumped(rows: &[(i32, i64)], kinds: &[i32]) -> Rows {
    rows.iter()
        .map(|&(kind, version)| (kind, version + i64::from(kinds.contains(&kind))))
        .collect()
}

fn all(rows: &[(i32, i64)]) -> Vec<i32> {
    rows.iter().map(|&(kind, _)| kind).collect()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one flow after another, as the issue asks
async fn row_versions_follow_each_save_as_ef_bumps_them() {
    let mgr = FerrofinUserManager::new(crate::test_support::test_db().await);

    // InitializeAsync sets its three grants on the new entity before its one
    // Add, so every row is inserted at 0.
    mgr.initialize().await.expect("initialize");
    let admin = mgr.get_first_user().await.expect("first").expect("admin");
    let (admin_version, permissions, preferences) = versions(&mgr, &admin.id).await;
    assert_eq!(admin_version, 0);
    assert!(
        permissions.iter().chain(&preferences).all(|&(_, v)| v == 0),
        "{permissions:?} {preferences:?}"
    );
    for kind in [
        PermissionKind::IsAdministrator,
        PermissionKind::EnableContentDeletion,
        PermissionKind::EnableRemoteControlOfOtherUsers,
    ] {
        assert!(
            has_permission(mgr.db.pool(), &admin.id, kind)
                .await
                .unwrap()
        );
    }

    // CreateUserAsync: one Add.
    let user = mgr.create_user("row-version").await.expect("create");
    let id = Uuid::parse_str(&user.id).unwrap();
    let created = versions(&mgr, &user.id).await;
    assert_eq!(created.0, 0);
    assert!(created.1.iter().chain(&created.2).all(|&(_, v)| v == 0));

    // UpdatePolicyAsync: Update(user) marks the user Modified on every save,
    // a permission or preference row only when its value changes.
    let mut policy = mgr.get_user_dto(&user, None).await.unwrap().policy.unwrap();
    policy.enable_content_deletion = true;
    policy.blocked_tags = vec!["tag".to_owned()];
    policy.login_attempts_before_lockout = 1;
    let changed_permission = [i32::from(PermissionKind::EnableContentDeletion)];
    let changed_preference = [i32::from(PreferenceKind::BlockedTags)];
    let after_policy = (
        1,
        bumped(&created.1, &changed_permission),
        bumped(&created.2, &changed_preference),
    );
    mgr.update_policy(id, &policy).await.unwrap();
    assert_eq!(versions(&mgr, &user.id).await, after_policy);
    mgr.update_policy(id, &policy).await.unwrap();
    assert_eq!(
        versions(&mgr, &user.id).await,
        (2, after_policy.1.clone(), after_policy.2.clone()),
        "an unchanged value bumps no row"
    );

    // UpdateConfigurationAsync: Update(user) again, and the one preference
    // whose value changed.
    let configuration = UserConfiguration {
        ordered_views: vec![Uuid::from_u128(7)],
        ..UserConfiguration::default()
    };
    mgr.update_configuration(id, &configuration).await.unwrap();
    let configured = versions(&mgr, &user.id).await;
    assert_eq!(
        configured,
        (
            3,
            after_policy.1.clone(),
            bumped(&after_policy.2, &[i32::from(PreferenceKind::OrderedViews)])
        )
    );

    // RenameUser: Entry(user).State = Modified, the user alone.
    mgr.rename_user(id, "row-version", "row-version-2")
        .await
        .unwrap();
    assert_eq!(
        versions(&mgr, &user.id).await,
        (4, configured.1.clone(), configured.2.clone())
    );

    // UpdateUserAsync: SetValues marks the user Modified only when a value
    // differs, so the startup wizard's save of the unchanged user bumps
    // nothing.
    let stored = mgr.get_user_by_id(id).await.unwrap().unwrap();
    mgr.update_user(&stored).await.unwrap();
    assert_eq!(versions(&mgr, &user.id).await.0, 4);
    mgr.update_user(&UserEntity {
        audio_language_preference: Some("fre".to_owned()),
        ..stored
    })
    .await
    .unwrap();
    assert_eq!(versions(&mgr, &user.id).await.0, 5);

    // ChangePassword's tracked save bumps the user; a sign-in writes through
    // ExecuteUpdateAsync and bumps nothing.
    mgr.change_password(id, "pw").await.unwrap();
    assert!(
        mgr.authenticate_user("row-version-2", "pw", "127.0.0.1", true)
            .await
            .unwrap()
            .is_some()
    );
    let signed_in = versions(&mgr, &user.id).await;
    assert_eq!(signed_in.0, 6);

    // The lockout: Update(user) on the untracked graph UserQuery loaded marks
    // the user and every one of its permission and preference rows Modified.
    assert!(
        mgr.authenticate_user("row-version-2", "wrong", "127.0.0.1", true)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        versions(&mgr, &user.id).await,
        (
            7,
            bumped(&signed_in.1, &all(&signed_in.1)),
            bumped(&signed_in.2, &all(&signed_in.2))
        )
    );
    assert!(
        has_permission(mgr.db.pool(), &user.id, PermissionKind::IsDisabled)
            .await
            .unwrap()
    );
}

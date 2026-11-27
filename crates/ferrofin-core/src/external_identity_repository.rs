//! The SQLite [`ExternalIdentityStore`]: links from an outside identity to a
//! user in the Ferrofin-owned `FerrofinExternalIdentities` table (migration
//! 0048). Linking ports LDAP-Auth's `PluginConfiguration.AddUser`
//! (jellyfin-plugin-ldapauth v24), with the plugin configuration's
//! `LdapUsers` array replaced by the table (D12 of the LDAP-Auth milestone).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_common::cryptography::constant_time_eq;
use ferrofin_db::Database;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::external_identities::{
    ConditionalLink, ExternalIdentity, ExternalIdentityKey, ExternalIdentityStore, LinkBy,
};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::db_error::db_err;

/// The database-backed [`ExternalIdentityStore`].
#[derive(Clone)]
pub struct FerrofinExternalIdentityStore {
    db: Database,
}

impl std::fmt::Debug for FerrofinExternalIdentityStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinExternalIdentityStore")
            .finish_non_exhaustive()
    }
}

impl FerrofinExternalIdentityStore {
    /// A store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// A `FerrofinExternalIdentities` row, as the store reads it.
#[derive(sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
struct Row {
    user_id: String,
    provider_id: String,
    issuer: String,
    subject: String,
    avatar_hash: Option<String>,
    date_created: DateTime<Utc>,
}

/// The avatar hash as stored: upstream's empty `ProfileImageHash` (no image)
/// is `NULL`, so "no hash" has one representation.
fn stored_hash(avatar_hash: Option<&str>) -> Option<&str> {
    avatar_hash.filter(|hash| !hash.is_empty())
}

/// The columns [`Row`] reads.
const COLUMNS: &str = r#""UserId", "ProviderId", "Issuer", "Subject", "AvatarHash", "DateCreated""#;

/// A row whose `UserId` is not a GUID is skipped (with a warning) rather than
/// read as the nil id. The foreign key to `Users` makes one unlikely.
fn to_identity(row: Row) -> Option<ExternalIdentity> {
    let Ok(user_id) = Uuid::parse_str(&row.user_id) else {
        tracing::warn!(
            stored_user_id = row.user_id,
            provider_id = row.provider_id,
            "external identities: a stored link's user id is not a GUID"
        );
        return None;
    };
    Some(ExternalIdentity {
        user_id,
        provider_id: row.provider_id,
        issuer: row.issuer,
        subject: row.subject,
        avatar_hash: row.avatar_hash,
        date_created: row.date_created,
    })
}

/// Whether a user has the stored id `user`.
async fn user_exists(tx: &mut SqliteConnection, user: &str) -> Result<bool, ServiceError> {
    let exists: Option<i64> = sqlx::query_scalar(r#"SELECT 1 FROM "Users" WHERE "Id" = ?1"#)
        .bind(user)
        .fetch_optional(tx)
        .await
        .map_err(db_err)?;
    Ok(exists.is_some())
}

/// Inserts the link of `identity` to `user_id` (stored as `user`).
async fn insert(
    tx: &mut SqliteConnection,
    user_id: Uuid,
    user: &str,
    identity: ExternalIdentityKey<'_>,
    avatar_hash: Option<&str>,
) -> Result<ExternalIdentity, ServiceError> {
    let sql = format!(
        r#"INSERT INTO "FerrofinExternalIdentities"
               ("UserId", "ProviderId", "Issuer", "Subject", "AvatarHash", "DateCreated")
           VALUES (?1, ?2, ?3, ?4, ?5, ?6)
           RETURNING {COLUMNS}"#
    );
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(user)
        .bind(identity.provider_id)
        .bind(identity.issuer)
        .bind(identity.subject)
        .bind(stored_hash(avatar_hash))
        .bind(datetime_to_db(Utc::now()))
        .fetch_one(tx)
        .await
        .map_err(db_err)?;
    Ok(ExternalIdentity {
        user_id,
        provider_id: row.provider_id,
        issuer: row.issuer,
        subject: row.subject,
        avatar_hash: row.avatar_hash,
        date_created: row.date_created,
    })
}

/// Whether one of the devices of the stored user `user` holds the access
/// token whose SHA-256 is `session`: a sign-out by any route deletes its
/// device's row.
async fn session_live(
    tx: &mut SqliteConnection,
    user: &str,
    session: &[u8; 32],
) -> Result<bool, ServiceError> {
    let tokens: Vec<String> =
        sqlx::query_scalar(r#"SELECT "AccessToken" FROM "Devices" WHERE "UserId" = ?1"#)
            .bind(user)
            .fetch_all(tx)
            .await
            .map_err(db_err)?;
    Ok(tokens
        .iter()
        .any(|token| constant_time_eq(&Sha256::digest(token.as_bytes()), session)))
}

#[async_trait]
impl ExternalIdentityStore for FerrofinExternalIdentityStore {
    async fn find(
        &self,
        identity: ExternalIdentityKey<'_>,
    ) -> Result<Option<ExternalIdentity>, ServiceError> {
        // Upstream's `GetLdapUser(ldapUid)` skips every entry whose `LdapUid` is
        // empty, so a directory entry without a uid never matches another
        // user's account.
        if identity.subject.is_empty() {
            return Ok(None);
        }
        let sql = format!(
            r#"SELECT {COLUMNS} FROM "FerrofinExternalIdentities"
               WHERE "ProviderId" = ?1 AND "Issuer" = ?2 AND "Subject" = ?3"#
        );
        let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(identity.provider_id)
            .bind(identity.issuer)
            .bind(identity.subject)
            .fetch_optional(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(row.and_then(to_identity))
    }

    async fn list(&self, provider_id: &str) -> Result<Vec<ExternalIdentity>, ServiceError> {
        let sql = format!(
            r#"SELECT {COLUMNS} FROM "FerrofinExternalIdentities"
               WHERE "ProviderId" = ?1 ORDER BY "Id""#
        );
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(provider_id)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows.into_iter().filter_map(to_identity).collect())
    }

    async fn list_for_user(
        &self,
        provider_id: &str,
        user_id: Uuid,
    ) -> Result<Vec<ExternalIdentity>, ServiceError> {
        // `FerrofinIX_ExternalIdentities_UserId` finds the user's rows.
        let sql = format!(
            r#"SELECT {COLUMNS} FROM "FerrofinExternalIdentities"
               WHERE "UserId" = ?1 AND "ProviderId" = ?2 ORDER BY "Id""#
        );
        let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(guid_to_db(user_id))
            .bind(provider_id)
            .fetch_all(self.db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows.into_iter().filter_map(to_identity).collect())
    }

    async fn link(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<ExternalIdentity, ServiceError> {
        let user = guid_to_db(user_id);
        // It reads before it writes: take the write lock first (CONTRIBUTING.md,
        // "Migrations and one-shot repairs"). The one transaction is what keeps
        // two first sign-ins writing at once from losing an entry.
        let mut tx = self
            .db
            .writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_err)?;
        if !user_exists(&mut tx, &user).await? {
            return Err(ServiceError::not_found(format!("user {user_id}")));
        }
        // Upstream's `AddUser`: "Ensure we do not have more than one entry for a
        // given user" — `RemoveUser(userGuid)`, then `RemoveUser(ldapUid)`. The
        // map was LDAP's own, so here both are scoped to the provider, and the
        // user's is scoped to the issuer too: LDAP's is always empty, and an
        // OIDC user keeps their link at another identity provider.
        sqlx::query(
            r#"DELETE FROM "FerrofinExternalIdentities"
               WHERE "UserId" = ?1 AND "ProviderId" = ?2 AND "Issuer" = ?3"#,
        )
        .bind(&user)
        .bind(identity.provider_id)
        .bind(identity.issuer)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            r#"DELETE FROM "FerrofinExternalIdentities"
               WHERE "ProviderId" = ?1 AND "Issuer" = ?2 AND "Subject" = ?3"#,
        )
        .bind(identity.provider_id)
        .bind(identity.issuer)
        .bind(identity.subject)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let link = insert(&mut tx, user_id, &user, identity, avatar_hash).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(link)
    }

    async fn link_unless_taken(
        &self,
        user_id: Uuid,
        identity: ExternalIdentityKey<'_>,
        by: LinkBy,
    ) -> Result<ConditionalLink, ServiceError> {
        let user = guid_to_db(user_id);
        // The write lock first, so no other link, unlink or sign-out lands
        // between a check below and the write (CONTRIBUTING.md, "Migrations
        // and one-shot repairs"). Each refusal rolls back, changing nothing.
        let mut tx = self
            .db
            .writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_err)?;
        if !user_exists(&mut tx, &user).await? {
            return Err(ServiceError::not_found(format!("user {user_id}")));
        }
        if let LinkBy::Session(session) = &by
            && !session_live(&mut tx, &user, session).await?
        {
            return Ok(ConditionalLink::SessionEnded);
        }
        let sql = format!(
            r#"SELECT {COLUMNS} FROM "FerrofinExternalIdentities"
               WHERE "ProviderId" = ?1 AND "Issuer" = ?2 AND "Subject" = ?3"#
        );
        let held: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(identity.provider_id)
            .bind(identity.issuer)
            .bind(identity.subject)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
        if let Some(row) = held {
            // A row whose user id is not a GUID names no account this can
            // read: refused as taken, since nothing here moves an identity.
            return Ok(match to_identity(row) {
                Some(link) if link.user_id == user_id => ConditionalLink::Kept(link),
                _ => ConditionalLink::Taken,
            });
        }
        let sql = format!(
            r#"SELECT {COLUMNS} FROM "FerrofinExternalIdentities"
               WHERE "UserId" = ?1 AND "ProviderId" = ?2 AND "Issuer" = ?3"#
        );
        let earlier: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&user)
            .bind(identity.provider_id)
            .bind(identity.issuer)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
        if earlier.is_some() {
            if matches!(by, LinkBy::Session(_)) {
                return Ok(ConditionalLink::AlreadyLinked);
            }
            sqlx::query(
                r#"DELETE FROM "FerrofinExternalIdentities"
                   WHERE "UserId" = ?1 AND "ProviderId" = ?2 AND "Issuer" = ?3"#,
            )
            .bind(&user)
            .bind(identity.provider_id)
            .bind(identity.issuer)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        let link = insert(&mut tx, user_id, &user, identity, None).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(ConditionalLink::Linked {
            link,
            replaced: earlier.and_then(to_identity),
        })
    }

    async fn set_avatar_hash(
        &self,
        identity: ExternalIdentityKey<'_>,
        avatar_hash: Option<&str>,
    ) -> Result<bool, ServiceError> {
        let done = sqlx::query(
            r#"UPDATE "FerrofinExternalIdentities" SET "AvatarHash" = ?4
               WHERE "ProviderId" = ?1 AND "Issuer" = ?2 AND "Subject" = ?3"#,
        )
        .bind(identity.provider_id)
        .bind(identity.issuer)
        .bind(identity.subject)
        .bind(stored_hash(avatar_hash))
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected() > 0)
    }

    async fn unlink(&self, identity: ExternalIdentityKey<'_>) -> Result<bool, ServiceError> {
        let done = sqlx::query(
            r#"DELETE FROM "FerrofinExternalIdentities"
               WHERE "ProviderId" = ?1 AND "Issuer" = ?2 AND "Subject" = ?3"#,
        )
        .bind(identity.provider_id)
        .bind(identity.issuer)
        .bind(identity.subject)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(done.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_traits::library::UserManager;

    use super::*;
    use crate::FerrofinUserManager;
    use crate::test_support::test_db;

    const LDAP: &str = "Jellyfin.Plugin.LDAP_Auth.LdapAuthenticationProviderPlugin";
    const OIDC: &str = "Ferrofin.Oidc";

    fn ldap(uid: &str) -> ExternalIdentityKey<'_> {
        ExternalIdentityKey {
            provider_id: LDAP,
            issuer: "",
            subject: uid,
        }
    }

    /// A store and a user manager over one fresh database, with users named
    /// `names`, returned in order.
    async fn fixture(
        names: &[&str],
    ) -> (
        FerrofinExternalIdentityStore,
        FerrofinUserManager,
        Vec<Uuid>,
    ) {
        let db = test_db().await;
        let users = FerrofinUserManager::new(db.clone());
        let mut ids = Vec::new();
        for name in names {
            let user = users.create_user(name).await.expect("create user");
            ids.push(Uuid::parse_str(&user.id).expect("user id"));
        }
        (FerrofinExternalIdentityStore::new(db), users, ids)
    }

    async fn count(store: &FerrofinExternalIdentityStore) -> i64 {
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "FerrofinExternalIdentities""#)
            .fetch_one(store.db.pool())
            .await
            .expect("count")
    }

    #[tokio::test]
    async fn a_link_is_found_by_its_identity_and_listed_by_its_provider() {
        let (store, _users, ids) = fixture(&["alice", "bob"]).await;
        let alice = store
            .link(ids[0], ldap("alice-uid"), Some("hash-a"))
            .await
            .expect("link alice");
        let bob = store
            .link(ids[1], ldap("bob-uid"), None)
            .await
            .expect("link bob");
        let oidc = ExternalIdentityKey {
            provider_id: OIDC,
            issuer: "https://idp.example",
            subject: "alice-sub",
        };
        store.link(ids[0], oidc, None).await.expect("link oidc");

        assert_eq!(alice.user_id, ids[0]);
        assert_eq!(alice.key(), ldap("alice-uid"));
        assert_eq!(alice.avatar_hash.as_deref(), Some("hash-a"));
        assert_eq!(
            store.find(ldap("alice-uid")).await.expect("find"),
            Some(alice.clone())
        );
        assert_eq!(
            store
                .find(oidc)
                .await
                .expect("find oidc")
                .map(|l| l.user_id),
            Some(ids[0])
        );
        assert_eq!(store.find(ldap("carol-uid")).await.expect("find"), None);
        // The subject is compared byte for byte (upstream's `Ordinal`), and the
        // issuer is part of the identity.
        assert_eq!(store.find(ldap("ALICE-UID")).await.expect("find"), None);
        let other_issuer = ExternalIdentityKey {
            issuer: "https://other.example",
            ..oidc
        };
        assert_eq!(store.find(other_issuer).await.expect("find"), None);

        assert_eq!(store.list(LDAP).await.expect("list"), vec![alice, bob]);
        assert_eq!(store.list(OIDC).await.expect("list oidc").len(), 1);
        assert_eq!(store.list("Other").await.expect("list other"), Vec::new());
    }

    /// One user's links at a provider, one per issuer, and never another
    /// user's or another provider's; none once the user is deleted.
    #[tokio::test]
    async fn a_users_links_are_listed_by_provider() {
        let (store, users, ids) = fixture(&["alice", "bob"]).await;
        let at = |issuer, subject| ExternalIdentityKey {
            provider_id: OIDC,
            issuer,
            subject,
        };
        let one = store
            .link(ids[0], at("https://one.example", "a-1"), None)
            .await
            .expect("first issuer");
        let two = store
            .link(ids[0], at("https://two.example", "a-2"), Some("hash"))
            .await
            .expect("second issuer");
        store
            .link(ids[1], at("https://one.example", "b-1"), None)
            .await
            .expect("bob");
        store
            .link(ids[0], ldap("alice-uid"), None)
            .await
            .expect("ldap");

        assert_eq!(
            store.list_for_user(OIDC, ids[0]).await.expect("alice"),
            vec![one, two]
        );
        let bobs = store.list_for_user(OIDC, ids[1]).await.expect("bob");
        assert_eq!(bobs.len(), 1);
        assert_eq!(bobs[0].subject, "b-1");
        assert_eq!(store.list_for_user(LDAP, ids[1]).await.expect("none"), []);
        assert_eq!(
            store
                .list_for_user(OIDC, Uuid::from_u128(0xDEAD))
                .await
                .expect("no such user"),
            []
        );

        users.delete_user(ids[0]).await.expect("delete alice");
        assert_eq!(store.list_for_user(OIDC, ids[0]).await.expect("gone"), []);
        assert_eq!(store.list_for_user(OIDC, ids[1]).await.expect("bob"), bobs);
    }

    #[tokio::test]
    async fn a_second_link_for_the_same_identity_replaces_the_first() {
        let (store, _users, ids) = fixture(&["alice", "bob"]).await;
        store
            .link(ids[0], ldap("shared-uid"), Some("old"))
            .await
            .expect("first link");
        let second = store
            .link(ids[1], ldap("shared-uid"), None)
            .await
            .expect("second link");

        assert_eq!(second.user_id, ids[1]);
        assert_eq!(
            store.find(ldap("shared-uid")).await.expect("find"),
            Some(second)
        );
        assert_eq!(count(&store).await, 1);
    }

    #[tokio::test]
    async fn a_user_keeps_one_link_per_provider() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        let oidc = ExternalIdentityKey {
            provider_id: OIDC,
            issuer: "https://idp.example",
            subject: "alice-sub",
        };
        store.link(ids[0], oidc, None).await.expect("oidc link");
        store
            .link(ids[0], ldap("old-uid"), None)
            .await
            .expect("first link");
        // A directory rename gives the user a new uid: the old link goes.
        store
            .link(ids[0], ldap("new-uid"), Some("hash"))
            .await
            .expect("relink");

        assert_eq!(store.find(ldap("old-uid")).await.expect("find"), None);
        let links = store.list(LDAP).await.expect("list");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].subject, "new-uid");
        assert_eq!(links[0].avatar_hash.as_deref(), Some("hash"));
        // Another provider's link of the same user stays.
        assert!(store.find(oidc).await.expect("find oidc").is_some());
    }

    /// OIDC: a user linked at one identity provider keeps that link when
    /// linked at a second, and a relink at the first replaces only its own.
    #[tokio::test]
    async fn a_user_keeps_one_link_per_provider_and_issuer() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        let at = |issuer, subject| ExternalIdentityKey {
            provider_id: OIDC,
            issuer,
            subject,
        };
        store
            .link(ids[0], at("https://one.example", "sub-1"), None)
            .await
            .expect("first issuer");
        store
            .link(ids[0], at("https://two.example", "sub-2"), None)
            .await
            .expect("second issuer");
        assert!(
            store
                .find(at("https://one.example", "sub-1"))
                .await
                .expect("find")
                .is_some(),
            "the first issuer's link stays"
        );
        store
            .link(ids[0], at("https://one.example", "sub-3"), None)
            .await
            .expect("relink at the first issuer");
        let mut subjects: Vec<String> = store
            .list(OIDC)
            .await
            .expect("list")
            .into_iter()
            .map(|l| l.subject)
            .collect();
        subjects.sort();
        assert_eq!(subjects, ["sub-2", "sub-3"]);
    }

    #[tokio::test]
    async fn linking_a_missing_user_is_not_found_and_changes_nothing() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        store
            .link(ids[0], ldap("alice-uid"), None)
            .await
            .expect("link");

        let err = store
            .link(Uuid::from_u128(0xDEAD), ldap("alice-uid"), None)
            .await
            .expect_err("no such user");
        assert!(matches!(err, ServiceError::NotFound(_)), "{err:?}");
        assert_eq!(
            store
                .find(ldap("alice-uid"))
                .await
                .expect("find")
                .map(|l| l.user_id),
            Some(ids[0]),
            "the failed link removed nothing"
        );
    }

    #[tokio::test]
    async fn concurrent_links_lose_no_entry() {
        let (store, _users, ids) = fixture(&["u0", "u1", "u2", "u3", "u4", "u5", "u6", "u7"]).await;
        let store = Arc::new(store);
        let uids: Vec<String> = Vec::from_fn(ids.len(), |i| format!("uid-{i}"));
        let mut tasks = Vec::new();
        for (id, uid) in ids.iter().copied().zip(uids.iter().cloned()) {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                store.link(id, ldap(&uid), None).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("link");
        }
        let mut subjects: Vec<String> = store
            .list(LDAP)
            .await
            .expect("list")
            .into_iter()
            .map(|l| l.subject)
            .collect();
        subjects.sort();
        assert_eq!(subjects, uids);
    }

    /// Users linking the same uid at once: each link is one transaction, so
    /// every call succeeds and one row is left. Split into separate
    /// statements, two would delete and then both insert.
    #[tokio::test]
    async fn concurrent_links_of_one_identity_leave_one_row() {
        let (store, _users, ids) = fixture(&["u0", "u1", "u2", "u3", "u4", "u5"]).await;
        let store = Arc::new(store);
        let mut tasks = Vec::new();
        for id in ids.iter().copied() {
            let store = Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                store.link(id, ldap("shared-uid"), None).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("link");
        }
        assert_eq!(count(&store).await, 1);
        let owner = store
            .find(ldap("shared-uid"))
            .await
            .expect("find")
            .map(|l| l.user_id);
        assert!(owner.is_some_and(|id| ids.contains(&id)), "{owner:?}");
    }

    /// One user linking several uids at once keeps one link for the provider.
    #[tokio::test]
    async fn concurrent_links_of_one_user_leave_one_link() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        let store = Arc::new(store);
        let mut tasks = Vec::new();
        for i in 0..6 {
            let store = Arc::clone(&store);
            let user = ids[0];
            tasks.push(tokio::spawn(async move {
                let uid = format!("uid-{i}");
                store.link(user, ldap(&uid), None).await.map(|_| ())
            }));
        }
        for task in tasks {
            task.await.expect("join").expect("link");
        }
        assert_eq!(count(&store).await, 1);
        assert_eq!(store.list(LDAP).await.expect("list").len(), 1);
    }

    /// A directory entry without a uid is linked as upstream's `AddUser` links
    /// it, but never found, so a second such user cannot take the first
    /// one's account.
    #[tokio::test]
    async fn an_empty_subject_is_never_found() {
        let (store, _users, ids) = fixture(&["alice", "bob"]).await;
        store
            .link(ids[0], ldap("alice-uid"), None)
            .await
            .expect("first link");
        let empty = store.link(ids[0], ldap(""), None).await.expect("empty");
        assert_eq!(empty.subject, "");

        assert_eq!(store.find(ldap("")).await.expect("find"), None);
        assert_eq!(
            store.find(ldap("alice-uid")).await.expect("find"),
            None,
            "the user's earlier link went, as upstream's RemoveUser(userGuid)"
        );
        store.link(ids[1], ldap(""), None).await.expect("bob empty");
        assert_eq!(store.find(ldap("")).await.expect("find"), None);
        assert_eq!(count(&store).await, 1);
    }

    #[tokio::test]
    async fn an_empty_avatar_hash_is_stored_as_none() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        let link = store
            .link(ids[0], ldap("alice-uid"), Some(""))
            .await
            .expect("link");
        assert_eq!(link.avatar_hash, None);
        store
            .set_avatar_hash(ldap("alice-uid"), Some(""))
            .await
            .expect("set");
        let hash: Option<String> =
            sqlx::query_scalar(r#"SELECT "AvatarHash" FROM "FerrofinExternalIdentities""#)
                .fetch_one(store.db.pool())
                .await
                .expect("hash");
        assert_eq!(hash, None);
    }

    /// A stored user id that is not a GUID (only possible with foreign keys
    /// off) is skipped rather than read as the nil id.
    #[tokio::test]
    async fn a_link_whose_user_id_is_not_a_guid_is_skipped() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        store.link(ids[0], ldap("a"), None).await.expect("link");
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(store.db.writer())
            .await
            .expect("foreign keys off");
        sqlx::query(
            r#"INSERT INTO "FerrofinExternalIdentities"
                   ("UserId", "ProviderId", "Subject", "DateCreated")
               VALUES ('not-a-guid', ?1, 'b', '2026-11-02 00:00:00.0000000')"#,
        )
        .bind(LDAP)
        .execute(store.db.writer())
        .await
        .expect("insert");

        assert_eq!(store.find(ldap("b")).await.expect("find"), None);
        let subjects: Vec<String> = store
            .list(LDAP)
            .await
            .expect("list")
            .into_iter()
            .map(|l| l.subject)
            .collect();
        assert_eq!(subjects, ["a"]);
    }

    #[tokio::test]
    async fn the_avatar_hash_is_set_on_an_existing_link_only() {
        let (store, _users, ids) = fixture(&["alice"]).await;
        store
            .link(ids[0], ldap("alice-uid"), None)
            .await
            .expect("link");

        assert!(
            store
                .set_avatar_hash(ldap("alice-uid"), Some("bWQ1"))
                .await
                .expect("set")
        );
        assert_eq!(
            store
                .find(ldap("alice-uid"))
                .await
                .expect("find")
                .and_then(|l| l.avatar_hash),
            Some("bWQ1".to_owned())
        );
        assert!(
            store
                .set_avatar_hash(ldap("alice-uid"), None)
                .await
                .expect("clear")
        );
        assert_eq!(
            store
                .find(ldap("alice-uid"))
                .await
                .expect("find")
                .and_then(|l| l.avatar_hash),
            None
        );
        assert!(
            !store
                .set_avatar_hash(ldap("nobody"), Some("x"))
                .await
                .expect("set missing")
        );
        assert_eq!(count(&store).await, 1);
    }

    #[tokio::test]
    async fn unlink_removes_only_that_identity() {
        let (store, _users, ids) = fixture(&["alice", "bob"]).await;
        store.link(ids[0], ldap("a"), None).await.expect("link a");
        store.link(ids[1], ldap("b"), None).await.expect("link b");

        assert!(store.unlink(ldap("a")).await.expect("unlink"));
        assert!(!store.unlink(ldap("a")).await.expect("unlink again"));
        assert_eq!(store.find(ldap("a")).await.expect("find"), None);
        assert!(store.find(ldap("b")).await.expect("find b").is_some());
    }

    /// `user_id`'s device holding the access token `token`; its row's id.
    async fn device(store: &FerrofinExternalIdentityStore, user_id: Uuid, token: &str) -> i64 {
        use ferrofin_traits::devices::DeviceManager as _;
        let now = Utc::now();
        crate::FerrofinDeviceManager::new(store.db.clone())
            .create_device(&ferrofin_db::entities::security::DeviceEntity {
                id: 0,
                access_token: token.into(),
                app_name: "app".into(),
                app_version: "1".into(),
                date_created: now,
                date_last_activity: now,
                date_modified: now,
                device_id: format!("device-{token}"),
                device_name: "name".into(),
                is_active: true,
                user_id: guid_to_db(user_id),
            })
            .await
            .expect("create device")
            .id
    }

    fn sha256(token: &str) -> [u8; 32] {
        Sha256::digest(token.as_bytes()).into()
    }

    /// A conditional link checks, inside its write, that the identity is not
    /// another user's and, for a session's link, that the user has no link at
    /// the issuer and the session is still one of the user's devices; every
    /// refusal changes nothing.
    #[tokio::test]
    async fn a_conditional_link_refuses_a_taken_identity_a_replacement_and_an_ended_session() {
        let (store, _users, ids) = fixture(&["alice", "bob"]).await;
        let (alice, bob) = (ids[0], ids[1]);
        let at = |subject| ExternalIdentityKey {
            provider_id: OIDC,
            issuer: "https://idp.example",
            subject,
        };
        device(&store, alice, "alice-token").await;
        let live = sha256("alice-token");
        let session = LinkBy::Session(live);

        let gone = sha256("signed-out-token");
        assert_eq!(
            store
                .link_unless_taken(alice, at("a-1"), LinkBy::Session(gone))
                .await
                .expect("refused"),
            ConditionalLink::SessionEnded
        );
        assert_eq!(count(&store).await, 0);

        let ConditionalLink::Linked { link, replaced } = store
            .link_unless_taken(alice, at("a-1"), session)
            .await
            .expect("linked")
        else {
            panic!("not linked");
        };
        assert_eq!((link.user_id, link.subject.as_str()), (alice, "a-1"));
        assert_eq!(replaced, None);
        assert_eq!(
            store
                .link_unless_taken(alice, at("a-1"), session)
                .await
                .expect("kept"),
            ConditionalLink::Kept(link.clone())
        );
        assert_eq!(
            store
                .link_unless_taken(alice, at("a-2"), session)
                .await
                .expect("refused"),
            ConditionalLink::AlreadyLinked,
            "a session's link never replaces one"
        );
        device(&store, bob, "bob-token").await;
        let bobs = sha256("bob-token");
        for by in [LinkBy::Session(bobs), LinkBy::Administrator] {
            assert_eq!(
                store
                    .link_unless_taken(bob, at("a-1"), by)
                    .await
                    .expect("refused"),
                ConditionalLink::Taken,
                "{by:?}"
            );
        }
        assert_eq!(store.list(OIDC).await.expect("list"), vec![link.clone()]);

        // An administrator's link replaces the user's at the issuer.
        let ConditionalLink::Linked {
            link: second,
            replaced,
        } = store
            .link_unless_taken(alice, at("a-2"), LinkBy::Administrator)
            .await
            .expect("replaced")
        else {
            panic!("not linked");
        };
        assert_eq!(replaced, Some(link));
        assert_eq!(store.list(OIDC).await.expect("list"), vec![second]);

        let missing = store
            .link_unless_taken(Uuid::from_u128(0xDEAD), at("x"), LinkBy::Administrator)
            .await
            .expect_err("no such user");
        assert!(matches!(missing, ServiceError::NotFound(_)), "{missing:?}");
    }

    /// The session check reads the devices inside the write: a device row
    /// deleted before it (a sign-out) ends the session, and another user's
    /// device holding the token is not the user's session.
    #[tokio::test]
    async fn a_conditional_link_follows_the_sessions_device_row() {
        let (store, _users, ids) = fixture(&["alice", "bob"]).await;
        let identity = ExternalIdentityKey {
            provider_id: OIDC,
            issuer: "https://idp.example",
            subject: "a-1",
        };
        let row = device(&store, ids[0], "alice-token").await;
        device(&store, ids[1], "bob-token").await;
        let bobs = sha256("bob-token");
        assert_eq!(
            store
                .link_unless_taken(ids[0], identity, LinkBy::Session(bobs))
                .await
                .expect("refused"),
            ConditionalLink::SessionEnded
        );
        sqlx::query(r#"DELETE FROM "Devices" WHERE "Id" = ?1"#)
            .bind(row)
            .execute(store.db.writer())
            .await
            .expect("signed out");
        let alices = sha256("alice-token");
        assert_eq!(
            store
                .link_unless_taken(ids[0], identity, LinkBy::Session(alices))
                .await
                .expect("refused"),
            ConditionalLink::SessionEnded
        );
        assert_eq!(count(&store).await, 0);
    }

    /// Accounts linking one identity from their sessions at once: each is one
    /// transaction under the write lock, so one links and the rest are
    /// refused.
    #[tokio::test]
    async fn concurrent_conditional_links_link_once() {
        let (store, _users, ids) = fixture(&["u0", "u1", "u2", "u3"]).await;
        let store = Arc::new(store);
        let mut tasks = Vec::new();
        for (i, id) in ids.iter().copied().enumerate() {
            let store = Arc::clone(&store);
            device(&store, id, &format!("token-{i}")).await;
            tasks.push(tokio::spawn(async move {
                let session = sha256(&format!("token-{i}"));
                let shared = ExternalIdentityKey {
                    provider_id: OIDC,
                    issuer: "https://idp.example",
                    subject: "shared",
                };
                store
                    .link_unless_taken(id, shared, LinkBy::Session(session))
                    .await
            }));
        }
        let mut linked = 0;
        for task in tasks {
            match task.await.expect("join").expect("an outcome") {
                ConditionalLink::Linked { .. } => linked += 1,
                ConditionalLink::Taken => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(linked, 1);
        assert_eq!(count(&store).await, 1);
    }

    /// `delete_user` removes the links itself, so it does not depend on the
    /// table's `ON DELETE CASCADE` being enforced.
    #[tokio::test]
    async fn deleting_a_user_removes_their_links_without_the_cascade() {
        let (store, users, ids) = fixture(&["alice", "bob"]).await;
        store.link(ids[0], ldap("a"), None).await.expect("link a");
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(store.db.writer())
            .await
            .expect("foreign keys off");

        users.delete_user(ids[0]).await.expect("delete alice");

        assert_eq!(count(&store).await, 0);
    }

    #[tokio::test]
    async fn deleting_a_user_removes_their_links() {
        let (store, users, ids) = fixture(&["alice", "bob"]).await;
        store.link(ids[0], ldap("a"), None).await.expect("link a");
        store
            .link(
                ids[0],
                ExternalIdentityKey {
                    provider_id: OIDC,
                    issuer: "https://idp.example",
                    subject: "a",
                },
                None,
            )
            .await
            .expect("link oidc");
        store.link(ids[1], ldap("b"), None).await.expect("link b");

        users.delete_user(ids[0]).await.expect("delete alice");

        assert_eq!(store.find(ldap("a")).await.expect("find"), None);
        assert_eq!(store.list(OIDC).await.expect("list"), Vec::new());
        assert_eq!(
            store
                .find(ldap("b"))
                .await
                .expect("find b")
                .map(|l| l.user_id),
            Some(ids[1])
        );
    }
}

//! [`FerrofinMediaAttachmentRepository`] — the concrete
//! [`MediaAttachmentRepository`] over `ferrofin-db`.
//!
//! Port of `MediaAttachmentRepository`. Reads and writes the
//! `AttachmentStreamInfos` table. In C# `SaveMediaAttachments` unconditionally
//! clears the item's existing attachments before adding the new set (so
//! replacing a media file with one lacking attachments correctly drops them);
//! that delete-then-insert-in-a-transaction shape is preserved. The C# `Map`
//! between `AttachmentStreamInfo` and the domain `MediaAttachment` is a
//! DTO-layer concern; the trait works directly on [`AttachmentStreamInfoEntity`]
//! rows.

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::AttachmentStreamInfoEntity;
use ferrofin_db::store::guid_to_db;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::persistence::{MediaAttachmentQuery, MediaAttachmentRepository};

use crate::db_error::db_err;

/// The concrete media-attachment repository.
#[derive(Clone)]
pub struct FerrofinMediaAttachmentRepository {
    db: Database,
}

impl std::fmt::Debug for FerrofinMediaAttachmentRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinMediaAttachmentRepository")
            .finish_non_exhaustive()
    }
}

impl FerrofinMediaAttachmentRepository {
    /// Creates a media-attachment repository over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl MediaAttachmentRepository for FerrofinMediaAttachmentRepository {
    async fn get_media_attachments(
        &self,
        filter: &MediaAttachmentQuery,
    ) -> Result<Vec<AttachmentStreamInfoEntity>, ServiceError> {
        let mut sql = String::from(r#"SELECT * FROM "AttachmentStreamInfos" WHERE "ItemId" = ?1"#);
        if filter.index.is_some() {
            sql.push_str(r#" AND "Index" = ?2"#);
        }
        sql.push_str(r#" ORDER BY "Index""#);

        let mut query =
            sqlx::query_as::<_, AttachmentStreamInfoEntity>(&sql).bind(guid_to_db(filter.item_id));
        if let Some(index) = filter.index {
            query = query.bind(i64::from(index));
        }
        query.fetch_all(self.db.pool()).await.map_err(db_err)
    }

    async fn save_media_attachments(
        &self,
        item_id: Uuid,
        attachments: &[AttachmentStreamInfoEntity],
    ) -> Result<(), ServiceError> {
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "AttachmentStreamInfos" WHERE "ItemId" = ?1"#)
            .bind(guid_to_db(item_id))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        for attachment in attachments {
            sqlx::query(
                r#"INSERT INTO "AttachmentStreamInfos"
                   ("ItemId", "Index", "Codec", "CodecTag", "Comment", "Filename", "MimeType")
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            )
            .bind(guid_to_db(item_id))
            .bind(attachment.index)
            .bind(&attachment.codec)
            .bind(&attachment.codec_tag)
            .bind(&attachment.comment)
            .bind(&attachment.filename)
            .bind(&attachment.mime_type)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FerrofinMediaAttachmentRepository;
    use crate::test_support::{seed_item, test_db};
    use ferrofin_db::entities::base_items::AttachmentStreamInfoEntity;
    use ferrofin_model::data::BaseItemKind;
    use ferrofin_traits::persistence::{MediaAttachmentQuery, MediaAttachmentRepository};
    use uuid::Uuid;

    fn attachment(index: i64, codec: &str) -> AttachmentStreamInfoEntity {
        AttachmentStreamInfoEntity {
            item_id: String::new(),
            index,
            codec: Some(codec.to_owned()),
            codec_tag: None,
            comment: None,
            filename: Some(format!("a{index}.ttf")),
            mime_type: None,
        }
    }

    #[tokio::test]
    async fn save_replaces_and_filter_by_index() {
        let db = test_db().await;
        let item = Uuid::new_v4();
        seed_item(&db, item, BaseItemKind::Movie).await;
        let repo = FerrofinMediaAttachmentRepository::new(db);

        repo.save_media_attachments(item, &[attachment(0, "ttf"), attachment(1, "otf")])
            .await
            .expect("save");

        let all = repo
            .get_media_attachments(&MediaAttachmentQuery {
                item_id: item,
                index: None,
            })
            .await
            .expect("all");
        assert_eq!(all.len(), 2);

        let one = repo
            .get_media_attachments(&MediaAttachmentQuery {
                item_id: item,
                index: Some(1),
            })
            .await
            .expect("one");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].codec.as_deref(), Some("otf"));

        // Saving an empty set clears the attachments (the C# replace semantics).
        repo.save_media_attachments(item, &[]).await.expect("clear");
        let all = repo
            .get_media_attachments(&MediaAttachmentQuery {
                item_id: item,
                index: None,
            })
            .await
            .expect("all");
        assert!(all.is_empty());
    }
}

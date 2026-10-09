//! Resolve the user preference portion of `InternalItemsQuery.SetUser`.

use ferrofin_db::{Database, enums::PreferenceKind};
use ferrofin_model::data::UnratedItem;
use ferrofin_traits::{error::ServiceError, options::InternalItemsQuery};
use ferrofin_util::string_extensions::{lower_invariant, remove_diacritics};

/// Loads preferences once per resolved query, shared by its page and count.
/// Explicit nonempty query restrictions retain the caller's override, as when
/// C# callers assign them after constructing `InternalItemsQuery(user)`.
pub(crate) async fn resolve(
    db: &Database,
    filter: &InternalItemsQuery,
) -> Result<Option<InternalItemsQuery>, ServiceError> {
    let Some(user) = &filter.user else {
        return Ok(None);
    };
    if filter.user_preferences_loaded {
        return Ok(None);
    }
    let preferences = crate::item_visibility_repository::preferences(db, &user.id).await?;
    let values = |kind: PreferenceKind| -> Vec<&str> {
        preferences
            .iter()
            .find(|(stored, _)| *stored == i32::from(kind))
            .map(|(_, value)| value.split(',').filter(|v| !v.is_empty()).collect())
            .unwrap_or_default()
    };
    let mut resolved = filter.clone();
    if resolved.block_unrated_items.is_empty() {
        resolved.block_unrated_items = values(PreferenceKind::BlockUnratedItems)
            .into_iter()
            .filter(|value| *value != "Other")
            .map(parse_unrated)
            .collect::<Result<_, _>>()?;
    }
    for (kind, target) in [
        (
            PreferenceKind::BlockedTags,
            &mut resolved.exclude_inherited_tags,
        ),
        (
            PreferenceKind::AllowedTags,
            &mut resolved.include_inherited_tags,
        ),
    ] {
        if target.is_empty() {
            *target = values(kind)
                .into_iter()
                .filter(|value| !value.trim().is_empty())
                .map(|value| lower_invariant(&remove_diacritics(value)))
                .collect();
        }
    }
    resolved.user_preferences_loaded = true;
    Ok(Some(resolved))
}

pub(crate) fn parse_unrated(value: &str) -> Result<UnratedItem, ServiceError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| ServiceError::backend(format!("invalid unrated preference: {error}")))
}

//! Port of `MediaBrowser.Controller.Entities.InternalPeopleQuery`.

use uuid::Uuid;

use super::InternalItemsQuery;

/// A query over the people (cast/crew) associated with items.
///
/// Mirrors C# `InternalPeopleQuery`. The C# `User` domain property is dropped:
/// callers pass the resolved user's id via [`user_id`](Self::user_id) instead,
/// per the port's identity-as-[`Uuid`] rule. All collections default to empty
/// and all optional filters to `None`, so [`Default`] matches the parameterless
/// C# constructor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct InternalPeopleQuery {
    /// The index of the first result to return (pagination offset).
    pub start_index: Option<i32>,

    /// The maximum number of people to return (`0` means "no limit" in C#).
    pub limit: i32,

    /// The item whose people are being queried.
    pub item_id: Uuid,

    /// Restrict to people under this parent, if set.
    pub parent_id: Option<Uuid>,

    /// The person types to include (e.g. `Actor`, `Director`). Empty = all.
    pub person_types: Vec<String>,

    /// The person types to exclude.
    pub exclude_person_types: Vec<String>,

    /// The maximum credit list order to include, if set.
    pub max_list_order: Option<i32>,

    /// Restrict to people who also appear in this item, if set.
    pub appears_in_item_id: Uuid,

    /// Case-insensitive substring the person name must contain.
    pub name_contains: Option<String>,

    /// Prefix the person name must start with.
    pub name_starts_with: Option<String>,

    /// Upper bound (exclusive) for the person name, for range paging.
    pub name_less_than: Option<String>,

    /// Lower bound for the person name (starts-with-or-greater), for range paging.
    pub name_starts_with_or_greater: Option<String>,

    /// The id of the user the query is scoped to (replaces C# `User`), if any.
    pub user_id: Option<Uuid>,

    /// Restrict to people the user has (not) favourited, if set.
    pub is_favorite: Option<bool>,

    /// Keep only people with a by-name `Person` item of the same name behind
    /// them. Set by callers that hand back items rather than credits
    /// (`GetPeopleItems`), so the total they report describes the people they
    /// can return (C# `MustHaveItem`, Jellyfin 12.2).
    pub must_have_item: bool,

    /// The item query whose access settings (library access, parental rating,
    /// tags) a person must satisfy through at least one item they are credited
    /// on (C# `AccessFilter`, Jellyfin 12.2). People belong to no library, so
    /// only their credits say whether a user may see them. `None` applies no
    /// restriction, which is what `/Persons` passes for a user nothing is
    /// hidden from.
    pub access_filter: Option<InternalItemsQuery>,
}

#[cfg(test)]
mod tests {
    use super::InternalPeopleQuery;
    use uuid::Uuid;

    #[test]
    fn default_is_empty() {
        let q = InternalPeopleQuery::default();
        assert_eq!(q.limit, 0);
        assert_eq!(q.item_id, Uuid::nil());
        assert!(q.person_types.is_empty());
        assert!(q.exclude_person_types.is_empty());
        assert!(q.user_id.is_none());
        assert!(q.is_favorite.is_none());
        assert!(!q.must_have_item);
        assert!(q.access_filter.is_none());
    }

    #[test]
    fn builder_style_population() {
        let item = Uuid::from_u128(7);
        let q = InternalPeopleQuery {
            item_id: item,
            limit: 25,
            person_types: vec!["Actor".into(), "Director".into()],
            is_favorite: Some(true),
            ..Default::default()
        };
        assert_eq!(q.item_id, item);
        assert_eq!(q.limit, 25);
        assert_eq!(q.person_types.len(), 2);
        assert_eq!(q.is_favorite, Some(true));
    }
}

//! Verbatim port of
//! `Jellyfin.Model.Tests/Entities/ProviderIdsExtensionsTests.cs`.
//!
//! The C# expected values are the oracle; assertions are not weakened.
//! C# `ArgumentNullException`-on-null-instance cases are elided because a Rust
//! `&self` receiver cannot be null; the null-`ProviderIds` cases are preserved
//! via `Option::None`. The C# `SetProviderId` argument-exception cases map to
//! the `Result`-returning [`set_provider_id_for`].

use std::collections::HashMap;

use ferrofin_model::entities_media::{self, IHasProviderIds, MetadataProvider, SetProviderIdError};

const EXAMPLE_IMDB_ID: &str = "tt0113375";

/// Test object mirroring the C# `ProviderIdsExtensionsTestsObject`.
struct TestObject {
    provider_ids: Option<HashMap<String, String>>,
}

impl TestObject {
    /// An object with an empty (present) provider-id map.
    fn empty() -> Self {
        Self {
            provider_ids: Some(HashMap::new()),
        }
    }

    /// An object whose provider-id map is null.
    fn null() -> Self {
        Self { provider_ids: None }
    }
}

impl IHasProviderIds for TestObject {
    fn provider_ids(&self) -> Option<&HashMap<String, String>> {
        self.provider_ids.as_ref()
    }

    fn provider_ids_mut(&mut self) -> &mut HashMap<String, String> {
        self.provider_ids.get_or_insert_with(HashMap::new)
    }

    fn provider_ids_opt_mut(&mut self) -> &mut Option<HashMap<String, String>> {
        &mut self.provider_ids
    }
}

#[test]
fn has_provider_id_null_provider_false() {
    let null_provider = TestObject::null();
    assert!(!entities_media::has_provider_id_for(
        &null_provider,
        MetadataProvider::Imdb
    ));
}

#[test]
fn has_provider_id_not_found_name_false() {
    assert!(!entities_media::has_provider_id_for(
        &TestObject::empty(),
        MetadataProvider::Imdb
    ));
}

#[test]
fn has_provider_id_found_name_true() {
    let mut provider = TestObject::empty();
    provider.provider_ids_mut().insert(
        MetadataProvider::Imdb.as_name().to_owned(),
        EXAMPLE_IMDB_ID.to_owned(),
    );
    assert!(entities_media::has_provider_id_for(
        &provider,
        MetadataProvider::Imdb
    ));
}

#[test]
fn has_provider_id_found_name_empty_value_false() {
    let mut provider = TestObject::empty();
    provider
        .provider_ids_mut()
        .insert(MetadataProvider::Imdb.as_name().to_owned(), String::new());
    assert!(!entities_media::has_provider_id_for(
        &provider,
        MetadataProvider::Imdb
    ));
}

#[test]
fn get_provider_id_not_found_name_null() {
    assert_eq!(
        entities_media::get_provider_id_for(&TestObject::empty(), MetadataProvider::Imdb),
        None
    );
}

#[test]
fn get_provider_id_null_provider_null() {
    let null_provider = TestObject::null();
    assert_eq!(
        entities_media::get_provider_id_for(&null_provider, MetadataProvider::Imdb),
        None
    );
}

#[test]
fn try_get_provider_id_not_found_name_false() {
    assert_eq!(
        entities_media::try_get_provider_id_for(&TestObject::empty(), MetadataProvider::Imdb),
        None
    );
}

#[test]
fn try_get_provider_id_null_provider_false() {
    let null_provider = TestObject::null();
    assert_eq!(
        entities_media::try_get_provider_id_for(&null_provider, MetadataProvider::Imdb),
        None
    );
}

#[test]
fn get_provider_id_found_name_id() {
    let mut provider = TestObject::empty();
    provider.provider_ids_mut().insert(
        MetadataProvider::Imdb.as_name().to_owned(),
        EXAMPLE_IMDB_ID.to_owned(),
    );
    assert_eq!(
        entities_media::get_provider_id_for(&provider, MetadataProvider::Imdb).as_deref(),
        Some(EXAMPLE_IMDB_ID)
    );
}

#[test]
fn try_get_provider_id_found_name_true() {
    let mut provider = TestObject::empty();
    provider.provider_ids_mut().insert(
        MetadataProvider::Imdb.as_name().to_owned(),
        EXAMPLE_IMDB_ID.to_owned(),
    );
    assert_eq!(
        entities_media::try_get_provider_id_for(&provider, MetadataProvider::Imdb),
        Some(EXAMPLE_IMDB_ID)
    );
}

#[test]
fn try_get_provider_id_found_name_empty_value_false() {
    let mut provider = TestObject::empty();
    provider
        .provider_ids_mut()
        .insert(MetadataProvider::Imdb.as_name().to_owned(), String::new());
    assert_eq!(
        entities_media::try_get_provider_id_for(&provider, MetadataProvider::Imdb),
        None
    );
}

#[test]
fn set_provider_id_null_remove() {
    // C#: SetProviderId(Imdb, null) throws; ProviderIds stays empty.
    let mut provider = TestObject::empty();
    // A null value is represented by the no-op `try_set_provider_id_for`.
    let ok = entities_media::try_set_provider_id_for(&mut provider, MetadataProvider::Imdb, None);
    assert!(!ok);
    assert!(provider.provider_ids().unwrap().is_empty());
}

#[test]
fn set_provider_id_empty_name_remove() {
    // C#: SetProviderId(Imdb, string.Empty) throws; existing entry preserved.
    let mut provider = TestObject::empty();
    provider.provider_ids_mut().insert(
        MetadataProvider::Imdb.as_name().to_owned(),
        EXAMPLE_IMDB_ID.to_owned(),
    );
    let err = entities_media::set_provider_id_for(&mut provider, MetadataProvider::Imdb, "");
    assert_eq!(err, Err(SetProviderIdError::NullOrWhitespace));
    assert_eq!(provider.provider_ids().unwrap().len(), 1);
}

#[test]
fn set_provider_id_non_empty_id_success() {
    let mut provider = TestObject::empty();
    entities_media::set_provider_id_for(&mut provider, MetadataProvider::Imdb, EXAMPLE_IMDB_ID)
        .unwrap();
    assert_eq!(provider.provider_ids().unwrap().len(), 1);
}

#[test]
fn set_provider_id_null_provider_success() {
    let mut null_provider = TestObject::null();
    entities_media::set_provider_id_for(
        &mut null_provider,
        MetadataProvider::Imdb,
        EXAMPLE_IMDB_ID,
    )
    .unwrap();
    assert_eq!(null_provider.provider_ids().unwrap().len(), 1);
}

#[test]
fn set_provider_id_null_provider_and_empty_name_success() {
    // C#: throws ArgumentException; ProviderIds stays null.
    let mut null_provider = TestObject::null();
    let err = entities_media::set_provider_id_for(&mut null_provider, MetadataProvider::Imdb, "");
    assert_eq!(err, Err(SetProviderIdError::NullOrWhitespace));
    assert!(null_provider.provider_ids().is_none());
}

#[test]
fn remove_provider_id_null_remove() {
    let mut provider = TestObject::empty();
    provider.provider_ids_mut().insert(
        MetadataProvider::Imdb.as_name().to_owned(),
        EXAMPLE_IMDB_ID.to_owned(),
    );
    entities_media::remove_provider_id_for(&mut provider, MetadataProvider::Imdb);
    assert!(provider.provider_ids().unwrap().is_empty());
}

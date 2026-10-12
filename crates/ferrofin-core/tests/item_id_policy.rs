//! `EnableCaseSensitiveItemIds` / `EnableNormalizedItemByNameIds` reach item-id
//! derivation. This is its own test binary because the policy is process-wide:
//! no other test shares it.

use std::sync::Arc;

use ferrofin_core::item_type_lookup::{IdDerivation, by_name_item_id, derive_item_id_with};
use ferrofin_core::{FerrofinServerApplicationPaths, FerrofinServerConfigurationManager};
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::configuration::ServerConfigurationManager;

fn mode() -> IdDerivation {
    IdDerivation::Jellyfin {
        program_data_path: None,
    }
}

/// `(scanned id differs by key case, by-name id differs by key case)`.
fn case_sensitivity() -> (bool, bool) {
    let scanned = |path: &str| derive_item_id_with(&mode(), BaseItemKind::Movie, path);
    let by_name = |path: &str| by_name_item_id(&mode(), BaseItemKind::Genre, path);
    (
        scanned("/m/Heat.mkv") != scanned("/m/heat.mkv"),
        by_name("/meta/Genre/Sci-Fi") != by_name("/meta/genre/sci-fi"),
    )
}

#[tokio::test]
async fn saved_settings_decide_whether_id_keys_keep_their_case() {
    let root = tempfile::tempdir().unwrap();
    let paths = Arc::new(FerrofinServerApplicationPaths::new(
        root.path().join("program"),
        root.path().join("log"),
        root.path().join("config"),
        root.path().join("cache"),
        root.path().join("web"),
    ));
    let manager = FerrofinServerConfigurationManager::load(Arc::clone(&paths))
        .await
        .unwrap();
    // Defaults: scanned ids are case-sensitive, by-name ids are normalized.
    assert_eq!(case_sensitivity(), (true, false));

    let mut configuration = (*manager.configuration().await.unwrap()).clone();
    // Case-sensitive but not normalized: a by-name id keeps its case too.
    configuration.enable_normalized_item_by_name_ids = false;
    manager.update_configuration(&configuration).await.unwrap();
    assert_eq!(case_sensitivity(), (true, true));

    // Not case-sensitive: every key is lowercased, whatever the other flag.
    for normalized in [true, false] {
        configuration.enable_case_sensitive_item_ids = false;
        configuration.enable_normalized_item_by_name_ids = normalized;
        manager.update_configuration(&configuration).await.unwrap();
        assert_eq!(
            case_sensitivity(),
            (false, false),
            "normalized={normalized}"
        );
    }

    // A restart picks up what was saved.
    let reloaded = FerrofinServerConfigurationManager::load(paths)
        .await
        .unwrap();
    assert!(!reloaded.snapshot().enable_case_sensitive_item_ids);
    assert_eq!(case_sensitivity(), (false, false));

    configuration.enable_case_sensitive_item_ids = true;
    configuration.enable_normalized_item_by_name_ids = true;
    reloaded.update_configuration(&configuration).await.unwrap();
    assert_eq!(case_sensitivity(), (true, false));
}

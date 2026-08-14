//! Tests for the install-time artifact validator: it must accept a real
//! `ferrofin:plugin` component (the shared WAT fixture) and reject anything
//! else with a guest-visible reason — this is the last gate before a
//! downloaded artifact is committed to the plugins directory.

use ferrofin_traits::plugins::PluginArtifactValidator as _;
use ferrofin_wasm::{PLUGIN_ABI, WasmArtifactValidator, WasmSettings};

#[test]
fn plugin_abi_matches_the_wit_world() {
    // Drift guard: the const must be exactly the world version declared in
    // the WIT contract, which the host build embeds.
    let wit = include_str!("../wit/ferrofin-plugin.wit");
    assert!(
        wit.contains(&format!("package {PLUGIN_ABI};")),
        "PLUGIN_ABI `{PLUGIN_ABI}` is not the package declared in wit/ferrofin-plugin.wit"
    );
}

#[tokio::test]
async fn accepts_the_fixture_component_and_reports_its_id() {
    let validator = WasmArtifactValidator::new(&WasmSettings::default()).unwrap();
    assert_eq!(validator.supported_abi(), PLUGIN_ABI);

    let component = wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).unwrap();
    let id = validator
        .validate(&component)
        .await
        .expect("fixture is valid");
    assert_eq!(id.to_string(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff");
}

#[tokio::test]
async fn rejects_garbage_and_non_component_wasm() {
    let validator = WasmArtifactValidator::new(&WasmSettings::default()).unwrap();

    let err = validator.validate(b"not wasm at all").await.unwrap_err();
    assert!(
        err.to_string()
            .contains("not a valid WebAssembly component"),
        "{err}"
    );

    // A valid CORE module is still not a component of the plugin world.
    let core = wat::parse_str("(module)").unwrap();
    let err = validator.validate(&core).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("not a valid WebAssembly component")
            || err.to_string().contains("does not instantiate"),
        "{err}"
    );

    // A component that is not the plugin world (empty component) fails at
    // instantiation/descriptor lookup, not with a crash.
    let empty = wat::parse_str("(component)").unwrap();
    let err = validator.validate(&empty).await.unwrap_err();
    assert!(
        err.to_string().contains("does not instantiate") || err.to_string().contains("descriptor"),
        "{err}"
    );
}

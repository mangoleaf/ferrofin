//! Tests for the install-time artifact validator: it must accept a real
//! `ferrofin:plugin` component (the shared WAT fixture) and reject anything
//! else with a guest-visible reason — this is the last gate before a
//! downloaded artifact is committed to the plugins directory.

use ferrofin_traits::plugins::PluginArtifactValidator as _;
use ferrofin_wasm::{PLUGIN_ABI, WasmArtifactValidator, WasmSettings};

mod common;
use common::named_provider_fixture;

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
    assert_eq!(id.id.to_string(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeffff");
    assert!(
        id.declared_egress.is_empty(),
        "fixture declares no public egress"
    );
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

#[tokio::test]
async fn rejects_a_provider_name_colliding_with_a_builtin_at_install() {
    // The install gate must catch a reserved provider name — otherwise the
    // admin sees "installed", a restart-required flag, and then a silently
    // absent provider (the load-time guard would refuse it on next boot).
    let validator = WasmArtifactValidator::new(&WasmSettings::default()).unwrap();

    let src = named_provider_fixture(
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeee1234",
        " TheMovieDb ", // padded, to also prove the trim happens here
    );
    let colliding = wat::parse_str(&src).unwrap();
    let err = validator.validate(&colliding).await.unwrap_err();
    assert!(
        err.to_string().contains("collides with a built-in fetcher"),
        "{err}"
    );

    // A well-behaved named provider still validates.
    let src = named_provider_fixture("aaaaaaaa-bbbb-cccc-dddd-eeeeeeee5678", "AcmeDb");
    let ok = wat::parse_str(&src).unwrap();
    validator
        .validate(&ok)
        .await
        .expect("a non-colliding named provider validates");
}

#[tokio::test]
async fn refuses_at_install_what_loading_would_skip() {
    let validator = WasmArtifactValidator::new(&WasmSettings::default()).unwrap();
    let fixture = ferrofin_wasm::TEST_FIXTURE_WAT;
    for (wat, reason) in [
        (
            fixture.replace(
                r#"(data (i32.const 128) "{\"a\":1}")"#,
                r#"(data (i32.const 128) "notjson")"#,
            ),
            "default-config is not valid JSON",
        ),
        (
            fixture.replace(
                r#"(data (i32.const 64)  "Hello")"#,
                r#"(data (i32.const 64)  "He\00lo")"#,
            ),
            "contains control characters",
        ),
        (
            fixture.replace(
                r#"(data (i32.const 64)  "Hello")"#,
                r#"(data (i32.const 64)  "     ")"#,
            ),
            "is empty",
        ),
        (
            // 300 bytes in otherwise-unused memory, pointed at by the
            // descriptor's name slot.
            fixture
                .replace(
                    r#"(data (i32.const 64)  "Hello")"#,
                    &format!(r#"(data (i32.const 2048) "{}")"#, "n".repeat(300)),
                )
                .replace(
                    "(i32.store (i32.const 520) (i32.const 64))\n      (i32.store (i32.const 524) (i32.const 5))",
                    "(i32.store (i32.const 520) (i32.const 2048))\n      (i32.store (i32.const 524) (i32.const 300))",
                ),
            "exceeds 256 bytes",
        ),
    ] {
        assert_ne!(wat, fixture, "the fixture patch for `{reason}` must apply");
        let component = wat::parse_str(&wat).unwrap();
        let err = validator.validate(&component).await.unwrap_err();
        assert!(err.to_string().contains(reason), "{reason}: {err}");
    }
}

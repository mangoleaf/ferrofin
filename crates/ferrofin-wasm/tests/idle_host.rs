//! The idle footprint: with no `*.wasm` installed, neither the plugin host nor
//! the install-time validator builds a wasm engine (each starts an
//! epoch-ticker thread) or the guest HTTP clients (each blocking client runs a
//! thread of its own), and a validation's engine ends with it.
//!
//! One test in its own binary, so no other test's threads share the process.
#![cfg(target_os = "linux")]

use ferrofin_traits::plugins::PluginArtifactValidator as _;
use ferrofin_wasm::{WasmArtifactValidator, WasmPluginHost, WasmSettings};

/// The names of the process's live threads (truncated to 15 bytes by Linux).
fn thread_names() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .expect("read /proc/self/task")
        .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
        .collect()
}

fn epoch_tickers() -> usize {
    thread_names()
        .iter()
        .filter(|name| name.starts_with("wasm-epoch"))
        .count()
}

#[test]
fn the_wasm_runtime_lives_only_while_a_plugin_or_a_validation_needs_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), b"not a plugin").unwrap();
    let before = thread_names().len();

    let host = WasmPluginHost::load(dir.path(), &WasmSettings::default()).unwrap();
    let missing =
        WasmPluginHost::load(&dir.path().join("absent"), &WasmSettings::default()).unwrap();
    let validator = WasmArtifactValidator::new(&WasmSettings::default());

    assert!(host.plugins().is_empty());
    assert!(missing.plugins().is_empty());
    assert_eq!(
        thread_names().len(),
        before,
        "an idle host and validator must start no epoch ticker or HTTP client thread"
    );

    // A validation builds an engine for itself; once it returns, its ticker
    // finds the engine gone at the next tick (1 s) and exits.
    let component = wat::parse_str(ferrofin_wasm::TEST_FIXTURE_WAT).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(validator.validate(&component))
        .expect("fixture is valid");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while epoch_tickers() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        epoch_tickers(),
        0,
        "the validator must not keep an engine after its validation returns"
    );
}

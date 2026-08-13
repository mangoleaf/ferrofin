//! The reference Ferrofin WASM plugin: greets via the host log on its task,
//! and counts the events it sees. Exists to (a) prove the `ferrofin:plugin`
//! world end-to-end with a real `cargo`-built component, (b) anchor the WIT
//! contract in CI, and (c) be the template plugin authors copy.

wit_bindgen::generate!({
    // The single source of truth for the contract — the host's WIT file.
    path: "../../crates/ferrofin-wasm/wit",
    world: "plugin",
});

use ferrofin::plugin::host;
use ferrofin::plugin::types::LogLevel;
use std::sync::atomic::{AtomicU64, Ordering};
// PluginDescriptor/TaskDescriptor are hoisted to the crate root by
// `generate!` (the world `use`s them), so they need no import.

/// Events observed since load (the host delivers only while enabled).
static EVENTS_SEEN: AtomicU64 = AtomicU64::new(0);

struct HelloPlugin;

impl Guest for HelloPlugin {
    fn descriptor() -> PluginDescriptor {
        PluginDescriptor {
            id: "3f9a2f60-88f1-4f52-b3f4-6f3a1c2d9e01".to_owned(),
            name: "Hello Ferrofin".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            description: "Reference WASM plugin: logs a greeting and counts server events."
                .to_owned(),
        }
    }

    fn default_config() -> String {
        r#"{"Greeting":"Hello from WASM"}"#.to_owned()
    }

    fn tasks() -> Vec<TaskDescriptor> {
        vec![TaskDescriptor {
            id: "greet".to_owned(),
            name: "Say hello".to_owned(),
            description: "Logs the configured greeting plus the events seen so far.".to_owned(),
            category: "Examples".to_owned(),
        }]
    }

    fn run_task(task_id: String) -> Result<(), String> {
        if task_id != "greet" {
            return Err(format!("unknown task `{task_id}`"));
        }
        // The admin-edited config arrives as JSON; read the greeting without
        // pulling a JSON dependency into the reference plugin.
        let config = host::get_config();
        let greeting = config
            .split_once("\"Greeting\":\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map_or("Hello from WASM", |(g, _)| g);
        host::log(
            LogLevel::Info,
            &format!(
                "{greeting} (events seen since load: {})",
                EVENTS_SEEN.load(Ordering::Relaxed)
            ),
        );
        Ok(())
    }

    fn on_event(event_name: String, _event_json: String) {
        EVENTS_SEEN.fetch_add(1, Ordering::Relaxed);
        host::log(LogLevel::Debug, &format!("saw event {event_name}"));
    }
}

export!(HelloPlugin);

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
// PluginDescriptor/TaskDescriptor/ItemSummary/MetadataResult are hoisted to
// the crate root by `generate!` (the world `use`s them), so no import.

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
        r#"{"Greeting":"Hello from WASM","ReportUrl":""}"#.to_owned()
    }

    fn config_pages() -> Vec<ConfigPage> {
        // A real authored settings page: the canonical jellyfin-web plugin
        // page shape (data-role="page" root + inline script against the
        // ApiClient/Dashboard globals). It loads the config with
        // getPluginConfiguration, lets the admin edit the greeting, and
        // saves with updatePluginConfiguration — the full round trip.
        let id = "3f9a2f60-88f1-4f52-b3f4-6f3a1c2d9e01";
        let html = format!(
            r#"<div id="helloFerrofinConfig" data-role="page" class="page type-interior pluginConfigurationPage">
  <div data-role="content"><div class="content-primary">
    <form class="helloFerrofinForm">
      <h1>Hello Ferrofin</h1>
      <div class="inputContainer">
        <label class="inputLabel" for="helloGreeting">Greeting</label>
        <input is="emby-input" id="helloGreeting" type="text" />
        <div class="fieldDescription">Logged by the "Say hello" task.</div>
      </div>
      <div class="inputContainer">
        <label class="inputLabel" for="helloReportUrl">Report URL</label>
        <input is="emby-input" id="helloReportUrl" type="text" />
        <div class="fieldDescription">Optional endpoint the "Analyze library" task reports to.</div>
      </div>
      <button is="emby-button" type="submit" class="raised button-submit block"><span>Save</span></button>
    </form>
  </div></div>
  <script type="text/javascript">
  (function () {{
    var pluginId = '{id}';
    var page = document.querySelector('#helloFerrofinConfig');
    page.addEventListener('pageshow', function () {{
      Dashboard.showLoadingMsg();
      ApiClient.getPluginConfiguration(pluginId).then(function (config) {{
        page.querySelector('#helloGreeting').value = config.Greeting || '';
        page.querySelector('#helloReportUrl').value = config.ReportUrl || '';
        Dashboard.hideLoadingMsg();
      }});
    }});
    page.querySelector('.helloFerrofinForm').addEventListener('submit', function (e) {{
      e.preventDefault();
      Dashboard.showLoadingMsg();
      ApiClient.getPluginConfiguration(pluginId).then(function (config) {{
        config.Greeting = page.querySelector('#helloGreeting').value;
        config.ReportUrl = page.querySelector('#helloReportUrl').value;
        ApiClient.updatePluginConfiguration(pluginId, config).then(
          Dashboard.processPluginConfigurationUpdateResult
        );
      }});
      return false;
    }});
  }})();
  </script>
</div>
"#
        );
        vec![ConfigPage {
            name: "hello-ferrofin".to_owned(),
            content: html.into_bytes(),
            enable_in_main_menu: false,
        }]
    }

    fn tasks() -> Vec<TaskDescriptor> {
        vec![
            TaskDescriptor {
                id: "greet".to_owned(),
                name: "Say hello".to_owned(),
                description: "Logs the configured greeting plus the events seen so far."
                    .to_owned(),
                category: "Examples".to_owned(),
            },
            TaskDescriptor {
                id: "analyze".to_owned(),
                name: "Analyze library".to_owned(),
                description: "Queries movies, writes a demo intro segment on the first one, \
                              and pings the configured ReportUrl."
                    .to_owned(),
                category: "Examples".to_owned(),
            },
        ]
    }

    fn run_task(task_id: String) -> Result<(), String> {
        match task_id.as_str() {
            "greet" => run_greet(),
            "analyze" => run_analyze(),
            other => Err(format!("unknown task `{other}`")),
        }
    }

    fn on_event(event_name: String, _event_json: String) {
        EVENTS_SEEN.fetch_add(1, Ordering::Relaxed);
        host::log(LogLevel::Debug, &format!("saw event {event_name}"));
    }

    fn metadata_lookup(
        item: ItemSummary,
        _provider_ids: Vec<(String, String)>,
    ) -> Result<Option<MetadataResult>, String> {
        // The reference "metadata source": recognizes one demo title. Real
        // plugins would consult their database (via http-fetch) here.
        if item.kind == "Movie" && item.name.contains("Bunny") {
            return Ok(Some(MetadataResult {
                overview: Some("A big rabbit deals with three tiny bullies. (Metadata \
                                contributed by the Hello Ferrofin WASM plugin.)"
                    .to_owned()),
                production_year: Some(2008),
                community_rating: Some(7.9),
                genres: vec!["Animation".to_owned(), "Short".to_owned()],
                provider_ids: vec![("HelloDb".to_owned(), "bbb-1".to_owned())],
            }));
        }
        Ok(None)
    }
}

fn run_greet() -> Result<(), String> {
    // The admin-edited config arrives as JSON; read the greeting without
    // pulling a JSON dependency into the reference plugin.
    let greeting = config_value("Greeting").unwrap_or_else(|| "Hello from WASM".to_owned());
    host::log(
        LogLevel::Info,
        &format!(
            "{greeting} (events seen since load: {})",
            EVENTS_SEEN.load(Ordering::Relaxed)
        ),
    );
    Ok(())
}

/// The E2 capability demo: read the library (query-items), persist a result
/// (write-media-segments), and report out (http-fetch).
fn run_analyze() -> Result<(), String> {
    use ferrofin::plugin::types::{HttpRequest, ItemQuery, MediaSegment};

    let movies = host::query_items(&ItemQuery {
        kinds: vec!["Movie".to_owned()],
        parent_id: None,
        search_term: None,
        limit: Some(10),
    })?;
    host::log(LogLevel::Info, &format!("found {} movie(s)", movies.len()));

    // "Analysis": write a demo intro segment (first 30 s) on the first movie.
    if let Some(first) = movies.first() {
        host::write_media_segments(
            &first.id,
            &[MediaSegment {
                segment_type: "Intro".to_owned(),
                start_ticks: 0,
                end_ticks: 30 * 10_000_000, // 30 s in 100 ns ticks
            }],
        )?;
        host::log(
            LogLevel::Info,
            &format!("wrote a demo intro segment on `{}`", first.name),
        );
    }

    // Report the run to the configured endpoint, when one is set.
    if let Some(url) = config_value("ReportUrl").filter(|u| !u.is_empty()) {
        let response = host::http_fetch(&HttpRequest {
            method: "POST".to_owned(),
            url,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: Some(format!("analyzed {} movie(s)", movies.len()).into_bytes()),
        })?;
        host::log(
            LogLevel::Info,
            &format!("report delivered (status {})", response.status),
        );
    }
    Ok(())
}

/// Reads a top-level string value out of the config JSON without a JSON
/// dependency (the reference plugin stays a single small file).
fn config_value(key: &str) -> Option<String> {
    let config = host::get_config();
    let marker = format!("\"{key}\":\"");
    config
        .split_once(marker.as_str())
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(v, _)| v.to_owned())
}

export!(HelloPlugin);

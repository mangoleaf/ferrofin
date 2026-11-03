//! The **Ferrofin** extension: the server's own settings, where the dashboard
//! shows the bootstrap configuration (CLI flags, `FERROFIN_*` environment
//! variables, `config.toml`).
//!
//! Unlike the other extensions it ports no Jellyfin plugin, so it is the one
//! [`disable_extensions`](crate::registered_extensions) leaves in place: it
//! contributes no tasks, hooks or providers, only a `/Plugins` entry and its
//! settings page (Dashboard → Plugins → Ferrofin, and the dashboard menu's
//! Ferrofin entry), which the dashboard's Disable toggle still turns off. The
//! page lists every setting `GET /Ferrofin/Settings` reports, edits the ones
//! that route marks editable, saves them with `POST /Ferrofin/Settings`, and
//! restarts the server to apply them. The settings themselves live in the
//! server binary, which owns the configuration, behind the
//! [`ServerSettingsManager`](ferrofin_traits::server_settings::ServerSettingsManager)
//! seam; the extension is what gives them a plugin identity and an enabled
//! flag, which that manager reads (a disabled plugin's routes answer `404`).

use std::sync::Arc;

use ferrofin_core::{PluginConfigPage, ScheduledTask};
use ferrofin_traits::plugins::PluginDescriptor;
use uuid::Uuid;

use crate::{Extension, ExtensionContext};

/// The Ferrofin plugin's id in `/Plugins`: Ferrofin's own, fixed for good, so
/// its enabled flag and configuration survive every upgrade.
pub const EXTENSION_ID: Uuid = Uuid::from_u128(0xe2dc_c94a_88c8_4bb1_bbf0_f5d2_7aa5_b053);

/// The Ferrofin settings extension (see the [module docs](self)).
#[derive(Debug, Clone, Copy, Default)]
pub struct FerrofinSettingsExtension;

impl Extension for FerrofinSettingsExtension {
    fn id(&self) -> Uuid {
        EXTENSION_ID
    }

    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: EXTENSION_ID,
            name: "Ferrofin".to_owned(),
            version: plugin_version(ferrofin_health::build_version()),
            description: "Ferrofin's own settings: paths, network, ffmpeg, library scans, \
                          logging, metadata providers and WASM plugins, as set by command-line \
                          flags, environment variables or config.toml."
                .to_owned(),
            enabled: true,
            has_image: false,
            can_uninstall: false,
            configuration_file_name: None,
        }
    }

    fn default_config(&self) -> Vec<u8> {
        b"{}".to_vec()
    }

    fn config_pages(&self) -> Vec<PluginConfigPage> {
        // Authored in this repository, not vendored from a plugin, so
        // `build.rs` does not pin or copy it. It holds no setting: the page is
        // served anonymously, and its script reads them from the admin-only
        // `GET /Ferrofin/Settings`.
        vec![PluginConfigPage {
            name: "Ferrofin".to_owned(),
            bytes: include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/assets/ferrofin/settings.html"
            ))
            .to_vec(),
            enable_in_main_menu: true,
        }]
    }

    fn tasks(&self, _cx: &ExtensionContext) -> Vec<Arc<dyn ScheduledTask>> {
        Vec::new()
    }
}

/// The plugin's version for the server built as `build` (its `git
/// describe`, `v1.5.0-12-gabc123`): that release, `1.5.0`, with the fourth
/// component .NET versions carry, since `/Plugins/{id}/{version}/…` binds the
/// version as one. A build with no release tag in its history (a bare commit)
/// reports the crate's version.
fn plugin_version(build: &str) -> String {
    let release = build
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()
        .unwrap_or_default();
    let numeric = release.split('.').count() == 3
        && release
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    if numeric {
        format!("{release}.0")
    } else {
        concat!(env!("CARGO_PKG_VERSION"), ".0").to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptor_names_ferrofin_under_its_fixed_id() {
        let descriptor = FerrofinSettingsExtension.descriptor();
        assert_eq!(descriptor.id, EXTENSION_ID);
        assert_eq!(FerrofinSettingsExtension.id(), EXTENSION_ID);
        assert_eq!(descriptor.name, "Ferrofin");
        assert_eq!(
            descriptor.version,
            plugin_version(ferrofin_health::build_version())
        );
        assert!(!descriptor.can_uninstall);
        assert_eq!(FerrofinSettingsExtension.default_config(), b"{}");
    }

    /// The extension's one page, its settings page, which jellyfin-web
    /// fetches by name: `configurationpage?name=Ferrofin`.
    fn settings_page() -> PluginConfigPage {
        let mut pages = FerrofinSettingsExtension.config_pages();
        let names: Vec<&str> = pages.iter().map(|page| page.name.as_str()).collect();
        assert_eq!(names, ["Ferrofin"]);
        pages.remove(0)
    }

    /// The settings page's HTML, as `GET /web/ConfigurationPage` serves it.
    fn settings_html() -> String {
        String::from_utf8(settings_page().bytes).expect("the page is UTF-8")
    }

    #[test]
    fn the_settings_page_is_registered_for_the_plugin_list_and_the_menu() {
        // jellyfin-web links the Settings button and the menu entry to the
        // page by its name, labelled with the plugin's name.
        assert!(
            settings_page().enable_in_main_menu,
            "the dashboard menu lists the page"
        );
        let html = settings_html();
        // The plugin-page shape jellyfin-web loads: it keeps only the
        // `data-role="page"` element and runs the scripts inside it.
        assert!(html.contains(r#"<div data-role="page""#), "{html}");
        assert!(html.contains("pluginConfigurationPage"), "{html}");
        assert!(html.contains("ApiClient.getUrl('Ferrofin/Settings')"));
    }

    #[test]
    fn the_settings_page_embeds_no_setting_and_renders_text_only() {
        let html = settings_html();
        // The page is anonymous (`GET /web/ConfigurationPage`): every setting
        // comes from the admin-only `GET /Ferrofin/Settings`, none is written
        // into it, so a new setting needs no page change. Each setting but
        // the OpenTelemetry ones has a `FERROFIN_*` variable.
        assert!(!html.contains("FERROFIN_"), "a setting is hardcoded");
        // Every string from the response is set as text or an attribute,
        // never parsed as HTML.
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "createContextualFragment",
            ".html(",
        ] {
            assert!(!html.contains(sink), "the page uses `{sink}`");
        }
    }

    #[test]
    fn the_settings_page_saves_through_the_settings_route_and_restarts() {
        let html = settings_html();
        // Save posts the changes, the server's one refusal shows beside the
        // setting: a browser's own bubble for `Min`/`Max` would stop the form
        // before the server could judge the value.
        assert!(html.contains("novalidate"), "{html}");
        assert!(html.contains("type: 'POST'"), "{html}");
        assert!(html.contains("{ Changes: "), "{html}");
        assert!(html.contains("ApiClient.restartServer()"), "{html}");
        // A control for each kind that is not plain text.
        for kind in [
            "Bool",
            "Integer",
            "CountOrAuto",
            "PathList",
            "NumberList",
            "Choice",
        ] {
            assert!(
                html.contains(&format!("case '{kind}':")),
                "no control for {kind}"
            );
        }
        // jellyfin-web upgrades these once they are in the document, and
        // `emby-input` needs its parent by then.
        for element in ["emby-input", "emby-checkbox", "emby-button", "emby-select"] {
            assert!(html.contains(&format!("'is', '{element}'")), "{element}");
        }
        assert!(
            html.contains(r#"data-require="emby-input,emby-button,emby-checkbox,emby-select""#),
            "{html}"
        );
    }

    #[test]
    fn the_settings_page_offers_choices_notes_and_settings_that_apply_at_once() {
        let html = settings_html();
        // A choice is a select of its options, the picked one's note below
        // it, and a Custom entry for a stored value that fits none.
        assert!(html.contains("setting.Options"), "{html}");
        assert!(html.contains("option.Note"), "{html}");
        assert!(html.contains("setting.Custom"), "{html}");
        // A row's longer note shows under its description.
        assert!(html.contains("setting.Note"), "{html}");
        assert!(html.contains("ferrofinSettingNote"), "{html}");
        // A row stored in the server configuration applies at once: no
        // restart note, and a save of it alone points to no restart.
        assert!(
            html.contains("setting.Store === 'ServerConfiguration'"),
            "{html}"
        );
        assert!(html.contains("setting.Applies === 'Immediately'"), "{html}");
        assert!(html.contains("result.RestartRequired"), "{html}");
    }

    #[test]
    fn the_settings_page_lists_the_settings_in_tabs_by_name_and_unit() {
        let html = settings_html();
        // A tab per group the report names, in its order; the panels are
        // shown one at a time.
        assert!(
            html.contains(r#"class="ferrofinTabs" role="tablist""#),
            "{html}"
        );
        assert!(html.contains("setAttribute('role', 'tab')"), "{html}");
        assert!(html.contains("setAttribute('role', 'tabpanel')"), "{html}");
        // Each row is headed by its readable name, with its key and variable
        // on a line below, and its value in its unit.
        assert!(html.contains("setting.Label"), "{html}");
        assert!(html.contains("ferrofinSettingIds"), "{html}");
        assert!(html.contains("setting.Unit"), "{html}");
        // The groups are named as the report names them: none is hardcoded.
        assert!(!html.contains("GROUP_TITLES"), "{html}");
    }

    #[test]
    fn the_settings_page_edits_a_setting_on_one_line_and_finds_it_by_search_and_filters() {
        let html = settings_html();
        // The value field has no "Set to" label: an unset one shows the
        // default as its placeholder, and Reset to default sits beside it.
        assert!(!html.contains("Set to"), "{html}");
        assert!(
            html.contains("setAttribute('placeholder', setting.Default)"),
            "{html}"
        );
        assert!(html.contains("ferrofinReset"), "{html}");
        // A setting fixed by a variable or a flag shows its value as text,
        // with a lock and the reason, which names what sets it.
        assert!(html.contains("ferrofinLocked"), "{html}");
        assert!(html.contains("icon('lock')"), "{html}");
        assert!(html.contains("setting.LockedReason"), "{html}");
        // A search box and three filters above the tabs, across every tab.
        for class in [
            "ferrofinSearch",
            "ferrofinFilterChanged",
            "ferrofinFilterRestart",
            "ferrofinFilterLocked",
        ] {
            assert!(html.contains(&format!("class=\"{class}\"")), "{class}");
        }
        // The search reads the name, the description, the key, the variable
        // and the flag.
        for member in [
            "setting.Label",
            "setting.Description",
            "setting.Key",
            "setting.EnvVar",
            "setting.Flag",
        ] {
            assert!(html.contains(&format!("{member} ||")), "{member}");
        }
        // Junk in a number box is sent for the server to refuse: the browser
        // reports it as an empty value, which would pass for no change.
        assert!(html.contains("validity.badInput"), "{html}");
        // Each tab counts its unsaved edits, and marks a refusal on it.
        assert!(html.contains("ferrofinTabCount"), "{html}");
        assert!(html.contains("ferrofinTabRefused"), "{html}");
    }

    #[rstest::rstest]
    #[case::release("v1.5.0", "1.5.0.0")]
    #[case::after_a_release("v1.4.0-55-ge60eae77a188", "1.4.0.0")]
    #[case::dirty("v1.4.0-55-ge60eae77a188-dirty", "1.4.0.0")]
    #[case::release_candidate("v1.5.0-rc.1", "1.5.0.0")]
    #[case::bare_commit("e60eae77a188", concat!(env!("CARGO_PKG_VERSION"), ".0"))]
    #[case::two_parts("v1.5", concat!(env!("CARGO_PKG_VERSION"), ".0"))]
    fn the_version_is_the_release_the_server_was_built_from(
        #[case] build: &str,
        #[case] version: &str,
    ) {
        assert_eq!(plugin_version(build), version);
    }
}

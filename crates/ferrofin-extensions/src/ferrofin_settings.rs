//! The **Ferrofin** extension: the server's own settings, where the dashboard
//! shows the bootstrap configuration (CLI flags, `FERROFIN_*` environment
//! variables, `config.toml`).
//!
//! Unlike the other extensions it ports no Jellyfin plugin, so it is the one
//! [`disable_extensions`](crate::registered_extensions) leaves in place: it
//! contributes no tasks, hooks or providers, only a `/Plugins` entry, which
//! the dashboard's Disable toggle still turns off. The settings themselves
//! live in the server binary, which owns the configuration, behind the
//! [`ServerSettingsManager`](ferrofin_traits::server_settings::ServerSettingsManager)
//! seam; the extension is what gives them a plugin identity and an enabled
//! flag, which that manager reads (a disabled plugin's routes answer `404`).

use std::sync::Arc;

use ferrofin_core::ScheduledTask;
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
        assert!(FerrofinSettingsExtension.config_pages().is_empty());
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

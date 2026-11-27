//! Adopting a Jellyfin install's plugin settings: those of the metadata plugins
//! Jellyfin builds in (TMDb, OMDb, MusicBrainz, `AudioDB`, Studio Images) and of
//! the plugins Ferrofin has a compiled-in counterpart of, which the composition
//! root names. Each one's `PluginConfiguration` document comes across once from
//! Jellyfin's `{program data}/plugins/configurations/<assembly>.xml`
//! (`BasePlugin<T>.ConfigurationFilePath`, `BasePluginOfT.cs:125,131`) into the
//! plugin's `config.json`, so an adopted server keeps the settings its admin
//! chose instead of coming up at the defaults.
//!
//! The document is read with the server configuration's importer
//! ([`crate::config_import`]) over the plugin's default configuration: a
//! member Ferrofin does not store is skipped, and one the document leaves out
//! keeps its default, as upstream's constructor value would. The XML file is
//! never changed or removed.
//!
//! The import runs per plugin, and only while that plugin has no
//! `config.json`, so a second boot, or an admin's later edit, is never
//! overwritten. The file it writes is what marks it done. The previous
//! version, running beside this one in a rolling update, writes a plugin's
//! `config.json` only when an admin saves it, so it cannot undo the import.

use std::path::Path;

use ferrofin_common::config_import::{Node, parse};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::plugins::PluginManager;
use uuid::Uuid;

use crate::config_import;

/// The document element of every plugin configuration upstream writes: the
/// class is `PluginConfiguration` in each plugin's own namespace.
const ROOT_ELEMENT: &str = "PluginConfiguration";

/// For each `(plugin id, configuration file name)` of `adopted`, imports the
/// document of that name in `configurations` (Jellyfin's
/// `plugins/configurations` directory) while the plugin has no configuration
/// in `plugin_state_dir` (the plugin manager's directory, holding
/// `{id}/config.json`).
///
/// A plugin that is not registered, or a document that is not there, is
/// skipped. A document that cannot be read or parsed, or a write that fails,
/// is logged, and the next boot tries again: this never stops the server
/// starting.
pub async fn import_adopted_plugin_configs(
    configurations: &Path,
    plugin_state_dir: &Path,
    plugins: &dyn PluginManager,
    adopted: &[(Uuid, &str)],
) {
    for &(id, file_name) in adopted {
        let xml_path = configurations.join(file_name);
        match import(&xml_path, plugin_state_dir, plugins, id).await {
            Ok(false) => {}
            Ok(true) => tracing::info!(
                path = %xml_path.display(),
                plugin_id = %id,
                "imported the plugin configuration of the adopted Jellyfin"
            ),
            Err(error) => tracing::warn!(
                path = %xml_path.display(),
                plugin_id = %id,
                %error,
                "the adopted plugin configuration could not be imported; the next boot tries again"
            ),
        }
    }
}

/// [`import_adopted_plugin_configs`]' work for one plugin: whether it imported.
async fn import(
    xml_path: &Path,
    plugin_state_dir: &Path,
    plugins: &dyn PluginManager,
    id: Uuid,
) -> Result<bool, ServiceError> {
    if plugins.get_plugin(id).await?.is_none() {
        return Ok(false);
    }
    let config_file = plugin_state_dir.join(id.to_string()).join("config.json");
    // `try_exists`, not `exists`: a configuration that cannot be checked must
    // not be taken for a missing one and overwritten.
    if tokio::fs::try_exists(&config_file)
        .await
        .map_err(|e| ServiceError::backend(format!("check {}: {e}", config_file.display())))?
    {
        return Ok(false);
    }
    let xml = match tokio::fs::read_to_string(xml_path).await {
        Ok(xml) => xml,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ServiceError::backend(format!("read: {e}"))),
    };
    let default: serde_json::Value =
        serde_json::from_slice(&plugins.get_plugin_configuration(id).await?)
            .map_err(|e| ServiceError::backend(format!("default configuration: {e}")))?;
    // The parser's message can quote the document's text (an unknown `&name;`
    // entity), a key or a password included, so none of it is logged.
    let unreadable = |_| ServiceError::backend("not a readable PluginConfiguration document");
    let shape = list_shapes(&default, &parse(&xml).map_err(unreadable)?);
    // No member is denied: no document holds a path into Jellyfin's container
    // (Merge Versions' and Intro Skipper's excluded paths are the libraries',
    // which adoption keeps as they are), and a key or an account is the admin's
    // own, kept in the same `config.json` the settings page writes.
    let mut imported =
        config_import::import_over(&shape, &xml, ROOT_ELEMENT, &[]).map_err(unreadable)?;
    empty_untouched_lists(&default, &mut imported);
    let bytes = serde_json::to_vec(&imported)
        .map_err(|e| ServiceError::backend(format!("serialize: {e}")))?;
    plugins.set_plugin_configuration(id, bytes).await?;
    Ok(true)
}

/// `default` with an entry in each empty list, to type the document's entries
/// by: the importer types a list's entries after the default's. Every list in
/// these documents holds text, either a `string[]` (TMDb's library ids, Intro
/// Skipper's exclusions) or objects whose members are all text (File
/// Transformation's `PluginDefinedTransformation`, a `Guid` and three strings).
/// So the entry is an empty string, or an object holding each member the
/// document's entries have, empty, and an id or a search text that looks like
/// a number or a bool is still read as text.
///
/// shortcut: an entry's members are all read as text; a plugin whose entries
/// hold a number, a bool or a list needs its entry shape declared.
fn list_shapes(default: &serde_json::Value, document: &Node) -> serde_json::Value {
    let mut shape = default.clone();
    if let serde_json::Value::Object(members) = &mut shape {
        for (name, value) in members.iter_mut() {
            if !value.as_array().is_some_and(Vec::is_empty) {
                continue;
            }
            let entry: serde_json::Map<String, serde_json::Value> = document
                .children
                .iter()
                .filter(|list| list.name == *name)
                .flat_map(|list| &list.children)
                .flat_map(|entry| &entry.children)
                .map(|member| (member.name.clone(), serde_json::json!("")))
                .collect();
            *value = if entry.is_empty() {
                serde_json::json!([""])
            } else {
                serde_json::json!([entry])
            };
        }
    }
    shape
}

/// Puts back empty each list [`list_shapes`] gave an entry that the document
/// did not replace.
fn empty_untouched_lists(default: &serde_json::Value, imported: &mut serde_json::Value) {
    if let (serde_json::Value::Object(defaults), serde_json::Value::Object(members)) =
        (default, imported)
    {
        for (key, value) in members.iter_mut() {
            let empty_by_default = defaults
                .get(key)
                .and_then(serde_json::Value::as_array)
                .is_some_and(Vec::is_empty);
            if empty_by_default && *value == serde_json::json!([""]) {
                *value = serde_json::json!([]);
            }
        }
    }
}

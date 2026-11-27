//! The one-way import of a Jellyfin `config/*.xml` document into Ferrofin's
//! JSON configuration. It lives in `ferrofin-common`, so the compiled-in
//! extensions (LDAP-Auth's adoption) run it without `ferrofin-core`; core's
//! callers keep this path. The tests here run it over the server's own
//! configuration, which only core builds.

pub use ferrofin_common::config_import::{
    BRANDING_XML_DENY, ENCODING_XML_DENY, LIVETV_OPTIONS_XML_DENY, LIVETV_XML_DENY,
    NETWORK_XML_DENY, SYSTEM_XML_DENY, Unimported, import_over, import_reporting,
};
// The export's tests compare documents as parsed trees.
#[cfg(test)]
pub(crate) use ferrofin_common::config_import::{Node, parse};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration_manager::default_server_configuration;
    use ferrofin_model::configuration::ServerConfiguration;

    /// A trimmed `config/system.xml` in the shapes the .NET XML serializer
    /// emits: scalars, `<string>` collections, a collection of objects, a
    /// nested object, and a self-closing empty element.
    const SYSTEM_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<ServerConfiguration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <LogFileRetentionDays>7</LogFileRetentionDays>
  <IsStartupWizardCompleted>true</IsStartupWizardCompleted>
  <EnableMetrics>true</EnableMetrics>
  <IsPortAuthorized>true</IsPortAuthorized>
  <CacheSize>800</CacheSize>
  <ServerName>basement</ServerName>
  <MetadataPath>/config/metadata</MetadataPath>
  <SortRemoveWords />
  <SortRemoveCharacters>
    <string>&amp;</string>
  </SortRemoveCharacters>
  <SortReplaceCharacters>
    <string>.</string>
    <string>!</string>
  </SortReplaceCharacters>
  <PluginRepositories>
    <RepositoryInfo>
      <Name>Someone Else's Plugins</Name>
      <Url>https://example.invalid/manifest.json</Url>
      <Enabled>true</Enabled>
    </RepositoryInfo>
  </PluginRepositories>
  <CastReceiverApplications>
    <CastReceiverApplication>
      <Id>F007D354</Id>
      <Name>Stable</Name>
    </CastReceiverApplication>
    <CastReceiverApplication>
      <Id>6F511C87</Id>
      <Name>Unstable</Name>
    </CastReceiverApplication>
  </CastReceiverApplications>
  <TrickplayOptions>
    <EnableHwAcceleration>true</EnableHwAcceleration>
    <Interval>10000</Interval>
  </TrickplayOptions>
</ServerConfiguration>"#;

    fn import_system(xml: &str) -> ServerConfiguration {
        import_over(
            &default_server_configuration(),
            xml,
            "ServerConfiguration",
            SYSTEM_XML_DENY,
        )
        .expect("system.xml imports")
    }

    fn imported_system() -> ServerConfiguration {
        import_system(SYSTEM_XML)
    }

    #[test]
    fn scalars_take_their_type_from_the_ferrofin_default() {
        let cfg = imported_system();
        assert!(cfg.is_startup_wizard_completed, "bool");
        assert!(cfg.enable_metrics);
        assert!(cfg.is_port_authorized);
        assert_eq!(cfg.log_file_retention_days, 7, "integer");
        assert_eq!(cfg.cache_size, 800);
        assert_eq!(cfg.server_name, "basement", "string");
    }

    #[test]
    fn string_collections_replace_the_default_and_are_unescaped() {
        let cfg = imported_system();
        assert_eq!(cfg.sort_remove_characters, ["&"]);
        assert_eq!(cfg.sort_replace_characters, [".", "!"]);
    }

    #[test]
    fn an_empty_element_clears_a_collection_that_has_a_non_empty_default() {
        // `SortRemoveWords` defaults to ["the", "a", "an"], so an operator who
        // emptied it must not have it handed back.
        assert_eq!(default_server_configuration().sort_remove_words.len(), 3);
        let sort_remove_words = imported_system().sort_remove_words;
        assert!(sort_remove_words.is_empty(), "{sort_remove_words:?}");
    }

    #[test]
    fn collections_of_objects_and_nested_objects_are_imported() {
        let cfg = imported_system();
        let apps = &cfg.cast_receiver_applications;
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].id, "F007D354");
        assert_eq!(apps[1].name, "Unstable");
        assert!(cfg.trickplay_options.enable_hw_acceleration);
        assert_eq!(cfg.trickplay_options.interval, 10_000);
    }

    #[test]
    fn a_nested_object_keeps_the_defaults_the_xml_does_not_mention() {
        let default = default_server_configuration();
        let cfg = imported_system();
        assert_eq!(
            cfg.trickplay_options.tile_width,
            default.trickplay_options.tile_width
        );
        assert_eq!(
            cfg.trickplay_options.qscale,
            default.trickplay_options.qscale
        );
        // Ferrofin's own opt-in: Jellyfin's XML never has it, so it is off.
        assert!(!cfg.trickplay_options.full_chroma);
    }

    #[test]
    fn denied_fields_are_not_imported() {
        let default = default_server_configuration();
        let cfg = imported_system();
        // A path inside the Jellyfin container, and a repository of .NET
        // assemblies Ferrofin cannot load. The fixture's repository is one the
        // default does NOT carry, so this fails if the deny list stops working.
        assert!(
            !default
                .plugin_repositories
                .iter()
                .any(|r| r.name.as_deref() == Some("Someone Else's Plugins")),
            "the fixture repository must not be one of the defaults"
        );
        assert_eq!(cfg.metadata_path, "");
        assert_eq!(cfg.plugin_repositories, default.plugin_repositories);
    }

    #[test]
    fn a_field_ferrofin_does_not_have_is_reported_and_the_rest_still_imports() {
        // Jellyfin writes `EnableIPv4`, which `ServerConfiguration` does not
        // have (a `PascalCase` derive spells it `EnableIpv4`): the import
        // reports it (`ferrofin-common`'s tests check that), and the fields
        // around it must still import.
        let xml = "<ServerConfiguration><EnableIPv4>false</EnableIPv4>\
                   <ServerName>basement</ServerName></ServerConfiguration>";
        assert_eq!(import_system(xml).server_name, "basement");
    }

    #[test]
    fn a_value_ferrofin_rejects_costs_only_its_own_field() {
        let xml = "<ServerConfiguration><ImageSavingConvention>Nonsense</ImageSavingConvention>\
                   <ServerName>basement</ServerName></ServerConfiguration>";
        let default = default_server_configuration();
        let cfg = import_system(xml);
        assert_eq!(cfg.image_saving_convention, default.image_saving_convention);
        assert_eq!(cfg.server_name, "basement");
    }

    #[test]
    fn an_empty_member_of_a_collection_entry_does_not_lose_the_whole_field() {
        // `PathSubstitutions` defaults to empty, so an entry has no schema at
        // all and `<To />` has to be read from the element alone. It is an
        // empty *string*, not null — `XmlSerializer` writes `<Foo />` for `""`
        // and omits a null member — and reading it as null would take the whole
        // `PathSubstitutions` field down with it, since `To` is a `String`.
        let xml = "<ServerConfiguration><PathSubstitutions><PathSubstitution>\
                   <From>/media</From><To /></PathSubstitution></PathSubstitutions>\
                   </ServerConfiguration>";
        let subs = import_system(xml).path_substitutions;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].from, "/media");
        assert_eq!(subs[0].to, "");
    }

    #[test]
    fn xsi_nil_is_the_one_thing_that_does_mean_null() {
        // How `XmlSerializer` writes a null *value* type. `CacheSize` is where
        // it shows up in practice: the C# leaves it null on a fresh instance.
        let xml = "<ServerConfiguration xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\
                   <ActivityLogRetentionDays xsi:nil=\"true\" />\
                   <ServerName>basement</ServerName></ServerConfiguration>";
        let cfg = import_system(xml);
        assert_eq!(cfg.activity_log_retention_days, None);
        assert_eq!(
            cfg.server_name, "basement",
            "the fields around it still import"
        );
    }

    #[test]
    fn a_numeric_looking_string_member_stays_a_string() {
        let xml = "<ServerConfiguration><PathSubstitutions><PathSubstitution>\
                   <From>/media</From><To>2</To></PathSubstitution></PathSubstitutions>\
                   </ServerConfiguration>";
        let subs = import_system(xml).path_substitutions;
        assert_eq!(subs.len(), 1, "the AllStrings reading has to rescue this");
        assert_eq!(subs[0].to, "2");
    }

    #[test]
    fn a_collection_entry_does_not_inherit_the_first_default_entry_s_lists() {
        // The default `MetadataOptions` give some types a non-empty
        // `DisabledMetadataFetchers`; an XML entry that omits the element means
        // the operator cleared it, not "copy whatever entry 0 had".
        let default = default_server_configuration();
        assert!(
            default
                .metadata_options
                .iter()
                .any(|o| !o.disabled_metadata_fetchers.is_empty()),
            "the default table must have a non-empty list for this to test anything"
        );
        let xml = "<ServerConfiguration><MetadataOptions>\
                   <MetadataOptions><ItemType>MusicVideo</ItemType></MetadataOptions>\
                   </MetadataOptions></ServerConfiguration>";
        let options = import_system(xml).metadata_options;
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].item_type.as_deref(), Some("MusicVideo"));
        assert!(
            options[0].disabled_metadata_fetchers.is_empty(),
            "{:?}",
            options[0].disabled_metadata_fetchers
        );
    }
}

//! The login page's sign-in buttons (B3, D7): one "Sign in with {name}"
//! `<form>` per provider that can sign users in, which
//! `GET /Branding/Configuration` appends to the login disclaimer.
//!
//! jellyfin-web 12.4 renders the disclaimer as markdown with HTML through
//! DOMPurify, which keeps a form and its button, and turns every link into one
//! that opens a new tab (disabled on TV layouts): a form is what opens the
//! sign-in in the same tab. Every client is served the same text (D17).

use std::fmt::Write as _;

use ferrofin_traits::error::ServiceError;

use super::sign_in::escape_html;
use super::{SsoConfig, SsoService, plugin_enabled};

impl SsoService {
    /// [`SsoManager::login_buttons`](ferrofin_traits::sso::SsoManager::login_buttons):
    /// the forms, built on the public URL the callback URLs are, or `None`.
    pub(super) async fn login_button_forms(&self) -> Result<Option<String>, ServiceError> {
        if !plugin_enabled(self.plugins.as_ref()).await? {
            return Ok(None);
        }
        let config = self.config().await?;
        if !config.show_login_button {
            return Ok(None);
        }
        // A set but unusable `PublicUrl` gives none, as for the callback URL.
        let Ok(Some(public_url)) = self.public_url(&config) else {
            return Ok(None);
        };
        Ok(forms(&public_url, &config))
    }
}

/// One form per provider of `config` that can sign users in (enabled and
/// accepted by the checks), in its order, submitting to its start route under
/// `public_url`; `None` when there is none. The provider's name is
/// admin-entered text and the URL is escaped too, so neither can leave its
/// element or attribute.
fn forms(public_url: &str, config: &SsoConfig) -> Option<String> {
    let action_base = escape_html(public_url);
    let mut forms = String::new();
    for (provider, _) in config
        .checked_providers()
        .into_iter()
        .filter(|(provider, refused)| provider.enabled && refused.is_none())
    {
        // The key passed `validate_key`: lowercase letters, digits and `-`.
        // Writing to a `String` cannot fail.
        let _ = write!(
            forms,
            "<form action=\"{action_base}/sso/oidc/{key}/start\" method=\"get\">\
             <button class=\"raised block emby-button button-submit\" type=\"submit\">\
             Sign in with {name}</button></form>",
            key = provider.key,
            name = escape_html(&provider.name),
        );
    }
    (!forms.is_empty()).then_some(forms)
}

#[cfg(test)]
mod tests {
    use ferrofin_model::sso::OidcProvider;
    use ferrofin_traits::plugins::PluginManager as _;
    use ferrofin_traits::sso::SsoManager as _;

    use super::super::tests::service;
    use super::super::{EXTENSION_ID, SsoConfig, SsoExtension};
    use crate::Extension as _;

    const PUBLISHED: &str = "https://media.example.org/jf";

    /// A stored provider document with `key`, `name` and `enabled`.
    fn provider(key: &str, name: &str, enabled: bool) -> serde_json::Value {
        serde_json::to_value(OidcProvider {
            key: key.to_owned(),
            name: name.to_owned(),
            enabled,
            issuer: "https://id.example.org".to_owned(),
            client_id: "ferrofin".to_owned(),
            ..OidcProvider::default()
        })
        .expect("provider")
    }

    fn button(key: &str, name: &str) -> String {
        format!(
            "<form action=\"https://media.example.org/jf/sso/oidc/{key}/start\" method=\"get\">\
             <button class=\"raised block emby-button button-submit\" type=\"submit\">\
             Sign in with {name}</button></form>"
        )
    }

    /// The buttons of a service published at `published_url` and holding
    /// `config`, its plugin enabled unless `enabled` is false.
    async fn buttons_for(
        enabled: bool,
        published_url: Option<&str>,
        config: &SsoConfig,
    ) -> Option<String> {
        let (sso, plugins, _dir) = service(true, published_url).await;
        plugins
            .set_plugin_configuration(EXTENSION_ID, serde_json::to_vec(config).expect("JSON"))
            .await
            .expect("configured");
        if !enabled {
            plugins.disable_plugin(EXTENSION_ID).await.expect("disable");
        }
        sso.login_buttons().await.expect("buttons")
    }

    fn with_providers(providers: Vec<serde_json::Value>) -> SsoConfig {
        SsoConfig {
            providers,
            ..SsoConfig::default()
        }
    }

    #[tokio::test]
    async fn one_form_per_provider_that_can_sign_in_in_their_order() {
        let config = with_providers(vec![
            provider("keycloak", "Keycloak", true),
            provider("off", "Switched off", false),
            provider("Bad Key", "Refused", true),
            provider("pocket", "Pocket ID", true),
        ]);
        assert_eq!(
            buttons_for(true, Some(PUBLISHED), &config).await,
            Some(button("keycloak", "Keycloak") + &button("pocket", "Pocket ID"))
        );
    }

    #[tokio::test]
    async fn the_name_and_the_url_are_escaped() {
        let config = SsoConfig {
            public_url: "https://media.example.org/a'b&c".to_owned(),
            ..with_providers(vec![provider(
                "evil",
                r#"<img src=x onerror=alert(1)> "Ops" & 'Co'"#,
                true,
            )])
        };
        let html = buttons_for(true, None, &config).await.expect("a button");
        assert_eq!(
            html,
            "<form action=\"https://media.example.org/a&#39;b&amp;c/sso/oidc/evil/start\" \
             method=\"get\"><button class=\"raised block emby-button button-submit\" \
             type=\"submit\">Sign in with &lt;img src=x onerror=alert(1)&gt; &quot;Ops&quot; \
             &amp; &#39;Co&#39;</button></form>"
        );
    }

    #[tokio::test]
    async fn nothing_while_disabled_switched_off_without_a_url_or_a_provider() {
        let one = with_providers(vec![provider("keycloak", "Keycloak", true)]);
        assert_eq!(buttons_for(false, Some(PUBLISHED), &one).await, None);
        let off = SsoConfig {
            show_login_button: false,
            ..one.clone()
        };
        assert_eq!(buttons_for(true, Some(PUBLISHED), &off).await, None);
        assert_eq!(buttons_for(true, None, &one).await, None);
        // A `PublicUrl` that is set but unusable gives none: nothing falls
        // back past it.
        let unusable = SsoConfig {
            public_url: "ftp://media.example.org".to_owned(),
            ..one.clone()
        };
        assert_eq!(buttons_for(true, Some(PUBLISHED), &unusable).await, None);
        let none_enabled = with_providers(vec![provider("off", "Off", false)]);
        assert_eq!(
            buttons_for(true, Some(PUBLISHED), &none_enabled).await,
            None
        );
        assert_eq!(
            buttons_for(true, Some(PUBLISHED), &with_providers(Vec::new())).await,
            None
        );
    }

    #[test]
    fn the_switch_is_on_by_default_and_on_the_settings_page() {
        assert!(SsoConfig::default().show_login_button);
        let stored: SsoConfig =
            serde_json::from_str(r#"{"PublicUrl":"","Providers":[]}"#).expect("reads");
        assert!(stored.show_login_button);
        let off: SsoConfig = serde_json::from_str(r#"{"ShowLoginButton":false}"#).expect("reads");
        assert!(!off.show_login_button);
        // As the settings page reads it: anything but `false` is on.
        let null: SsoConfig = serde_json::from_str(r#"{"ShowLoginButton":null}"#).expect("reads");
        assert!(null.show_login_button);

        let page = String::from_utf8(SsoExtension.config_pages().remove(0).bytes).expect("UTF-8");
        for needle in [
            "class=\"ssoShowLoginButton\"",
            "checkbox.checked = config.ShowLoginButton !== false;",
            "config.ShowLoginButton = checkbox.checked;",
        ] {
            assert!(page.contains(needle), "{needle}");
        }
    }
}

//! A user's roles at an OIDC provider (B8, B9): the required role, checked
//! on the verified identity before any account is looked up or created, and
//! role sync (D6), which keeps the administrator flag in step with the
//! administrator role at each sign-in, before the session is minted.
//!
//! Roles are read from the provider's role claim only: a top-level claim
//! holding an array of strings or one string, from the ID token, else from
//! the userinfo answer (the callback asks for it while a role setting is in
//! use, [`reads_roles`]). A nested claim is not read. Names are compared
//! exactly. Roles are read at sign-in only, so a change at the identity
//! provider reaches a user's rights at their next sign-in.

use ferrofin_model::sso::OidcProvider;
use ferrofin_traits::error::ServiceError;
use serde_json::Value;
use uuid::Uuid;

use super::SsoService;
use super::sign_in::{Refusal, VerifiedIdentity};

/// The user lacks the provider's required role.
const MISSING_ROLE: Refusal = Refusal::account(
    "missing_role",
    "Your account at the identity provider does not have the role this server requires. Ask an \
     administrator for access.",
);

/// The roles `identity` holds in `provider`'s role claim: each string of an
/// array, or the one string; none for a missing claim or any other value.
#[must_use]
pub fn roles<'a>(provider: &OidcProvider, identity: &'a VerifiedIdentity) -> Vec<&'a str> {
    match identity.claims.get(&provider.role_claim) {
        Some(Value::String(role)) => vec![role.as_str()],
        Some(Value::Array(roles)) => roles.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Whether `identity` holds `role`, compared exactly.
fn has_role(provider: &OidcProvider, identity: &VerifiedIdentity, role: &str) -> bool {
    roles(provider, identity).contains(&role)
}

/// Whether `provider` reads roles at all: a required role, or role sync. Its
/// role claim is then required, and fetched from userinfo when the ID token
/// lacks it.
#[must_use]
pub fn reads_roles(provider: &OidcProvider) -> bool {
    !provider.required_role.is_empty() || provider.role_sync
}

/// Refuses `identity` when it lacks `provider`'s required role, whether role
/// sync is on or not (B8).
pub(super) fn check_required_role(
    provider: &OidcProvider,
    identity: &VerifiedIdentity,
) -> Result<(), Refusal> {
    if provider.required_role.is_empty() || has_role(provider, identity, &provider.required_role) {
        Ok(())
    } else {
        Err(MISSING_ROLE)
    }
}

impl SsoService {
    /// With `provider`'s role sync on, brings the rights of the account
    /// `user_id` in step with `identity`'s roles (D6): the administrator flag
    /// follows the administrator role, when one is set (B9). The last
    /// administrator is never demoted: the user manager keeps the flag, logs a
    /// warning, and the sign-in goes on. Off, nothing is changed.
    pub(super) async fn sync_roles(
        &self,
        provider: &OidcProvider,
        identity: &VerifiedIdentity,
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        if !provider.role_sync {
            return Ok(());
        }
        if !provider.admin_role.is_empty() {
            let administrator = has_role(provider, identity, &provider.admin_role);
            self.users
                .apply_external_administrator(user_id, administrator)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

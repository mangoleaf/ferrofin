//! A user's roles at an OIDC provider (B8, B9, B10): the required role,
//! checked on the verified identity before any account is looked up or
//! created, and role sync (D6), which keeps the administrator flag, the
//! libraries and the two Live TV permissions in step with the mapped roles at
//! each sign-in, before the session is minted.
//!
//! Roles are read from the provider's role claim only: a top-level claim
//! holding an array of strings or one string, from the ID token, else from
//! the userinfo answer (the callback asks for it while a role setting is in
//! use, [`reads_roles`]). A nested claim is not read. Names are compared
//! exactly. Roles are read at sign-in only, so a change at the identity
//! provider reaches a user's rights at their next sign-in.

use std::collections::BTreeSet;

use ferrofin_model::sso::OidcProvider;
use ferrofin_model::users::UserPolicy;
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

/// Trims every role of `provider`'s access mappings, as a save trims its
/// text settings, and drops the empty ones from the role lists. A library
/// mapping keeps an empty role, which [`validate_mappings`] refuses.
pub(super) fn trim_mappings(provider: &mut OidcProvider) {
    for roles in [
        &mut provider.all_folders_roles,
        &mut provider.live_tv_roles,
        &mut provider.live_tv_management_roles,
    ] {
        for role in roles.iter_mut() {
            *role = role.trim().to_owned();
        }
        roles.retain(|role| !role.is_empty());
    }
    for mapping in &mut provider.folder_roles {
        mapping.role = mapping.role.trim().to_owned();
    }
}

/// Checks every role of `provider`'s access mappings: a name, used as
/// written, so not empty, with no control character and no space at either
/// end.
///
/// # Errors
///
/// The message to show the admin.
pub(super) fn validate_mappings(provider: &OidcProvider) -> Result<(), String> {
    let mapped = provider
        .all_folders_roles
        .iter()
        .chain(provider.folder_roles.iter().map(|mapping| &mapping.role))
        .chain(&provider.live_tv_roles)
        .chain(&provider.live_tv_management_roles);
    for role in mapped {
        if role.is_empty() || role.chars().any(char::is_control) || role.trim() != role {
            return Err(
                "each role of the library and Live TV mappings must be a name, with no control \
                 characters and no space at either end"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// Sets the four members of `policy` the access mappings own from `roles`
/// (B10): every library for a role of `AllFoldersRoles`, the libraries of
/// each `FolderRoles` entry whose role is held, and each Live TV permission
/// for a role of its list. With none of the mapped roles, no library and no
/// Live TV. Every other member is left as it is.
///
/// A library id is kept as the mapping has it: one no library has any more
/// matches nothing, as a deleted library's id in a policy saved from the
/// dashboard does. The same libraries in another order are kept in the
/// stored order, so they are no change.
fn apply_access(provider: &OidcProvider, roles: &[&str], policy: &mut UserPolicy) {
    let held = |role: &String| roles.contains(&role.as_str());
    policy.enable_all_folders = provider.all_folders_roles.iter().any(held);
    policy.enable_live_tv_access = provider.live_tv_roles.iter().any(held);
    policy.enable_live_tv_management = provider.live_tv_management_roles.iter().any(held);
    let mut folders = Vec::new();
    for mapping in provider.folder_roles.iter().filter(|m| held(&m.role)) {
        for folder in &mapping.folders {
            if !folders.contains(folder) {
                folders.push(*folder);
            }
        }
    }
    if folders.iter().collect::<BTreeSet<_>>() != policy.enabled_folders.iter().collect() {
        policy.enabled_folders = folders;
    }
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
    /// follows the administrator role, when one is set (B9), and the
    /// libraries and Live TV follow the access mappings (B10). The last
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
        let roles = roles(provider, identity);
        if !provider.admin_role.is_empty() {
            let administrator = roles.contains(&provider.admin_role.as_str());
            self.users
                .apply_external_administrator(user_id, administrator)
                .await?;
        }
        self.sync_access(provider, &roles, user_id).await
    }

    /// Sets the account's libraries and Live TV from `roles`
    /// ([`apply_access`]) over its current policy, read after the
    /// administrator flag, and saves the policy only when one of those four
    /// members changed, since a save announces the user as updated. The save
    /// writes the whole policy, as the dashboard's does, so an edit landing
    /// between the read and the save is overwritten, as with two dashboard
    /// saves.
    ///
    /// No lock is held here: a sign-in through SSO never takes the user's
    /// login lock, which the save takes, and the flag's transaction above is
    /// over.
    async fn sync_access(
        &self,
        provider: &OidcProvider,
        roles: &[&str],
        user_id: Uuid,
    ) -> Result<(), ServiceError> {
        let user = self
            .users
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| ServiceError::not_found(format!("user {user_id}")))?;
        let current = self
            .users
            .get_user_dto(&user, None)
            .await?
            .policy
            .ok_or_else(|| ServiceError::backend("the account has no policy"))?;
        let mut policy = current.clone();
        apply_access(provider, roles, &mut policy);
        if policy != current {
            self.users.update_policy(user_id, &policy).await?;
            tracing::info!(
                oidc_provider = %provider.key,
                user_id = %user.id,
                "sign-in set the library and Live TV access"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

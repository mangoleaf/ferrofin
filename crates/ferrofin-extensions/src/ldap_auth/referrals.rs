//! Following referrals in the plugin's searches: what upstream gets from
//! Novell's client (`Novell.Directory.Ldap.NETStandard` 3.6.0) through
//! `GetSearchConstraints`, which turns `ReferralFollowing` on with an
//! `LdapAuthHandler` that binds at every referred server as the configured
//! bind DN. Upstream sets those constraints on `LocateLdapUser`'s search,
//! `GetFilteredUsers` and the admin filter's search at sign-in, which here all
//! go through `search_as_bind_dn`. The connection test's base search and the
//! user's own bind do not follow referrals, as upstream's do not.
//!
//! A search reference (a continuation, RFC 4511 section 4.5.3) and a
//! `referral` result (section 4.1.10) are followed alike: to the first of
//! their URLs that connects and takes the bind (the URLs are alternatives, as
//! Novell's `GetReferralConnection` tries them), searching there as the
//! referral says. The entries found there join the search's, after those of
//! the server that referred, and a referral that server sends in turn is
//! followed the same way, depth first. A referral none of whose URLs leads to
//! a server that takes the bind (the name does not resolve, nothing answers,
//! TLS fails, the bind DN is refused, or the URL does not parse) is skipped,
//! and the search logs one warning naming each server skipped and why (owner
//! decision, gl-1207). A referred server that fails the search fails the whole
//! search, and the error names the server (owner decision).
//!
//! The search a referral continues keeps its attributes; it takes the URL's
//! DN, filter and scope when the URL names them, else the referred search's,
//! a one-level search continuing at its base, as Novell's `RebuildRequest`
//! does.
//!
//! Divergences from Novell's client (see `docs/PLUGINS_UPSTREAM.md`):
//! - **The referred connection keeps the configured TLS.** Novell opens it
//!   with default options (plain LDAP, no StartTLS) and binds over it, so the
//!   bind password crosses the network in clear. Here a configuration that
//!   uses LDAPS or StartTLS uses it at the referred server too, with the same
//!   certificate checks (against the referred server's name) and client
//!   certificate, and an `ldaps://` URL is LDAPS whatever is configured. A URL
//!   without a port takes the protocol's: 636 for LDAPS, else 389. The bind
//!   password still goes to whichever server the directory names, as upstream
//!   sends it.
//! - **A referral that leads to no server is skipped wherever it arrives.**
//!   Novell's search results keep the `LdapReferralException` its chase
//!   throws without counting it as an entry (`BatchOfResults`): one met after
//!   the last entry is dropped (`ResetVectors`), as Active Directory sends its
//!   references, one met before another entry is thrown at the next `Next()`,
//!   and a `referral` result ends the results empty. The order of a search's
//!   entries and references is not kept here, and upstream logs nothing.
//! - **A chain stops after [`REFERRAL_HOP_LIMIT`] referrals.** Novell's
//!   `HopLimit` is 10 too, but its search results restart the count at every
//!   referral they read, so there a referral loop never ends.
//! - **URLs are read as RFC 4516 writes them**: the DN and the filter
//!   percent-decoded, and the URL's scope used when it names one. Novell sends
//!   the DN as written and ignores the scope, and a `referral` result's
//!   filter, which RFC 4511 section 4.1.10 says to use.

use std::borrow::Cow;

use super::{
    ConnectionOptions, LdapConfig, LdapDirectory, LdapEntry, LdapError, REFERRAL_HOP_LIMIT,
    SearchOutcome, SearchScope, novell_bind,
};

/// The port of an LDAP URL that names none, over plain LDAP or StartTLS.
const LDAP_PORT: u16 = 389;
/// The port of an LDAP URL that names none, over LDAPS.
const LDAPS_PORT: u16 = 636;

/// A search as one server is asked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Search {
    /// The base DN.
    pub(super) base: String,
    /// The scope under the base.
    pub(super) scope: SearchScope,
    /// The filter.
    pub(super) filter: String,
}

/// A referral waiting to be followed.
struct Pending {
    /// Its URLs, alternatives of each other.
    urls: Vec<String>,
    /// A search reference (a continuation), rather than a `referral` result.
    continuation: bool,
    /// The search it continues, as the server that sent it was asked it.
    search: Search,
    /// That server, `host:port`.
    from: String,
    /// How many referrals in a row lead here, this one included.
    hops: usize,
}

/// What following one referral came to.
enum Chased {
    /// A referred server took the bind and answered.
    Answered {
        /// That server, `host:port`.
        server: String,
        /// The search it was asked.
        search: Search,
        /// Its answer, which may refer further.
        answer: Result<SearchOutcome, LdapError>,
    },
    /// No URL of the referral led to a server that took the bind: why each
    /// failed, in the order they were tried.
    NoServer(Vec<LdapError>),
}

/// Follows every referral in `answer`, the answer of the server `options`
/// names to `search`, and those the referred servers send in turn. Returns
/// the entries every server found, the first server's first, then each
/// referral's, depth first: a referred server's own referrals are followed
/// before the next referral of the server that sent it. An error `answer`
/// that is not a referral is returned as it is.
///
/// A referral that leads to no server is skipped (owner decision, gl-1207),
/// and the search then logs one warning naming each server skipped and why,
/// whether it succeeds or not.
pub(super) async fn follow(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    options: &ConnectionOptions,
    search: &Search,
    attributes: &[&str],
    answer: Result<SearchOutcome, LdapError>,
) -> Result<Vec<LdapEntry>, LdapError> {
    // Why each skipped referral's URLs failed, each reason once.
    let mut skipped: Vec<String> = Vec::new();
    let found = async {
        let mut entries = Vec::new();
        let mut pending = Vec::new();
        let configured = server_label(&options.server, options.port);
        take(answer, search, &configured, 0, &mut entries, &mut pending)?;
        while let Some(referral) = pending.pop() {
            match chase(directory, config, options, attributes, &referral).await? {
                Chased::Answered {
                    server,
                    search,
                    answer,
                } => take(
                    answer,
                    &search,
                    &server,
                    referral.hops,
                    &mut entries,
                    &mut pending,
                )?,
                Chased::NoServer(failures) => {
                    for failure in failures.iter().map(ToString::to_string) {
                        if !skipped.contains(&failure) {
                            skipped.push(failure);
                        }
                    }
                }
            }
        }
        Ok(entries)
    }
    .await;
    if !skipped.is_empty() {
        tracing::warn!(
            referrals = ?skipped.join("; "),
            "skipped LDAP referrals that lead to no server: their part of the directory was \
             not searched"
        );
    }
    found
}

/// Takes `answer`, which `server` gave to `search` after `hops` referrals:
/// its entries join `entries` and its referrals `pending`, the first on top.
fn take(
    answer: Result<SearchOutcome, LdapError>,
    search: &Search,
    server: &str,
    hops: usize,
    entries: &mut Vec<LdapEntry>,
    pending: &mut Vec<Pending>,
) -> Result<(), LdapError> {
    let referral = |urls, continuation| Pending {
        urls,
        continuation,
        search: search.clone(),
        from: server.to_owned(),
        hops: hops + 1,
    };
    match answer {
        Ok(found) => {
            entries.extend(found.entries);
            pending.extend(
                found
                    .references
                    .into_iter()
                    .rev()
                    .map(|urls| referral(urls, true)),
            );
        }
        Err(LdapError::Referred { urls, .. }) => pending.push(referral(urls, false)),
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Follows `referral`: connects to the first of its URLs that connects and
/// takes the bind as the configured bind DN (`LdapAuthHandler`), keeping the
/// configured TLS (`options`), and searches there. Returns that server, the
/// search it was asked and its answer, which may refer further; or, when no
/// URL leads to a server that takes the bind, why each failed (Novell's
/// `GetReferralConnection` fails alike). A referral past the hop limit, one
/// naming no URL, and a referred server that fails the search are errors.
async fn chase(
    directory: &dyn LdapDirectory,
    config: &LdapConfig,
    options: &ConnectionOptions,
    attributes: &[&str],
    referral: &Pending,
) -> Result<Chased, LdapError> {
    if referral.hops > REFERRAL_HOP_LIMIT {
        return Err(LdapError::ReferralHopLimit {
            server: referral.from.clone(),
        });
    }
    if referral.urls.is_empty() {
        // RFC 4511 gives every referral at least one URL.
        return Err(LdapError::ReferralUrl(
            "the referral names no URL".to_owned(),
        ));
    }
    let mut failures = Vec::new();
    for url in &referral.urls {
        let url = match ReferralUrl::parse(url) {
            Ok(url) => url,
            Err(reason) => {
                tracing::debug!(error = %reason, "an LDAP referral URL cannot be followed");
                failures.push(LdapError::ReferralUrl(reason));
                continue;
            }
        };
        let target = url.options(options);
        let server = server_label(&target.server, target.port);
        tracing::debug!(server = ?server, from = ?referral.from, "following an LDAP referral");
        let mut connection = match directory.connect(&target).await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::debug!(server = ?server, error = ?error.to_string(), "could not connect to a referred LDAP server");
                failures.push(LdapError::ReferralFailed {
                    server,
                    reason: Box::new(error),
                });
                continue;
            }
        };
        if let Err(error) = novell_bind(
            connection.as_mut(),
            &config.ldap_bind_user,
            config.ldap_bind_password.expose(),
        )
        .await
        {
            connection.unbind().await;
            tracing::debug!(server = ?server, error = ?error.to_string(), "a referred LDAP server refused the bind DN");
            failures.push(LdapError::ReferralFailed {
                server,
                reason: Box::new(error),
            });
            continue;
        }
        let search = url.continue_search(&referral.search, referral.continuation);
        let answer = connection
            .search(&search.base, search.scope, &search.filter, attributes)
            .await;
        connection.unbind().await;
        return match answer {
            Ok(_) | Err(LdapError::Referred { .. }) => Ok(Chased::Answered {
                server,
                search,
                answer,
            }),
            Err(error) => Err(LdapError::ReferralFailed {
                server,
                reason: Box::new(error),
            }),
        };
    }
    Ok(Chased::NoServer(failures))
}

/// `host:port`, an IPv6 address in brackets, for messages.
fn server_label(server: &str, port: i32) -> String {
    let host = server
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(server);
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// An LDAP URL (RFC 4516) as following a referral reads it: Novell's
/// `LdapUrl`, decoding the DN and the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReferralUrl {
    /// `ldaps://`.
    secure: bool,
    /// The server's host name or address, without brackets.
    host: String,
    /// The port, when the URL names one.
    port: Option<u16>,
    /// The base DN, when the URL names one.
    dn: Option<String>,
    /// The scope, when the URL names one.
    scope: Option<SearchScope>,
    /// The filter, when the URL names one.
    filter: Option<String>,
}

impl ReferralUrl {
    /// Reads `url`, or says why it cannot be followed, without quoting it (an
    /// LDAP URL may carry a password in an extension).
    fn parse(url: &str) -> Result<Self, String> {
        let url = url.trim();
        let url = url
            .strip_prefix('<')
            .and_then(|inner| inner.strip_suffix('>'))
            .unwrap_or(url);
        let url = strip_prefix_ignoring_case(url, "URL:").unwrap_or(url);
        let (secure, rest) = if let Some(rest) = strip_prefix_ignoring_case(url, "ldap://") {
            (false, rest)
        } else if let Some(rest) = strip_prefix_ignoring_case(url, "ldaps://") {
            (true, rest)
        } else {
            return Err("it is not an ldap:// or ldaps:// URL".to_owned());
        };
        let (authority, tail) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
        let (host, port) = authority_parts(authority)?;
        let mut parsed = Self {
            secure,
            host,
            port,
            dn: None,
            scope: None,
            filter: None,
        };
        if let Some(path) = tail.strip_prefix('/') {
            // dn ? attributes ? scope ? filter ? extensions; the attributes
            // are the search's, and the extensions are ignored, as Novell
            // ignores them.
            let mut parts = path.splitn(5, '?');
            parsed.dn = parts
                .next()
                .map(decode)
                .transpose()?
                .filter(|dn| !dn.is_empty());
            let _attributes = parts.next();
            parsed.scope = parts.next().map(scope).transpose()?.flatten();
            parsed.filter = parts
                .next()
                .map(decode)
                .transpose()?
                .filter(|filter| !filter.is_empty());
        } else if let Some(dn) = tail.strip_prefix("??") {
            // Novell's broken referral format, which its `LdapUrl` reads:
            // `ldap://host:port??dn`, the DN where the attributes belong. It
            // is no RFC 4516 URL, so its DN is taken as written, not decoded.
            parsed.dn = dn
                .split('?')
                .next()
                .filter(|dn| !dn.is_empty())
                .map(str::to_owned);
        }
        Ok(parsed)
    }

    /// Where and how to connect: the configured TLS kept, never weakened
    /// (see the module's divergences).
    fn options(&self, configured: &ConnectionOptions) -> ConnectionOptions {
        let use_ssl = configured.use_ssl || self.secure;
        // StartTLS on an LDAPS connection fails, so an `ldaps://` URL takes
        // its place.
        let use_start_tls = configured.use_start_tls && (configured.use_ssl || !self.secure);
        let port = self
            .port
            .unwrap_or(if use_ssl { LDAPS_PORT } else { LDAP_PORT });
        ConnectionOptions {
            server: self.host.clone(),
            port: i32::from(port),
            use_ssl,
            use_start_tls,
            verification: configured.verification.clone(),
            client_certificate: configured.client_certificate.clone(),
        }
    }

    /// The search this URL continues `search` with: the URL's DN, filter and
    /// scope where it names them, else `search`'s, a continued one-level
    /// search at its base (RFC 4511 section 4.5.3, Novell's `RebuildRequest`).
    fn continue_search(&self, search: &Search, continuation: bool) -> Search {
        let scope = self.scope.unwrap_or(match search.scope {
            SearchScope::OneLevel if continuation => SearchScope::Base,
            scope => scope,
        });
        Search {
            base: self.dn.clone().unwrap_or_else(|| search.base.clone()),
            scope,
            filter: self.filter.clone().unwrap_or_else(|| search.filter.clone()),
        }
    }
}

/// `text` without `prefix`, compared ignoring ASCII case.
fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// The host and the port of a URL's `host[:port]` or `[address][:port]`.
fn authority_parts(authority: &str) -> Result<(String, Option<u16>), String> {
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, rest) = bracketed
            .split_once(']')
            .ok_or("its IPv6 address has no closing bracket")?;
        match rest {
            "" => (host, None),
            _ => (
                host,
                Some(
                    rest.strip_prefix(':')
                        .ok_or("its IPv6 address is not followed by a port")?,
                ),
            ),
        }
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return Err("it names no server".to_owned());
    }
    // The host reaches errors an administrator reads and the logs: no
    // whitespace or control characters, which no host name or address has.
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("its server is not a host name or an address".to_owned());
    }
    let port = match port {
        None | Some("") => None,
        Some(port) => Some(
            port.parse::<u16>()
                .map_err(|_| "its port is not a TCP port".to_owned())?,
        ),
    };
    Ok((host.to_owned(), port))
}

/// A percent-encoded DN or filter, decoded.
fn decode(text: &str) -> Result<String, String> {
    percent_encoding::percent_decode_str(text)
        .decode_utf8()
        .map(Cow::into_owned)
        .map_err(|_| "its DN or filter is not UTF-8 once decoded".to_owned())
}

/// A URL's scope (`base`, `one` or `sub`, ignoring case), or `None` when it
/// names none.
fn scope(scope: &str) -> Result<Option<SearchScope>, String> {
    Ok(Some(match scope.to_ascii_lowercase().as_str() {
        "" => return Ok(None),
        "base" => SearchScope::Base,
        "one" => SearchScope::OneLevel,
        "sub" => SearchScope::Subtree,
        _ => return Err("it names a scope other than base, one or sub".to_owned()),
    }))
}

#[cfg(test)]
mod referral_tests;

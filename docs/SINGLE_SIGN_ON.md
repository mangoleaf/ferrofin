# Single Sign-On (OpenID Connect)

Single Sign-On lets users sign in to Ferrofin with an OpenID Connect identity provider, such as
Keycloak, Authentik or Pocket ID. jellyfin-web's login page gets a "Sign in with `{name}`"
button, and native apps sign in by approving their Quick Connect code through the same sign-in.
No client needs changing.

It is an internal plugin of Ferrofin's own ([`EXTENSIONS.md`](EXTENSIONS.md#single-sign-on)):
compiled in, **off by default**, and turned on under Dashboard → Plugins. It is not the
community SSO-Auth plugin for Jellyfin. It uses other routes, reads none of that plugin's
settings, and imports none of its links (see
[Adopting a Jellyfin that used SSO-Auth](#adopting-a-jellyfin-that-used-the-sso-auth-plugin)).

**What was tested.** Unless a passage says it was read from code or not tried, it was run
against Keycloak 26.7, through the `#[ignore]`d live test
(`apps/ferrofin-server/tests/oidc_keycloak.rs`) and by hand from jellyfin-web 12.4. Not tried:
any other identity provider (the names given for Authentik and Pocket ID come from their
documentation), a real native app, the Android app, and a private certificate authority.

## Before you start

**The public URL.** The identity provider sends users back to the server's callback URL:

```
{public URL}/sso/oidc/{key}/callback
```

The public URL is the address users reach the server at, base path included, such as
`https://media.example.org` or `https://example.org/jellyfin`. It is the plugin's **Public URL**
setting, or the server's published URL when that setting is empty: `--published-url`,
`FERROFIN_PUBLISHED_URL` or `published_url` in `config.toml` ([`CONFIG.md`](CONFIG.md)), which
Ferrofin Settings shows, read-only, as **Published server URL**. Dashboard → Networking's
published server URIs are not used. Ferrofin never builds it from the request's `Host` header.
With neither set, a sign-in is refused, and the settings page says where to set one. The settings
page shows each provider's callback URL: register exactly that URL at the identity provider.

`{key}` is the short name you give the provider when you add it, such as `keycloak`. It is part
of the registered callback URL, so it cannot be changed once the provider is saved. To change
it, add the provider again under a new key and register the new callback URL.

**An `https` issuer.** The issuer must be an `https` URL. A provider's **Allow an http issuer**
switch accepts an `http` one, for an identity provider on a network you trust (and for a test
container). Over `http`, codes and tokens travel unencrypted and can be read or changed on the
way. No check on the sign-in itself can be switched off. Ferrofin always uses the authorization
code flow with PKCE (`S256`), a `state` bound to the browser by a cookie, and a nonce. It always
checks the ID token's signature, issuer, audience, `azp`, expiry and nonce, and the `iss` of the
callback when the provider sends one. It accepts only ID tokens signed with an asymmetric algorithm the
provider advertises (such as `RS256`), never `none` or an HMAC keyed with the client secret.
Ferrofin follows no redirect from the identity provider.

**A private certificate authority.** Ferrofin trusts the system's certificate store. An
identity provider whose certificate comes from your own certificate authority is trusted once
that authority is in the store. Where you cannot add it (a container image), point
`SSL_CERT_FILE` at a PEM bundle, or `SSL_CERT_DIR` at a directory of certificates. Either one
*replaces* the system store for every outbound TLS connection the server makes: metadata
providers, plugin repositories, image downloads, LDAP-Auth's LDAPS and StartTLS, and the OTLP
trace exporter included. So the bundle must hold the public
authorities as well as yours: concatenate the system bundle (`/etc/ssl/certs/ca-certificates.crt`
on Debian) and your authority's certificate. This was read from the HTTP client as built
(reqwest 0.13 with `rustls-platform-verifier`, which reads the store through
`rustls-native-certs` 0.8), not run against a private authority.

## Enable the plugin and add a provider

1. Dashboard → Plugins → **Single Sign-On**: enable it, then open its settings.
2. Fill in **Public URL**, unless the server's published URL is the address users reach the
   server at, and **Save**.
3. **Add provider**, fill in the fields below, and **Save provider**.

Saving tests the provider's discovery document. The provider's row then shows the
authorization, token and userinfo endpoints, the key set and how many signing keys were read,
or why the test failed. A provider that fails the test is still saved, so you can fix the
identity provider and save again. Every change applies at once, with no restart.

| Field | Default | What it is |
|---|---|---|
| Key | — | 1 to 32 lowercase letters, digits or `-`. Part of the callback URL, fixed once saved. |
| Name | — | The name on the sign-in button. |
| Enabled | on | Off, the provider is kept but no one signs in with it. |
| Issuer URL | — | The issuer, whose discovery document is `{issuer}/.well-known/openid-configuration`. It must be exactly the `issuer` that document names, trailing slash included or not. |
| Allow an http issuer | off | See above. |
| Client ID, Client secret | — | The confidential client registered at the identity provider. The secret is write-only: the page never shows it again, and saving with the field empty keeps the stored one. |
| Scopes | `openid profile` | Separated by spaces; `openid` is required. |
| Username claim | `preferred_username` | The claim a new account's name is taken from. |
| Create accounts at first sign-in | on | See [Who signs in](#who-signs-in). |
| Sync profile pictures | off | See [Profile pictures](#profile-pictures). |
| Role claim | `groups` | See [Roles](#roles). |
| Required role | empty | See [Roles](#roles). |
| Sync rights from roles | off | See [Roles](#roles), and read its warning. |
| Administrator role, Roles with every library, Add a role with some libraries, Roles allowed Live TV, Roles allowed to manage Live TV recordings | empty | Used only with role sync on. |

The page's **Login page** switch, **Show a sign-in button for each provider**, is described in
[What clients show](#what-clients-show).

### Keycloak (tested)

[`contrib/oidc-test-realm/`](../contrib/oidc-test-realm/README.md) holds the realm the tests use,
as an import file: it is the reference for the settings below.

- On Ferrofin's side: key `keycloak`, issuer `https://keycloak.example.org/realms/<realm>` (no
  trailing slash), client ID the client's (`ferrofin` in the test realm), its secret, and the
  default scopes, username claim and role claim.
- The client (Clients → Create client, OpenID Connect): **Client authentication** on, so it is
  confidential; **Standard flow** only; **Valid redirect URIs** the callback URL. Under
  **Advanced**, set the PKCE method to `S256` if you want Keycloak to require PKCE (Ferrofin
  sends it either way). The secret is under **Credentials**.
- Groups as roles: on the client's dedicated scope (Client scopes → `<client>-dedicated`), add
  a mapper by configuration, **Group Membership**, with token claim name `groups`, **Full group
  path** off, and **Add to ID token** and **Add to userinfo** on. Ferrofin's defaults then read
  the user's group names. Keycloak leaves the `groups` claim out entirely for a user in no
  group, and such a user has no role.
- Keycloak's realm roles are in a nested claim (`realm_access.roles`), which Ferrofin does not
  read. To use roles instead of groups, add a **User Realm Role** mapper with a top-level token
  claim name such as `roles`, **Multivalued** on, and set the provider's **Role claim** to
  `roles` (not tested; the tests use groups).

What was tested is the import file's client and mapper. The console steps above follow
Keycloak 26's documentation and were not clicked through.

### Authentik and Pocket ID (not tested)

| Ferrofin | Authentik (an OAuth2/OpenID provider and its application) | Pocket ID (an OIDC client) |
|---|---|---|
| Issuer URL | `https://authentik.example.org/application/o/<application slug>/`, **with** the trailing slash | `https://id.example.org`, the Pocket ID address |
| Client ID, Client secret | the provider's Client ID and Client Secret, client type Confidential | shown when the client is created; the secret only once |
| Callback URL | the provider's **Redirect URIs** | the client's **Callback URLs** |
| Signature | select a **Signing Key**: without one, Authentik signs ID tokens with `HS256` and the client secret, which Ferrofin refuses | — |
| Roles | its `profile` scope carries the user's groups as `groups` | add `groups` to **Scopes**; the claim is `groups` |

## Who signs in

An identity is matched to its Ferrofin account by its issuer and subject (the `sub` claim),
never by name.

- **A first sign-in** with **Create accounts at first sign-in** on creates an account named by
  the username claim, and links it. A name an account already has is refused: no account is ever
  taken over by name. A name Ferrofin does not take as a username is refused too. With the
  switch off, a user no account is linked to is refused, and an administrator links one by hand
  (see [Linking](#linking)).
- **A new account** gets every library, as a new local account does, unless role sync narrows it.
  Its authentication provider is **Single Sign-On**
  (`Ferrofin.SingleSignOn.SsoAuthenticationProvider`).
- **The account rules of a password sign-in apply**: a disabled account, remote access, the
  parental schedule, device access and the session limit refuse a sign-in with the reason. A
  refused sign-in is in the activity log, and the sign-in page says why.

**Passwords.** An account Single Sign-On created has a password no one knows: when it creates
the account, Ferrofin stores the hash of a random secret and throws the secret away. Every
password is refused with the usual `401`, an empty one included, and a refused password does not
count toward the account's lockout, so no one can lock it out by guessing. No password can be
set while Single Sign-On is its provider. A forgotten-password PIN is issued for it as for any
account (asked for from the local network), but it cannot be redeemed (read from the code). To
give such an account a password, change its authentication provider on its profile page to the
default one, then set a password on its Password tab. Until then no password signs in to it,
an empty one included. Under the default provider it is a local account like any other: a refused
password counts toward its lockout, and resetting its password on that tab removes the stored one
(an administrator's cannot be reset to none), after which it signs in with an empty password until
one is set.

A local account a user links to an identity keeps its password: both sign in to it.

With the plugin disabled, its accounts cannot sign in at all; a refused password still does not
count toward their lockout. With all extensions off (`FERROFIN_DISABLE_EXTENSIONS`), the Single
Sign-On provider is not registered: its accounts are refused as accounts of an unknown provider,
and there a refused password does count toward their lockout. Either way their links and
settings stay, and work again once the plugin is back.

Saving the profile page (Dashboard → Users → the user) of such an account while the plugin is
disabled, or while all extensions are off, gives it the default provider: the server then lists
only that one, so jellyfin-web hides the provider choice and saves it. The account still has its
password no one knows, so no password signs in to it, and once the plugin is enabled again it
signs in through Single Sign-On as before. To give it the Single Sign-On provider back, choose
that provider on the same page once the plugin is enabled. (Seen with the API call the page
makes, and read from jellyfin-web 12.4's user page.)

## Roles

Roles are read from the provider's **Role claim**: a top-level claim of the ID token, or of the
userinfo answer when the ID token lacks it, holding a list of names or one name. Names are
compared exactly. A nested claim is not read, so map groups or roles to a top-level claim at the
identity provider. Some providers send the claim only for a scope that asks for it, such as
`groups`: add that scope to **Scopes**. A sign-in without the claim has no role.

Roles are read only at sign-in. A change at the identity provider, a demotion included, reaches a
user at their next sign-in. A session already open keeps its rights until then.

- **Required role**: a user without it is refused, and no account is created or linked for them.
  It applies to signing in, to linking, and to approving a Quick Connect code. Empty lets every
  user of the provider in.
- **Sync rights from roles** (role sync), off by default: off, a sign-in never changes a user's
  rights. On, at each sign-in:
  - **Administrator role**: a user with it is made an administrator, and a user without it loses
    the flag, except the last administrator, who keeps it (the server logs a warning). Empty
    never changes the flag.
  - **The access roles** (**Roles with every library**, the roles with some libraries, **Roles
    allowed Live TV**, **Roles allowed to manage Live TV recordings**) set the user's libraries
    and Live TV, replacing what the dashboard set by hand. Nothing else of their access changes.

> **Warning:** with role sync on and no access role filled in, every user of that provider has
> no library and no Live TV after their next sign-in, administrators included. The settings page
> asks before it saves such a provider. An empty **Administrator role** changes nothing, but empty
> access roles take everything away.

## Linking

A user signed in to Ferrofin links their account to an identity at `{public URL}/sso/link`
(jellyfin-web's profile page has no place for it). The page uses the session jellyfin-web keeps
in that browser and lists the user's links. A link is a new way into the account, so a session is
not enough to add one:

- **The password.** Starting a link asks for the account's current Ferrofin password. It is
  checked as a sign-in checks it, through the account's own authentication provider (Ferrofin's,
  or LDAP's for an LDAP account): a wrong one counts toward the account's failed sign-ins and the
  login backoff paces the next attempt, as at the login page. An account without a password sets
  one on its profile first. The page sends the password once and keeps it nowhere.
- **Accounts Single Sign-On created** have a password no one knows
  ([Who signs in](#who-signs-in)), so they cannot link themselves: the page says so, and an
  administrator adds further links on the settings page.
- **A link is never replaced here.** A user linked at a provider removes that link before linking
  another sign-in there; removing it signs every session out, the one that removes it included.
  An identity another account has is refused.
- **An attempt ends with its session.** A link attempt the user started and has not finished at
  the identity provider links nothing once the session that started it has ended, however it
  ended: signing out, Dashboard → Devices, a password change made from another session (which
  signs every other session out), or a link removed. (A sign-out in the very instant the link is
  made, while the server is writing it, can still let it through; a link removed cannot.)

Linking never changes the account's provider or password.

Administrators see every link under **Linked accounts** on the settings page. There they remove
any link, or link an account by hand with the identity's subject: the `sub` claim of its ID
tokens, which Keycloak's admin console shows as the user's ID. An administrator's link replaces
the account's link at that provider, if it has one.

Each link added and removed, by the user or an administrator, is in the activity log
(Dashboard → Activity): "A sign-in at `{provider}` was linked to `{user}`" or "… was unlinked
from …", with who did it, and the identity's issuer and subject in the entry's details. Their
types are `UserIdentityLinked` and `UserIdentityUnlinked`.

Removing a link signs out every session of the account, and ends every attempt to link a sign-in
that the account started and has not finished at the identity provider. An account Single Sign-On
created whose last link is removed cannot sign in at all, until an administrator links it again
or gives it another authentication provider. Deleting a user removes their links.

## Native apps: Quick Connect

Native apps sign in with a password or Quick Connect (Android TV and Swiftfin offer nothing else,
read from their code). With Quick Connect enabled on the server (Dashboard → General, "Enable
Quick Connect on this server"), a user signs an app in like this:

1. In the app, choose Quick Connect: it shows a six-digit code.
2. In a browser, open `{public URL}/sso/quickconnect`, enter the code, and choose **Sign in with
   `{name}`**.
3. Sign in at the identity provider. The page asks "Sign in the device showing code ... as ...?":
   confirm it within one minute.

The app is then signed in as that account, and the browser is not. Nothing is approved without
the confirmation, so a link carrying a code that someone sends you signs nothing in unless you
confirm it. The required role and role sync apply as at a browser sign-in, and so do the account
rules (a disabled account, and device access and the session limit for the app's device), except
that remote access is judged for the browser that confirms, and the parental schedule at the
moment it confirms: the approval is not given the app's address, and Jellyfin's own approval
judges none (read from the code).
This was run with the requests an app makes (`POST /QuickConnect/Initiate`, its poll and
`POST /Users/AuthenticateWithQuickConnect`), not yet with a real app.

## Profile pictures

With a provider's **Sync profile pictures** on (off by default), a user's profile picture follows
the identity provider's `picture` claim. After each sign-in or Quick Connect approval, the image
it names is downloaded under the server's image download limits. An image on a private address,
a loopback one included, needs that address in `image_private_http_allow`
(`FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW`, [`CONFIG.md`](CONFIG.md)), and images over
`image_download_max_mb` are refused. The image replaces the user's picture, one they set in
Ferrofin included, when it is new or has changed, or when the user has none; the same image is not
stored again. The download never delays or fails the sign-in: a picture that cannot be fetched
changes nothing and is tried again at the next sign-in. A sign-in without the claim leaves the
picture as it is. With the switch on and no `picture` in the ID token, the sign-in asks the
userinfo endpoint for it, one extra request of up to 10 seconds. In Keycloak, the built-in
`profile` scope fills `picture` from a user attribute named `picture`, which the realm's user
profile must declare.

## What clients show

- **jellyfin-web** shows a **Sign in with `{name}`** button under its login form for each enabled
  provider, once a public URL is known. It opens the identity provider in the same tab, and the
  sign-in ends on the home screen. The button is added to the login disclaimer
  `GET /Branding/Configuration` serves; it is never stored. Saving Dashboard → Branding never
  stores it either, even though the branding editor shows it.
- **Native apps** get the same disclaimer, since the server cannot tell who asks. An app that
  shows the disclaimer on its own login screen may show the button's text, or its HTML, there,
  with no working button. Read from the apps' code and not looked at yet: Android TV 0.19.10
  renders the disclaimer as Markdown with HTML, which would leave the label as plain text, and
  Swiftfin 1.6.1 renders it as inline Markdown, which would show the form's markup. The **Show a
  sign-in button for each provider** switch removes the button for every app.
- **The Android app**, which wraps jellyfin-web, has not been tried. An identity provider that
  refuses sign-ins in an embedded browser would break its button there; the app can still sign in
  through Quick Connect.

## Secrets, logs and backups

- The client secrets are kept in
  `{config_dir}/plugins/67a29a1d-e02b-4aaa-bba0-4b2275e765d0/private.json`, readable by the
  server's user only (`0600`). No route returns them, the plugin's
  configuration (`config.json`, which `GET /Plugins/{id}/Configuration` returns) does not hold
  them. No log line or trace holds a secret, a code, a token or a claim set: every live test
  fails if the client secret, a password, a code, `state`, the nonce, the flow cookie, a token,
  any JWT or an identity's subject is in what the server logged at `debug`, and a by-hand pass
  searched four servers' logs and their exported traces for those and for claim names.
- **A server backup holds the client secrets.** A backup archives the whole configuration
  directory, this file included. Archives are written `0600` and no route downloads one, but a
  copy you take elsewhere carries the secrets: keep it as private as the server, or rotate the
  secrets at the identity provider after restoring one on another machine.

## Limits

These are fixed; none is a setting:

- A sign-in must finish within 10 minutes of its start. The page after it hands its one-time code
  to jellyfin-web within 60 seconds.
- A browser holds one sign-in at a time (one cookie): starting a second, in another tab, ends the
  first.
- Pending sign-ins are capped at 10 per address (a new one drops that address's oldest) and 1000
  in all.
- Calls to the identity provider: 5 seconds to connect, 10 seconds per request, answers up to
  1 MiB. Discovery and keys are cached for an hour, and fetched again for an unknown key.
- Pending sign-ins are kept in memory: a restart, or a rolling update that sends the callback to
  another pod, loses them, and the user starts again.

## Adopting a Jellyfin that used the SSO-Auth plugin

Nothing of the community SSO-Auth plugin is imported: not its settings (`SSO-Auth.xml`), not its
links. Single Sign-On starts off. The plugin's accounts keep their saved provider,
`Jellyfin.Plugin.SSO_Auth.Api.SSOController`, which nothing registers, so a password never signs
in to them, as on a Jellyfin without the plugin. Accounts of other providers sign in as before.
To carry those users over, add the providers, register Ferrofin's callback URLs at the identity
provider (Ferrofin's routes are not the plugin's), and link each account by hand under **Linked
accounts**, with the identity's subject. Or give an account the default provider and a password.

## Going back to Jellyfin

Adoption is one-way: going back to Jellyfin means restoring `jellyfin.db.pre-ferrofin`, the copy
taken before Ferrofin first changed the database ([`INSTALL.md`](INSTALL.md)), which holds no
account Single Sign-On created. Ferrofin's export to a Jellyfin database
(`ferrofin-server export-jellyfin`) is hidden while it is unfinished. It drops Ferrofin's own
tables, the identity links among them, and keeps every account's saved provider, so an account
Single Sign-On created carries a provider Jellyfin does not know and cannot sign in there until
it has the default provider. Saving its profile page in Jellyfin's dashboard gives it that
provider (with only the default one, the page hides the choice and saves it), and from then on
the account signs in with a password an administrator sets after that save (one set before it is
ignored). Until then no password signs in to it, an empty one included: its stored hash is in
Jellyfin's own password format, which Jellyfin checks as it checks any other (read from
jellyfin-web 12.4 and Jellyfin 12.4, not run on Jellyfin). The export's loss report says the
same.

## Known limits

- While a provider cannot be reached, every start of a sign-in fetches its discovery document
  again.
- A browser that cannot store the session (storage blocked or full) sees "Sign-in failed" on the
  page after the identity provider, although a session was opened. It ends at the next sign-in
  from that browser, or when an administrator revokes it under Dashboard → Devices.
- A browser's device is first named "Web Browser", until jellyfin-web renames it.
- If the server stops between creating an account and linking it, the account keeps the name
  unlinked, and that user's next sign-in is refused as a name collision. An administrator links
  it by hand or deletes it.

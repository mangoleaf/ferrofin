# OIDC test realm

A Keycloak realm to try Ferrofin's Single Sign-On plugin against, by hand from jellyfin-web and in
the `#[ignore]`d live test (`crates/ferrofin-extensions/tests/oidc_keycloak.rs`). It is test data
only. `ferrofin-test-realm.json` is a realm import file: load it into a Keycloak container you
start for the purpose, or into a Keycloak you already run.

**Every password and the client secret in the file are fixed and published here, so they are for
a throwaway container only.** After you import the realm into a Keycloak anyone else can reach,
set a fresh password for each user and regenerate the client secret (below), and keep the new
values outside the repository.

## What is in it

The realm is `ferrofin-test`, so its issuer is `{Keycloak URL}/realms/ferrofin-test`.

| User | Password in the file | Group | The ID token's `groups` claim |
|---|---|---|---|
| `oidc-admin` | `oidc-admin-password` | `ferrofin-admins` | `["ferrofin-admins"]` |
| `oidc-member` | `oidc-member-password` | `ferrofin-users` | `["ferrofin-users"]` |
| `oidc-guest` | `oidc-guest-password` | none | absent (Keycloak leaves out an empty list) |

The client is `ferrofin`, confidential, with secret `ferrofin-test-client-secret`:

- the authorization code flow only (no implicit flow, no password grant), with PKCE required and
  `S256` the only method it accepts;
- redirect URIs `http://127.0.0.1:*` and `http://localhost:*`, so a Ferrofin on any local port
  can sign in (Keycloak takes a wildcard only at the end of a redirect URI);
- a `groups` mapper that puts the user's group names, without their path, in a top-level `groups`
  claim of the ID token, the access token and the userinfo answer.

Login with an email address, registration, password reset and brute-force detection are off.

## In a Keycloak container

```sh
docker run --rm --name ferrofin-oidc-test -p 127.0.0.1:8080:8080 \
  -e KC_BOOTSTRAP_ADMIN_USERNAME=admin -e KC_BOOTSTRAP_ADMIN_PASSWORD=admin \
  -v "$PWD/contrib/oidc-test-realm:/opt/keycloak/data/import:ro" \
  quay.io/keycloak/keycloak:26.7.4 start-dev --import-realm
```

The realm is imported at start-up, and the issuer is `http://localhost:8080/realms/ferrofin-test`.
Keycloak builds the issuer from the address it is asked at, so Ferrofin must use exactly that
address. It is plain `http`, so turn on the provider's "allow an http issuer" switch. Stop the
container (`docker stop ferrofin-oidc-test`) to throw everything away; the next start imports the
file again.

## In an existing Keycloak

Import the file as a new realm:

- in the admin console: the realm list, **Create realm**, **Resource file**, choose
  `ferrofin-test-realm.json`, **Create**;
- or through the admin REST API, with an administrator's access token:
  `curl -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json'
  --data @contrib/oidc-test-realm/ferrofin-test-realm.json https://keycloak.example.org/admin/realms`.

Then replace the published values, in the realm `ferrofin-test`:

1. **Users**, each of the three, **Credentials**, **Reset password**, with **Temporary** off
   (or `PUT /admin/realms/ferrofin-test/users/{id}/reset-password`).
2. **Clients**, `ferrofin`, **Credentials**, **Regenerate** the client secret
   (or `POST /admin/realms/ferrofin-test/clients/{id}/client-secret`, which answers the new one).
3. For a Ferrofin that is not on `127.0.0.1` or `localhost`, add its callback URL to the client's
   **Valid redirect URIs**: the settings page shows it for each provider,
   `{public URL}/sso/oidc/{provider}/callback`.

## Running Ferrofin against it

1. Start a server on a scratch data directory, with the public URL its callback URLs are built on:

   ```sh
   cargo run -p ferrofin-server -- --data-dir ./target/oidc-check --bind 127.0.0.1 --port 8096 \
     --published-url http://127.0.0.1:8096
   ```

2. In jellyfin-web as an administrator: **Dashboard**, **Plugins**, **Single Sign-On**, enable it,
   then open its settings.
3. **Add provider**: key `keycloak`, any name, the realm's issuer, client id `ferrofin` and the
   client secret; for the container, tick "allow an http issuer". **Save**.
4. The provider's row shows the discovery test: the authorization, token and userinfo endpoints and
   the key set below `{issuer}/protocol/openid-connect/`, and how many signing keys were read.
   Saved with a wrong issuer (`…/realms/ferrofin-missing`), the row says why the test failed, and
   the provider is still saved.
5. Open `http://127.0.0.1:8096/sso/oidc/keycloak/start` in a browser and sign in as a realm user.
   The page you land on names the Ferrofin account that sign-in created and linked; nobody is
   signed in to it yet. Dashboard → Users lists it with Single Sign-On as its provider, and no
   password signs in to it, not even an empty one. Loading that page's URL again is refused, and
   so is finishing in another browser a sign-in started in this one.
6. With the provider's "Create accounts at first sign-in" off, a realm user with no account is
   refused, and no account appears. A local account named like a realm user makes that user's
   sign-in refused, and the local account is left as it was.

## The live test

`crates/ferrofin-extensions/tests/oidc_keycloak.rs` reads the realm's values from the environment
and returns early with a message while any is unset. Keep them in a file outside the repository,
readable by you only, such as `~/.config/ferrofin/oidc-test.env`:

```sh
FERROFIN_OIDC_TEST_ISSUER=http://localhost:8080/realms/ferrofin-test
FERROFIN_OIDC_TEST_CLIENT_ID=ferrofin
FERROFIN_OIDC_TEST_CLIENT_SECRET=ferrofin-test-client-secret
FERROFIN_OIDC_TEST_ADMIN_USER=oidc-admin
FERROFIN_OIDC_TEST_ADMIN_PASSWORD=oidc-admin-password
FERROFIN_OIDC_TEST_ADMIN_GROUP=ferrofin-admins
FERROFIN_OIDC_TEST_MEMBER_USER=oidc-member
FERROFIN_OIDC_TEST_MEMBER_PASSWORD=oidc-member-password
FERROFIN_OIDC_TEST_MEMBER_GROUP=ferrofin-users
FERROFIN_OIDC_TEST_GUEST_USER=oidc-guest
FERROFIN_OIDC_TEST_GUEST_PASSWORD=oidc-guest-password
```

(the container's values; a shared Keycloak's are the fresh ones you set). Then:

```sh
set -a; . ~/.config/ferrofin/oidc-test.env; set +a
cargo test -p ferrofin-extensions --test oidc_keycloak -- --include-ignored --nocapture
```

It saves the realm's client as a provider and checks that discovery finds the realm's endpoints
and keys, and that it fails cleanly, with the provider still saved, for a realm that does not exist
and for the issuer written with a trailing `/`. Then it signs the member user in: it starts a
sign-in, fills in Keycloak's own login form, and checks that the callback Keycloak sends the
browser back to creates and links the member's account under the Single Sign-On provider, that no
password (the realm's, or an empty one) signs in to that account or counts toward its lockout, that
the same callback is refused a second time, that a callback without the flow cookie (another
browser) is refused, and that a second sign-in finds the same account. It builds callback URLs
on `http://127.0.0.1:8096`, which the realm's redirect URIs accept; nothing needs to listen there.
The other users and the groups are for the later sign-in checks.

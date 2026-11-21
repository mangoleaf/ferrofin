# OIDC test realm

A Keycloak realm to try Ferrofin's Single Sign-On plugin against, by hand from jellyfin-web and in
the `#[ignore]`d live test (`apps/ferrofin-server/tests/oidc_keycloak.rs`). It is test data
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
   You land on jellyfin-web's home screen, signed in to the Ferrofin account that sign-in created
   and linked; the address bar never shows a token. Dashboard → Users lists the account with
   Single Sign-On as its provider, and no password signs in to it, not even an empty one.
   Dashboard → Devices lists the browser once, and the activity log has the sign-in. Loading the
   callback's URL again is refused, and so is finishing in another browser a sign-in started in
   this one.
6. Disable the account (or turn off its remote access and come from another address, give it a
   parental schedule, limit its devices or sessions) and sign in again: the page says why, no
   session opens, and the activity log has the failed sign-in.
7. With the provider's "Create accounts at first sign-in" off, a realm user with no account is
   refused, and no account appears. A local account named like a realm user makes that user's
   sign-in refused, and the local account is left as it was.
8. With the provider's "Sync profile pictures" on, a user's picture follows the `picture` claim,
   which Keycloak's built-in `profile` scope fills from a user attribute named `picture`. Keycloak
   keeps only the attributes the realm's user profile declares, and this realm declares none
   beyond the defaults, so give one user a picture for the check: **Realm settings**, **User
   profile**, **Create attribute** `picture`, viewed and edited by admins only; then that user's
   **Attributes**, `picture`, the URL of an image. Ferrofin downloads it under its image download
   limits, so an image on a private address, a loopback one included, also needs that address in
   the server's `FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW`. Sign in, and the account's picture in
   jellyfin-web is that image. Sign in again and nothing is stored again; change the image at its
   URL and the next sign-in replaces it. With the image's server stopped, or the address not
   allowed, the sign-in is as quick as before and the picture stays. Remove the attribute and
   its declaration when you are done.

## The live test

`apps/ferrofin-server/tests/oidc_keycloak.rs` reads the realm's values from the environment
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
cargo test -p ferrofin-server --test oidc_keycloak -- --include-ignored --nocapture
```

It runs on the server the composition root wires, through its router, one fresh server per test,
and builds callback URLs on `http://127.0.0.1:8096`, which the realm's redirect URIs accept; nothing
needs to listen there. Each test starts a sign-in, fills in Keycloak's own login form, and hands
the callback Keycloak sends the browser back to to the router. Its eight tests check:

1. **Discovery**: saving the realm's client as a provider finds the realm's endpoints and keys,
   and fails cleanly, the provider still saved, for a realm that does not exist and for the issuer
   written with a trailing `/`.
2. **Sign-in** (`oidc-member`): the completion page's code swaps for a session at
   `POST /sso/oidc/exchange` whose token opens `GET /Users/Me`; the account was created and linked
   under the Single Sign-On provider; no password (the realm's, or an empty one) signs in to it,
   through the user manager or `POST /Users/AuthenticateByName`, or counts toward its lockout; the
   same callback and the same code are each refused a second time, and so are a callback without
   the flow cookie (another browser), one with a tampered `state` and one with a tampered `iss`;
   a second sign-in finds the same account.
3. **Provisioning** (`oidc-member`): with "Create accounts at first sign-in" off the user is
   refused and no account is made; with it on, a local account with the same name makes the
   sign-in refused, is neither linked nor changed, and its password still signs in.
4. **The required group and the administrator flag**: with `ferrofin-users` required,
   `oidc-guest` (no group, so no `groups` claim) is refused and no account is created for it,
   while `oidc-member` signs in; with role sync on and `ferrofin-admins` as the administrator
   role, `oidc-admin`'s first session is an administrator's and `oidc-member`'s is not.
5. **The administrator role moving**: with role sync on, `oidc-admin` is made an administrator,
   then keeps the flag as the last administrator once the role moves to `ferrofin-users`;
   `oidc-member` then gains it, and `oidc-admin`'s next sign-in demotes them.
6. **Libraries and Live TV**: `ferrofin-users` mapped to one library and `ferrofin-admins` to
   every library, Live TV and its recordings; `oidc-member`'s first session has only that library,
   `oidc-admin`'s every library and both Live TV permissions, `oidc-guest`'s none of them.
7. **Linking** (`oidc-guest`): a local account signed in with its password links itself at
   `POST /sso/oidc/keycloak/link`; the identity then signs in to it and so does the password;
   removing the link signs out all three sessions, and the identity is refused again.
8. **Quick Connect** (`oidc-member`): an app's code entered at `/sso/quickconnect`, signed in at
   Keycloak and confirmed signs the app in, on its own device only.

Every test also captures what the process logs meanwhile (at `debug`, the server's crates at
`trace`, span fields included) and fails if a secret it handled is in it: the client secret (also
as the token request's `Basic` credentials), the users' passwords, the code, `state`, the nonce,
the flow cookie, the exchange code, the session tokens, a JWT, an identity's subject. With
`--nocapture` each test prints what it verified and how many bytes of log it searched.

## By hand

Some checks need Keycloak's admin login, which the live test does not take, or a browser or an
app. They are run by hand against a scratch server (above, "Running Ferrofin against it"), and
the realm is put back afterwards:

- **A demotion at Keycloak**: with role sync on and `ferrofin-admins` as the administrator role,
  sign `oidc-admin` in (an administrator), take it out of `ferrofin-admins`, and sign in again: no
  longer an administrator (the server's own seeded administrator keeps another one). Put it back
  in the group, and the next sign-in makes it one again. The admin REST calls this was run with,
  with an administrator's access token: `DELETE` (out) or `PUT` (back in)
  `/admin/realms/ferrofin-test/users/{user id}/groups/{group id}`.
- **Profile pictures**: step 8 of "Running Ferrofin against it".
- **jellyfin-web**: steps 5 to 7 of "Running Ferrofin against it" from a browser, the login page's button
  ([`docs/SINGLE_SIGN_ON.md`](../../docs/SINGLE_SIGN_ON.md#what-clients-show)), `/sso/link` and the
  settings page's **Linked accounts**, and `/sso/quickconnect` with a code an app started.
- **Native apps** (not yet run): approve a real app's Quick Connect code through
  `/sso/quickconnect`, look at what each app's login screen shows while a provider is enabled,
  and try the button in the Android app, which wraps jellyfin-web.

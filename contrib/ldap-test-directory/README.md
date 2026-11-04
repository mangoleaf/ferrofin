# LDAP test directory

A throwaway OpenLDAP directory to try Ferrofin's LDAP-Auth against, from jellyfin-web or a
native client. It is test data only: every password is fixed and written below, the directory
administrator's included. Run it on a machine you control, publish its ports only on an address
your own network reaches (Docker's published ports bypass host firewalls such as ufw), and
remove it when you are done.

It needs `bash`, `docker` and `openssl`. The image (`alpine:3.22` with OpenLDAP) is built on
the first start; it is the one `crates/ferrofin-extensions/tests/ldap_directory.rs` uses.

## Start and stop

```sh
# Ferrofin on the same machine:
contrib/ldap-test-directory/directory.sh up

# Ferrofin on another machine, reaching this one as 192.168.1.20:
contrib/ldap-test-directory/directory.sh up --host 192.168.1.20 --bind 192.168.1.20

contrib/ldap-test-directory/directory.sh settings   # print the settings again
contrib/ldap-test-directory/directory.sh logs       # follow the directory's log: every connection, bind and search
contrib/ldap-test-directory/directory.sh down       # remove both containers
rm -r ~/.local/state/ferrofin-ldap-test-directory    # and the keys, certificates and seed (or your --dir)
```

- `--host` is the name or address Ferrofin reaches the directory by. It goes into the server
  certificate and the referral, so it must be what you type as "LDAP Server".
- `--bind` is the address the ports are published on, `127.0.0.1` unless you give one.
- `--dir` (or `FERROFIN_LDAP_TEST_DIR`) is where the configuration, seed and certificates,
  private keys included, are written: `~/.local/state/ferrofin-ldap-test-directory` by
  default (under `$XDG_STATE_HOME` when that is set). Give `settings` the same `--dir`. The
  root CA is `<dir>/main/ca.pem`: copy it where the Ferrofin server can read it when that
  server runs elsewhere or as another user.

Each `up` starts both directories again from their seed, with a new CA.

| Container | LDAP and StartTLS | LDAPS |
|---|---|---|
| `ferrofin-ldap-test-directory`, the directory | 3389 | 3636 |
| `ferrofin-ldap-test-partners`, which the directory refers searches to | 4389 | 4636 |

## What is in it

| Sign in with | Who | Notes |
|---|---|---|
| `ada` / `ada-secret` | Ada Lovelace | an administrator through the admin filter, with a picture |
| `alan` / `alan-secret` | Alan Turing | a user |
| `grace` / `grace-secret` | Grace Hopper | found only through the referral, with the base DN `dc=example,dc=org` |
| `edsger` / `edsger-secret` | Edsger Dijkstra | refused: the search filter does not let him in |

A user can type their uid, their full name or their mail (`LDAP Search Attributes`); their
Ferrofin name is their full name (`LDAP Username Attribute: cn`). The bind user
`uid=jellyfin,ou=people,dc=example,dc=org` (password `jellyfin-bind-secret`) may read every
entry and set passwords. The directory administrator is `cn=admin,dc=example,dc=org` with
password `admin-secret`; `modify` below binds as it.

To change an entry, send LDIF as the directory's administrator:

```sh
printf 'dn: uid=alan,ou=people,dc=example,dc=org\nchangetype: modify\nreplace: cn\ncn: Alan M. Turing\n' |
  contrib/ldap-test-directory/directory.sh modify
contrib/ldap-test-directory/directory.sh picture alan photo.jpg   # replace Alan's jpegPhoto
```

## Checking a client against it

The steps a person runs with a real client, such as Jellyfin for Android, Swiftfin, Android TV
or Jellyfin Media Player. They check what a scripted pass cannot: the client's own sign-in
screen, how it shows a refused or failed sign-in, the picture it displays, and the session it
opens.

1. In jellyfin-web as an administrator: Dashboard → Plugins → LDAP-Auth → Enable, then
   Settings. Enter what `directory.sh up` printed, tick StartTLS and give the root CA path.
   "Save and Test LDAP Server Settings" shows `Connect (Success); Set StartTLS (Success); Bind
   (Success); Base Search (Found 5 Entities)`. Tick "Enable User Creation", "Allow Password
   Change" and "Enable profile image synchronization", set a "Password Reset Url" such as
   `https://example.org/reset?u=$userName`, and save.
2. In the client, sign in as `ada`. She lands on the home screen as Ada Lovelace, with the
   dashboard (she is an administrator) and her picture, a white A on blue.
3. Sign out, sign in as `alan`: no dashboard. Then `edsger`, and `alan` with a wrong password:
   both refused, as with a wrong password on a local account.
4. As `alan`, change the password in the client's settings, sign out and sign in with the new
   one. `directory.sh logs` shows a `PASSMOD` for his entry.
5. "Forgot password" for `Alan Turing` (where the client offers it) shows the reset URL with
   `alan turing` in it.
6. `docker pause ferrofin-ldap-test-directory`, then sign in as `ada`: after about 10 seconds
   the client reports a server error, and a local account signs in meanwhile without waiting.
   `docker unpause ferrofin-ldap-test-directory`, and `ada` signs in again.
7. Optional, the referral: set the base DN to `dc=example,dc=org` and save; `grace` signs in.
   "Save and Test LDAP Server Settings" then stops at the referral (`Search result reference
   received, and referral following is off`), as Jellyfin's plugin does: that test follows no
   referral, sign-in does.

Active Directory behaves differently in places (its referrals, `memberOf` filters, password
changes), and this directory does not stand in for it.

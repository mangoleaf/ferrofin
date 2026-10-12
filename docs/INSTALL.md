# Installing on bare metal (systemd)

The Docker image and Helm chart in the [README](../README.md#quickstart) bundle everything.
This page is the other path: Ferrofin as a Debian package (or the release tarball) on a
Debian/Ubuntu host under systemd, with jellyfin-ffmpeg and the jellyfin-web client from
Jellyfin's own apt repository.

| Path | What lives there |
|---|---|
| `/usr/bin/ferrofin-server` | the binary (the `.deb` puts it here) |
| `/etc/ferrofin/config.toml` | configuration ([`docs/CONFIG.md`](CONFIG.md)); a conffile, never overwritten on upgrade |
| `/lib/systemd/system/ferrofin.service` | the unit ([`contrib/systemd/ferrofin.service`](../contrib/systemd/ferrofin.service)) |
| `/var/lib/ferrofin` | `jellyfin.db`, `cache/` (transcodes), `log/`, `plugins/`, `config/` |
| `/usr/lib/jellyfin-ffmpeg/` | jellyfin-ffmpeg (`ffmpeg`, `ffprobe`), from the `jellyfin-ffmpeg8` package |
| `/usr/share/jellyfin/web/` | jellyfin-web's built client, from the `jellyfin-web` package, served at `/web` |

## 1. Jellyfin's apt repository (ffmpeg and the web client)

Ferrofin does not ship its own ffmpeg. It uses **jellyfin-ffmpeg**, the same build the
release image bundles: SIMD tonemapping, current libx264/libx265, `libfdk_aac`, and
`--enable-chromaprint` for the intro skipper. Your distro's `ffmpeg` works for basic
transcodes but lacks several of those, and Ferrofin plans every transcode against the
binary it probes at startup, so the package depends on `jellyfin-ffmpeg8` and `jellyfin-web`
outright. apt can only satisfy that once the repository is configured:

```sh
sudo apt-get install -y ca-certificates curl gnupg
curl -fsSL https://repo.jellyfin.org/jellyfin_team.gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/jellyfin.gpg
echo "deb [signed-by=/usr/share/keyrings/jellyfin.gpg] https://repo.jellyfin.org/debian $(. /etc/os-release; echo "$VERSION_CODENAME") main" \
  | sudo tee /etc/apt/sources.list.d/jellyfin.list
sudo apt-get update
```

Replace `debian` with `ubuntu` in the repository line on Ubuntu. Do **not** install the
`jellyfin-server` package on the same host unless you mean to run both; they would race for
port 8096.

## 2. Install the package

Download the `.deb` for your architecture from the
[releases page](https://github.com/mangoleaf/ferrofin/releases) and install it; apt pulls
`jellyfin-ffmpeg8` and `jellyfin-web` with it:

```sh
V=$(curl -fsSL https://api.github.com/repos/mangoleaf/ferrofin/releases/latest \
  | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')   # latest release, without its v
A=$(dpkg --print-architecture)                        # amd64 or arm64
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/v$V/ferrofin_${V}_$A.deb"
sudo apt-get install -y "./ferrofin_${V}_$A.deb"
```

v1.3.1 is the only release whose packages are named `ferrofin_1.3.1-1_$A.deb`
([#36](https://github.com/mangoleaf/ferrofin/issues/36)); apt upgrades from it normally.

The package creates the `ferrofin` system user and `/var/lib/ferrofin`, installs the
unit, and **does not enable or start it**: configure a password or be ready to complete
the web setup wizard on first boot. Give the `ferrofin` user read access to your
media, typically by adding it to the group that owns the library. Keep the service stopped
until you choose the fresh-install or migration procedure in step 4 below.

### Without the package: the release tarball

The tarball carries only the binary. Do step 1, install the two packages by hand
(`sudo apt-get install -y jellyfin-ffmpeg8 jellyfin-web`), and with a checkout of this
repository for the `contrib/` files:

```sh
V=v1.0.1; T=x86_64-unknown-linux-gnu             # or aarch64-unknown-linux-gnu
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/$V/ferrofin-$V-$T.tar.gz"
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/$V/ferrofin-$V-$T.tar.gz.sha256"
sha256sum -c "ferrofin-$V-$T.tar.gz.sha256"
tar xzf "ferrofin-$V-$T.tar.gz"
sudo install -m 755 "ferrofin-$V-$T/ferrofin-server" /usr/local/bin/ferrofin-server
sudo useradd --system --user-group --home /var/lib/ferrofin --shell /usr/sbin/nologin ferrofin
sudo install -d -o ferrofin -g ferrofin -m 0750 /var/lib/ferrofin
sudo install -d /etc/ferrofin
sudo install -o root -g ferrofin -m 640 contrib/debian/config.toml /etc/ferrofin/config.toml
sudo install -m 644 contrib/systemd/ferrofin.service /etc/systemd/system/ferrofin.service
sudo mkdir -p /etc/systemd/system/ferrofin.service.d
printf '[Service]\nExecStart=\nExecStart=/usr/local/bin/ferrofin-server --config /etc/ferrofin/config.toml\n' \
  | sudo tee /etc/systemd/system/ferrofin.service.d/binary.conf
sudo systemctl daemon-reload
```

The drop-in repoints `ExecStart=` at `/usr/local/bin`; everything else in the unit is
shared with the package. The unit is installed but **not started yet**. Choose the
fresh-install or migration procedure below before starting it.

## 3. The unit

The unit runs as the `ferrofin` user with no extra sandboxing, like Jellyfin's own unit.
It adds the `render` group (`SupplementaryGroups=render`) so VAAPI/QSV transcoding can open
`/dev/dri/renderD*` without changing the user's group membership. The unit names ffmpeg, ffprobe
and the web client explicitly through `FERROFIN_FFMPEG_PATH`, `FERROFIN_FFPROBE_PATH` and
`FERROFIN_WEB_DIR`, because systemd's `PATH` does not include `/usr/lib/jellyfin-ffmpeg`
and discovery would otherwise land on Debian's `/usr/bin/ffmpeg`.

## 4. First startup: choose one path

### Fresh install

If you are migrating from Jellyfin, skip this subsection. Starting against an empty data
directory creates `ferrofin.db` and JSON configuration, which take precedence over the
Jellyfin database and XML files you would copy later.

For a headless first boot, you can set `admin_password` in
`/etc/ferrofin/config.toml` using `sudoedit` before starting the service. A local
config file is sufficient: the package makes it readable only by root and the
`ferrofin` group (`root:ferrofin`, mode `0640`). No secret-management service is
required. Once the account exists, you can remove the `admin_password` entry;
it is only used when creating the first user and does not reset existing passwords.
Alternatively, leave it unset and set the password in the web setup wizard.
Ferrofin does not generate or log an administrator password.

```sh
sudo systemctl enable --now ferrofin
journalctl -u ferrofin -f                         # startup diagnostics
```

Open `http://host:8096/web`.

### Migrate an existing Jellyfin installation

Ferrofin supports database adoption from **Jellyfin 10.11.8, 10.11.9, 10.11.10, 10.11.11,
12.0.0 and 12.1.0**. All six releases passed the live adoption suite on 2026-09-16,
including 12.1.0 reached directly from 10.11.8 and through 12.0.0. The
[support matrix and test record](../adoption/README.md#supported-and-tested-versions)
identify the seven tested fixture paths and the server image used.

The gate checks the exact migration history and refuses unknown or incomplete histories;
support does not extend automatically to other 10.11.x or 12.x releases. Upgrade an older
installation to a supported version under Jellyfin before copying it; do not edit migration
history to bypass the check. Keep the original Jellyfin installation intact until Ferrofin
has been verified so you can roll back if needed.

#### Stop both servers and copy the Jellyfin state

Install `rsync` and stop both services before copying. Run the checked-in script from a
Ferrofin checkout. It uses the Debian package paths by default and assigns the copied files
to the `ferrofin` user. Ferrofin's `data_dir` has the same layout as Jellyfin's
`/var/lib/jellyfin`, so each path keeps its relative position; the `data/` inside each
holds the database, playlists and collections:

| Jellyfin | Ferrofin | Contents |
|---|---|---|
| `/var/lib/jellyfin/data/jellyfin.db` (and `-wal`/`-shm`) | `{data_dir}/data/` | the database |
| `/var/lib/jellyfin/data/playlists/`, `collections/` | `{data_dir}/data/` | playlist and collection folders |
| `/var/lib/jellyfin/root/default/` | `{data_dir}/root/default/` | library definitions |
| `/var/lib/jellyfin/metadata/` | `{data_dir}/metadata/` | images and downloaded metadata |
| `/etc/jellyfin/` | `{config_dir}/` | XML configuration |

The script reads `data_dir` and `config_dir` from `/etc/ferrofin/config.toml`. The package
sets only `data_dir`, so `config_dir` is `{data_dir}/config`; `/etc/ferrofin` keeps just
`config.toml` and stays read-only to the service.

The rest of Jellyfin's `data/` (subtitle and attachment extraction caches, backups, task
history) is not copied, nor are `plugins/` and `Subtitle Edit/`. `root/default/` holds
only the library definitions (`.mblink` files naming each media path); media stays where
it is.

```sh
sudo systemctl stop jellyfin ferrofin
sudo apt-get install -y rsync
sudo scripts/migrate-jellyfin.sh
```

The script must run after installing Ferrofin but before its first start. It refuses to
copy over an existing Ferrofin database, because `--ignore-existing` would otherwise
silently keep that database instead of adopting Jellyfin's. It leaves the original
Jellyfin files untouched. For Docker or other non-Debian layouts, `--help` lists the options
for the Jellyfin and Ferrofin directories.

Jellyfin .NET plugins cannot run in Ferrofin; they require Ferrofin-compatible
replacements. `network.xml` carries remote-access policy, IP filters, trusted proxies, and
local-network definitions. A copied symbolic link still points at its original target, so
ensure any linked external state remains available to Ferrofin. If Jellyfin uses custom
cache, metadata, or configuration paths, copy those separately to the configured Ferrofin
paths.

#### Unicode usernames

Usernames retain their displayed spelling. Lookup and uniqueness use ICU simple uppercase
with .NET's invariant-casing exceptions: `münchen` and `MÜNCHEN` identify the same
account, but `i` and dotless `ı` remain distinct. This also applies to account creation
and renaming. Accents are not stripped and visually similar letters from different
scripts are not merged.

Older Ferrofin installations may already contain case-variant duplicates. Migration
checks all normalized keys before changing username data and refuses collisions with
the account IDs and names. Resolve conflicting names in the original installation,
then retry from a clean copy. Do not delete or combine accounts to bypass the error.

#### Check paths, start, and verify adoption

Keep media available at the paths stored by Jellyfin, or update the library paths before
scanning. If Jellyfin used a custom metadata directory, copy its contents into the new
`metadata/` directory: the XML import intentionally excludes Jellyfin's `MetadataPath`.
Machine-specific encoder, cache, and certificate paths are also not automatically carried
over; configure those for this host, including HTTPS if your deployment requires it.

```sh
sudo systemctl enable --now ferrofin
journalctl -u ferrofin -f
```

Verify the following before resuming normal use:

- The journal reports database adoption and the `jellyfin.db.pre-ferrofin` backup beside
  the adopted database. Investigate any configuration-import warnings.
- Your existing users can log in, and watch history, libraries, library settings, and
  artwork are present. Library `options.xml` files are imported into `options.json`.
- Remote access, IP filters, trusted proxies, and local-network definitions match your
  intended policy; check them before exposing the replacement server to remote clients.
- Playback uses the configured jellyfin-ffmpeg executable.

The copy includes all source files, but only settings supported by Ferrofin are imported;
review warnings about unsupported fields. Keep Jellyfin stopped while Ferrofin uses the
same media paths, and prevent its service/container from automatically restarting.

Adoption is one-way. To roll back before retiring Jellyfin, stop Ferrofin and restart
Jellyfin against its untouched original state. Do not point Jellyfin at the adopted
database. Changes made in Ferrofin after migration are not copied back.
See [`docs/UPGRADING.md`](UPGRADING.md).

## Upgrading

Install the new `.deb` the same way; a running server is restarted by the package, and
`/etc/ferrofin/config.toml` is a conffile, so your edits survive. For a tarball install,
replace the binary in `/usr/local/bin` and `systemctl restart ferrofin`.

Ferrofin's own migrations run on start. A database that a newer version has migrated may
not open under an older one, so back up `jellyfin.db` before a major upgrade. `apt-get
remove` keeps `/etc/ferrofin` and `/var/lib/ferrofin`; `apt-get purge` removes the
configuration and still leaves `/var/lib/ferrofin` (the database and the pre-adoption
Jellyfin backup) for you to delete by hand.

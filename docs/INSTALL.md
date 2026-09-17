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
| `/var/lib/ferrofin/data` | `jellyfin.db`, `cache/` (transcodes), `log/`, `plugins/`, `config/` |
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
V=1.0.1; A=amd64                                  # or arm64
curl -fsSLO "https://github.com/mangoleaf/ferrofin/releases/download/v$V/ferrofin_${V}_$A.deb"
sudo apt-get install -y "./ferrofin_${V}_$A.deb"
```

The package creates the `ferrofin` system user and `/var/lib/ferrofin/data`, installs the
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
sudo install -d -o ferrofin -g ferrofin -m 0750 /var/lib/ferrofin /var/lib/ferrofin/data
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

The unit runs with `ProtectSystem=strict`: the filesystem is read-only except
`/var/lib/ferrofin/data`. Scans and playback only read media, so that is enough for most
installs. If you enable deleting items or saving metadata/images into the library from the
UI, add that library to `ReadWritePaths=` in a drop-in:

```sh
sudo systemctl edit ferrofin      # opens an override; add:
# [Service]
# ReadWritePaths=/srv/media
```

For VAAPI/QSV hardware transcoding uncomment the `DeviceAllow=` and
`SupplementaryGroups=` lines the same way. The unit names ffmpeg, ffprobe and the web
client explicitly through `FERROFIN_FFMPEG_PATH`, `FERROFIN_FFPROBE_PATH` and
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
history to bypass the check. Adoption is one-way: keep a full backup to return to Jellyfin.

#### Stop both servers and copy the complete state

Keep Jellyfin and Ferrofin stopped throughout the copy. The example below uses the Debian
package layout: all data in `/var/lib/jellyfin`, with configuration stored separately in
`/etc/jellyfin`. Change both source paths for your installation. For Docker, stop the
container and use the host paths of its data/config mounts; layouts vary by image.

Copy **all files**, including hidden files, from the entire data and configuration
directories. Preserve the directory structure: `data/jellyfin.db` can stay nested because
Ferrofin detects that layout. Include the database's `-wal` and `-shm` companions when
present. Do not copy a database while either server is writing to it.

```sh
sudo sh <<'SH'
set -eu
systemctl stop jellyfin ferrofin
source_data=/var/lib/jellyfin
source_config=/etc/jellyfin
destination=/var/lib/ferrofin/data

test -d "$source_data"
test -d "$source_config"
test -f "$source_data/data/jellyfin.db" || test -f "$source_data/jellyfin.db"
test -f "$source_config/system.xml"
test -f "$source_config/network.xml"

# An independent backup, readable only by root. Originals are left in place.
backup=$(mktemp -d /var/lib/ferrofin-migration.XXXXXX)
printf 'Migration backup: %s\n' "$backup"
mkdir "$backup/jellyfin-data" "$backup/jellyfin-config"
cp -a "$source_data/." "$backup/jellyfin-data/"
cp -a "$source_config/." "$backup/jellyfin-config/"

# Preserve a previous Ferrofin installation, including its DB, WAL and JSON.
# Mixing it into the copy would cause its database/settings to win on startup.
if [ -e "$destination" ]; then
    mv "$destination" "$backup/previous-ferrofin-data"
fi
mkdir -p "$destination"
cp -a "$backup/jellyfin-data/." "$destination/"
# A copied config symlink must not send writes back into the original install.
if [ -L "$destination/config" ]; then
    mv "$destination/config" "$backup/copied-config-symlink"
fi
mkdir -p "$destination/config"
cp -a "$backup/jellyfin-config/." "$destination/config/"
chown -R ferrofin:ferrofin "$destination"
SH
```

If a preflight check fails, verify the source paths and locate the missing file before
continuing. If the destination is a mount point, use a separate empty destination and
update `data_dir` and the unit's writable paths instead of moving the mount point.

This copies library definitions (`root/default/`), metadata and images (`metadata/`),
playlists, plugin files, and configuration alongside the database. Jellyfin .NET plugins
are retained in the copy but cannot run in Ferrofin; they require Ferrofin-compatible
replacements. A copied symbolic link still points at its original target: separately back
up any external state directories and arrange access to them. Include any separately
configured cache, metadata, or configuration directories in your backup as well.

The destination's `config/` is Ferrofin's default `config_dir` for this guide. If you set
`config_dir` explicitly, copy the configuration there instead, using a clean destination.
`network.xml` is essential: it carries remote-access policy, IP filters, trusted proxies,
and local-network definitions. Omitting it restores defaults, including remote access
enabled and an empty IP filter. Existing Ferrofin JSON takes precedence over copied XML,
which is why the example preserves the previous destination and starts with a clean one.

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

Adoption is one-way. To roll back, stop Ferrofin and restart Jellyfin against its untouched
original state, or restore the full backup while both are stopped. Do not point Jellyfin
at the adopted database. Changes made in Ferrofin after migration are not copied back.
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

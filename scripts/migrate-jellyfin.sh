#!/usr/bin/env bash
# Copy a Jellyfin installation into Ferrofin's data directory; see --help.
set -euo pipefail

program=${0##*/}

usage() {
    cat <<EOF
Usage: $program [OPTION]...

Copy a Jellyfin installation into Ferrofin's data directory so Ferrofin adopts it
on its first start. Run as root, after stopping both services and before Ferrofin
has ever started with the destination.

Ferrofin's data_dir has the layout of Jellyfin's data directory, so each path
keeps its relative position:

  JELLYFIN/data/jellyfin.db*    -> DATA_DIR/data/
  JELLYFIN/data/playlists/      -> DATA_DIR/data/playlists/
  JELLYFIN/data/collections/    -> DATA_DIR/data/collections/
  JELLYFIN/root/default/        -> DATA_DIR/root/default/   (library definitions)
  JELLYFIN/metadata/            -> DATA_DIR/metadata/       (images)
  JELLYFIN_CONFIG/              -> CONFIG_DIR/              (XML configuration)

The rest of Jellyfin's data/ holds caches Ferrofin rebuilds or does not read, and
its .NET plugins/ and "Subtitle Edit/" cannot run in Ferrofin; none are copied.
Files already at the destination are kept. The Jellyfin files are not modified.

Options:
  -j, --jellyfin DIR          Jellyfin data directory (default: /var/lib/jellyfin)
  -J, --jellyfin-config DIR   Jellyfin configuration directory (default: /etc/jellyfin)
  -f, --ferrofin-config FILE  Ferrofin config.toml to read data_dir and config_dir
                              from (default: /etc/ferrofin/config.toml)
  -d, --data-dir DIR          Ferrofin data_dir (default: data_dir from the
                              config.toml, else /var/lib/ferrofin)
  -c, --config-dir DIR        Ferrofin config_dir (default: config_dir from the
                              config.toml, else DATA_DIR/config)
  -h, --help                  Show this help and exit
EOF
}

die() {
    echo "$program: $*" >&2
    exit 1
}

source_data=/var/lib/jellyfin
source_config=/etc/jellyfin
ferrofin_toml=/etc/ferrofin/config.toml
destination=
destination_config=

while (($#)); do
    case $1 in
        -h | --help)
            usage
            exit 0
            ;;
        -j | --jellyfin | -J | --jellyfin-config | -f | --ferrofin-config | -d | --data-dir | -c | --config-dir)
            (($# >= 2)) || die "option $1 requires an argument"
            option=$1
            value=$2
            shift 2
            ;;
        --*=*)
            option=${1%%=*}
            value=${1#*=}
            shift
            ;;
        *)
            die "unknown argument: $1 (see --help)"
            ;;
    esac
    case $option in
        -j | --jellyfin) source_data=$value ;;
        -J | --jellyfin-config) source_config=$value ;;
        -f | --ferrofin-config) ferrofin_toml=$value ;;
        -d | --data-dir) destination=$value ;;
        -c | --config-dir) destination_config=$value ;;
        *) die "unknown option: $option (see --help)" ;;
    esac
done

[[ ${EUID} -eq 0 ]] || die "run this script as root (for example: sudo $0)"
command -v rsync >/dev/null 2>&1 || die "rsync is required; install it with your package manager"

toml_value() {
    [[ -f "$ferrofin_toml" ]] || return 0
    sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*\$/\1/p" "$ferrofin_toml" | tail -n 1
}

destination=${destination:-$(toml_value data_dir)}
destination=${destination:-/var/lib/ferrofin}
destination_config=${destination_config:-$(toml_value config_dir)}
destination_config=${destination_config:-$destination/config}

[[ -f "$source_data/data/jellyfin.db" ]] || die "no jellyfin.db found at $source_data/data/jellyfin.db"
[[ -d "$source_data/root/default" ]] || die "no library definitions found at $source_data/root/default"
getent passwd ferrofin >/dev/null || die "the ferrofin service user does not exist; install the Ferrofin package first"
for db in ferrofin.db jellyfin.db data/jellyfin.db; do
    [[ ! -e "$destination/$db" ]] || die "a database already exists at $destination/$db; migrate before the first Ferrofin start"
done

copy() {
    mkdir -p "$2"
    rsync -a --ignore-existing --chown=ferrofin:ferrofin "$1/" "$2/"
    echo "Copied $1 -> $2"
}

mkdir -p "$destination/data"
for db in "$source_data"/data/jellyfin.db "$source_data"/data/jellyfin.db-wal "$source_data"/data/jellyfin.db-shm; do
    if [[ -f "$db" ]]; then
        rsync -a --chown=ferrofin:ferrofin "$db" "$destination/data/"
        echo "Copied $db -> $destination/data/"
    fi
done
for dir in playlists collections; do
    if [[ -d "$source_data/data/$dir" ]]; then
        copy "$source_data/data/$dir" "$destination/data/$dir"
    fi
done
copy "$source_data/root/default" "$destination/root/default"
if [[ -d "$source_data/metadata" ]]; then
    copy "$source_data/metadata" "$destination/metadata"
fi
if [[ -d "$source_config" ]]; then
    copy "$source_config" "$destination_config"
fi

chown ferrofin:ferrofin "$destination" "$destination/data" "$destination/root"

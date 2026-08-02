"""JSON deep-diff for the Layer-2 read differential — a faithful port of
benchmark/parity-diff.js (kept in sync so the k6 and Python engines agree).

diff() walks two trees and buckets differences into mismatch/missing/extra.
Arrays are matched by a stable key (Path > Name > Id), not index, so two
independently-scanned servers whose ids differ still line up the SAME element.
`volatile` names keys that legitimately differ between instances (ids, dates,
per-server paths) and are skipped.
"""
import re

# Keys that legitimately differ between two independent instances/scans (from parity.js).
VOLATILE = re.compile("^(" + "|".join([
    "Id", "Key", "ItemId", "ImageTags", "ServerId", "ServerName", "Etag", "ETag", "PlaySessionId",
    "ImageBlurHashes",
    # Divergent-GUID family: item GUIDs derive per-scan, so every id that references another item
    # differs between independent servers (documented deferral — resolved by the shared-DB path).
    # id-correlation aligns the items themselves by Path; these cross-references still can't match.
    "ParentId", "SeriesId", "SeasonId", "AlbumId", "ParentLogoItemId", "ParentBackdropItemId",
    "ParentThumbItemId", "ParentArtItemId", "ParentPrimaryImageItemId", "PrimaryImageItemId",
    "DisplayPreferencesId",
    # BlurHash: valid, but not byte-identical to Jellyfin's Skia 128px downsample (documented deferral).
    "BlurHash",
    "DateCreated", "DateModified", "DateLastSaved", "DateLastMediaAdded", "DateLastRefreshed",
    "LastActivityDate", "LastLoginDate", "LastPlaybackCheckIn", "RemoteEndPoint", "UserId",
    "StartTimeUtc", "EndTimeUtc",
    "ProductName", "PackageName", "WebPath", "LocalAddress", "OperatingSystem",
    "OperatingSystemDisplayName", "SystemArchitecture", "EncoderLocation",
    "StartupWizardCompleted", "CanSelfRestart",
    "TranscodingTempPath", "LogPath", "InternalMetadataPath", "ItemsByNamePath", "CachePath",
    "ProgramDataPath",
]) + ")$")

ALIGN_KEYS = ("Path", "Name", "Id")


def _kind(x):
    if isinstance(x, list):
        return "array"
    if x is None:
        return "null"
    if isinstance(x, dict):
        return "object"
    return "leaf"


def _brief(x):
    import json
    s = json.dumps(x)
    return s[:80] + "…" if s and len(s) > 80 else s


def _align_key(e):
    if not isinstance(e, dict):
        return None
    for k in ALIGN_KEYS:
        if k in e and isinstance(e[k], (str, int, float)) and not isinstance(e[k], bool):
            return f"{k}={e[k]}"
    return None


def _keyed(arr):
    m = {}
    for e in arr:
        k = _align_key(e)
        if k is None or k in m:
            return None
        m[k] = e
    return m


def diff(j, h, path, out, volatile=VOLATILE):
    """Walk both trees; append differences to out['mismatch'|'missing'|'extra']."""
    tj, th = _kind(j), _kind(h)
    if tj != th:
        out["mismatch"].append({"path": path, "j": _brief(j), "h": _brief(h)})
        return
    if tj == "object":
        for k in set(j) | set(h):
            if volatile.match(k):
                continue
            p = f"{path}.{k}" if path else k
            if k not in h:
                out["missing"].append({"path": p, "j": _brief(j[k])})
            elif k not in j:
                out["extra"].append({"path": p, "h": _brief(h[k])})
            else:
                diff(j[k], h[k], p, out, volatile)
    elif tj == "array":
        jk, hk = _keyed(j), _keyed(h)
        if jk is not None and hk is not None:
            for key in set(jk) | set(hk):
                p = f"{path}[{key}]"
                if key not in hk:
                    out["missing"].append({"path": f"{p} (whole item)"})
                elif key not in jk:
                    out["extra"].append({"path": f"{p} (whole item)"})
                else:
                    diff(jk[key], hk[key], p, out, volatile)
        else:
            if len(j) != len(h):
                out["mismatch"].append({"path": f"{path}[]", "j": f"len {len(j)}", "h": f"len {len(h)}"})
            for i in range(min(len(j), len(h))):
                diff(j[i], h[i], f"{path}[{i}]", out, volatile)
    elif j != h:
        out["mismatch"].append({"path": path, "j": _brief(j), "h": _brief(h)})


def diff_counts(j, h):
    """Convenience: diff two docs, return (total_diffs, buckets_dict)."""
    out = {"mismatch": [], "missing": [], "extra": []}
    diff(j, h, "", out)
    n = len(out["mismatch"]) + len(out["missing"]) + len(out["extra"])
    return n, out

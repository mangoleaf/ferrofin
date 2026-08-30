#!/usr/bin/env python3
"""Phase 0 of the parity-verification plan: generate the per-operation parity ledger.

Enumerates every operation in the vendored Jellyfin OpenAPI contract
(all 412 ops) and emits one ledger row each into `parity/ledger.json`, then
renders the human dashboard `parity/LEDGER.md` with the headline number.

Signals wired now (Phase 0):
  route    registered / 501-stub   <- ferrofin-api handlers::REAL_ROUTES
  depth    REAL / PARTIAL / HOLLOW / STUB   <- REAL_ROUTES + optional classify.tsv overlay
  deep_verified / classification / last_verified   <- parity/seed.json (fix-loop results)
  verification_method   <- the layer that produced the row (see parity/verification.py)

`deep_verified` alone is not the headline: a row is counted as DEEP-VERIFIED only
when its `verification_method` is "body-diff", i.e. the response (or a write's
read-back) was itself diffed clean. Every other row says which WEAKER thing it
established — named properties agreed, a write's effect was confirmed, only the
status class matched, or both servers were empty — and is counted and rendered in
its own section. A reader must be able to tell them apart without opening a probe.

There is NO DEFAULT method. A row that carries a verdict without declaring how it
was earned fails `--check`, and so does a method outside the closed set in
`parity/verification.py`. A default is exactly how a weak probe borrows a strong
headline: batch A2 found a 15-boolean probe recording `deep_verified` for rows
whose bodies were known not to diff clean.

Columns left null are untested and are filled by later layers (contract sweep =
status_conformant/schema_valid; differential replay = deep_verified at scale).

Run from the repo root:
  parity/gen-ledger.py                 # emit ledger.json + LEDGER.md
  parity/gen-ledger.py classify.tsv    # overlay HOLLOW/PARTIAL depth from an audit
"""
import glob
import json
import os
import re
import sys
from collections import Counter, defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import classification  # noqa: E402  — the closed set of classification categories
import verification  # noqa: E402  — the closed set of verification methods

METHODS = ("get", "post", "put", "delete", "patch", "head")
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def norm(method, path):
    """Param-name-agnostic key matching REAL_ROUTES/seed to spec paths.

    Mirrors the router's `to_axum_path`: any segment containing a `{placeholder}`
    collapses to `{}`; literal-only segments are kept verbatim. Copied from the
    api-status skill's scan.py (the origin of this normalization).
    """
    segments = ["{}" if "{" in seg else seg for seg in path.split("/")]
    return (method.lower(), "/".join(segments))


def load_spec():
    files = sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))
    if not files:
        sys.exit("no contracts/jellyfin-openapi-*.json")
    return json.load(open(files[-1]))


def load_real_routes():
    mod = open(os.path.join(ROOT, "crates/ferrofin-api/src/handlers/mod.rs")).read()
    blk = mod[mod.index("pub const REAL_ROUTES"):]
    blk = blk[: blk.index("\n];")]
    pairs = re.findall(r'\(\s*"(\w+)"\s*,\s*"([^"]+)"\s*,?\s*\)', blk)
    return set(norm(m, p) for m, p in pairs)


def load_extension_routes():
    """{normalized route: extension-id} from the EXTENSION_ROUTES const —
    the machine-readable ownership manifest next to REAL_ROUTES (compile-time
    asserted to be a subset of it). Routes not listed are core."""
    mod = open(os.path.join(ROOT, "crates/ferrofin-api/src/handlers/mod.rs")).read()
    try:
        blk = mod[mod.index("pub const EXTENSION_ROUTES"):]
    except ValueError:
        return {}
    blk = blk[: blk.index("\n];")]
    triples = re.findall(r'\(\s*"(\w+)"\s*,\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,?\s*\)', blk)
    return {norm(m, p): ext for m, p, ext in triples}


def load_overlay(path):
    overlay = {}
    if path:
        for line in open(path):
            line = line.rstrip("\n")
            if line and not line.startswith("#"):
                state, method, p = line.split("\t")
                overlay[norm(method, p)] = state
    return overlay


def load_seed():
    seed_path = os.path.join(ROOT, "suite/parity/seed.json")
    if not os.path.exists(seed_path):
        return {}, None
    seed = json.load(open(seed_path))
    by_key = {}
    for row in seed["rows"]:
        method, _, p = row["operation"].partition(" ")
        by_key[norm(method, p)] = row
    return by_key, seed.get("last_verified")


def load_layer2(filename):
    """Layer-2 results (write journeys or read diffs) — feed deep_verified/classification."""
    path = os.path.join(ROOT, filename)
    if not os.path.exists(path):
        return {}, None
    data = json.load(open(path))
    by_key = {norm(*op.split(" ", 1)): r for op, r in data["rows"].items()}
    return by_key, data.get("last_verified") or None


def load_sweep():
    path = os.path.join(ROOT, "suite/parity/sweep-results.json")
    if not os.path.exists(path):
        return {}
    rows = json.load(open(path))["rows"]
    return {norm(*op.split(" ", 1)): r for op, r in rows.items()}


def build_rows(spec, real, overlay, curated, sweep, owners):
    rows = []
    for path, item in spec["paths"].items():
        for method, op in item.items():
            if method not in METHODS:
                continue
            k = norm(method, path)
            depth = overlay.get(k) or ("REAL" if k in real else "STUB")
            s = curated.get(k, {})
            sw = sweep.get(k, {})
            rows.append({
                "operation": f"{method.upper()} {path}",
                "tag": (op.get("tags") or ["_untagged"])[0],
                "owner": owners.get(k, "core"),
                "route": "registered" if k in real else "501-stub",
                "depth": depth,
                "status_conformant": sw.get("status_conformant"),
                "schema_valid": sw.get("schema_valid"),
                "note": sw.get("note", ""),
                "deep_verified": s.get("deep_verified"),
                # HOW the row was verified, verbatim from the layer that produced
                # it — never defaulted. `check()` rejects a verdict with no method,
                # a method outside the closed set, or a method on a row that has no
                # verdict. See parity/verification.py for what each name claims.
                "verification_method": s.get("verification_method"),
                "classification": s.get("classification", ""),
                # WHAT the divergence is, from the closed set in
                # parity/classification.py. It picks the row's section and its
                # tally; the prose does not. Only the curated overlay sets it —
                # a live layer's own "flagged:" text is recognised by
                # `classification.bucket` instead.
                "category": s.get("category"),
                "last_verified": s.get("last_verified") if s else None,
            })
    rows.sort(key=lambda r: (r["operation"].split(" ", 1)[1], r["operation"]))
    return rows


def is_headline(r):
    """Counted in the ledger's deep-verified number: the response itself diffed clean."""
    return r["deep_verified"] is True and r.get("verification_method") == verification.HEADLINE


def verified_by(r, method):
    return r["deep_verified"] is True and r.get("verification_method") == method


def bucket_of(r):
    """The classification bucket a row renders in, or `None`.

    A row that is deep-verified carries its classification as a NOTE on its own
    verified line (see `listing`), not as a standing divergence — so it is in no
    bucket. That is unchanged; what changed is that a row which is NOT verified
    is bucketed by its declared `category` and never by prose-prefixing.
    """
    if r["deep_verified"] is True:
        return None
    return classification.bucket(r.get("category"), r["classification"])


def in_bucket(r, name):
    """Whether a row renders in exactly this bucket.

    The predicates this replaced were `is_open_work` / `is_flagged` /
    `is_accepted`, and the last of them was defined as "has a classification and
    is neither of the other two" — a DEFAULT into the section whose heading says
    a human closed the question. That default swept an unreviewed detector flag,
    an op no probe has ever run, and a measured lab-state artefact into the
    accepted tally. A row now belongs to exactly one declared bucket, or to
    none, and `check()` rejects a category that names no bucket.
    """
    return bucket_of(r) == name


def render_md(rows):
    total = len(rows)
    # The headline number means exactly what its heading says: the response (or
    # the write's read-back) was diffed clean, field for field. Rows verified some
    # OTHER way are real verification and a weaker claim, so each method is counted
    # and listed on its own — folding any of them in would redefine the number
    # instead of earning it.
    deep = sum(1 for r in rows if is_headline(r))
    by_method = Counter(r["verification_method"] for r in rows if r["deep_verified"] is True)
    other = sum(n for m, n in by_method.items() if m != verification.HEADLINE)
    buckets = Counter(b for b in (bucket_of(r) for r in rows) if b)
    accepted = buckets[classification.SETTLED]
    untested = sum(1 for r in rows if r["deep_verified"] is None and not r["classification"])
    depth_counts = defaultdict(int)
    route_counts = defaultdict(int)
    for r in rows:
        depth_counts[r["depth"]] += 1
        route_counts[r["route"]] += 1

    pct = lambda n: f"{100 * n // total}%"
    out = []
    out.append("# Ferrofin ⇄ Jellyfin parity ledger\n")
    out.append("_Generated by `parity/gen-ledger.py` — do not hand-edit; edit `parity/seed.json` "
               "or the classify overlay and regenerate._\n")
    sc_yes = sum(1 for r in rows if r["status_conformant"] is True)
    sc_run = sum(1 for r in rows if r["status_conformant"] is not None)
    sv_yes = sum(1 for r in rows if r["schema_valid"] is True)
    sv_run = sum(1 for r in rows if r["schema_valid"] is not None)
    layer1 = (f"Layer 1: {sc_yes}/{sc_run} status-conformant · {sv_yes}/{sv_run} schema-valid"
              if sc_run or sv_run else "status-conformance + schema-validation not yet run — Layer 1")
    not_accepted = " · ".join(
        f"{buckets[b]} {classification.BUCKETS[b][1].split(' (')[0].lower()}"
        for b in classification.BUCKETS
        if b != classification.SETTLED and buckets[b])
    out.append(f"**{deep}/{total} deep-verified · {other} verified another way · "
               f"{accepted} accepted divergence · {not_accepted or 'nothing else classified'} · "
               f"{untested} untested**  \n_{layer1}_\n")
    out.append("_deep-verified means exactly one thing: "
               f"{verification.METHODS[verification.HEADLINE][2]}. "
               "It is the ONLY method counted in that number. Every other verified row "
               "declares which weaker thing it established — see the table below — and no "
               "row may reach this ledger without declaring a method "
               "(`gen-ledger.py --check` rejects it)._\n")

    out.append("## How each row was verified\n")
    out.append("_One closed set, defined in `parity/verification.py`. A row's method is the "
               "claim it earns; nothing is inferred and nothing defaults._\n")
    out.append("| | method | ops | what was actually compared |")
    out.append("|---|---|---:|---|")
    for m, (glyph, label, meaning) in verification.METHODS.items():
        head = " **(the headline)**" if m == verification.HEADLINE else ""
        out.append(f"| {glyph} | `{m}` — {label}{head} | {by_method.get(m, 0)} | {meaning} |")
    out.append(f"| · | _untested_ | {untested} | no probe produced a verdict — including "
               "probes that ran and compared nothing, which is an absence of evidence, "
               "not a pass |")
    out.append("")

    out.append("## Depth (what the wired handler actually does)\n")
    out.append("| depth | ops | % |")
    out.append("|---|---:|---:|")
    for k in ("REAL", "PARTIAL", "HOLLOW", "STUB"):
        out.append(f"| {k} | {depth_counts[k]} | {pct(depth_counts[k])} |")
    out.append(f"| **route registered** | {route_counts['registered']} | {pct(route_counts['registered'])} |")
    out.append(f"| **route 501-stub** | {route_counts['501-stub']} | {pct(route_counts['501-stub'])} |")
    out.append("")
    out.append("## Ownership (core vs compiled-in extensions)\n")
    out.append("_Extensions must not dilute or flatter core's coverage number — "
               "each owner's deep-verified share stands alone._\n")
    bucket_cols = list(classification.BUCKETS)
    header = " | ".join(classification.BUCKETS[b][1].split(" (")[0].lower() for b in bucket_cols)
    out.append(f"| owner | ops | deep-verified | verified another way | {header} | untested |")
    out.append("|---|---:|---:|" + "---:|" * (len(bucket_cols) + 2))
    by_owner = defaultdict(list)
    for r in rows:
        by_owner[r["owner"]].append(r)
    for owner in sorted(by_owner, key=lambda o: (o != "core", o)):
        rs = by_owner[owner]
        d = sum(1 for r in rs if is_headline(r))
        o = sum(1 for r in rs if r["deep_verified"] is True and not is_headline(r))
        owned = Counter(b for b in (bucket_of(r) for r in rs) if b)
        u = sum(1 for r in rs if r["deep_verified"] is None and not r["classification"])
        cells = " | ".join(str(owned[b]) for b in bucket_cols)
        out.append(f"| {owner} | {len(rs)} | {d} ({100 * d // len(rs)}%) | {o} | {cells} | {u} |")
    out.append("")

    def listing(method):
        glyph = verification.METHODS[method][0]
        for r in rows:
            if verified_by(r, method):
                # A green row can still carry a recorded divergence (e.g. a probe
                # that is only meaningful at a pinned limit, or a candidate universe
                # that is empty on both servers). Show it here — dropping the note
                # is how a real difference becomes invisible.
                note = f" — {r['classification']}" if r["classification"] not in ("", "ok") else ""
                out.append(f"- {glyph} `{r['operation']}`{note}")
        out.append("")

    out.append("## Deep-verified (response + read-back diffed clean vs Jellyfin 10.11.8)\n")
    out.append("_The headline. Nothing else on this page is counted in it._\n")
    listing(verification.HEADLINE)
    for m, (glyph, label, meaning) in verification.METHODS.items():
        if m == verification.HEADLINE:
            continue
        out.append(f"## {label.capitalize()} — {glyph} `{m}` (NOT deep-verified)\n")
        out.append(f"_{meaning[0].upper()}{meaning[1:]}. A real verification and a weaker "
                   "one: never counted in the deep-verified number._\n")
        listing(m)

    # One section per bucket, in the order parity/classification.py declares
    # them, so a reader can tell a closed question from an untested op from a
    # named gap without opening a probe. Only the first heading claims a
    # decision; the rest say in their own titles that they are not one.
    for name, (glyph, heading, meaning) in classification.BUCKETS.items():
        out.append(f"## {heading}\n")
        out.append(f"_{meaning}_\n")
        listed = [r for r in rows if in_bucket(r, name)]
        for r in listed:
            out.append(f"- {glyph} `{r['operation']}` — {r['classification']}")
        if not listed:
            out.append("_(none)_")
        out.append("")
    out.append("## Full ledger\n")
    out.append("_status/schema: ✅ pass · ⚠️ fail · · untested_  \n")
    legend = " · ".join(f"{g} {m}" for m, (g, _l, _d) in verification.METHODS.items())
    out.append(f"_verified: {legend} · ⚠️ fail · · untested. Only ✅ is deep-verified._\n")
    out.append("| operation | route | depth | status | schema | verified | classification |")
    out.append("|---|---|---|---|---|---|---|")
    mark = {True: "✅", False: "⚠️", None: "·"}
    for r in rows:
        vm = r["verification_method"]
        deep_mark = (verification.METHODS[vm][0] if r["deep_verified"] is True and vm
                     else mark[r["deep_verified"]])
        out.append(f"| `{r['operation']}` | {r['route']} | {r['depth']} | "
                   f"{mark[r['status_conformant']]} | {mark[r['schema_valid']]} | "
                   f"{deep_mark} | {r['classification']} |")
    out.append("")
    return "\n".join(out)


def build_curated():
    """Merge curated deep-verification: seed.json (reads) + journey-results.json (writes),
    each row carrying its own last_verified stamp."""
    seed, seed_stamp = load_seed()
    reads, r_stamp = load_layer2("suite/parity/reads-results.json")
    journeys, j_stamp = load_layer2("suite/parity/journey-results.json")
    accepted, a_stamp = load_layer2("suite/parity/classifications.json")
    sweep = load_sweep()
    sweep_stamp = None
    sp = os.path.join(ROOT, "suite/parity/sweep-results.json")
    if os.path.exists(sp):
        sweep_stamp = json.load(open(sp)).get("last_verified")
    # Precedence: static seed < sweep single-item diff < live curated read diff < write journeys
    # < terminal phase < asset layer < stream layer < curated accepted classifications
    # (later, more authoritative wins).
    curated = {k: {**v, "last_verified": seed_stamp} for k, v in seed.items()}
    for k, v in sweep.items():
        if "deep_verified" in v:   # only GET 200/200 ops the sweep deep-diffed
            # Spread the row, exactly like every other layer below: rebuilding a
            # fresh literal here DROPPED the sweep's `verification_method`, so this
            # layer's 55 headline rows were the one set whose stamp could never
            # reach the ledger — they defaulted into the body-diff count instead.
            curated[k] = {**v, "last_verified": sweep_stamp}
    for k, v in reads.items():
        if v.get("deep_verified") is not None or v.get("classification"):
            curated[k] = {**v, "last_verified": r_stamp}
    for k, v in journeys.items():
        curated[k] = {**v, "last_verified": j_stamp}
    # The terminal phase (restore / restart / shutdown): lifecycle effects observed on both
    # servers, the same effect-verdict shape as the write journeys.
    terminal, t_stamp = load_layer2("suite/parity/terminal-results.json")
    for k, v in terminal.items():
        curated[k] = {**v, "last_verified": t_stamp}
    # Layer-3 binary/asset differential (image/font/css): a live verdict for the ops that
    # return non-JSON bodies, applied like the other live layers so a curated
    # accepted-divergence classification can still override its auto-flag below. Most of
    # these rows stamp `verification_method: "property"` themselves — two different image
    # encoders cannot produce identical bytes, so the bar is the DECLARED properties
    # (status, media type, decoded container, dimensions) and the row says so rather than
    # borrowing the body-diff headline. Only the file family (same hardlinked fixture,
    # compared by sha256) claims "body-diff".
    assets, as_stamp = load_layer2("suite/parity/asset-results.json")
    for k, v in assets.items():
        curated[k] = {**v, "last_verified": as_stamp}
    # The stream-signature layer (direct play / HLS / subtitles / trickplay): same shape.
    streams, st_stamp = load_layer2("suite/parity/stream-results.json")
    for k, v in streams.items():
        curated[k] = {**v, "last_verified": st_stamp}
    # Curated accepted-divergence classifications win the classification field over the auto
    # "flagged: verify" text (human decision > detector). deep_verified stays as the live layer
    # reported it (these diverge by design and are not deep-verified); a row not otherwise present
    # is created as a classified (non-verified) divergence so it counts, not "untested".
    for k, v in accepted.items():
        row = curated.get(k, {"deep_verified": None})
        row["classification"] = v["classification"]
        # The category travels with the text it belongs to. Dropping it here is
        # what left `category` a field nothing read.
        row["category"] = v.get("category")
        row["last_verified"] = a_stamp
        curated[k] = row
    return curated


def _guards_fire():
    """Prove the --check guards actually reject, on synthetic rows.

    A guard that has never been seen to fail is a guard nobody has tested. These
    three shapes are exactly what used to slip into the headline silently.
    """
    base = {"operation": "GET /x", "deep_verified": True, "classification": "",
            "verification_method": None, "category": None}
    def rejected(row):
        try:
            check([{**base, **row}], {})
        except AssertionError:
            return True
        except Exception:
            return True
        return False
    assert rejected({}), "a verdict with no method must be rejected"
    assert rejected({"verification_method": "hand-wave"}), "an unknown method must be rejected"
    assert rejected({"deep_verified": None, "verification_method": "body-diff"}), \
        "a method with no verdict must be rejected"
    # The classification guards, same shape: a category outside the closed set
    # renders in NO section (a silent drop), and a curated row with prose but no
    # category is the exact defect this replaced — it used to land in the
    # accepted tally by saying nothing.
    assert rejected({"verification_method": "body-diff", "category": "made-up"}), \
        "an unknown classification category must be rejected"
    try:
        check([{**base, "verification_method": "body-diff"}],
              {("get", "/x"): {"classification": "something", "category": None}})
    except AssertionError:
        pass
    else:
        raise AssertionError("a curated row with no category must be rejected")
    # …and the positive control: a declared category passes.
    check([{**base, "verification_method": "body-diff", "category": "instance"}],
          {("get", "/x"): {"classification": "instance: x", "category": "instance"}})


def check(rows, curated):
    """Self-check: the ledger must cover 412 ops and every curated row must match one
    (a typo'd path silently drops its classification otherwise)."""
    if len(rows) != 412:
        # _guards_fire() calls back in with a synthetic single row; skip the
        # coverage assertion for it and let the method guards below do the work.
        assert len(rows) == 1, f"expected 412 ops, got {len(rows)}"
    keys = {norm(*r["operation"].split(" ", 1)) for r in rows}
    unmatched = [k for k in curated if k not in keys]
    assert not unmatched, f"curated rows match no spec op: {unmatched}"
    # Every row that carries a VERDICT must declare HOW it was earned, from the
    # closed set in parity/verification.py. There is no default: a missing method
    # used to fall straight into the headline deep-verified count, which is a
    # stated assurance nobody earned. And a row with NO verdict must carry no
    # method either — a method there asserts a comparison that never ran.
    unstamped = [r["operation"] for r in rows
                 if r["deep_verified"] is not None and not r["verification_method"]]
    assert not unstamped, (f"{len(unstamped)} row(s) carry a verdict with no "
                           f"verification_method: {unstamped[:10]}")
    unknown = [(r["operation"], r["verification_method"]) for r in rows
               if r["verification_method"] and r["verification_method"] not in verification.VALID]
    assert not unknown, f"verification_method outside {sorted(verification.VALID)}: {unknown[:10]}"
    phantom = [r["operation"] for r in rows
               if r["deep_verified"] is None and r["verification_method"]]
    assert not phantom, (f"{len(phantom)} row(s) declare a verification_method with no "
                         f"verdict — nothing was compared: {phantom[:10]}")
    # A row buckets in exactly one place, and only the `accepted` bucket claims
    # a human closed the question. The guard is now structural — `bucket_of`
    # returns ONE bucket — so what is left to check is that every curated
    # category is one this ledger knows how to render. An unknown category used
    # to be indistinguishable from an accepted divergence; now it renders
    # nowhere, which is a silent DROP, so it must fail loudly instead.
    unknown_cat = [(r["operation"], r["category"]) for r in rows
                   if r.get("category") and r["category"] not in classification.VALID]
    assert not unknown_cat, (f"classification category outside "
                             f"{sorted(classification.VALID)}: {unknown_cat[:10]}")
    # …and that a curated row declares one at all. Without this, a hand-added
    # classifications.json entry with prose but no `category` buckets nowhere
    # and vanishes from every section and every tally.
    uncategorised = [op for op, v in curated.items()
                     if v.get("classification") and not v.get("category")
                     and not v["classification"].startswith(("flagged", "ok"))]
    assert not uncategorised, (f"{len(uncategorised)} curated row(s) carry a classification "
                               f"with no category: {uncategorised[:10]}")
    by = Counter(r["verification_method"] for r in rows if r["deep_verified"] is True)
    other = ", ".join(f"{n} {m}" for m, n in sorted(by.items()) if m != verification.HEADLINE)
    print(f"ok: {len(rows)} ops, all {len(curated)} curated rows matched, "
          f"{by[verification.HEADLINE]} deep-verified (the headline), "
          f"verified another way: {other or 'none'}")


def main():
    spec = load_spec()
    real = load_real_routes()
    curated = build_curated()
    sweep = load_sweep()
    owners = load_extension_routes()

    if "--check" in sys.argv:
        _guards_fire()
        check(build_rows(spec, real, {}, curated, sweep, owners), curated)
        return

    classify_path = next((a for a in sys.argv[1:] if not a.startswith("--")), None)
    overlay = load_overlay(classify_path)
    rows = build_rows(spec, real, overlay, curated, sweep, owners)

    with open(os.path.join(ROOT, "suite/parity/ledger.json"), "w") as f:
        json.dump({"operations": rows}, f, indent=2)
        f.write("\n")
    with open(os.path.join(ROOT, "suite/parity/LEDGER.md"), "w") as f:
        f.write(render_md(rows))

    deep = sum(1 for r in rows if is_headline(r))
    by = Counter(r["verification_method"] for r in rows if r["deep_verified"] is True)
    other = ", ".join(f"{n} {m}" for m, n in sorted(by.items()) if m != verification.HEADLINE)
    print(f"wrote parity/ledger.json + parity/LEDGER.md — {len(rows)} ops, "
          f"{deep} deep-verified (bodies diffed); verified another way: {other or 'none'}")


if __name__ == "__main__":
    main()

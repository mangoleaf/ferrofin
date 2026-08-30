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
                # The note of the layer that OWNS the row (curated precedence),
                # falling back to the sweep's. It used to be sweep-only, so a
                # reads-owned row's evidence ("984 field(s) compared") never
                # reached the ledger at all.
                "note": s.get("note") or sw.get("note", ""),
                "deep_verified": s.get("deep_verified"),
                # HOW the row was verified, verbatim from the layer that produced
                # it — never defaulted. `check()` rejects a verdict with no method,
                # a method outside the closed set, or a method on a row that has no
                # verdict. See parity/verification.py for what each name claims.
                "verification_method": s.get("verification_method"),
                "classification": s.get("classification", ""),
                # What the probe did NOT compare, verbatim from the layer. A
                # green row whose probe projected a field away is honest only
                # while the reader can see WHICH field, so these are rendered
                # (see "Verified with a recorded caveat") rather than living in
                # a comment in the probe source.
                "caveats": list(s.get("caveats") or ()),
                "last_verified": s.get("last_verified") if s else None,
            })
    rows.sort(key=lambda r: (r["operation"].split(" ", 1)[1], r["operation"]))
    return rows


def is_headline(r):
    """Counted in the ledger's deep-verified number: the response itself diffed clean."""
    return r["deep_verified"] is True and r.get("verification_method") == verification.HEADLINE


def verified_by(r, method):
    return r["deep_verified"] is True and r.get("verification_method") == method


def is_open_work(r):
    """A named, NOT-accepted divergence — an open work item, not a settled decision.

    These must never render under the "accepted — not a bug" heading: a reader
    scanning that section would take a real gap for a closed question.
    """
    return r["classification"].startswith("open-work")


def is_flagged(r):
    """Auto-generated detector text ("flagged: … (verify)") that NOBODY has reviewed.

    Rendering these under "accepted — not a bug" is the same defect as an unstamped
    row in the deep count, pointed the other way: a detector's guess read as a
    settled decision. They get their own section until a human classifies them.
    """
    return r["classification"].startswith("flagged")


def is_accepted(r):
    return (r["classification"] and r["deep_verified"] is not True
            and not is_open_work(r) and not is_flagged(r))


#: "N field(s) compared" / "N fields compared", however a layer phrases it.
COMPARED_RE = re.compile(r"(\d+) fields?\(?s?\)? compared")


def evidence(r):
    """One phrase saying how MUCH was compared, for the deep-verified listing.

    `✅` says a body was diffed; it does not say whether that body had 984
    comparable leaves or one. `GET /Localization/Cultures` (984) and
    `GET /System/Info/Public` (1 — `Version`, every other field VOLATILE) render
    the same tick, and a reader had to open the probe to tell them apart.
    """
    note = r.get("note") or ""
    m = COMPARED_RE.search(note)
    if m:
        n = int(m.group(1))
        return f"{n} field{'' if n == 1 else 's'} compared"
    if "'file'" in note or '"file"' in note:
        return "sha256 of the served bytes"
    if "text/vtt" in note or "text/srt" in note or "application/x-subrip" in note:
        return "exact decoded text"
    return ""


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
    accepted = sum(1 for r in rows if is_accepted(r))
    openwork = sum(1 for r in rows if is_open_work(r) and r["deep_verified"] is not True)
    flagged = sum(1 for r in rows if is_flagged(r) and r["deep_verified"] is not True)
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
    out.append(f"**{deep}/{total} deep-verified · {other} verified another way · "
               f"{accepted} classified-divergence · {openwork} open work · "
               f"{flagged} flagged (unreviewed) · {untested} untested**  \n_{layer1}_\n")
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
    out.append("| owner | ops | deep-verified | verified another way | classified | open work "
               "| flagged | untested |")
    out.append("|---|---:|---:|---:|---:|---:|---:|---:|")
    by_owner = defaultdict(list)
    for r in rows:
        by_owner[r["owner"]].append(r)
    for owner in sorted(by_owner, key=lambda o: (o != "core", o)):
        rs = by_owner[owner]
        d = sum(1 for r in rs if is_headline(r))
        o = sum(1 for r in rs if r["deep_verified"] is True and not is_headline(r))
        c = sum(1 for r in rs if is_accepted(r))
        w = sum(1 for r in rs if is_open_work(r) and r["deep_verified"] is not True)
        fl = sum(1 for r in rs if is_flagged(r) and r["deep_verified"] is not True)
        u = sum(1 for r in rs if r["deep_verified"] is None and not r["classification"])
        out.append(f"| {owner} | {len(rs)} | {d} ({100 * d // len(rs)}%) | {o} | {c} | {w} "
                   f"| {fl} | {u} |")
    out.append("")

    def listing(method, show_evidence=False):
        glyph = verification.METHODS[method][0]
        for r in rows:
            if verified_by(r, method):
                ev = f" _({evidence(r)})_" if show_evidence and evidence(r) else ""
                # A green row can still carry a recorded divergence (e.g. a probe
                # that is only meaningful at a pinned limit, or a candidate universe
                # that is empty on both servers). Show it here — dropping the note
                # is how a real difference becomes invisible.
                note = f" — {r['classification']}" if r["classification"] not in ("", "ok") else ""
                out.append(f"- {glyph} `{r['operation']}`{ev}{note}")
        out.append("")

    out.append("## Deep-verified (response + read-back diffed clean vs Jellyfin 10.11.8)\n")
    out.append("_The headline. Nothing else on this page is counted in it._  \n"
               "_The parenthesis is HOW MUCH was compared — the number of non-volatile "
               "leaves the diff actually walked. A clean diff of one leaf and a clean diff "
               "of 984 are both real and they are not the same evidence, so the page says "
               "which it was._\n")
    listing(verification.HEADLINE, show_evidence=True)
    for m, (glyph, label, meaning) in verification.METHODS.items():
        if m == verification.HEADLINE:
            continue
        out.append(f"## {label.capitalize()} — {glyph} `{m}` (NOT deep-verified)\n")
        out.append(f"_{meaning[0].upper()}{meaning[1:]}. A real verification and a weaker "
                   "one: never counted in the deep-verified number._\n")
        listing(m)

    out.append("## Classified divergence (accepted — not a bug)\n")
    out.append("_Settled decisions. An item here is a question that has been closed._\n")
    for r in rows:
        if is_accepted(r):
            out.append(f"- ⚠️ `{r['operation']}` — {r['classification']}")
    out.append("")
    out.append("## Open work (NOT accepted — a named divergence still to port)\n")
    out.append("_These are real gaps with an owner and a path, kept OUT of the accepted "
               "section: rendering an unfinished port under \"accepted\" is how a gap "
               "quietly becomes a decision._\n")
    for r in rows:
        if is_open_work(r) and r["deep_verified"] is not True:
            out.append(f"- ⛏ `{r['operation']}` — {r['classification']}")
    out.append("")
    out.append("## Flagged by a detector — UNREVIEWED (not accepted, not verified)\n")
    out.append("_A probe recorded a divergence and wrote its own text. Nobody has looked at "
               "these yet: they are neither an accepted divergence nor a named work item. "
               "They sat under \"accepted — not a bug\" until this section existed._\n")
    for r in rows:
        if is_flagged(r) and r["deep_verified"] is not True:
            out.append(f"- 🔍 `{r['operation']}` — {r['classification']}")
    out.append("")

    # ---- green rows that still carry a divergence or a projection -----------
    #
    # A row can be `deep_verified` AND still hide something: a curated
    # classification explaining a real behavioural difference, or a probe that
    # projected a field out of the comparison. Both used to be visible only in
    # the one-line `classification` cell of the 412-row table, which nobody
    # scans. Rule: never delete the only record of a real divergence — so when
    # the row goes green, the record moves HERE rather than disappearing.
    caveated = [r for r in rows
                if r["deep_verified"] is True and (r["caveats"] or r["classification"]
                                                   not in ("", "ok"))]
    out.append("## Verified WITH a recorded caveat\n")
    out.append("_These rows diffed clean, and something about them is still worth "
               "knowing: a field the probe did not compare, or an accepted divergence "
               "underneath a green verdict. A green row is not the same as a row with "
               "nothing left to say._\n")
    if not caveated:
        out.append("_(none)_\n")
    for r in caveated:
        glyph = verification.METHODS[r["verification_method"]][0]
        out.append(f"- {glyph} `{r['operation']}`")
        if r["classification"] not in ("", "ok"):
            out.append(f"  - classification: {r['classification']}")
        for c in r["caveats"]:
            out.append(f"  - NOT compared: {c}")
    out.append("")
    out.append("## Full ledger\n")
    out.append("_status/schema: ✅ pass · ⚠️ fail · · untested_  \n")
    legend = " · ".join(f"{g} {m}" for m, (g, _l, _d) in verification.METHODS.items())
    out.append(f"_verified: {legend} · ⚠️ fail · · untested · ⛔ a verdict with no declared "
               "method (rejected before this file is written; it must never appear). "
               "Only ✅ is deep-verified._\n")
    out.append("| operation | route | depth | status | schema | verified | classification |")
    out.append("|---|---|---|---|---|---|---|")
    mark = {True: "✅", False: "⚠️", None: "·"}
    for r in rows:
        vm = r["verification_method"]
        if r["deep_verified"] is True:
            # An unstamped verdict must NEVER borrow the body-diff tick. `check()`
            # runs before anything is written, so this branch is unreachable — it
            # renders a refusal rather than a claim if it ever is reached.
            deep_mark = verification.METHODS[vm][0] if vm else "⛔"
        else:
            deep_mark = mark[r["deep_verified"]]
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
        row["last_verified"] = a_stamp
        curated[k] = row
    # NB the overlay above SPREADS onto the live row, so a live layer's
    # `caveats` survive a curated classification landing on the same op. They
    # are different statements: the classification says why a divergence is
    # accepted, the caveats say what the probe declined to compare.
    return curated


def _guards_fire():
    """Prove the --check guards actually reject, on synthetic rows.

    A guard that has never been seen to fail is a guard nobody has tested. These
    three shapes are exactly what used to slip into the headline silently.
    """
    base = {"operation": "GET /x", "deep_verified": True, "classification": "",
            "verification_method": None, "caveats": []}
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
    # An open work item, and an unreviewed detector flag, must never render under
    # "accepted — not a bug" — that heading is a claim about a HUMAN decision.
    assert not [r for r in rows if is_open_work(r) and is_accepted(r)]
    assert not [r for r in rows if is_flagged(r) and is_accepted(r)]
    # A caveat is a claim about a row's evidence, so it must belong to a row
    # that HAS a verdict — a caveat on an untested row would read as "we
    # verified this, except…" about a probe that never ran.
    orphan = [r["operation"] for r in rows if r["caveats"] and r["deep_verified"] is None]
    assert not orphan, f"caveats on rows with no verdict: {orphan[:10]}"
    by = Counter(r["verification_method"] for r in rows if r["deep_verified"] is True)
    other = ", ".join(f"{n} {m}" for m, n in sorted(by.items(), key=lambda kv: kv[0] or "")
                      if m != verification.HEADLINE)
    print(f"ok: {len(rows)} ops, all {len(curated)} curated rows matched, "
          f"{by[verification.HEADLINE]} deep-verified (the headline), "
          f"verified another way: {other or 'none'}")


def main():
    spec = load_spec()
    real = load_real_routes()
    curated = build_curated()
    sweep = load_sweep()
    owners = load_extension_routes()

    # The rule is enforced on EVERY generation, not only when someone types
    # --check. It used to be check-on-request: `sweep.sh` ran bare `gen-ledger.py`,
    # no CI job or bats test ran `--check`, and an unstamped verdict row was
    # written to ledger.json AND LEDGER.md complete — rendering the body-diff tick
    # and absorbed into "verified another way" — before the process happened to
    # die on an unrelated sort. A rule enforced only when a human asks for it is
    # the same defect this file exists to close.
    _guards_fire()
    check(build_rows(spec, real, {}, curated, sweep, owners), curated)
    if "--check" in sys.argv:
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
    other = ", ".join(f"{n} {m}" for m, n in sorted(by.items(), key=lambda kv: kv[0] or "")
                      if m != verification.HEADLINE)
    print(f"wrote parity/ledger.json + parity/LEDGER.md — {len(rows)} ops, "
          f"{deep} deep-verified (bodies diffed); verified another way: {other or 'none'}")


if __name__ == "__main__":
    main()

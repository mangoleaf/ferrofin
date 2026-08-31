#!/usr/bin/env python3
"""Layer-2 read depth with id-correlation (Phase 1 task 6).

Independent scans give the same media file different GUIDs on each server, so a
naive item-scoped read diff compares different titles (the fix-loop's unresolved
item_detail/item_similar noise). This engine aligns items across servers by Path
(identical — both mount the same media), then for each item-scoped endpoint issues
the request with EACH server's own id and deep-diffs the responses with the
volatile denylist. Extends the read set well beyond the seeded 30.

Emits parity/reads-results.json (deep_verified per read op); gen-ledger.py ingests
it, superseding the static seed for the ops it re-verifies live.

Run via sweep.sh (idempotently connects to the already-up servers), or directly:
  FERROFIN_URL=... JELLYFIN_URL=... parity/reads.py
Offline self-check:
  parity/reads.py --check
"""
import collections
import datetime
import json
import os
import re
import string
import urllib.parse
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up          # noqa: E402
from parity_diff import diff_stats                  # noqa: E402
import verification                                  # noqa: E402

CORRELATE_LIMIT = 5   # item-scoped endpoints are exercised against this many Path-aligned items

#: musicbrainz.org rate-limits at ~1 request/second per IP, and both containers
#: plus this harness share the lab's egress address. A 503 comes back as an
#: empty result list, so the MusicBrainz-backed `posted` legs are paced and
#: retried rather than reported as a divergence (or, worse, as agreement).
MB_PACE = 2.5
MB_RETRIES = 4


def token_get(base, path, token, method="GET"):
    # `method` exists for the handful of contract READS upstream models as a
    # POST — `POST /Plugins/{pluginId}/Manifest` is `GetPluginManifest`, a pure
    # read with a POST verb. Diffing those bodies is the same job; only the verb
    # differs, and hardcoding GET is what kept that row on a hand-written
    # "no shared plugin id" note instead of a measurement.
    st, raw = http(method, base + path, token)
    if st != 200 or not raw:
        return st, None
    try:
        return st, json.loads(raw)
    except ValueError:
        return st, None

def token_post(base, path, token, body):
    """POST `body` as JSON; returns `(status, parsed body or None)`."""
    st, raw = http("POST", base + path, token, json.dumps(body))
    if st != 200 or not raw:
        return st, None
    try:
        return st, json.loads(raw)
    except ValueError:
        return st, None

# ---------------------------------------------------------------- endpoint set

def plain(op, url):
    return {"op": op, "kind": "plain", "url": lambda c: url}


def user(op, url, project=None):
    # url may reference {u} (user id) plus resolved per-server context keys (genre/studio/person/
    # year/series/season). By-name values are URL-encoded and identical across servers (same NFO);
    # series/season ids are per-server (same title on both → clean diff).
    #
    # `project(body, ctx)` narrows what is compared, and is allowed ONLY to
    # translate a value that is genuinely per-instance into one that is not —
    # never to drop a field. The one user is /LiveTv/Info's EnabledUsers, which
    # is a list of raw user GUIDs; see there.
    return {"op": op, "kind": "user", "url": lambda c: url.format(**c), "project": project}


def _extra_url(tmpl):
    """One `extra` URL builder: `None` when the context has no seed for it.

    An unseeded placeholder must NOT collapse the template into a different
    URL. `{channel}` with no channels turns
    `/Items/{channel}?userId={user}` into `/Items/?userId=...`, which answers
    200 on BOTH servers with a full item list — the leg would then be counted
    as tested-and-clean while comparing something that is not an item fetch at
    all. Returning None makes the run SKIP it loudly instead (same guard the
    channel-seeded fact in `recordings_group_invariants` already uses).

    A key that is missing from the context entirely is still a KeyError, so the
    self-check keeps catching a typo'd placeholder.
    """
    keys = [f for _, f, _, _ in string.Formatter().parse(tmpl) if f]

    def url_of(c):
        for k in keys:
            if k not in c:
                raise KeyError(k)
        if any(not c[k] for k in keys):
            return None
        return tmpl.format(**c)

    return url_of


def item(op, tmpl, extra_seeds=(), extra=()):
    # tmpl contains {u} and {i}; filled per server (own user + own correlated item id).
    #
    # `extra_seeds` names additional per-server context keys to probe with the
    # SAME template, appended to the Path-correlated media pairs. It exists for
    # seeds a media item can never reach: the playlists folder's ancestor chain
    # is the only one that climbs to the `AggregateFolder` (a movie's stops at
    # the `UserRootFolder`), so without it the whole aggregate-root model is
    # untested by this layer.
    #
    # `extra` adds whole URLs that are NOT id-correlated pairs: each is formatted
    # with that server's own context, so an id which both servers derive
    # identically (a Live TV channel — `GetInternalChannelId` hashes only the
    # tuner's own id) can be pinned on the same row. Both kinds are diffed and
    # recorded exactly like a correlated pair, status divergence included; they
    # are extra legs, never a replacement for the pairs.
    return {"op": op, "kind": "item",
            "url": lambda c, i: tmpl.format(u=c["user"], i=i),
            "extra_seeds": tuple(extra_seeds),
            "extra": [_extra_url(t) for t in extra]}


def multi(op, legs, seed=None, reap=None, method="GET", caveats=None):
    """Several URLs folded into ONE ledger row (rows are keyed by op, so a second
    entry would clobber the first). Every leg is diffed; the buckets are unioned.

    A leg is a URL template, or a `(template, project)` pair whose
    `project(body)` reshapes what is compared. A REDUCTIVE projection — one that
    narrows the comparison — is allowed ONLY where the rest of the body is
    provably non-comparable between two independent instances (the Suggestions
    paging leg is the one case, see there), never to make a divergence
    disappear. An ADDITIVE one (`with_item_order`) needs no such licence: it
    only ever adds comparisons.

    `seed(base, token, ctx)` runs on BOTH servers immediately before this row's
    legs and `reap(base, token, ctx)` immediately after, in a `finally`. Scoping
    the write to the one row it exists for is deliberate: the lyric seeds change
    the seeded track's DTO (Jellyfin gains a `MediaStreams` Lyric entry and flips
    `HasLyrics`), and state held across the whole read set is state that can
    contaminate an unrelated row later."""
    out = []
    for leg in legs:
        tmpl, project = leg if isinstance(leg, tuple) else (leg, None)
        out.append({"tmpl": tmpl,
                    "url": (lambda c, t=tmpl: t.format(**c)),
                    "project": project})
    return {"op": op, "kind": "multi", "legs": out, "seed": seed, "reap": reap,
            "http_method": method, "caveats": list(caveats or ())}


def post_leg(url, body, retry_empty=False, tag=None, requires=None):
    """One leg of a [`posted`] row. `body(ctx)` builds the JSON for THAT server,
    so a leg can carry the server's own item id.

    `retry_empty` marks a leg whose emptiness would be a HARNESS artefact rather
    than an answer — the MusicBrainz-backed searches, where musicbrainz.org
    rate-limits at roughly one request per second per IP and answers 503, which
    both servers turn into `[]`. Such a leg is retried until BOTH sides answer
    non-empty, and if BOTH are still empty it is DROPPED from the row with a
    note, because `[] vs []` compares nothing and must never be reported as
    agreement. A leg whose correct answer IS `[]` (the fetcher-gate legs) must
    leave this false.

    Two deliberate non-behaviours of `retry_empty`, both of which used to
    launder a Ferrofin failure into a green row:
      * it never fires on a NON-200. `token_post` returns `(status, None)` for
        anything but 200-with-JSON, and a falsy body is indistinguishable from
        `[]`; the runner therefore settles the statuses FIRST, so a Ferrofin
        500 against a Jellyfin 200 is a status mismatch, not a drop;
      * it needs BOTH sides empty. One side empty and the other carrying hits,
        after every retry, is a divergence — the rate limiter is shared, but it
        does not answer for the server that did return data, and "one of them
        found nothing" is exactly the shape a broken search has.

    `tag`/`requires` pin a POSITIVE CONTROL to an assertion whose expected
    answer is `[]`. A gate leg asserting "no fetcher may run" is satisfied
    equally by a working gate and by a rate-limited provider; naming the
    control's `tag` in the gated leg's `requires` makes the runner drop the
    gate leg unless the control really did return content on both servers in
    the same pass.
    """
    return {"url": url, "body": body, "retry_empty": retry_empty,
            "tag": tag, "requires": requires}


def post_leg_outcome(leg, hs, hb, js, jb, proven):
    """What a `posted` leg's two responses mean, as ONE word, so the rule is
    testable instead of being an `if`-ladder buried in the runner.

    Order matters, and each step exists because the one below it used to swallow
    a real result:

      `uncontrolled`  the leg asserts `[]` (a fetcher gate) and its positive
                      control did not return content in this pass, so the
                      assertion is unattributable — drop it.
      `status`        the two servers disagreed on the HTTP status. The loudest
                      possible result. Settled BEFORE any emptiness test,
                      because `token_post` reports a Ferrofin 500 as a falsy
                      body and it would otherwise look like "the provider
                      answered empty".
      `unavailable`   both refused identically, or neither returned JSON. Not a
                      divergence and not evidence: the leg compared nothing.
      `rate-limited`  both answered 200 and BOTH came back empty on a leg whose
                      emptiness is a harness artefact. Dropped with a note —
                      `[] vs []` compares nothing and must never read as
                      agreement. One side empty is NOT this: it falls through
                      and diffs, which is a divergence.
      `compare`       diff the two bodies.
    """
    if leg["requires"] and leg["requires"] not in proven:
        return "uncontrolled"
    if hs != js:
        return "status"
    if hs != 200 or hb is None or jb is None:
        return "unavailable"
    if leg["retry_empty"] and not hb and not jb:
        return "rate-limited"
    return "compare"


def leaf_note(per_leg):
    """The " (leaves A+B+C)" fragment a multi-leg row's note carries, or "" when
    the caller kept no per-leg counts.

    Spelled out leg by leg on purpose. A row whose legs are `[14, 5, 0]` has
    three CLEAN legs and two legs of evidence; summing them to 19 and printing
    "3/3 clean" invites the reader to divide, and the leg that compared nothing
    is exactly the one — a fetcher gate asserting `[]` — whose silence must stay
    visible. A row whose every leg compared something reads the same way, so
    there is no special case to remember.
    """
    if not per_leg:
        return ""
    # The clause is only worth carrying when a zero sits NEXT TO real evidence —
    # that is the case where a reader would otherwise credit the empty leg with
    # a share of the total. A row whose every leg compared nothing already says
    # so in `record`'s `empty-corpus` detail, and repeating it there just makes
    # the note longer than the finding.
    hidden_zero = 0 in per_leg and any(per_leg)
    return (f" ({'+'.join(str(c) for c in per_leg)} leaves compared"
            + ("; the 0 is a leg whose ASSERTION is emptiness)" if hidden_zero else ")"))


def posted(op, legs):
    """A POST-shaped READ folded into one ledger row.

    The `RemoteSearch/<Kind>` family is POST by contract but a SEARCH by
    behaviour — v10.11.8 `ItemLookupController` only calls
    `IProviderManager.GetRemoteSearchResults` and returns `Ok`, so nothing is
    mutated and there is no read-back. These rows live here, not in
    `journeys.py`, precisely because there IS no write effect: both servers get
    the byte-identical body and their two RESPONSES are deep-diffed, which is
    the same claim every GET row makes. The method is derived by
    `verification.read_method` from what the diff actually compared, so a family
    with no provider on either side lands `empty-corpus` rather than borrowing
    the headline.
    """
    return {"op": op, "kind": "posted", "legs": legs}


def invariant(op, fn):
    """A row verified by PROPERTIES rather than a body diff, for an endpoint whose
    body genuinely cannot match (see `similar_invariants`). `fn(base, token, ctx)`
    returns a flat dict of named facts; the row is clean when both servers report
    the identical dict AND no fact is False.

    This is a WEAKER claim than the ledger's headline ("response + read-back
    diffed clean"), so the row is stamped `verification_method: "property"` and
    `gen-ledger.py` counts and renders it in its own section. Never let a
    property row into the body-diff count — that would redefine the number
    instead of earning it."""
    return {"op": op, "kind": "invariant", "fn": fn, "method": "property"}


# ------------------------------------------------ display-preferences seeding
#
# The `usersettings?client=emby` GET only ever sees a VIRGIN auto-vivified row,
# so it can catch a wrong creation default and nothing else. Everything the POST
# path normalizes — the skip-length fallbacks, the home-section rebuild with its
# per-order default substitution, the `landing-*` ViewType strip — is invisible
# to it. This writes one deterministic DTO to a dedicated client key on BOTH
# servers before the probe loop, so a second leg can read the result back.
#
# The client key is its own (`parityreads`): journeys.py writes `parity`, and
# nothing orders the two layers.
DISPLAY_PREFS_CLIENT = "parityreads"

DISPLAY_PREFS_SEED = {
    "Id": "usersettings",
    "Client": DISPLAY_PREFS_CLIENT,
    "SortBy": "SortName",
    "SortOrder": "Ascending",
    "RememberIndexing": False,
    "RememberSorting": False,
    "ScrollDirection": "Horizontal",
    "ShowBackdrop": True,
    "ShowSidebar": False,
    "PrimaryImageHeight": 250,
    "PrimaryImageWidth": 250,
    "CustomPrefs": {
        # persisted home sections, incl. an unparseable type that must fall back
        # to `defaults[3]` (ResumeBook) and an order past that 8-entry table.
        "homesection0": "smalllibrarytiles",
        "homesection1": "resume",
        "homesection3": "bogusvalue",
        "homesection9": "alsobogus",
        # a valid ViewType survives, an invalid one is stripped
        "landing-abc": "movies",
        "landing-bad": "notaviewtype",
        # empty value => the C# fallback (30000), not the supplied ""
        "skipForwardLength": "",
        "enableNextVideoInfoOverlay": "false",
    },
}


def seed_display_preferences(base, token, ctx):
    """POSTs [`DISPLAY_PREFS_SEED`]; returns the status so a failure is visible."""
    return http("POST",
                f"{base}/DisplayPreferences/usersettings"
                f"?userId={ctx['u']}&client={DISPLAY_PREFS_CLIENT}",
                token, json.dumps(DISPLAY_PREFS_SEED))[0]


# ------------------------------------------------------------------- lyrics
#
# `GET /Audio/{itemId}/Lyrics` has no corpus of its own: the synthetic tracks
# carry no lyrics, so the row was 404=404 — an agreement that proves nothing
# (an unconditional-404 handler passes it). The fix is the same one
# `seed_display_preferences` uses: write the SAME bytes to both servers, then
# diff what each one parsed them into. A LyricDto is a deterministic function
# of the uploaded file, so this is a real cross-server body diff — it is what
# caught Ferrofin inventing a `Metadata` block, dropping `Cues`, and eating a
# `.txt`'s blank lines. The upload lands in each server's own metadata folder
# (never the read-only media mount) and is reaped after the read set runs.
#
# One seed per parser branch: a synced `.lrc` carrying metadata tags, an
# enhanced `.elrc` with word-level time tags (the cue oracle, shaped like
# upstream's `Fleetwood Mac - Rumors.elrc`), and a plain `.txt` whose blank
# lines and trailing newline must survive.
LYRIC_SEEDS = [
    ("lrc", "[ar:Parity Artist]\n[ti:Parity Title]\n[al:Parity Album]\n"
            "[00:01.00]First line\n[00:05.50]Second line\n[01:02.25]Third line\n"),
    # The enhanced seed deliberately spans every branch of the decoder, because a
    # corpus in which EVERY line starts with a word tag cannot fail on the one
    # class of cue bug this row exists to catch. (It did not: Ferrofin dropped
    # the position-0 cue that leading text owes, and neither the old seed nor
    # upstream's own `Fleetwood Mac - Rumors.elrc` — 31 word-tag-first lines out
    # of 31 — could see it.) The four shapes, in order:
    #   1. word tag first, with whitespace-only segments between the words;
    #   2. TEXT BEFORE the first word tag — `LrcTimedTextUtils.TimedTextToObject`
    #      seeds its tag list with the LINE's start, so this owes a cue at
    #      position 0 carrying `[00:14.69]`;
    #   3. a word tag with nothing after it — an `IndexState.End` index, whose
    #      cue spans the whole line and whose trailing slice emits nothing;
    #   4. TWO line time tags, which `LrcLyricParser.Decode` refuses to attribute
    #      word tags to, so `<00:35.00>` must survive in the text verbatim and
    #      the line must appear twice with no cues at all.
    ("elrc", "[00:06.84] <00:06.84> Every <00:07.20>   <00:07.56> night <00:07.87>   "
             "<00:08.19> that <00:08.46>   <00:08.79> goes <00:09.19>   <00:09.59> between\n"
             "[00:14.69]I feel <00:15.15>a little <00:15.96>less\n"
             "[00:20.00]closing<00:21.00>\n"
             "[00:25.00][00:30.00]two starts <00:35.00>here\n"),
    ("txt", "Plain line one\n\n   indented line\nlast\n"),
]
LYRIC_SEED_WAIT_S = 15   # Jellyfin serves an uploaded lyric only once its queued refresh ran
LYRIC_REAP_ROUNDS = 3    # an item can hold more than one sidecar; Jellyfin resolves one at a time


def lyric_seed_ids(base, token, user_id):
    """The audio items the lyric seeds are written to — the first three by PATH.

    Path is the stable cross-server key (the same reason `path_id_map` uses it),
    so both servers seed the same three tracks and the read legs line up.
    """
    b = get_json(base, f"/Items?userId={user_id}&recursive=true&includeItemTypes=Audio"
                       f"&limit=500&fields=Path", token)
    by_path = {i["Path"]: i["Id"] for i in (b or {}).get("Items") or [] if i.get("Path")}
    ids = [by_path[p] for p in sorted(by_path)[:len(LYRIC_SEEDS)]]
    return ids + [""] * (len(LYRIC_SEEDS) - len(ids))


def lyric_ids(ctx):
    """The three seeded audio ids, in LYRIC_SEEDS order."""
    return [ctx["lyric_lrc"], ctx["lyric_elrc"], ctx["lyric_txt"]]


def lyric_visible(base, token, aid):
    """True once the server serves the uploaded lyric back."""
    return http("GET", f"{base}/Audio/{aid}/Lyrics", token)[0] == 200


def seed_lyrics(base, token, ctx):
    """Uploads one lyric per seed id and waits until each reads back.

    Records the ids that actually landed on `ctx`, so the reap only chases files
    that exist, and returns the upload statuses so a failure is visible rather
    than silently turning the row back into a 404=404.
    """
    statuses = []
    landed = []
    for (ext, body), aid in zip(LYRIC_SEEDS, lyric_ids(ctx)):
        if not aid:
            statuses.append(None)
            continue
        st = http("POST", f"{base}/Audio/{aid}/Lyrics?fileName=parity.{ext}", token, body)[0]
        statuses.append(st)
        if st == 200:
            landed.append(aid)
    for aid in landed:
        for _ in range(LYRIC_SEED_WAIT_S):
            if lyric_visible(base, token, aid):
                break
            time.sleep(1)
    ctx["_lyrics_seeded"] = landed
    return statuses


def reap_lyrics(base, token, ctx):
    """Removes the seeded lyrics again, so the later layers see the fixture as
    they found it.

    Jellyfin's DELETE unlinks only the files it can see as resolved
    `MediaStreamType.Lyric` rows (`LyricManager.DeleteLyricsAsync`), and its GET
    reads those same rows — so a GET that is NOT 200 does not mean the file is
    gone, it equally means the queued refresh has not run yet. Treating that as
    "already reaped" is how this batch's diagnostic phase left lyric files
    inside the Jellyfin container for good. So: wait until the file is VISIBLE
    (only then can DELETE find it), delete it, then wait until it is gone — and
    say so loudly if either wait runs out, because residue on a shared pair is
    asymmetric state that poisons somebody else's measurement.
    """
    def wait_until(aid, visible):
        for _ in range(LYRIC_SEED_WAIT_S):
            if lyric_visible(base, token, aid) == visible:
                return True
            time.sleep(1)
        return False

    left = []
    for aid in ctx.get("_lyrics_seeded", []):
        # Delete in rounds: an item that ended up with more than one sidecar
        # (an aborted earlier run) only exposes one resolved Lyric stream at a
        # time, so one DELETE is not necessarily the last one.
        first = True
        for _ in range(LYRIC_REAP_ROUNDS):
            if not wait_until(aid, True):
                # Nothing (more) visible. On the FIRST round that is the bad
                # case the whole poll exists for: the upload was accepted but
                # the server never served it back, so a file may be sitting on
                # disk that no DELETE can reach.
                if first:
                    left.append(aid)
                break
            first = False
            http("DELETE", f"{base}/Audio/{aid}/Lyrics", token)
            wait_until(aid, False)
        else:
            left.append(aid)
    ctx["_lyrics_seeded"] = []
    if left:
        print(f"  WARNING: {base} still holds seeded lyrics for {left} — they are "
              f"asymmetric state on a shared pair; delete them before the next run",
              file=sys.stderr)
    return left


# ------------------------------------------------- by-name ordering + options
#
# `parity_diff.diff` aligns arrays by `Name` (ALIGN_KEYS), which is what lets the
# by-name rows compare at all across two independent instances — but it also
# makes ORDER invisible. A `sortOrder=Descending` that the server silently
# dropped diffed clean for exactly that reason. These projections keep the WHOLE
# body and ADD the ordered name list under a synthetic key, so the ordering
# becomes a diffable field. They strengthen the comparison; they never narrow it.

def with_item_order(body):
    out = dict(body)
    out["_ItemNameOrder"] = [i.get("Name") for i in body.get("Items") or []]
    return out


# ------------------------------------------------------------------- /Years
#
# `/Years` carries ONE field that cannot be diffed, and it is an upstream bug.
# `YearsController.GetYears` builds its result as
#
#     new QueryResult(startIndex, totalCount == -1 ? ibnItemsArray.Count : totalCount, dtos)
#
# where `totalCount` is the OUT-PARAM of
# `folder.GetRecursiveChildren(user, query, out totalCount)`
# (Folder.cs:1450-1458) — the count of the underlying MEDIA items, not of the
# years. With a user resolved (which `RequestHelpers.GetUserId` always does) that
# branch always runs, so the lab's Jellyfin answers `TotalRecordCount: 559` while
# returning 3 years, and the value tracks the filter (Series -> 8, Audio -> 9,
# parentId=Movies -> 500). Ferrofin reports the distinct-year count, which is
# what the field means and what pages the list.
#
# So this ONE key is dropped from the leg comparison and recorded as an accepted
# jellyfin-bug divergence in classifications.json with that citation. Everything
# else about the page — the year SET, the ORDER (added below as a diffable
# field), and StartIndex — is compared strictly, and the companion
# `GET /Years/{year}` row diffs the whole per-year DTO with nothing removed.
# Ferrofin's own total is gated where it can be asserted per-server: the
# `items_root_ancestors_and_years_over_real_http` integration test requires it to
# equal the distinct-year count and to be unchanged by `limit`/`startIndex`.
def years_page(body):
    out = with_item_order(body)
    out.pop("TotalRecordCount", None)
    return out


def years_unordered(body):
    """`years_page` with the year SEQUENCE normalised — for the no-sortBy leg.

    This is the second projection in the whole read set, and it is allowed for
    the same reason as the first: the removed thing is provably not comparable
    between two independent instances, and its removal is DECLARED on the row's
    `caveats` rather than buried here. `YearsController.GetAllItems` ends in
    LINQ `Distinct()`, which preserves first-occurrence order, so with no
    `sortBy` Jellyfin's order IS its `Folder.GetRecursiveChildren` enumeration
    order — a walk of the in-memory BaseItem tree that Ferrofin does not have.

    Everything else about the default call shape is still compared strictly:
    the year set, and every field of every year DTO. Before this leg existed the
    row's nine legs all pinned `sortBy=SortName` and the default shape — the one
    jellyfin-web actually sends when a view has no sort — was never issued at
    all."""
    out = years_page(body)
    order = out.pop("_ItemNameOrder", None)
    if order is not None:
        out["_ItemNameSet"] = sorted(order)
    if isinstance(out.get("Items"), list):
        out["Items"] = sorted(out["Items"], key=lambda i: str(i.get("Name")))
    return out


# Providers Ferrofin compiles in that the lab's stock Jellyfin 10.11.8 does not
# ship. Verified against that server's own `GET /Plugins`, which lists only
# AudioDB and MusicBrainz — so these are structurally extra BY DESIGN (see
# CLAUDE.md "Current scope": every remote provider is always compiled, gated
# per library; Tier-1a extensions are compiled in too).
#
# They are dropped BY NAME from the fetcher lists on both sides. This is
# deliberately not a `parity_diff.VOLATILE` entry: VOLATILE hides a field
# everywhere, while this removes four named rows from one endpoint and leaves
# every other provider — including their DefaultEnabled — fully compared.
FERROFIN_ONLY_PROVIDERS = {"TheTVDB", "FanArt", "Open Subtitles", "IntroSkipper"}


def without_ferrofin_only_providers(body):
    def strip(lst):
        return [o for o in lst or [] if o.get("Name") not in FERROFIN_ONLY_PROVIDERS]

    out = dict(body)
    for key in ("MetadataSavers", "MetadataReaders", "SubtitleFetchers",
                "LyricFetchers", "MediaSegmentProviders"):
        out[key] = strip(out.get(key))
    type_options = []
    for block in out.get("TypeOptions") or []:
        b = dict(block)
        b["MetadataFetchers"] = strip(b.get("MetadataFetchers"))
        b["ImageFetchers"] = strip(b.get("ImageFetchers"))
        # SupportedImageTypes is the FLATTENED union of every compiled image
        # provider's GetSupportedImages, so the removed providers' image types
        # cannot be un-mixed from it by name. It is compared as-is, and the
        # residual (Ferrofin lists Art/Disc/Banner that FanArt really can
        # fetch) is recorded as an accepted divergence in classifications.json
        # rather than projected away here.
        type_options.append(b)
    out["TypeOptions"] = type_options
    return out


# ------------------------------------------------------- /Similar invariants
#
# A movie seed's /Similar body cannot diff, for TWO independent reasons, both
# measured on this lab pair:
#   1. Jellyfin 10.11.8 orders the result Random (LibraryController.cs:801,
#      `OrderBy = [(ItemSortBy.Random, Ascending)]`). Five identical calls at
#      limit=12 returned five different SETS, because limit < |universe|.
#   2. The two servers run DIFFERENT ALGORITHMS on purpose. 10.11.8 builds one
#      flat `InternalItemsQuery { Genres = item.Genres, Tags = item.Tags }`;
#      Ferrofin ports upstream master's weighted `MovieSimilarItemsProvider`
#      (genre 10 / tag 5 / studio 5 / director 50 / actor 15). At limit=1000 the
#      universes measured 299 (J) vs 432 (F), with J a STRICT SUBSET of F.
#
# Widening the denylist or comparing fewer fields would "fix" that dishonestly —
# and a set comparison would still pass if Ferrofin returned seven random
# unrelated movies. These are the properties that DO hold on both servers today,
# each of which such an answer would break.
SIMILAR_KINDS = {"Movie", "Trailer", "LiveTvProgram"}
SIMILAR_FIELDS = "Genres,Tags,Studios,People,SortName"

# upstream master `MovieSimilarItemsProvider.cs:27-31` — the weights Ferrofin ports.
GENRE_WEIGHT, TAG_WEIGHT, STUDIO_WEIGHT, DIRECTOR_WEIGHT, ACTOR_WEIGHT = 10, 5, 5, 50, 15


def _related_keys(dto):
    """The seed-relatedness dimensions of one DTO: its genres, tags, studios and
    people. The union of 10.11.8's query (genres+tags) and master's scored
    dimensions (+studios+people) — an item sharing NONE of them with the seed is
    unreachable by either algorithm."""
    keys = set()
    keys |= {("g", g) for g in dto.get("Genres") or []}
    keys |= {("t", t) for t in dto.get("Tags") or []}
    keys |= {("s", (s or {}).get("Name")) for s in dto.get("Studios") or []}
    keys |= {("p", (p or {}).get("Name")) for p in dto.get("People") or []}
    return {k for k in keys if k[1]}


def _people(dto):
    """The (Name, Type) pairs master's scorer matches on.

    Master matches candidates by `PeopleId` alone
    (`MovieSimilarItemsProvider.cs:271-297`: the candidate side carries NO
    PersonType predicate, and the weight comes from the SOURCE row's type) —
    and `PeopleId` IS the pair, because "the Peoples table has one row per
    (Name, PersonType)" (`Jellyfin.Server.Implementations/Item/PeopleRepository.cs:47`;
    `People.PersonType` is a column on the People row, not on the map). Ferrofin's
    SQL joins the same way (`similar_items_repository.rs:70-77`, `pm2` on
    `PeopleId`). So a (Name, Type) intersection re-derives exactly what both
    sides compute — it is not a stronger model, and a person credited as
    Director on the seed and Writer on a candidate scores 0 in all three.
    """
    return {((p or {}).get("Name"), (p or {}).get("Type")) for p in dto.get("People") or []}


def _master_score(seed, cand):
    """One candidate's score under master's `MovieSimilarItemsProvider`."""
    g = len({g for g in seed.get("Genres") or []} & {g for g in cand.get("Genres") or []})
    t = len({t for t in seed.get("Tags") or []} & {t for t in cand.get("Tags") or []})
    st = len({(s or {}).get("Name") for s in seed.get("Studios") or []}
             & {(s or {}).get("Name") for s in cand.get("Studios") or []})
    people = _people(seed) & _people(cand)
    p = sum(DIRECTOR_WEIGHT if kind == "Director" else ACTOR_WEIGHT
            for _, kind in people if kind in ("Director", "Actor", "GuestStar"))
    return GENRE_WEIGHT * g + TAG_WEIGHT * t + STUDIO_WEIGHT * st + p


def _obeys_own_candidate_rule(base, token, ctx, seed_dto, items, limit):
    """Whether `items` is what THIS server's documented algorithm must produce.

    The two servers run different algorithms on purpose, so each is held to its
    own — and each rule is sharp enough that a page of unrelated movies fails it.

    - Jellyfin 10.11.8 (`LibraryController.cs:791-801`) queries `Genres = item
      .Genres, Tags = item.Tags`. Every row must therefore share a genre or a
      tag with the seed. On this fixture only 299 of the 500 movies qualify, so
      the clause discriminates.
    - Ferrofin ports master's weighted `MovieSimilarItemsProvider`, which is
      deterministic here: the answer must be the model's top-N, IN ORDER, by
      (score desc, SortName asc), recomputed independently from the DTO fields.
      Nothing weaker would notice a scorer that quietly stopped weighting.
      The (score desc) half is master's (`MovieSimilarItemsProvider.cs:189-195`
      is `OrderByDescending(score)`); the SortName/Id tiebreak is FERROFIN's
      own — master has no tiebreak at all, so equal-scoring rows fall out in
      whatever order the DB handed them. This clause therefore pins Ferrofin's
      rule, which is strictly more determined than the C#'s, not a re-derivation
      of it. That is a deliberate choice recorded on the ledger rows, not a
      claim about upstream.
    """
    if ctx.get("server") != "ferrofin":
        seed_gt = {("g", g) for g in seed_dto.get("Genres") or []} \
            | {("t", t) for t in seed_dto.get("Tags") or []}
        return all(({("g", g) for g in i.get("Genres") or []}
                    | {("t", t) for t in i.get("Tags") or []}) & seed_gt for i in items)
    _, allb = token_get(base, f"/Items?userId={ctx['user']}&recursive=true"
                              f"&includeItemTypes=Movie&limit=100000&sortBy=SortName"
                              f"&fields={SIMILAR_FIELDS}", token)
    pool = [i for i in ((allb or {}).get("Items") or []) if i.get("Id") != seed_dto.get("Id")]
    scored = [(-_master_score(seed_dto, c), c.get("SortName") or "", c.get("Name"))
              for c in pool]
    expected = [n for s, _, n in sorted(x for x in scored if x[0] < 0)][:limit]
    return [i.get("Name") for i in items] == expected


SIMILAR_ALIASES = ("Items", "Movies", "Trailers", "Albums", "Artists")


# ---------------------------------------------------------------- package repositories
#
# The repository list is admin-settable to the same value on both servers and the
# catalogue is derived from it, so these rows ARE diffable — they are not
# instance state, which is what the old classification claimed.
#
# Both servers ship exactly ONE repository by default and it is the SAME one
# (Jellyfin seeds it from the `RunMigrationOnSetup` routines
# `Add/Readd/UpdateDefaultPluginRepository`; Ferrofin seeds the post-migration
# value directly in `default_server_configuration`), so `GET /Repositories` diffs
# as an ordinary body.
#
# `GET /Packages` aggregates what those repositories publish. Diffing it against
# the live repo.jellyfin.org would make the row depend on upstream content that
# changes without notice, so this seed points BOTH servers at the fixed manifests
# the lab's fixture container serves (see suite/perf/livetv-source.py) and the
# reap puts each server's own list back. Nothing is added to
# `parity_diff.VOLATILE` for these rows: with the same manifest on both, the
# bodies are byte-comparable.

FIXTURE_REPOS = [
    {"Name": "Parity Fixture A", "Url": "http://livetv-source:8000/manifest.json",
     "Enabled": True},
    {"Name": "Parity Fixture B", "Url": "http://livetv-source:8000/manifest-b.json",
     "Enabled": True},
    # A disabled repository must be skipped entirely
    # (`if (repository.Enabled && repository.Url is not null)`). This URL is
    # SERVED, and serves something different on every request (see
    # suite/perf/livetv-source.py's poison_manifest): a fetch by either server
    # puts a package with a per-request guid into that server's catalogue and the
    # body diff goes red, while the flag being honoured leaves both catalogues
    # untouched. An unservable URL could not tell those apart — a 404 is instant,
    # so a server that ignored the flag would warn, skip, and produce a
    # byte-identical catalogue anyway. (The absolute form of the same assertion —
    # that the disabled URL is never REQUESTED — is a hit-counted unit test,
    # `plugin_manager::tests::a_disabled_repository_is_never_fetched`; a
    # cross-server body diff cannot fail a behaviour both servers share.)
    {"Name": "Parity Fixture Disabled",
     "Url": "http://livetv-source:8000/manifest-poison.json",
     "Enabled": False},
]


def with_package_order(body):
    """ADDITIVE projection: attach the catalogue's ORDER as lists of plain strings.

    `parity_diff` aligns arrays of objects by Name/Id and never compares
    position, so the two things `GetAvailablePackages` actually decides about
    ORDER would diff clean whatever they were: which package comes first, and
    where a second repository's version lands inside an existing package's list
    (`MergeSortedList`, descending). A list of plain strings is the one array
    shape parity_diff compares positionally, so this makes both gateable.

    Additive — every original field is still compared. Non-list bodies pass
    through untouched (the selfcheck exercises the row-agnostic shape)."""
    if not isinstance(body, list):
        return body
    return {
        "_Packages": body,
        "_PackageOrder": [p.get("name") for p in body],
        "_VersionOrder": {
            p.get("name"): [v.get("version") for v in (p.get("versions") or [])]
            for p in body
        },
    }


def seed_package_repositories(base, token, ctx):
    """Points both servers at the lab's fixed manifests, remembering what was there."""
    st, before = token_get(base, "/Repositories", token)
    ctx["_repos_before"] = before if isinstance(before, list) else []
    st, _ = http("POST", base + "/Repositories", token, json.dumps(FIXTURE_REPOS))
    return {"read_before": st, "set_repositories": st}


def reap_package_repositories(base, token, ctx):
    """Restores each server's own repository list, so later layers see it as found."""
    st, _ = http("POST", base + "/Repositories", token,
                 json.dumps(ctx.get("_repos_before") or []))
    if st >= 300:
        print(f"  WARN: could not restore /Repositories on {base} (status {st})")
    return {"restore": st}


def hex_object(guid):
    """A dashless guid in .NET's `X` spelling: `{0xa,0xb,0xc,{0xd0,…,0xd7}}`."""
    tail = ",".join(f"0x{guid[i:i + 2]}" for i in range(16, 32, 2))
    return f"{{0x{guid[0:8]},0x{guid[8:12]},0x{guid[12:16]},{{{tail}}}}}"


def packages_by_name_invariants(base, token, ctx):
    """Every branch of `InstallationManager.FilterPackages`, as booleans.

    `GET /Packages/{name}` is scored by Layer-1 today with `{name}` filled from a
    GENRE (sweep.py's generic path-param fill), so the live probe was
    `GET /Packages/Action` — 404 on both servers, "status conformant", and the
    lookup itself never exercised. These facts are each derived from the SERVER'S
    OWN catalogue entry, so the row diffs without ever comparing two catalogues,
    and every one of them is a branch of the C#:

        if (!id.IsEmpty())          … Where(x => x.Id.Equals(id));
        else if (name is not null)  … Where(x => x.Name.Equals(name, OrdinalIgnoreCase));

    The guid is bound as `Guid?`, so `Guid.TryParse`'s WHOLE format set must
    resolve — N, D, B, P and X, with whitespace trimmed at both ends — and
    anything else must be a 400 from model binding. The N (dashless) spelling is
    the one both servers emit and jellyfin-web echoes back; `urn:uuid:` is the
    one .NET refuses and `uuid::Uuid::parse_str` accepts, so it is asserted as a
    400 rather than left out.
    """
    facts = {}
    st, catalog = token_get(base, "/Packages", token)
    facts["packages_status_200"] = st == 200
    if not isinstance(catalog, list) or not catalog:
        facts["catalog_non_empty"] = False
        return facts
    facts["catalog_non_empty"] = True
    pkg = catalog[0]
    name, guid = pkg.get("name") or "", pkg.get("guid") or ""
    hyphenated = "-".join([guid[0:8], guid[8:12], guid[12:16], guid[16:20], guid[20:32]])
    qn = urllib.parse.quote(name)

    def status(path):
        st, _ = http("GET", base + path, token)
        return st

    facts["exact_name_200"] = status(f"/Packages/{qn}") == 200
    facts["case_insensitive_name_200"] = (
        status(f"/Packages/{urllib.parse.quote(name.swapcase())}") == 200)
    facts["guid_n_format_200"] = status(f"/Packages/{qn}?assemblyGuid={guid}") == 200
    facts["guid_d_format_200"] = status(f"/Packages/{qn}?assemblyGuid={hyphenated}") == 200
    facts["guid_b_format_200"] = status(
        f"/Packages/{qn}?assemblyGuid=%7B{hyphenated}%7D") == 200
    # The P ("(guid)") and X ("{0x…}") spellings and the whitespace trim are the
    # rest of `Guid.TryParse`'s set, and `urn:uuid:` is the one spelling it
    # REFUSES that Rust's `Uuid::parse_str` takes. Enumerating N/D/B and stopping
    # is exactly where the two servers used to disagree, so the row went green
    # over a live difference: `(guid)` was 400 here / 200 there, `urn:uuid:` 200
    # here / 400 there, and `{0x…}` 400 here / 200 there.
    facts["guid_p_format_200"] = status(
        f"/Packages/{qn}?assemblyGuid=%28{hyphenated}%29") == 200
    facts["guid_x_format_200"] = status(
        f"/Packages/{qn}?assemblyGuid={urllib.parse.quote(hex_object(guid), safe='')}") == 200
    facts["padded_guid_is_trimmed_200"] = status(
        f"/Packages/{qn}?assemblyGuid=%20{hyphenated}%20") == 200
    facts["urn_guid_is_400"] = status(
        f"/Packages/{qn}?assemblyGuid=urn%3Auuid%3A{hyphenated}") == 400
    # `!id.IsEmpty()` short-circuits the name entirely, so a wrong name still
    # resolves the guid's package.
    st, body = token_get(base, f"/Packages/zzz-no-such-package?assemblyGuid={guid}", token)
    facts["guid_beats_name"] = st == 200 and (body or {}).get("guid") == guid
    facts["empty_guid_ignored_200"] = status(f"/Packages/{qn}?assemblyGuid=") == 200
    facts["nil_guid_falls_through_200"] = status(
        f"/Packages/{qn}?assemblyGuid=00000000000000000000000000000000") == 200
    facts["bad_guid_is_400"] = status(f"/Packages/{qn}?assemblyGuid=notaguid") == 400
    facts["unknown_name_is_404"] = status("/Packages/zzz-no-such-package") == 404
    # The single-package body must be exactly the catalogue's entry for it.
    st, one = token_get(base, f"/Packages/{qn}", token)
    facts["body_equals_catalog_entry"] = one == pkg
    return facts


def guide_info_invariants(base, token, ctx):
    """The properties `GET /LiveTv/GuideInfo` must have on BOTH servers.

    `GuideManager.GetGuideInfo` is `now .. now + GetGuideDays()`, so the two
    endpoints of the window are a per-request instant and cannot be byte-equal
    between two servers answering microseconds apart. Everything *else* about
    the window can be, and is compared here: its LENGTH (the bug this row was
    hiding — Ferrofin ignored `LiveTvOptions.GuideDays` and always answered 7
    days where Jellyfin answers the configured value clamped to 1..14), its
    now-relativity (an epoch/default window is the regression this route already
    had once), and the .NET 7-fractional-digit wire format.

    This is deliberately NOT a `parity_diff.VOLATILE` entry: denylisting
    StartDate/EndDate would have swallowed the GuideDays divergence whole.
    """
    del ctx
    facts = {}
    before = datetime.datetime.now(datetime.timezone.utc)
    st, b = token_get(base, "/LiveTv/GuideInfo", token)
    after = datetime.datetime.now(datetime.timezone.utc)
    facts["status_200"] = st == 200
    if not isinstance(b, dict):
        return facts
    start_raw, end_raw = b.get("StartDate"), b.get("EndDate")
    facts["has_both_bounds"] = bool(start_raw and end_raw)
    if not facts["has_both_bounds"]:
        return facts

    def parse(v):
        return datetime.datetime.fromisoformat(v.replace("Z", "+00:00"))

    # `Utf8JsonWriter`'s `FFFFFFF` trims a fraction's trailing zeros, so the
    # DIGIT COUNT is a property of the instant, not of the server — sampled 60
    # times, Jellyfin emits 5, 6 and 7 digits and Ferrofin emits 6 and 7. Only
    # the shape is comparable: UTC `Z`, no offset, never more than .NET tick
    # precision.
    wire = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d{1,7})?Z$")

    start, end = parse(start_raw), parse(end_raw)
    # The exact instant is per-request; that it IS this request's instant is not.
    facts["start_is_request_instant"] = (before - datetime.timedelta(seconds=120)
                                         <= start
                                         <= after + datetime.timedelta(seconds=120))
    # The window length in whole seconds — identical on both servers or the row
    # fails, which is what makes a 7-vs-14 day divergence visible.
    facts["window_seconds"] = int((end - start).total_seconds())
    facts["window_is_whole_days"] = facts["window_seconds"] % 86400 == 0
    facts["start_wire_format"] = bool(wire.match(start_raw))
    facts["end_wire_format"] = bool(wire.match(end_raw))
    # Both bounds are stamped from ONE `UtcNow`, so their sub-second parts agree.
    # `AddDays` cannot change them, and a window built from two separate reads
    # would not survive this.
    facts["bounds_share_subsecond"] = start.microsecond == end.microsecond
    return facts


guide_info_invariants.alias = "LiveTvGuideInfo"


#: A groupId that resolves in no id space on either server. The value is
#: arbitrary BY DESIGN — see `recording_group_invariants`.
PROBE_GROUP_GUID = "f0f0f0f0-1111-2222-3333-444444444444"

# A GUID no plugin can have on either server, for the miss paths.
PROBE_PLUGIN_GUID = "11111111-1111-1111-1111-111111111111"


def recording_group_invariants(base, token, ctx):
    """`GET /LiveTv/Recordings/Groups/{groupId}` — an OBSOLETE endpoint whose
    whole C# body is `return NotFound();`.

    v10.11.8 `Jellyfin.Api/Controllers/LiveTvController.cs:955-966`:

        [HttpGet("Recordings/Groups/{groupId}")]
        [Authorize(Policy = Policies.LiveTvAccess)]
        [Obsolete("This endpoint is obsolete.")]
        public ActionResult<BaseItemDto> GetRecordingGroup([FromRoute, Required] Guid groupId)
            => NotFound();

    `git grep RecordingGroup v10.11.8 -- '*.cs'` returns only the two controller
    signatures: there is no `LiveTvManager.GetRecordingGroups`, no group-id
    derivation and no DTO anywhere in the 10.11.8 tree. The answer is therefore
    groupId-INDEPENDENT, and no recording can ever make it 200 — which is why
    this is a property row and not a body diff: there is no 200 body to diff,
    ever, on either server.

    What the facts actually claim, then, is the thing a lookup-that-happens-to-
    miss would fail: that ids which DO resolve in other id spaces (a real Live
    TV channel, a real recording — recordings exist on both servers now) are
    still 404, that the policy gate is in front of the `NotFound`, and that a
    malformed groupId is a route-binding 400 rather than the handler's 404.

    Only statuses and a boolean go into the facts. The ids themselves are
    per-server (each instance minted its own recording), so putting one in a
    fact would be a guaranteed false red.
    """
    facts = {}

    def status(group_id, tok=token):
        return token_get(base, f"/LiveTv/Recordings/Groups/{group_id}", tok)[0]

    facts["nil_guid"] = status("00000000-0000-0000-0000-000000000000")
    facts["random_guid"] = status(PROBE_GROUP_GUID)
    if ctx.get("channel"):
        # A real id from a NEIGHBOURING id space: proof the handler never looks
        # anything up, rather than looking in an empty table.
        facts["live_channel_id_does_not_resolve"] = status(ctx["channel"])
    recordings = (get_json(base, "/LiveTv/Recordings?limit=1", token) or {}).get("Items") or []
    recording_id = recordings[0].get("Id") if recordings else ""
    # Recorded as a fact so the leg above cannot silently degrade to "there was
    # nothing to try": the old classification claimed this row needed a
    # recording to exist, and this is where that claim is settled.
    facts["recording_seed_present"] = bool(recording_id)
    if recording_id:
        facts["recording_id_does_not_resolve"] = status(recording_id)
    # `Guid` route binding rejects before the handler runs: 400, not 404.
    facts["malformed_groupid"] = status("notaguid")
    # `Policies.LiveTvAccess` sits in front of the `NotFound`, so an anonymous
    # caller must not be able to tell this route from any other.
    facts["unauthenticated"] = status(PROBE_GROUP_GUID, None)
    return facts


recording_group_invariants.alias = "LiveTvRecordingGroup"


# ------------------------------------------------------------ plugin identity
#
# The two servers share NO plugin id. Ferrofin registers compiled-in extensions
# and staged WASM components; stock Jellyfin 10.11.8 registers five bundled .NET
# provider plugins (TMDb b8715ed1…, MusicBrainz 8c95c4d2…, OMDb a628c0da…,
# AudioDB a629c0da…, Studio Images 872a7849…). An id-correlated BODY diff is
# therefore impossible on every `{pluginId}` route, and seeding one server's id
# on both would compare a hit against a miss — worse than not measuring.
#
# What IS diffable, and what found four real defects, is the SHAPE each server
# gives its OWN plugin, plus the id-INDEPENDENT rejection contract (an unknown
# guid, an unparseable version, a version that misses, no credentials), which is
# determined by the controller rather than by which plugins a server has. Each op
# below gets its OWN probe exercising its OWN route: six ledger rows must be six
# measurements, not one claimed six times.


def plugin_seed(base, token):
    """`(plugin id, version)` for a plugin THIS server has, or `("", "")`."""
    plugins = token_get(base, "/Plugins", token)[1] or []
    if not plugins:
        return "", ""
    pid = sorted(p["Id"] for p in plugins)[0]
    return pid, next(p.get("Version") or "" for p in plugins if p["Id"] == pid)


def plugin_configuration_invariants(base, token, ctx):
    """`GET /Plugins/{pluginId}/Configuration`.

    A plugin's configuration is where its API key, username and password live,
    and `PluginsController` is `[Authorize(Policy = Policies.RequiresElevation)]`
    at CLASS level with exactly one `[AllowAnonymous]` override, `GetPluginImage`
    (v10.11.8 Jellyfin.Api/Controllers/PluginsController.cs:25 and :221).
    """
    del ctx
    facts = {}
    pid, _ = plugin_seed(base, token)
    facts["plugin_seed_present"] = bool(pid)
    if pid:
        st, config = token_get(base, f"/Plugins/{pid}/Configuration", token)
        facts["status"] = st
        # The contract's `BasePluginConfiguration` is an OBJECT. Ferrofin used
        # to store a posted `null` verbatim and serve the bare token back.
        facts["is_object"] = isinstance(config, dict)
    facts["unknown_id"] = token_get(
        base, f"/Plugins/{PROBE_PLUGIN_GUID}/Configuration", token)[0]
    facts["malformed_id"] = token_get(base, "/Plugins/notaguid/Configuration", token)[0]
    facts["anonymous"] = token_get(
        base, f"/Plugins/{PROBE_PLUGIN_GUID}/Configuration", None)[0]
    facts["anonymous_list"] = token_get(base, "/Plugins", None)[0]
    return facts


plugin_configuration_invariants.alias = "PluginsConfiguration"


def plugin_configuration_write_invariants(base, token, ctx):
    """`POST /Plugins/{pluginId}/Configuration`.

    Upstream deserializes the body into the plugin's own `ConfigurationType`
    (v10.11.8 PluginsController.cs:186-201), which has three consequences a raw
    byte-store does not have — and Ferrofin had none of them:

      * `if (configuration is not null)` — a `null` body is a NO-OP that still
        answers 204. Ferrofin stored the literal `null` and the next read
        answered `null`, destroying the plugin's configuration from an
        admin-reachable route.
      * a key the type does not declare is DROPPED by the deserializer.
      * the write is a full REPLACE: a key the body omits falls back to the C#
        property default.

    Every leg restores the snapshot it took, so the shared lab pair is left
    exactly as it was found. The probe writes only values the server itself just
    handed back, plus one deliberately-unknown key that must not survive.
    """
    del ctx
    facts = {}
    pid, _ = plugin_seed(base, token)
    facts["plugin_seed_present"] = bool(pid)
    facts["unknown_id"] = http("POST", base + f"/Plugins/{PROBE_PLUGIN_GUID}/Configuration",
                               token, "{}")[0]
    if not pid:
        return facts
    path = f"/Plugins/{pid}/Configuration"
    snapshot = token_get(base, path, token)[1]
    facts["snapshot_is_object"] = isinstance(snapshot, dict)
    if not isinstance(snapshot, dict):
        return facts

    def post(body):
        return http("POST", base + path, token, body)[0]

    def read_back():
        return token_get(base, path, token)[1]

    # An identity write changes nothing.
    facts["identity_write_status"] = post(json.dumps(snapshot))
    facts["identity_write_is_a_noop"] = read_back() == snapshot
    # A `null` body is a no-op that still answers 204.
    facts["null_write_status"] = post("null")
    facts["null_write_is_a_noop"] = read_back() == snapshot
    # A key the plugin's configuration type does not declare is dropped.
    probe_key = "FerrofinParityProbeUnknownKey"
    facts["unknown_key_write_status"] = post(json.dumps({**snapshot, probe_key: "x"}))
    facts["unknown_key_is_dropped"] = probe_key not in (read_back() or {})
    # Restore, whatever the server did with the two writes above.
    post(json.dumps(snapshot))
    facts["restored"] = read_back() == snapshot
    return facts


plugin_configuration_write_invariants.alias = "PluginsConfigurationWrite"


def plugin_manifest_invariants(base, token, ctx):
    """`POST /Plugins/{pluginId}/Manifest`.

    `MediaBrowser.Common/Plugins/PluginManifest.cs` gives every property an
    explicit lowercase `[JsonPropertyName]`, so this one response is camelCase
    against the server's PascalCase policy, and the id is spelled `guid` in the
    dashless `N` form `JsonGuidConverter` writes. Ferrofin answered five
    PascalCase keys and a hyphenated `Id` — not one key in common with what a
    client reads.
    """
    del ctx
    facts = {}
    pid, _ = plugin_seed(base, token)
    facts["plugin_seed_present"] = bool(pid)
    if pid:
        st, manifest = token_post(base, f"/Plugins/{pid}/Manifest", token, None)
        manifest = manifest or {}
        facts["status"] = st
        facts["keys"] = ",".join(sorted(manifest))
        facts["guid_dashless"] = "-" not in (manifest.get("guid") or "-")
        facts["guid_matches_route"] = (
            (manifest.get("guid") or "").replace("-", "").lower() == pid.replace("-", "").lower())
        facts["auto_update_is_bool"] = isinstance(manifest.get("autoUpdate"), bool)
        facts["assemblies_is_list"] = isinstance(manifest.get("assemblies"), list)
        facts["status_value"] = manifest.get("status")
        facts["timestamp"] = manifest.get("timestamp")
    facts["unknown_id"] = token_post(
        base, f"/Plugins/{PROBE_PLUGIN_GUID}/Manifest", token, None)[0]
    facts["anonymous"] = token_post(
        base, f"/Plugins/{PROBE_PLUGIN_GUID}/Manifest", None, None)[0]
    return facts


plugin_manifest_invariants.alias = "PluginsManifest"


# ---------------------------------------------------- Plugins Enable / Disable
#
# Nothing exercised these two ops in ANY layer before this: sweep hands writes to
# Layer 2, journeys.py contains no plugin journey at all, and the four probes
# above cover Configuration/Manifest/Image only. Both rows sat on a hand-written
# classification about a *different* op (the pre-restart window of
# `POST /Packages/Installed/{name}`) while the real divergences went unmeasured.
#
# MUTATION HYGIENE — read this before editing either probe. On Jellyfin,
# `PluginManager.ChangePluginState` (v10.11.8
# Emby.Server.Implementations/Plugins/PluginManager.cs:513) early-returns
# `true` when `plugin.Manifest.Status == state`, and that early return is the
# ONLY path that reaches `ProcessAlternative`, whose last two lines are
# `plugin.Manifest.Status = PluginStatus.Restart; plugin.Manifest.AutoUpdate =
# false;` (:882, commented "This value is memory only"). So a NO-OP toggle —
# enabling an already-Active plugin — permanently pins that plugin at `Restart`
# in memory, poisoning `plugin_manifest_invariants`' `status_value` fact and
# every later `GET /Plugins` diff, until the container restarts. Neither probe
# below ever issues a toggle whose target equals the current status, and both
# restore. A concurrent lane already left Jellyfin's AudioDB at `Restart` this
# way; do not add an "idempotent toggle" fact here.
#
# WHAT IS AND IS NOT DIFFED. Every rejection fact is id-independent and agrees
# exactly. The mutating leg's HTTP status does NOT agree, and it is included on
# purpose — see the `jellyfin-bug` half of these rows in classifications.json.
# Jellyfin answers 404 for a toggle that SUCCEEDS: its five plugins are bundled
# in-assembly, so `LocalPlugin.Path` is a .dll FILE (`CreatePluginInstance`,
# :541/:559), `SaveManifest` does `File.WriteAllText(Path.Combine(path,
# "meta.json"))` and catches only `ArgumentException` (:364), the resulting
# `DirectoryNotFoundException` escapes the action, and `ExceptionMiddleware`
# maps it to 404 — after `ChangePluginState` has already flipped the status in
# memory. Ferrofin answers 204 and is correct. Making that fact green by
# dropping it would be the dishonest version of this row.


def plugin_status(base, token, pid):
    """The `Status` this server reports for `pid` in `GET /Plugins`, or None."""
    return next((p.get("Status") for p in (token_get(base, "/Plugins", token)[1] or [])
                 if p.get("Id") == pid), None)


def restore_plugin_state(base, token, pid, ver, want):
    """Put `pid` back to the `want` status ("Active"/"Disabled") it started in.

    Called from a `finally`, because the mutate/restore pair is otherwise a real
    poisoning hazard: an interrupt or a timed-out restore between the two calls
    leaves the Jellyfin plugin Disabled in memory for the rest of the container's
    life, which corrupts `plugin_manifest_invariants`' status_value and every
    later GET /Plugins diff on this pair — the exact damage the mutation-hygiene
    note at the top of this section exists to prevent. The `restored` fact makes
    that visible after the fact; this prevents it.

    It READS the status first and issues nothing when it already matches. That is
    load-bearing, not an optimisation: on Jellyfin a toggle to the status the
    plugin is already in early-returns from `ChangePluginState` into
    `ProcessAlternative`, which pins the plugin at `Status = Restart` in memory
    until the container restarts. A blind restore would be the poisoning it is
    meant to undo.
    """
    if plugin_status(base, token, pid) == want:
        return
    verb = "Enable" if want == "Active" else "Disable"
    http("POST", base + f"/Plugins/{pid}/{ver}/{verb}", token, None)


def plugin_toggle_facts(base, token, verb):
    """The id-independent rejection facts for `POST /Plugins/{id}/{ver}/{verb}`.

    `PluginsController` is `[Authorize(Policy = Policies.RequiresElevation)]` at
    class level and binds `[FromRoute, Required] Version version`, then resolves
    `_pluginManager.GetPlugin(pluginId, version)` and `NotFound()`s on a miss
    (v10.11.8 PluginsController.cs:71 / :94, PluginManager.cs:293-311). Every
    fact here is therefore determined by the CONTRACT, not by which plugins the
    server happens to have — which is what makes them diffable across two servers
    that share no plugin id.
    """
    pid, ver = plugin_seed(base, token)
    facts = {"plugin_seed_present": bool(pid)}
    facts["unknown_id"] = http(
        "POST", base + f"/Plugins/{PROBE_PLUGIN_GUID}/1.0.0/{verb}", token, None)[0]
    facts["anonymous"] = http(
        "POST", base + f"/Plugins/{PROBE_PLUGIN_GUID}/1.0.0/{verb}", None, None)[0]
    if not pid:
        return facts, pid, ver
    facts["malformed_version"] = http(
        "POST", base + f"/Plugins/{pid}/notaversion/{verb}", token, None)[0]
    facts["wrong_version"] = http(
        "POST", base + f"/Plugins/{pid}/9.9.9.9/{verb}", token, None)[0]
    # .NET's `Version` treats an absent component as -1, so `1.0` never equals
    # `1.0.0` — the installed plugin's version with its last component dropped
    # must miss. Ferrofin ports that rule (`plugin_at_version`).
    short = ".".join(ver.split(".")[:-1])
    if short and short != ver:
        facts["short_version"] = http(
            "POST", base + f"/Plugins/{pid}/{short}/{verb}", token, None)[0]
    return facts, pid, ver


def plugin_disable_invariants(base, token, ctx):
    """`POST /Plugins/{pluginId}/{version}/Disable`.

    The mutating leg drives Disable from `Active` (never a no-op) and restores
    with one Enable. `status_after_disable` is the fact that matters and it
    AGREES: both servers report `Disabled`, because Jellyfin's in-memory status
    flip lands before the exception that costs it the 204. `disable_status` is
    the one that diverges (F 204 / J 404) and is kept.
    """
    del ctx
    facts, pid, ver = plugin_toggle_facts(base, token, "Disable")
    if not pid:
        return facts
    before = plugin_status(base, token, pid)
    facts["was_active_before"] = before == "Active"
    if before != "Active":
        return facts   # unknown starting state: measure nothing rather than guess
    try:
        facts["disable_status"] = http(
            "POST", base + f"/Plugins/{pid}/{ver}/Disable", token, None)[0]
        facts["status_after_disable"] = plugin_status(base, token, pid)
    finally:
        restore_plugin_state(base, token, pid, ver, before)
    facts["restored"] = plugin_status(base, token, pid) == before
    return facts


plugin_disable_invariants.alias = "PluginsDisable"


def plugin_enable_invariants(base, token, ctx):
    """`POST /Plugins/{pluginId}/{version}/Enable`.

    Mirror of the Disable probe on its OWN route: Disable is the SETUP (so the
    measured Enable is a real state change, not the `ProcessAlternative` trap),
    Enable is the measurement, and the plugin ends where it started.
    """
    del ctx
    facts, pid, ver = plugin_toggle_facts(base, token, "Enable")
    if not pid:
        return facts
    before = plugin_status(base, token, pid)
    facts["was_active_before"] = before == "Active"
    if before != "Active":
        return facts
    try:
        http("POST", base + f"/Plugins/{pid}/{ver}/Disable", token, None)   # setup, not measured
        facts["disabled_for_setup"] = plugin_status(base, token, pid) == "Disabled"
        facts["enable_status"] = http(
            "POST", base + f"/Plugins/{pid}/{ver}/Enable", token, None)[0]
        facts["status_after_enable"] = plugin_status(base, token, pid)
    finally:
        # The measured Enable is itself the restore, so this normally reads the
        # status and issues nothing; it fires only when the run died between the
        # setup Disable and the measurement.
        restore_plugin_state(base, token, pid, ver, before)
    facts["restored"] = plugin_status(base, token, pid) == before
    return facts


plugin_enable_invariants.alias = "PluginsEnable"


def plugin_image_invariants(base, token, ctx):
    """`GET /Plugins/{pluginId}/{version}/Image`.

    The 200 body is out of reach on both servers by construction — no plugin on
    either reports `HasImage`, and a shared image subject would need the SAME
    external plugin installed on both, which needs the .NET assembly loading
    Ferrofin does not have. The status paths are fully diffable, and they are
    where the defect was: `{version}` is a `Version` bound by the model binder
    and matched with `Version.Equals` (v10.11.8 Emby.Server.Implementations/
    Plugins/PluginManager.cs:293-311), and Ferrofin discarded the segment
    entirely — `notaversion` answered as if it were the installed version.

    This is also the ONE action upstream marks `[AllowAnonymous]`
    (PluginsController.cs:221), against the controller's class-level elevation.
    """
    del ctx
    facts = {}
    pid, version = plugin_seed(base, token)
    facts["plugin_seed_present"] = bool(pid)
    facts["version_seed_present"] = bool(version)
    if pid and version:
        facts["installed_version"] = token_get(
            base, f"/Plugins/{pid}/{version}/Image", token)[0]
        facts["wrong_version"] = token_get(
            base, f"/Plugins/{pid}/9.9.9.9/Image", token)[0]
        facts["malformed_version"] = token_get(
            base, f"/Plugins/{pid}/notaversion/Image", token)[0]
        # `[AllowAnonymous]`: the same answer without a token.
        facts["anonymous_installed_version"] = token_get(
            base, f"/Plugins/{pid}/{version}/Image", None)[0]
    facts["unknown_id"] = token_get(
        base, f"/Plugins/{PROBE_PLUGIN_GUID}/1.0.0/Image", token)[0]
    facts["malformed_id"] = token_get(base, "/Plugins/notaguid/1.0.0/Image", token)[0]
    return facts


plugin_image_invariants.alias = "PluginsImage"


def similar_invariants_for(alias):
    """The invariant probe bound to ONE alias.

    `LibraryController.GetSimilarItems` is a single method behind six
    `[HttpGet]` attributes, but "they are the same method" is a claim to TEST,
    not to assume: each ledger row must exercise its OWN route, or /Trailers'
    404 / nil-seed / count contract is never actually issued against
    /Trailers. (Probing one alias for every row is the same duplication this
    harness fixed in sweep.py.)"""
    def probe(base, token, ctx):
        return similar_invariants(base, token, ctx, alias)
    probe.__name__ = f"similar_invariants_{alias.lower()}"
    probe.alias = alias
    return probe


def similar_invariants(base, token, ctx, alias="Movies"):
    """The properties a /{kind}/{itemId}/Similar answer must have on BOTH servers.

    Every value is comparable across servers, so a divergence surfaces as a
    normal diff row. Bounds that must tolerate a Jellyfin quirk say so and cite
    the C#; nothing here is widened to make Ferrofin pass.

    `alias` is the route this row owns: every fact below is issued against
    `/{alias}/{seed}/Similar`, so each of the three property rows is its own
    measurement."""
    u, seed = ctx["user"], ctx["movie"]
    facts = {}
    st, sb = token_get(base, f"/Items/{seed}?userId={u}&fields={SIMILAR_FIELDS}", token)
    if sb is None:
        return {"seed_readable": False}
    seed_keys = _related_keys(sb)
    facts["seed_has_related_keys"] = bool(seed_keys)

    limit = 12
    st, b = token_get(base, f"/{alias}/{seed}/Similar?userId={u}&limit={limit}"
                            f"&fields={SIMILAR_FIELDS}", token)
    items = (b or {}).get("Items") or []
    facts["status_200"] = st == 200
    facts["start_index_0"] = (b or {}).get("StartIndex") == 0
    facts["total_equals_len"] = (b or {}).get("TotalRecordCount") == len(items)
    facts["seed_not_returned"] = all(i.get("Id") != seed for i in items)
    facts["kinds_restricted"] = all(i.get("Type") in SIMILAR_KINDS for i in items)
    # THE clause that rejects "seven random unrelated movies": every row must
    # share a genre, tag, studio or person with the seed.
    # NOTE: on the synthetic loop fixture EVERY movie shares at least one of
    # these dimensions with the seed (measured: 0 of 500 fall outside), so this
    # clause alone is vacuous HERE and is kept only as the cheap floor for a
    # real library. `obeys_own_candidate_rule` below is the one that bites.
    facts["all_rows_related_to_seed"] = all(_related_keys(i) & seed_keys for i in items)
    facts["obeys_own_candidate_rule"] = _obeys_own_candidate_rule(
        base, token, ctx, sb, items, limit)
    facts["page_is_full"] = len(items) >= limit
    # Ferrofin returns exactly `limit`; 10.11.8 returns limit+4 because
    # BaseItemRepository.PrepareFilterQuery (v10.11.8:1427-1430) adds 4 when
    # EnableGroupByMetadataKey and never trims back. The +4 is the ONLY slack
    # here and it is a named Jellyfin bug, not a tolerance for Ferrofin.
    facts["limit_honoured_within_jellyfin_plus_4"] = len(items) <= limit + 4

    # The controller's own status contract, identical in 10.11.8 and master:
    #   `if (item is null) { return NotFound(); }`
    #   `itemId.IsEmpty() ? (user is null ? RootFolder : GetUserRootFolder()) : ...`
    missing = "aaaaaaaaaaaaaaaaaaaaaaaaaaaa0099"
    st, _ = http("GET", base + f"/{alias}/{missing}/Similar?userId={u}", token)
    facts["unknown_seed_is_404"] = st == 404
    st, nb = token_get(base, f"/{alias}/00000000000000000000000000000000/Similar?userId={u}", token)
    facts["nil_seed_is_200_empty"] = st == 200 and not ((nb or {}).get("Items") or [])

    # `if (item is Episode || (item is IItemByName && item is not MusicArtist))
    #  return new QueryResult<BaseItemDto>();`
    if ctx.get("episode"):
        st, eb = token_get(base, f"/{alias}/{ctx['episode']}/Similar?userId={u}", token)
        facts["episode_seed_short_circuits"] = st == 200 and not ((eb or {}).get("Items") or [])

    # Every alias is ONE C# method (`LibraryController.GetSimilarItems`, six
    # [HttpGet] attributes), so the five non-/Shows routes must answer with the
    # same rows for the same seed. Compared as SETS at a limit above the
    # universe: below it, Jellyfin's Random draws a different random SUBSET per
    # call and no two aliases could ever agree. Measured: at limit=1000 both
    # servers are set-stable across repeated draws.
    wide = 1000
    sets = []
    for other in SIMILAR_ALIASES:
        _, ab = token_get(base, f"/{other}/{seed}/Similar?userId={u}&limit={wide}", token)
        sets.append(tuple(sorted(i.get("Name") or i.get("Id") or ""
                                 for i in ((ab or {}).get("Items") or []))))
    facts["aliases_agree"] = len(set(sets)) == 1
    facts["aliases_non_empty"] = bool(sets and sets[0])
    return facts


# ------------------------------------------------------------ scheduled tasks

def tasks_projection(body):
    """What `GET /ScheduledTasks` legitimately CAN be diffed on.

    `LastExecutionResult` is the one genuinely per-run field — which tasks have
    run at all on this instance, and when — so it is dropped. Everything else is
    fully determined by the server's own task registry and must match.

    `Order` is listed EXPLICITLY because parity_diff aligns arrays by Name: the
    wire order would otherwise never be compared, and it is C# behaviour
    (`_taskManager.ScheduledTasks.OrderBy(o => o.Name)`), not an accident.

    `WireId` is listed explicitly for the same reason and it is NOT cosmetic:
    `Id` is in the global VOLATILE denylist, so without this alias the one field
    that addresses a task — the field every `/ScheduledTasks/{taskId}` URL, every
    dashboard link and every stored bookmark carries — would sit invisible under
    a 600-field green diff. It is a PORTABLE value, not an instance one:
    `ScheduledTaskWorker.Id` is `ScheduledTask.GetType().FullName.GetMD5()
    .ToString("N")` (v10.11.8:219), the same on every Jellyfin install, so two
    servers that disagree on it have mutually incompatible task URLs. Ferrofin
    used to emit the task key here, which 404s on Jellyfin and vice versa;
    `ferrofin_traits::tasks::task_id_for_key` now reproduces the C# derivation.
    The map is keyed by `Key`, so the key SET is compared by the same walk."""
    if not isinstance(body, list):
        return body
    return {
        "Order": [t.get("Name") for t in body],
        "Tasks": {t.get("Key"): {**{k: v for k, v in t.items()
                                    if k != "LastExecutionResult"},
                                 "WireId": t.get("Id")}
                  for t in body},
    }


# ------------------------------------------------------------------- storage

STORAGE_FOLDERS = ("ProgramDataFolder", "LogFolder", "InternalMetadataFolder")

#: Two independent captures cannot be simultaneous, so free space may move
#: between the two requests. 64 MiB is far below any real divergence (the bug
#: this guards against was a 15.2 GiB constant offset) and far above ordinary
#: write noise on an idle lab.
FREE_SPACE_TOLERANCE = 64 * 1024 * 1024


def storage_invariants(base, token, ctx):
    """`GET /System/Info/Storage` properties that hold on BOTH servers.

    NOT a body diff, and it must never be recorded as one: the `Path` strings are
    the container layout (Ferrofin's `/config/cache` vs Jellyfin's `/cache`,
    `/config/web` vs `/jellyfin/jellyfin-web`) and the byte counts are sampled at
    two different instants. What IS comparable, because both containers stat the
    SAME host filesystem:

      * the exact key set of every folder object (C# `FolderStorageDto`);
      * `DeviceId == Path` — on Unix `DriveInfo.Name` is the constructor argument
        verbatim, so `StorageHelper.GetFreeSpaceOf(path)` echoes the path;
      * `FreeSpace + UsedSpace`, which is `DriveInfo.TotalSize` and is therefore
        byte-equal across the two servers. This is the fact that catches a wrong
        `UsedSpace` formula (subtracting `f_bfree` instead of `f_bavail`
        under-reported it by the root reservation on every folder);
      * `StorageType`, the `DriveType.ToString()` of the same filesystem;
      * the libraries, compared as a SET of (Name, sorted folder paths) — never
        positionally: the array ORDER is inherited from
        `GET /Library/VirtualFolders` and belongs to that row.

    ORDER-DEPENDENT, deliberately: the `libraries` fact holds in the canonical
    layer order (sweep → reads → journeys → …), where both servers hold only the
    fixture libraries. Run reads AFTER journeys and it goes red, because
    creating a BoxSet makes Jellyfin auto-create a `Collections` virtual folder
    (`{data}/collections`) and a DVR path makes it create `Recordings`, and
    Ferrofin creates NEITHER — measured directly: `POST /Collections` on
    Ferrofin makes the BoxSet but leaves `GET /Library/VirtualFolders`
    unchanged. That is a real gap on `GET /Library/VirtualFolders`, which this
    row merely projects, and the fact is left un-weakened so it says so.

    The missing-folder branch (`-1`/`-1`/null/null) is NOT reachable here — no
    probe can point both servers at a directory that does not exist — so it is
    covered by a unit test in `ferrofin-core`'s `system_manager` instead."""
    facts = {}
    st, body = token_get(base, "/System/Info/Storage", token)
    facts["status_200"] = st == 200
    if not isinstance(body, dict):
        return facts

    folders = {k: v for k, v in body.items()
               if isinstance(v, dict) and "Path" in v}
    facts["folder_count"] = len(folders)
    facts["folder_keys"] = sorted(folders)
    facts["folder_object_keys"] = sorted({k for f in folders.values() for k in f})
    # `DeviceId = driveInfo.Name`, and on Unix `DriveInfo.Name` is the ctor
    # argument verbatim — so it echoes the path. It is absent exactly on the
    # folders that hit `StorageHelper.GetFreeSpaceOf`'s catch arm, which also
    # reports -1/-1 and no StorageType; that pairing is asserted too.
    facts["device_id_equals_path"] = all(
        f.get("DeviceId") == f.get("Path")
        for f in folders.values() if f.get("DeviceId") is not None)
    facts["absent_keys_pair_with_minus_one"] = all(
        (f.get("DeviceId") is None) == (f.get("FreeSpace") == -1)
        and (f.get("StorageType") is None) == (f.get("FreeSpace") == -1)
        and (f.get("FreeSpace") == -1) == (f.get("UsedSpace") == -1)
        for f in folders.values())
    facts["free_space_is_int"] = all(isinstance(f.get("FreeSpace"), int)
                                     for f in folders.values())

    # TotalSize of the shared host filesystem — identical on both servers.
    for name in STORAGE_FOLDERS:
        f = folders.get(name) or {}
        free, used = f.get("FreeSpace"), f.get("UsedSpace")
        facts[f"{name}.total"] = (free + used
                                  if isinstance(free, int) and isinstance(used, int)
                                  else None)
        facts[f"{name}.storage_type"] = f.get("StorageType")

    # Free space is sampled at two different instants; bucket it so a genuine
    # divergence still shows while ordinary write noise does not.
    free = (folders.get("ProgramDataFolder") or {}).get("FreeSpace")
    facts["program_data_free_bucket"] = (free // FREE_SPACE_TOLERANCE
                                         if isinstance(free, int) else None)

    libs = body.get("Libraries")
    if isinstance(libs, list):
        facts["libraries"] = sorted(
            (lib.get("Name"), tuple(sorted(f.get("Path") for f in (lib.get("Folders") or []))))
            for lib in libs)
        facts["library_folders_have_storage_type"] = all(
            f.get("StorageType") is not None
            for lib in libs for f in (lib.get("Folders") or []))
        facts["library_folder_device_id_equals_path"] = all(
            f.get("DeviceId") == f.get("Path")
            for lib in libs for f in (lib.get("Folders") or []))
    return facts


# ------------------------------------------------------------------ trailers

def trailers_invariants(base, token, ctx):
    """`GET /Trailers` — Jellyfin 10.11.8 500s on its own route, so the two
    responses cannot be diffed against each other.

    `TrailersController` holds a DI-constructed `ItemsController` whose
    `ControllerContext` is never set, so `HttpContext`/`User` are null and the
    first statement of `ItemsController.GetItems` (`User.GetIsApiKey()`) throws
    a NullReferenceException. The vendored contract declares only 200/401/403/503
    for this path and the C# action is `[ProducesResponseType(Status200OK)]`, so
    200 is the required behaviour and Ferrofin's is the correct one.

    That leaves a cross-route oracle, which is exactly how the C# DEFINES the
    endpoint: `GetTrailers` is `GetItems(..., includeItemTypes: [Trailer],
    indexNumber: null)`. `/Items?includeItemTypes=Trailer` works on BOTH servers,
    so the CONTENT is comparable there, and Ferrofin's own `/Trailers` is held to
    equal its own `/Items` answer.

    Jellyfin's 500 is PINNED: if upstream ever fixes it, this fact flips and the
    row goes red loudly instead of passing in silence."""
    u = ctx["user"]
    q = "userId=%s&recursive=true&sortBy=SortName&sortOrder=Ascending&limit=1000" % u
    facts = {}

    st_t, tb = token_get(base, f"/Trailers?{q}", token)
    st_i, ib = token_get(base, f"/Items?{q}&includeItemTypes=Trailer&fields=Path", token)
    facts["items_trailer_status_200"] = st_i == 200

    jellyfin = ctx.get("server") == "jellyfin"
    # Each server against ITS OWN documented behaviour, folded into one fact that
    # must be True on both. Ferrofin must serve the contract's 200; Jellyfin's
    # 500 is PINNED here, so an upstream fix turns this False on the Jellyfin
    # side and the row goes red loudly instead of passing in silence.
    facts["route_matches_documented_behaviour"] = (st_t == 500) if jellyfin else (st_t == 200)
    # Ferrofin's hand-rolled /Trailers must answer what its own /Items does for
    # the same query — the C# route IS literally that delegation. VACUOUS on the
    # Jellyfin side, which never returns a body to compare; stated rather than
    # hidden, and it is a Ferrofin self-consistency check, not a parity claim.
    tbi = (tb or {}).get("Items") or []
    ibi = (ib or {}).get("Items") or []
    facts["trailers_equals_own_items_where_the_route_works"] = jellyfin or (
        [i.get("Id") for i in tbi] == [i.get("Id") for i in ibi]
        and (tb or {}).get("TotalRecordCount") == (ib or {}).get("TotalRecordCount"))

    items = (ib or {}).get("Items") or []
    facts["count"] = len(items)
    facts["names"] = sorted(i.get("Name") or "" for i in items)
    facts["all_types_are_Trailer"] = all(i.get("Type") == "Trailer" for i in items)
    facts["total_equals_len"] = (ib or {}).get("TotalRecordCount") == len(items)
    facts["start_index_zero"] = (ib or {}).get("StartIndex") == 0
    # A non-empty corpus is what makes `names`/`count` mean anything; the fixture
    # ships trailer extras (suite/perf/gen-fixtures.sh) so this must hold.
    facts["corpus_not_empty"] = len(items) > 0
    return facts
def guide_window_body(ctx, body):
    """`body` plus the `MinStartDate`/`MaxStartDate` window BOTH servers hold.

    A no-op when `shared_guide_window` found no overlap to name — the leg then
    runs unpinned and its diff says so, rather than the harness inventing a
    window neither server covers.
    """
    if ctx.get("guide_from") and ctx.get("guide_to"):
        return {**body, "MinStartDate": ctx["guide_from"], "MaxStartDate": ctx["guide_to"]}
    return dict(body)


READS = [
    plain("GET /System/Info", "/System/Info"),
    # The dashboard's plugin-page LIST. Jellyfin serves five entries here — its
    # five IN-TREE provider plugins, compiled into `MediaBrowser.Providers` —
    # and Ferrofin served `[]`, recorded (backwards) as "plugin configuration
    # pages come from external plugins Ferrofin doesn't host". Both filter legs
    # are probed so the `enableInMainMenu` predicate is exercised in both
    # directions rather than only where it happens to select everything.
    multi("GET /web/ConfigurationPages", [
        "/web/ConfigurationPages",
        "/web/ConfigurationPages?enableInMainMenu=false",
        "/web/ConfigurationPages?enableInMainMenu=true",
    ]),
    # ...and the configuration those pages edit. The ids are the `Id` overrides
    # on the five `Plugin.cs` files, so they are the same on any Jellyfin; the
    # bodies are each plugin's `PluginConfiguration` defaults. Ferrofin 404'd all
    # five, which is what made the settings unreachable rather than merely
    # unstyled.
    multi("GET /Plugins/{pluginId}/Configuration", [
        "/Plugins/b8715ed1-6c47-4528-9ad3-f72deb539cd4/Configuration",   # TMDb
        "/Plugins/872a7849-1171-458d-a6fb-3de3d442ad30/Configuration",   # Studio Images
        "/Plugins/a628c0da-fac5-4c7e-9d1a-7134223f14c8/Configuration",   # OMDb
        "/Plugins/8c95c4d2-e50c-4fb0-a4f3-6c06ff0f9a1a/Configuration",   # MusicBrainz
        "/Plugins/a629c0da-fac5-4c7e-931a-7174223f14c8/Configuration",   # AudioDB
    ]),
    # The manifest of the same five shared ids. Upstream models
    # `GetPluginManifest` as a POST; it is a read, so it is diffed here rather
    # than journeyed. Its wire type is the ONE camelCase DTO on this surface
    # (`PluginManifest` carries an explicit `[JsonPropertyName]` on every
    # property and spells `Id` as `guid`), and Ferrofin answered with a
    # five-field PascalCase projection of its own invention — 200 on both sides,
    # unrelated bodies, invisible to the breadth sweep and covered by an
    # "instance: the two servers share no plugin id" note that stopped being
    # true the moment those five ids were registered.
    #
    # `status` is compared too, deliberately: it is the field
    # `PluginManager.ProcessAlternative` flips to `Restart` in memory after an
    # enable/disable, which is the one part of this surface that still diverges
    # (see the classification on POST /Plugins/{pluginId}/{version}/Enable).
    # On the lane-3 pair that makes the TMDb leg red TODAY — an earlier
    # reviewer's Enable probe left Jellyfin's in-memory TMDb at
    # `status: "Restart"`, `autoUpdate: false` — while the other four ids are
    # byte-identical. Keeping the leg is the point: dropping the one id that can
    # diverge would leave nothing here able to see the divergence at all.
    multi("POST /Plugins/{pluginId}/Manifest", [
        "/Plugins/b8715ed1-6c47-4528-9ad3-f72deb539cd4/Manifest",   # TMDb
        "/Plugins/872a7849-1171-458d-a6fb-3de3d442ad30/Manifest",   # Studio Images
        "/Plugins/a628c0da-fac5-4c7e-9d1a-7134223f14c8/Manifest",   # OMDb
        "/Plugins/8c95c4d2-e50c-4fb0-a4f3-6c06ff0f9a1a/Manifest",   # MusicBrainz
        "/Plugins/a629c0da-fac5-4c7e-931a-7174223f14c8/Manifest",   # AudioDB
    ], method="POST"),
    plain("GET /System/Endpoint", "/System/Endpoint"),
    plain("GET /Localization/Cultures", "/Localization/Cultures"),
    plain("GET /Users/Me", "/Users/Me"),
    plain("GET /Sessions", "/Sessions"),
    user("GET /UserViews", "/UserViews?userId={u}"),
    user("GET /Library/MediaFolders", "/Library/MediaFolders"),
    # `GET /Items/Root` takes NO item id — both servers resolve the same
    # deterministic UserRootFolder — so sweep's single-item pass was already
    # comparing like for like and its diff was honest. It lives here now so the
    # ledger sources it from a full body diff rather than a Layer-1 spot check.
    user("GET /Items/Root", "/Items/Root?userId={u}"),
    user("GET /Library/VirtualFolders", "/Library/VirtualFolders"),
    # SIX legs, because the recursive item universe is what this op is, and a
    # Movie-only leg cannot see a whole kind going missing from it. The first
    # two are order-checked (see `with_item_order`).
    #
    # Leg 2 is the `UserRootFolder` browse, and it is not a duplicate of
    # `GET /Items/Root`: C# `ItemsController.GetItems` answers it from
    # `folder.GetChildren(user, true)` instead of a query (ItemsController.cs:307,
    # 525-529), which is a code path nothing else in this layer reaches — no
    # sort, no paging, no item-type filter, and the aggregate's virtual children
    # APPENDED rather than sorted in. Measured on 10.11.8, `sortBy=…`,
    # `limit=…&startIndex=…` and `includeItemTypes=…` all leave the answer
    # unchanged, so the leg pins the branch as well as the order.
    #
    # Leg 3 is an AUDIO leg alongside the movie one: the movie page can never
    # see a field that only an `Audio` DTO carries, and `HasLyrics` — which C#
    # `DtoService` emits on every Audio row outside the `ItemFields` system —
    # was missing from every Ferrofin audio DTO with nothing to catch it.
    multi("GET /Items", [
        ("/Items?userId={u}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=SortName&fields=Path",
         with_item_order),
        ("/Items?userId={u}&parentId={root}&sortBy=SortName&sortOrder=Descending&limit=2&startIndex=1",
         with_item_order),
        "/Items?userId={u}&recursive=true&includeItemTypes=Audio&limit=50&sortBy=SortName&fields=Path",
        # The guide. Jellyfin stores every airing as a real `BaseItems` row
        # parented to the channel and TOP-parented to the Live TV `UserView`
        # (`CreateItems(newPrograms, currentChannel, …)`, v10.11.8
        # src/Jellyfin.LiveTv/Guide/GuideManager.cs:277), so the recursive
        # universe holds the whole guide. Ferrofin held none of it — measured
        # F 0 / J 338 — while `/LiveTv/Programs` answered 338 on both, so the
        # data was there and only the item rows were missing.
        #
        # PROJECTED, and this is the one leg on this row that is. Each server's
        # guide is a ROLLING window anchored to its own last refresh
        # (GuideManager.cs:215-231), and `/Items` — unlike `/LiveTv/Programs` —
        # has NO `minStartDate`/`maxStartDate` to pin one with, so the two pages
        # are index-aligned across two different windows and every airing's
        # Name/StartDate/EndDate reads as a divergence the moment either server
        # refreshes. That is per-instance refresh state, not a body difference.
        # What the leg claims instead is exactly what it exists to claim: that
        # the guide is REACHABLE through this route at all, in the same size and
        # with the same DTO shape. The per-airing body IS fully diffed — by the
        # `POST /LiveTv/Programs` legs inside the shared window, and by the
        # `GET /Items/{itemId}` programme leg, which is seeded from inside that
        # same window and so names the SAME airing on both servers.
        ("/Items?userId={u}&recursive=true&includeItemTypes=LiveTvProgram"
         "&limit=100000&sortBy=StartDate,SortName&enableTotalRecordCount=true",
         lambda b: {"TotalRecordCount": b.get("TotalRecordCount"),
                    "PageLength": len(b.get("Items") or []),
                    "Types": sorted({i.get("Type") for i in (b.get("Items") or [])}),
                    # The union of keys across the page: a field the mirror
                    # failed to carry would drop out of it.
                    "ItemKeys": sorted({k for i in (b.get("Items") or []) for k in i})}),
        # …and the same page under `locationTypes=FileSystem`, which must be
        # EMPTY on both. An airing has no file behind it, so
        # `LiveTvProgram`'s constructor sets `IsVirtualItem = true` (v10.11.8
        # MediaBrowser.Controller/LiveTv/LiveTvProgram.cs:26-29), and
        # `ItemsController.GetItems` translates `locationTypes` onto
        # `query.IsVirtualItem` (:437-447). Ferrofin ignored BOTH
        # `locationTypes` and `excludeLocationTypes` entirely, so this leg
        # answered with the whole guide where Jellyfin answers with nothing —
        # a filter the client believes it applied and the server never did.
        # This leg's assertion IS emptiness, so it is only meaningful next to
        # the non-empty one above.
        "/Items?userId={u}&recursive=true&includeItemTypes=LiveTvProgram"
        "&locationTypes=FileSystem&limit=100000&enableTotalRecordCount=true",
        # The unfiltered recursive page, projected to its TYPE CENSUS and total.
        #
        # The projection is not a narrowing of what is compared — every kind on
        # this page is body-diffed by its own leg or its own row — it is the ONE
        # thing no other leg can see: which kinds exist in the universe at all,
        # and how the grouping collapses them. With a user and an EMPTY
        # `includeItemTypes`, `EnableGroupByPresentationUniqueKey` is TRUE
        # (Jellyfin.Server.Implementations/Item/BaseItemRepository.cs:1557-1589)
        # and the query groups on `PresentationUniqueKey`. Only
        # `MetadataService.UpdatePresentationUniqueKey` ever populates that
        # column (MediaBrowser.Providers/Manager/MetadataService.cs:332-336) and
        # the guide refresh calls `RefreshMetadata` on a CHANNEL only
        # (GuideManager.cs:305) — so every airing's key is NULL, SQLite groups
        # NULLs together, and the entire guide shows up here as exactly ONE
        # `Program`. Verified in the oracle database: 4 channel rows carry their
        # own id as the key, all 338 programme rows carry NULL. A mirror that
        # minted a key per airing would move this census from 1 to 338 and put
        # the whole guide on every user's home screen.
        ("/Items?userId={u}&recursive=true&limit=100000&enableTotalRecordCount=true",
         lambda b: {"TotalRecordCount": b.get("TotalRecordCount"),
                    "TypeCensus": dict(sorted(collections.Counter(
                        i.get("Type") for i in (b.get("Items") or [])).items()))}),
    ], caveats=[
        "The per-airing BODY on the LiveTvProgram leg — that leg compares only "
        "TotalRecordCount, the page length, the Type set and the UNION of DTO "
        "keys across the page. Each server's guide is a rolling window anchored "
        "to its own last refresh (v10.11.8 GuideManager.cs:215-231) and `/Items` "
        "has no minStartDate/maxStartDate to pin one with, so the two pages are "
        "index-aligned across two different windows. Every field of a programme "
        "IS diffed, by the `POST /LiveTv/Programs` legs inside the shared window "
        "and by the `GET /Items/{itemId}` programme leg seeded from it.",
        "The item bodies on the unfiltered recursive leg — it compares only "
        "TotalRecordCount and the per-Type census, which is the one thing no "
        "other leg can see (which kinds exist in the universe, and how "
        "`EnableGroupByPresentationUniqueKey` collapses them). Every kind on "
        "that page is body-diffed by its own leg or its own row.",
    ]),
    user("GET /Items/Latest", "/Items/Latest?userId={u}&limit=20&fields=Path"),
    user("GET /UserItems/Resume", "/UserItems/Resume?userId={u}&limit=12&fields=Path"),
    user("GET /Shows/NextUp", "/Shows/NextUp?userId={u}&limit=24"),
    user("GET /Shows/Upcoming", "/Shows/Upcoming?userId={u}&limit=24"),
    user("GET /Genres", "/Genres?userId={u}"),
    user("GET /Persons", "/Persons?userId={u}&limit=100"),
    user("GET /Studios", "/Studios?userId={u}"),
    user("GET /Items/Filters", "/Items/Filters?userId={u}&includeItemTypes=Movie"),
    user("GET /Items/Filters2", "/Items/Filters2?userId={u}&includeItemTypes=Movie"),
    # `SuggestionsController` orders Random too, but unlike /Similar it runs the
    # SAME query on both servers — Random only PERMUTES, it does not change the
    # set. At limit >= |universe| the answer is fully determined, and
    # parity_diff aligns Items by Path, so this is a real whole-DTO diff of all
    # 500 movies. limit=10 sampled 10 of 552 at random and could never be clean.
    multi("GET /Items/Suggestions", [
        "/Items/Suggestions?userId={u}&type=Movie&limit=100000&enableTotalRecordCount=true",
        "/Items/Suggestions?userId={u}&mediaType=Audio&limit=100000&enableTotalRecordCount=true",
        "/Items/Suggestions?userId={u}&type=Series&limit=100000&enableTotalRecordCount=true",
        "/Items/Suggestions?userId={u}&type=Episode&limit=100000&enableTotalRecordCount=true",
        "/Items/Suggestions?userId={u}&type=MusicAlbum&limit=100000&enableTotalRecordCount=true",
        # The kinds Ferrofin's recursive user-root universe is missing entirely
        # (MusicArtist/Folder/TvChannel/UserView: F 0 vs J 3/3/2/1). Kept in the
        # probe on purpose — the client-visible answer for those types really is
        # wrong, and dropping the leg would only hide it.
        "/Items/Suggestions?userId={u}&type=MusicArtist&limit=100000&enableTotalRecordCount=true",
        "/Items/Suggestions?userId={u}&type=TvChannel&limit=100000&enableTotalRecordCount=true",
        # Paging contract only. `SuggestionsController` orders `(Random,
        # Descending)` (v10.11.8 SuggestionsController.cs:83) on BOTH servers,
        # so a page NARROWER than the universe draws a different subset per
        # call — measured 2026-08-29: three consecutive calls returned three
        # different triples on each server. The full-limit legs above are the
        # ones that compare Items; here the Items array is provably
        # non-comparable and the leg is projected onto what IS determined:
        # StartIndex echoes, TotalRecordCount is page-independent, and the page
        # holds exactly `limit` rows. Diffing the Items here would keep the row
        # red forever and destroy its ability to signal when the two REAL
        # divergences (item universe, MediaSources/MediaStreams fields) are
        # fixed — a probe that can never go green is not a gate.
        ("/Items/Suggestions?userId={u}&type=Movie&limit=3&startIndex=7&enableTotalRecordCount=true",
         lambda b: {"StartIndex": b.get("StartIndex"),
                    "TotalRecordCount": b.get("TotalRecordCount"),
                    "PageLength": len(b.get("Items") or [])}),
    ], caveats=[
        "Items on the ONE narrow-page leg (limit=3&startIndex=7) — not on the "
        "seven full-limit legs above, which diff the whole universe field for "
        "field. `SuggestionsController` orders `(Random, Descending)` "
        "(v10.11.8 SuggestionsController.cs:83) on BOTH servers, so a page "
        "narrower than the universe draws a different subset per call (measured "
        "2026-08-29: three consecutive calls returned three different triples on "
        "each server). That leg compares what IS determined — the StartIndex "
        "echo, the page-independent TotalRecordCount, and the page length.",
    ]),
    user("GET /Movies/Recommendations", "/Movies/Recommendations?userId={u}"),
    user("GET /Search/Hints", "/Search/Hints?userId={u}&searchTerm=a&limit=20"),
    # item-scoped — correlated by Path, each server queried with its own id
    # …plus a Live TV channel, which Jellyfin resolves through this very route
    # because `GuideManager.GetChannel` stores every channel as a real
    # `BaseItems` row parented to the Live TV view. The channel id is NOT
    # correlated by Path — it does not need to be: `GetInternalChannelId` hashes
    # the tuner's own id, so both servers mint the same GUID from the same
    # lineup, and each side is asked for its own `{channel}` anyway.
    # …and a guide programme, for the same reason and with the same identity
    # argument: `LiveTvDtoService.GetInternalProgramId` hashes
    # `{listingsProviderId}_{start:O}_{channelExternalId}`, all three of which
    # both servers derive from the same XMLTV and the same M3U, so the GUID is
    # the same on both (measured: 328 of 338 ids join exactly, the ten-row gap
    # being the rolling guide window sliding between the two refreshes). The
    # seed is picked inside the SHARED guide window so both sides name the same
    # airing.
    item("GET /Items/{itemId}", "/Items/{i}?userId={u}&fields=Path,MediaSources,MediaStreams,Overview,Genres",
         extra=["/Items/{channel}?userId={user}&fields=Path,MediaSources,MediaStreams,Overview,Genres",
                "/Items/{program}?userId={user}&fields=Path,MediaSources,MediaStreams,Overview,Genres"]),
    # A movie seed cannot be body-diffed (Random order + a deliberately different
    # candidate algorithm) — verified by properties instead, see
    # `similar_invariants`.
    # The six plugin ops. All six are stamped `property`, never `body-diff`:
    # no shared plugin id exists between a Rust server with compiled-in
    # extensions and a stock Jellyfin with five bundled .NET provider plugins,
    # so there is no shared subject to diff. What WOULD earn a body diff is the
    # same plugin installed on both, which Jellyfin can only do with a .NET
    # assembly — see the residual in classifications.json.
    invariant("GET /Plugins/{pluginId}/Configuration", plugin_configuration_invariants),
    invariant("POST /Plugins/{pluginId}/Configuration", plugin_configuration_write_invariants),
    invariant("POST /Plugins/{pluginId}/Manifest", plugin_manifest_invariants),
    invariant("GET /Plugins/{pluginId}/{version}/Image", plugin_image_invariants),
    # The two state-toggle ops. Same rule as above — one probe per route, each
    # exercising the route it is named for; the sibling verb only ever appears as
    # setup or restore, never as the measurement. See the mutation-hygiene note
    # above `plugin_status`.
    invariant("POST /Plugins/{pluginId}/{version}/Enable", plugin_enable_invariants),
    invariant("POST /Plugins/{pluginId}/{version}/Disable", plugin_disable_invariants),
    invariant("GET /Items/{itemId}/Similar", similar_invariants_for("Items")),
    # …plus the PLAYLISTS FOLDER, whose single ancestor is the `AggregateFolder`
    # `LibraryManager.CreateRootFolder()` parents it to (LibraryManager.cs:855-885).
    # The five movie seeds all stop at the `UserRootFolder`, so this leg is what
    # actually exercises the aggregate hop.
    item("GET /Items/{itemId}/Ancestors", "/Items/{i}/Ancestors?userId={u}",
         extra_seeds=("playlists_folder",)),
    item("GET /Items/{itemId}/PlaybackInfo", "/Items/{i}/PlaybackInfo?userId={u}"),
    item("GET /Items/{itemId}/Images", "/Items/{i}/Images"),
    # The Identify-dialog id fields, and the metadata editor that re-serves them.
    # FANNED ACROSS KINDS ON PURPOSE: the descriptor list is chosen by
    # `Supports(item)`, so a Movie-only probe (what sweep did) cannot see the
    # Season/Episode/Audio arms — and the MusicBrainz block was ordered wrongly
    # in exactly the Audio arm. Intra-provider ORDER is part of the response
    # (`ProviderManager` sorts `OrderBy(ProviderName)`, which is STABLE), so the
    # deep diff must stay order-sensitive here; do not relax it to a set compare.
    multi("GET /Items/{itemId}/ExternalIdInfos", [
        "/Items/{movie}/ExternalIdInfos",
        "/Items/{series}/ExternalIdInfos",
        "/Items/{season}/ExternalIdInfos",
        "/Items/{episode}/ExternalIdInfos",
        "/Items/{album_id}/ExternalIdInfos",
        "/Items/{audio_id}/ExternalIdInfos",
        "/Items/{person_id}/ExternalIdInfos",
    ]),
    multi("GET /Items/{itemId}/MetadataEditor", [
        "/Items/{movie}/MetadataEditor",
        "/Items/{series}/MetadataEditor",
        "/Items/{season}/MetadataEditor",
        "/Items/{episode}/MetadataEditor",
        "/Items/{album_id}/MetadataEditor",
        "/Items/{audio_id}/MetadataEditor",
        "/Items/{person_id}/MetadataEditor",
    ]),
    # "Choose Image". Same reason for fanning: Ferrofin listed NO provider for a
    # Season and only OMDb for an Episode, which a movie-only probe cannot show.
    multi("GET /Items/{itemId}/RemoteImages/Providers", [
        "/Items/{movie}/RemoteImages/Providers",
        "/Items/{series}/RemoteImages/Providers",
        "/Items/{season}/RemoteImages/Providers",
        "/Items/{episode}/RemoteImages/Providers",
        "/Items/{album_id}/RemoteImages/Providers",
        "/Items/{audio_id}/RemoteImages/Providers",
        "/Items/{person_id}/RemoteImages/Providers",
    ]),
    # The image candidates themselves. The unscoped leg is kept even though it
    # cannot come back clean — Ferrofin ships fanart.tv and Jellyfin bakes an
    # OMDb key, both recorded in classifications.json — because dropping it would
    # leave the endpoint's real shape (TotalRecordCount, Providers) unchecked.
    # The `providerName=TheMovieDb` legs are the strict ones: both servers run the
    # same provider there, so those bodies must match field for field AND in
    # order, which is what pins the preferred-language filter and
    # `OrderByLanguageDescending`. Live TMDB, so a candidate list that changes
    # upstream between the two calls shows here as noise, not a server bug.
    multi("GET /Items/{itemId}/RemoteImages", [
        "/Items/{movie}/RemoteImages",
        "/Items/{movie}/RemoteImages?providerName=TheMovieDb",
        "/Items/{movie}/RemoteImages?providerName=TheMovieDb&includeAllLanguages=true",
        "/Items/{movie}/RemoteImages?providerName=TheMovieDb&type=Logo",
        "/Items/{series}/RemoteImages?providerName=TheMovieDb",
    ]),
    invariant("GET /Movies/{itemId}/Similar", similar_invariants_for("Movies")),
    invariant("GET /Trailers/{itemId}/Similar", similar_invariants_for("Trailers")),
    # The NON-movie seeds ARE diffable: upstream master serves Series/MusicAlbum/
    # MusicArtist from the same flat Genres/Tags query 10.11.8 builds, so the two
    # servers run the same algorithm — INCLUDING its
    # `OrderBy = [(ItemSortBy.Random, Ascending)]`, which Ferrofin now emits too
    # (it briefly ordered SortName, which answered a different ITEM at
    # limit<universe: `/Items/{audio}/Similar?limit=1` gave "Track 02" on five
    # straight Ferrofin calls while Jellyfin alternated 02/03).
    #
    # What that costs the probe, stated plainly: Random on both sides means the
    # ORDER can never be diffed, and neither can any page narrower than the
    # candidate pool. So `limit` is pinned FAR above the fixture's pool, where
    # the answer is the whole set and therefore fully determined; parity_diff
    # aligns the array by Name, so what these rows actually gate is the SET,
    # TotalRecordCount and every per-item DTO field. A narrower page here would
    # be red forever on both servers and prove nothing.
    #
    # These rows were never probed at all before (sweep fed all six aliases a
    # MOVIE id), which is why the music/series similarity path went unverified.
    user("GET /Shows/{itemId}/Similar", "/Shows/{series}/Similar?userId={u}&limit=1000"),
    # NOTE: green-but-vacuous on the current fixture — gen-fixtures.sh gives each
    # of the 3 albums a UNIQUE genre, so the candidate universe is empty on BOTH
    # servers and this row diffs `{"Items": [], "TotalRecordCount": 0}` against
    # itself. Recorded in classifications.json so a ledger reader sees it; the
    # fixture now shares a genre across two artists, which bites at the next
    # wipe-and-provision. Until then the album matching is covered by the
    # `an_album_seed_matches_albums_sharing_a_genre_only` unit test.
    user("GET /Albums/{itemId}/Similar", "/Albums/{album_id}/Similar?userId={u}&limit=1000"),
    user("GET /Artists/{itemId}/Similar", "/Artists/{artist_id}/Similar?userId={u}&limit=1000"),
    # by-name + shows (need the NFO metadata + shows fixture); {genre}/{studio}/{person} are
    # URL-encoded names shared across servers, {series} is the per-server first-series id.
    user("GET /Genres/{genreName}", "/Genres/{genre}?userId={u}"),
    user("GET /Studios/{name}", "/Studios/{studio}?userId={u}"),
    user("GET /Persons/{name}", "/Persons/{person}?userId={u}"),
    user("GET /Shows/{seriesId}/Seasons", "/Shows/{series}/Seasons?userId={u}"),
    user("GET /Shows/{seriesId}/Episodes", "/Shows/{series}/Episodes?userId={u}"),
    # by-name music (needs the music fixture: tagged tracks → identical artists/genres on
    # both); {artist}/{musicgenre} are URL-encoded names, {artist_id} is per-server.
    user("GET /Artists", "/Artists?userId={u}"),
    user("GET /Artists/AlbumArtists", "/Artists/AlbumArtists?userId={u}"),
    user("GET /Artists/{name}", "/Artists/{artist}?userId={u}"),
    # The by-name list plumbing (order, name range, includeItemTypes scoping) is
    # shared by /Genres, /Studios, /Artists and /MusicGenres, so one family of
    # aliases here covers the same bugs on all of them. `with_item_order` makes
    # the ordering diffable — `diff` aligns arrays by Name, which is what let a
    # dropped `sortOrder` diff clean.
    multi("GET /MusicGenres", [
        ("/MusicGenres?userId={u}", with_item_order),
        ("/MusicGenres?userId={u}&limit=100", with_item_order),
        ("/MusicGenres?userId={u}&startIndex=1&limit=5", with_item_order),
        ("/MusicGenres?userId={u}&sortBy=SortName&sortOrder=Descending", with_item_order),
        ("/MusicGenres?userId={u}&nameStartsWithOrGreater=J", with_item_order),
        ("/MusicGenres?userId={u}&nameStartsWithOrGreater=j", with_item_order),
        ("/MusicGenres?userId={u}&nameLessThan=Jb", with_item_order),
        ("/MusicGenres?userId={u}&includeItemTypes=Audio", with_item_order),
        ("/MusicGenres?userId={u}&includeItemTypes=Movie", with_item_order),
    ]),
    user("GET /MusicGenres/{genreName}", "/MusicGenres/{musicgenre}?userId={u}"),
    # `/Years` had NO depth probe at all — the only coverage was the breadth
    # sweep's single-item align, and it could not see that the handler ignored
    # every filter and sort parameter in the C# signature (measured: J
    # `?includeItemTypes=Series` -> ["2021"], F -> all three years; `?recursive=
    # false` -> J [], F all three; `?sortBy=SortName&sortOrder=Descending` a
    # silent no-op). `years_page` keeps the whole body except the one upstream-
    # buggy key (see its comment) and ADDS the year order as a diffable field.
    multi("GET /Years", [
        ("/Years?userId={u}&sortBy=SortName&sortOrder=Ascending", years_page),
        ("/Years?userId={u}&sortBy=SortName&sortOrder=Descending", years_page),
        ("/Years?userId={u}&sortBy=SortName&includeItemTypes=Series", years_page),
        ("/Years?userId={u}&sortBy=SortName&includeItemTypes=Movie", years_page),
        ("/Years?userId={u}&sortBy=SortName&excludeItemTypes=Movie", years_page),
        ("/Years?userId={u}&sortBy=SortName&mediaTypes=Audio", years_page),
        # `recursive=false` walks the ROOT's direct children (the library
        # folders), which carry no ProductionYear — so the honest answer is an
        # empty page, and Ferrofin used to return every year in the library.
        ("/Years?userId={u}&sortBy=SortName&recursive=false", years_page),
        ("/Years?userId={u}&sortBy=SortName&limit=1&startIndex=1", years_page),
        # An empty-guid userId is "not provided" upstream
        # (`RequestHelpers.GetUserId` tests `userId.IsNullOrEmpty()`), not a
        # rejected id; Ferrofin used to 400 here.
        ("/Years?userId=00000000-0000-0000-0000-000000000000&sortBy=SortName", years_page),
        # THE DEFAULT CALL SHAPE — no sortBy at all, which every other leg pins.
        # It is the one axis on which the two servers do NOT agree, so it is
        # compared through `years_unordered`: every field of every year DTO, plus
        # the year SET, with only the SEQUENCE normalised. See that function for
        # why the sequence is not reproducible, and the row's `caveats` for the
        # reader who never opens this file.
        ("/Years?userId={u}", years_unordered),
    ], caveats=[
        "TotalRecordCount — dropped from every leg. Jellyfin reports the "
        "recursive MEDIA-child count out-param of "
        "`folder.GetRecursiveChildren(user, query, out totalCount)` "
        "(v10.11.8 Folder.cs:1450-1458): 559 next to 3 Items on this fixture, "
        "tracking the filter (Series -> 8, Audio -> 9, parentId=Movies -> 500). "
        "Ferrofin reports the distinct-year count, which is what pages the list; "
        "recorded as a jellyfin-bug and gated per-server by "
        "`items_root_ancestors_and_years_over_real_http`.",
        "Item ORDER on the no-sortBy leg only — the SET and every DTO field are "
        "compared, the sequence is sorted on both sides first. `GetAllItems` is "
        "`items.Select(ProductionYear).Where(> 0).Distinct()` "
        "(v10.11.8 YearsController.cs:220-227) and LINQ `Distinct()` preserves "
        "FIRST-OCCURRENCE order, so Jellyfin's resting order is the order years "
        "appear in its in-memory `Folder.GetRecursiveChildren` walk (measured "
        "2020,2022,2021); Ferrofin sorts ascending. Reproducing it needs the "
        "BaseItem tree Ferrofin deliberately does not have. Every OTHER leg pins "
        "`sortBy=SortName`, where the two agree exactly, including under paging.",
    ]),
    # Full per-year DTO diff, nothing projected away: the item counts
    # (`ChildCount`/`MovieCount`/`SeriesCount`/`AlbumCount`/`SongCount`), the
    # `UserData.Key`, `DisplayPreferencesId`, `SortName` and `Path`. The counts
    # were 0 on every Ferrofin year before this batch, and the first two fields
    # were invisible to every row in the campaign while they sat in
    # `parity_diff.VOLATILE`.
    multi("GET /Years/{year}", [
        "/Years/{year1}?userId={u}",
        "/Years/{year2}?userId={u}",
        "/Years/{year3}?userId={u}",
        # A year the fixture has no item for is still materialized on demand
        # (`LibraryManager.GetYear` always creates) and must report zero counts
        # on both — the regression guard for on-demand Year creation.
        "/Years/1850?userId={u}",
        # An explicitly-empty userId falls back to the authenticated caller.
        "/Years/{year1}?userId=00000000-0000-0000-0000-000000000000",
    ]),
    # Instant mixes are shuffled: the diff aligns by Name, so the SET of tracks is what is
    # compared (with the whole fixture under `limit`, both sides hold every track).
    user("GET /Artists/InstantMix", "/Artists/InstantMix?id={artist_id}&userId={u}&limit=100"),
    user("GET /MusicGenres/InstantMix", "/MusicGenres/InstantMix?name={musicgenre}&userId={u}&limit=100"),
    # Live TV (needs the tuner fixture): channels are keyed by Name across servers; the
    # airing programmes by Name too (the guide is identical on both).
    # Tuner DISCOVERY. `LiveTvController.DiscoverTuners([FromQuery] bool
    # newDevicesOnly = false)` (v10.11.8 LiveTvController.cs:1146-1150) →
    # `TunerHostManager.DiscoverTuners`, which UDP-broadcasts the 20-byte
    # HDHomeRun discovery datagram, waits 3 s, and then — when the flag is set —
    # drops every device whose `DeviceId` is already on a configured tuner host
    # (TunerHostManager.cs:102-121).
    #
    # Both legs are needed and neither is redundant. The fake device
    # (`suite/perf/hdhomerun-source.py`) answers the broadcast, so the `false`
    # leg diffs a real discovered `TunerHostInfo` field for field; the fixture
    # then CONFIGURES that same device, so the `true` leg must be empty on both
    # — and it is only empty because the filter runs. Ferrofin's handler took no
    # query parameter at all until this was measured: it answered the device to
    # both spellings, where Jellyfin answers it to one. With a single leg that
    # divergence is invisible.
    #
    # Two legs, ~3 s each per server, because the wait IS the protocol. The
    # `/LiveTv/Tuners/Discvover` alias route is deliberately NOT a second row:
    # it is the same handler behind a second path (the contract carries
    # upstream's typo), so a Layer-2 row there would measure the router, which
    # Layer 1 already does.
    multi("GET /LiveTv/Tuners/Discover", [
        "/LiveTv/Tuners/Discover?newDevicesOnly=false",
        "/LiveTv/Tuners/Discover?newDevicesOnly=true",
    ]),
    user("GET /LiveTv/Channels", "/LiveTv/Channels?userId={u}"),
    user("GET /LiveTv/Channels/{channelId}", "/LiveTv/Channels/{channel}?userId={u}"),
    user("GET /LiveTv/Programs", "/LiveTv/Programs?channelIds={channel}&isAiring=true&userId={u}"),
    # The BODY form of the row above. `LiveTvController.GetPrograms([FromBody]
    # GetProgramsDto)` (v10.11.8 LiveTvController.cs:654-695) builds the
    # identical `InternalItemsQuery` as the query-string overload and calls the
    # same `_liveTvManager.GetPrograms`, so an equivalent body must return the
    # equivalent guide — on each server AND across the pair. It lives here and
    # not in journeys.py for the `RemoteSearch` reason: nothing is mutated,
    # there is no read-back, and `posted` settles both STATUSES before any body
    # compare, which is what keeps a Ferrofin 422 against a Jellyfin 200 from
    # being silently dropped instead of failing.
    posted("POST /LiveTv/Programs", [
        # 1. The body twin of the GET leg above — same filters, same answer.
        post_leg("/LiveTv/Programs", lambda c: {
            "ChannelIds": [c["channel"]], "IsAiring": True, "UserId": c["u"]}),
        # 2. The whole guide inside the WINDOW BOTH SERVERS HOLD — every field of
        #    every programme in it, unpaged. The window pin is not a narrowing of
        #    what is compared: each server anchors its rolling ~7-day guide to
        #    its OWN last refresh (`GuideManager.RefreshChannelsInternal` keeps
        #    `[now - 1h, now - 1h + GuideDays)`), so an unpinned leg compares
        #    Ferrofin's window against Jellyfin's and calls the offset a body
        #    divergence. See `shared_guide_window`, which MEASURES the overlap
        #    from the two servers' own answers rather than hard-coding one.
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {"UserId": c["u"]})),
        # 3. Paging, with the tie-break PINNED. `LiveTvManager.GetPrograms`
        #    (LiveTvManager.cs:199-206) falls back to `OrderBy = [(StartDate,
        #    Ascending)]` with NO secondary key, and the fixture holds two
        #    programmes per StartDate. A page edge that cuts such a tie orders
        #    differently even between Jellyfin's OWN paged and unpaged answers,
        #    so a bare startIndex/limit leg would be permanently and
        #    meaninglessly red. Naming the second key makes the page boundary a
        #    real assertion again rather than a coin toss.
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {
            "UserId": c["u"], "StartIndex": 10, "Limit": 5,
            "SortBy": ["StartDate", "SortName"],
            "SortOrder": ["Ascending", "Ascending"]})),
        # 4. The DTO options — `Fields`, image types and user data all reach
        #    `GetProgramsDto.into_parts`'s `DtoOptions`. `Limit` cuts a
        #    StartDate tie, so the second sort key is named here too.
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {
            "UserId": c["u"], "Limit": 5,
            "SortBy": ["StartDate", "SortName"],
            "SortOrder": ["Ascending", "Ascending"],
            "Fields": ["ChannelInfo", "Overview", "Genres"],
            "EnableImages": True, "ImageTypeLimit": 1,
            "EnableImageTypes": ["Primary"], "EnableUserData": True})),
        # 5-7. THE DELIMITED-STRING FORM. Seven `GetProgramsDto` properties carry
        #    `JsonCommaDelimitedCollectionConverterFactory` upstream (`Genres`
        #    carries the PIPE factory), so `"SortBy":"StartDate"` is as valid as
        #    `["StartDate"]`, and an entry the `TypeConverter` refuses is
        #    silently dropped rather than rejected
        #    (JsonDelimitedCollectionConverter.Read). Ferrofin bound all seven as
        #    plain arrays and answered 422 to every string form; these legs are
        #    the regression pin for that fix, and leg 7 pins the DROP semantics
        #    specifically — a strict parser would 400 where Jellyfin returns the
        #    real channel's programmes.
        #
        #    These three are window-pinned as well: a leg that 422s on one side
        #    fails on the STATUS before any body compare, so the pin cannot mask
        #    the regression it exists to catch — it only stops the guide offset
        #    from reporting a second, false divergence on top of it.
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {
            "ChannelIds": c["channel"], "SortBy": "StartDate,SortName",
            "SortOrder": "Ascending,Ascending", "Fields": "Overview",
            "EnableImageTypes": "Primary", "UserId": c["u"], "Limit": 5})),
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {
            "Genres": "News", "UserId": c["u"], "Limit": 5,
            "SortBy": "StartDate,SortName", "SortOrder": "Ascending,Ascending"})),
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {
            "ChannelIds": "not-a-guid," + c["channel"],
            "SortBy": "NotASort,StartDate,SortName",
            "SortOrder": "Ascending,Ascending", "UserId": c["u"], "Limit": 5})),
        # 8. `UserId` is NOT defaulted from the caller on the body form —
        #    `body.UserId.IsNullOrEmpty() ? null : GetUserById(...)`, unlike the
        #    query-string overload. An anonymous-user guide is a different DTO.
        post_leg("/LiveTv/Programs", lambda c: guide_window_body(c, {
            "Limit": 3, "SortBy": ["StartDate", "SortName"],
            "SortOrder": ["Ascending", "Ascending"]})),
        # 9-10. THE BINDER'S REJECTION STATUS. The array arm of
        #    `JsonDelimitedCollectionConverter` stays strict, so
        #    `{"SortBy":["NotASort"]}` is a body-binding FAILURE — and ASP.NET's
        #    `[ApiController]` filter answers it 400 with ValidationProblemDetails
        #    (v10.11.8 Jellyfin.Api/BaseJellyfinApiController.cs:12-18; nothing in
        #    the tree replaces the default `InvalidModelStateResponseFactory`).
        #    axum's `Json` rejection is 422 `text/plain`, so every body-taking
        #    Ferrofin route diverged until `ferrofin-api`'s `JsonBody` extractor
        #    replaced it. Leg 10 pins the OPPOSITE half of the same defect:
        #    serde's derived impl binds a JSON sequence to a struct positionally,
        #    so Ferrofin answered `[]` with 200 and the WHOLE guide where
        #    System.Text.Json 400s — a malformed body silently accepted as a
        #    valid one.
        #
        #    Only the STATUS is the assertion here, and that is deliberate:
        #    `post_leg_outcome` settles `hs != js` BEFORE the 200 check, so a
        #    Ferrofin 422-or-200 against a Jellyfin 400 fails this row as
        #    `status`, and once both answer 400 the leg records as `unavailable`
        #    and compares nothing. The `errors` dictionary is NOT diffed: its
        #    keys and messages carry .NET type names
        #    ("Jellyfin.Data.Enums.ItemSortBy") and its `traceId` is a
        #    per-request ASP.NET activity id — neither is reproducible, and
        #    neither belongs in parity_diff.VOLATILE.
        post_leg("/LiveTv/Programs", lambda c: {"SortBy": ["NotASort"]}),
        post_leg("/LiveTv/Programs", lambda c: []),
    ]),
    user("GET /LiveTv/Programs/Recommended", "/LiveTv/Programs/Recommended?userId={u}&isAiring=true&limit=5"),
    user("GET /LiveTv/Timers/Defaults", "/LiveTv/Timers/Defaults"),
    # `EnabledUsers` is `user.Id.ToString("N")` for every user who may use Live
    # TV (`LiveTvManager.GetLiveTvInfo`). The list's LENGTH and MEMBERSHIP are
    # exactly what a regression in that filter — a dropped
    # `EnableLiveTvAccess` check, a missing tuner-host guard — would change, so
    # they must stay compared; only the GUID itself cannot match, because each
    # server minted the `bench` account independently. Substituting each
    # server's own id for that server's username keeps a wrong count, a missing
    # user and a user who should have been filtered out all failing the diff.
    # An `EnabledUsers` entry in parity_diff.VOLATILE would hide all three.
    #
    # The projection also SORTS, which is a second, separate narrowing and is
    # called out here because the rest of this batch documents every one.
    # `LiveTvManager.GetLiveTvInfo` (LiveTvManager.cs:1207-1210) emits
    # `_userManager.Users.Where(IsLiveTvEnabled)` in store order, so ordering IS
    # a signal upstream — but it is not a comparable one across two servers whose
    # `bench` accounts were provisioned independently, in separate transactions,
    # with independently minted GUIDs. Sorting discards only that incomparable
    # signal: count, membership and duplicates all still fail the diff. Drop the
    # `sorted()` the day the harness provisions users in a pinned order on both
    # servers, and the row gets its ordering check back for free.
    invariant("GET /LiveTv/GuideInfo", guide_info_invariants),
    invariant("GET /LiveTv/Recordings/Groups/{groupId}", recording_group_invariants),
    user("GET /LiveTv/Info", "/LiveTv/Info",
         project=lambda b, c: {**b, "EnabledUsers": sorted(
             c["users_by_id"].get(i, i) for i in (b.get("EnabledUsers") or []))}),
    user("GET /LiveTv/TunerHosts/Types", "/LiveTv/TunerHosts/Types"),
    # The tuner/listings administration reads. Both bodies are derived entirely
    # from the shared fixture (the M3U's names/numbers/stream URLs and the
    # guide's <channel> list), so they are byte-comparable across servers even
    # though each server's listings-provider id differs — the id is resolved per
    # server into {listings_provider}, never diffed.
    user("GET /LiveTv/ChannelMappingOptions",
         "/LiveTv/ChannelMappingOptions?providerId={listings_provider}"),
    user("GET /LiveTv/ListingProviders/Lineups",
         "/LiveTv/ListingProviders/Lineups?id={listings_provider}"),

    # ------------------------------------------------------------------ Identify
    #
    # `POST /Items/RemoteSearch/<Kind>` — POST by contract, a SEARCH by
    # behaviour (see `posted`). Everything below is pinned by a PROVIDER ID, not
    # by a name: musicbrainz.org returns 96 equal-score hits for "Abbey Road"
    # and its page order is not stable request-to-request (measured), while the
    # dedup in `ProviderManager.GetRemoteSearchResults` collapses them to
    # whichever release came back first — so a name search's single result is
    # nondeterministic on ONE server, let alone two. A lookup by id takes the
    # `LookupRelease`/`LookupArtist` path and returns one stable document. That
    # is why there is no `ProviderIds`/`PremiereDate` entry in
    # parity_diff.VOLATILE for these rows: nothing here needs one.
    #
    # Needs outbound musicbrainz.org reachability from BOTH containers; with
    # none, the `retry_empty` legs drop out and the row records "no comparable
    # response" rather than passing vacuously.
    posted("POST /Items/RemoteSearch/MusicAlbum", [
        # 1. The deterministic lookup, AND the positive control for leg 3.
        #    `MusicBrainzAlbumProvider.GetSearchResults` short-circuits on a
        #    known release id, so both servers fetch the same single MB document
        #    and every field must agree. It carries the SAME `ItemId` as the
        #    gate leg with `IncludeDisabledProviders: True`, which is the
        #    override C# checks first (`ProviderManager.GetRemoteSearchResults`
        #    :801-830), so the only difference between this leg and leg 3 is the
        #    gate itself — content here and `[]` there is attributable to the
        #    checkbox and to nothing else. Without this pairing a musicbrainz.org
        #    503 satisfies leg 3 exactly as well as a working gate does.
        post_leg("/Items/RemoteSearch/MusicAlbum", lambda c: {
            "ItemId": c["album_id"],
            "SearchInfo": {"Name": "Abbey Road", "ProviderIds": {
                "MusicBrainzAlbum": "6bb3793b-f991-378e-9bff-0bd3117f2298"}},
            "IncludeDisabledProviders": True},
            retry_empty=True, tag="mb-album-reachable"),
        # 2. The dateless-release sentinel. MusicBrainz dates this release `""`;
        #    MetaBrainz still builds a `PartialDate`, so C# emits
        #    `PremiereDate: 0001-01-01T00:00:00.0000000Z` with NO `ProductionYear`
        #    (`Date?.NearestDate` / `Date?.Year`). Ferrofin used to drop the field.
        post_leg("/Items/RemoteSearch/MusicAlbum", lambda c: {
            "SearchInfo": {"Name": "Abbey Road", "ProviderIds": {
                "MusicBrainzAlbum": "372f7e64-08dd-3ffb-913a-f29e5fe2b9d5"}},
            "IncludeDisabledProviders": True}, retry_empty=True),
        # 3. The fetcher gate. The fixture's Music library has every "Metadata
        #    downloaders" box cleared, so with an `ItemId` naming an album in it
        #    and no `IncludeDisabledProviders` override, `CanRefreshMetadata` ->
        #    `IsMetadataFetcherEnabled` lets NO fetcher run: `[]` on both. This
        #    leg is the one that used to be red — Ferrofin ignored `ItemId`,
        #    `IncludeDisabledProviders` and the library entirely. Its empty
        #    answer is the ASSERTION, so it is never retried away — and for the
        #    same reason it is only credited when leg 1 proved, in the same
        #    pass, that the provider WOULD have answered.
        post_leg("/Items/RemoteSearch/MusicAlbum", lambda c: {
            "ItemId": c["album_id"],
            "SearchInfo": {"Name": "Abbey Road", "ProviderIds": {
                "MusicBrainzAlbum": "6bb3793b-f991-378e-9bff-0bd3117f2298"}},
            "IncludeDisabledProviders": False}, requires="mb-album-reachable"),
    ]),
    posted("POST /Items/RemoteSearch/MusicArtist", [
        # The artist lookup by `MusicBrainzArtist` id — `LookupArtist`, one
        # stable document, carrying the life-span begin as PremiereDate. Same
        # `ItemId` + `IncludeDisabledProviders: True` shape as the album row, so
        # it is also the gate leg's positive control.
        post_leg("/Items/RemoteSearch/MusicArtist", lambda c: {
            "ItemId": c["artist_id"],
            "SearchInfo": {"Name": "Radiohead", "ProviderIds": {
                "MusicBrainzArtist": "a74b1b7f-71a5-4011-9441-d0b5e4122711"}},
            "IncludeDisabledProviders": True},
            retry_empty=True, tag="mb-artist-reachable"),
        # …and the same fetcher gate, on this server's own `Artist 01`.
        post_leg("/Items/RemoteSearch/MusicArtist", lambda c: {
            "ItemId": c["artist_id"],
            "SearchInfo": {"Name": "Radiohead", "ProviderIds": {
                "MusicBrainzArtist": "a74b1b7f-71a5-4011-9441-d0b5e4122711"}},
            "IncludeDisabledProviders": False}, requires="mb-artist-reachable"),
    ]),
    # MusicVideo and Book have NO remote search provider on either side, so both
    # answer `[]` unconditionally and the row can only earn `empty-corpus` —
    # `verification.read_method` derives that from the diff having compared zero
    # leaves, so the row cannot borrow the body-diff headline. This is CORRECT,
    # not a gap: `git grep -n "IRemoteMetadataProvider<" v10.11.8` lists AudioDb
    # album/artist, MusicBrainz album/artist, Omdb episode/series/movie/trailer
    # and Tmdb boxset/movie/person/episode/season/series, and no MusicVideo or
    # Book arm anywhere — upstream's `MusicVideoMetadataService` and
    # `BookMetadataService` are LOCAL services. Ferrofin registers nothing for
    # either kind either, so the two empty sets have the same cause.
    #
    # The bodies are well-formed on purpose: v10.11.8 dereferences
    # `searchInfo.SearchInfo.MetadataLanguage` with no null check
    # (ProviderManager.cs, `GetRemoteSearchResults`) and answers 500 to a body
    # with no `SearchInfo`, where Ferrofin defaults it and answers `200 []`.
    # Probing that would flag the row for a Jellyfin defect no client provokes.
    posted("POST /Items/RemoteSearch/MusicVideo", [
        post_leg("/Items/RemoteSearch/MusicVideo", lambda c: {
            "SearchInfo": {"Name": "Thriller", "Artists": ["Michael Jackson"],
                           "Year": 1983, "MetadataLanguage": "en",
                           "MetadataCountryCode": "US"},
            "IncludeDisabledProviders": True}),
    ]),
    posted("POST /Items/RemoteSearch/Book", [
        post_leg("/Items/RemoteSearch/Book", lambda c: {
            "SearchInfo": {"Name": "Dune", "Year": 1965, "MetadataLanguage": "en",
                           "MetadataCountryCode": "US", "ProviderIds": {}},
            "IncludeDisabledProviders": True}),
    ]),

    # resolvable-path-param GETs the breadth sweep couldn't fill (needs a real id).
    # The add-library options. `isNewLibrary` is a DIFFERENT answer, not a hint:
    # it decides which providers come pre-ticked, so both values are probed.
    # The projection removes the four providers Ferrofin compiles in that stock
    # Jellyfin does not ship — by name, and only from this endpoint.
    multi("GET /Libraries/AvailableOptions", [
        ("/Libraries/AvailableOptions", without_ferrofin_only_providers),
        ("/Libraries/AvailableOptions?libraryContentType=movies",
         without_ferrofin_only_providers),
        ("/Libraries/AvailableOptions?libraryContentType=tvshows",
         without_ferrofin_only_providers),
        ("/Libraries/AvailableOptions?libraryContentType=music",
         without_ferrofin_only_providers),
        ("/Libraries/AvailableOptions?libraryContentType=movies&isNewLibrary=true",
         without_ferrofin_only_providers),
        ("/Libraries/AvailableOptions?libraryContentType=tvshows&isNewLibrary=true",
         without_ferrofin_only_providers),
    ]),
    user("GET /ScheduledTasks/{taskId}", "/ScheduledTasks/{task}"),
    # One row, five legs — the unfiltered listing plus each isHidden/isEnabled
    # leg. The filters are the part no layer ever exercised, and they are where
    # the `IConfigurableScheduledTask` carve-out lives.
    multi("GET /ScheduledTasks", [
        ("/ScheduledTasks", tasks_projection),
        ("/ScheduledTasks?isHidden=true", tasks_projection),
        ("/ScheduledTasks?isHidden=false", tasks_projection),
        ("/ScheduledTasks?isEnabled=true", tasks_projection),
        ("/ScheduledTasks?isEnabled=false", tasks_projection),
    ]),
    # Paths and instantaneous byte counts are the container's own; the filesystem
    # totals, the DriveType word and the key set are not — see the docstring.
    invariant("GET /System/Info/Storage", storage_invariants),
    # Jellyfin 500s on its own /Trailers route, so the row is earned through the
    # cross-route oracle the C# controller is literally defined as.
    invariant("GET /Trailers", trailers_invariants),
    multi("GET /DisplayPreferences/{displayPreferencesId}", [
        # leg 1: the virgin auto-vivified row (catches a wrong creation default).
        "/DisplayPreferences/usersettings?userId={u}&client=emby",
        # leg 2: read back after `seed_display_preferences` wrote the same DTO to
        # both servers — this is what covers the POST normalization.
        "/DisplayPreferences/usersettings?userId={u}&client=" + DISPLAY_PREFS_CLIENT,
    ]),
    user("GET /Devices/Info", "/Devices/Info?id={device}"),
    # GET /Devices/Options is exercised in the write journey (needs a device that has options set).
    # Host-filesystem browsing: both containers mount the identical fixture tree at /media/synth,
    # so the listing and the parent resolution are byte-identical — not instance-specific.
    plain("GET /Environment/DirectoryContents",
          "/Environment/DirectoryContents?path=%2Fmedia%2Fsynth%2Fmovies&includeFiles=true&includeDirectories=true"),
    plain("GET /Environment/ParentPath", "/Environment/ParentPath?path=%2Fmedia%2Fsynth%2Fmovies"),
    # One row, three legs — one per parser branch (see LYRIC_SEEDS): the synced
    # `.lrc`, the enhanced `.elrc` whose word cues are the real oracle, and the
    # plain `.txt`. Seeded with identical bytes on both servers, so every field
    # of the parsed LyricDto (line text, Start ticks, Cues, Metadata) is diffed.
    multi("GET /Audio/{itemId}/Lyrics", [
        "/Audio/{lyric_lrc}/Lyrics",
        "/Audio/{lyric_elrc}/Lyrics",
        "/Audio/{lyric_txt}/Lyrics",
    ], seed=seed_lyrics, reap=reap_lyrics),
    # Package repositories + the catalogue derived from them. Both servers ship
    # the same single default repository, so this is an ordinary body diff.
    plain("GET /Repositories", "/Repositories"),
    # Seeded onto the lab's fixed manifests so the catalogue is deterministic.
    # ONE leg, deliberately: a `multi` row unions its legs' comparable-field
    # counts, so pairing /Packages with a second, always-populated endpoint would
    # let the row score `body-diff` on the strength of the other leg even when
    # the catalogue came back empty on BOTH servers — the hollow-body shape this
    # harness exists to catch. Alone, an unreachable fixture leaves the row with
    # nothing compared and `verification.read_method` refuses to call it verified,
    # which is the honest outcome. (`/Repositories` has its own row above, and the
    # write's effect on both surfaces is the `j_repositories` journey.)
    multi("GET /Packages", [
        ("/Packages", with_package_order),
    ], seed=seed_package_repositories, reap=reap_package_repositories),
    invariant("GET /Packages/{name}", packages_by_name_invariants),
]


# ---------------------------------------------------------------- correlation

def path_id_map(base, token, user_id):
    """Path -> id for movies on one server (Path is the stable cross-server key)."""
    b = get_json(base, f"/Items?userId={user_id}&recursive=true&includeItemTypes=Movie"
                       f"&fields=Path&limit=500&sortBy=SortName", token)
    out = {}
    for it in (b or {}).get("Items", []):
        if it.get("Path"):
            out[it["Path"]] = it["Id"]
    return out


def correlate(hmap, jmap):
    """Shared Paths -> list of (ferrofin_id, jellyfin_id), capped."""
    shared = sorted(set(hmap) & set(jmap))
    return [(hmap[p], jmap[p]) for p in shared[:CORRELATE_LIMIT]]

# ---------------------------------------------------------------- run

def resolve_named(base, token, user_id):
    """Per-server context for the by-name/shows endpoints. Names are URL-encoded (shared across
    servers via the same NFO); the series id is per-server (same title on both)."""
    def first_named(path):
        """The first item of a by-name listing, by SortName so both servers pick the same one."""
        items = (get_json(base, f"{path}?userId={user_id}&limit=1&sortBy=SortName", token)
                 or {}).get("Items") or []
        return items[0] if items else {}

    def first_name(path):
        return urllib.parse.quote(first_named(path).get("Name") or "")

    def first_id(kind):
        b = get_json(base, f"/Items?userId={user_id}&recursive=true&includeItemTypes={kind}"
                           f"&limit=1&sortBy=SortName", token)
        it = (b or {}).get("Items") or []
        return it[0]["Id"] if it else ""

    def first_task():
        tasks = get_json(base, "/ScheduledTasks", token) or []
        return tasks[0]["Id"] if tasks and tasks[0].get("Id") else ""

    def first_device():
        items = (get_json(base, "/Devices", token) or {}).get("Items") or []
        return items[0]["Id"] if items and items[0].get("Id") else ""

    def playlists_folder_id():
        """The `{data}/playlists` folder, found by Path the way C#
        `CollectionManager.FindFolders` finds a container — never by Type, since
        `ManualPlaylistsFolder` is a client type name both servers now emit."""
        items = (get_json(base, "/Library/MediaFolders", token) or {}).get("Items") or []
        for it in items:
            if (it.get("Path") or "").replace("\\", "/").endswith("/playlists"):
                return it.get("Id") or ""
        return ""

    def user_root_id():
        """The `UserRootFolder`, per server (`GET /Items/Root`, which is
        `LibraryManager.GetUserRootFolder()`). Both servers derive the same GUID
        from the same application paths, but it is resolved per server rather
        than pinned so a divergence in the derivation shows up as a status/body
        difference on the leg instead of being papered over by a constant."""
        return (get_json(base, "/Items/Root", token) or {}).get("Id") or ""

    def first_years(n):
        """The first `n` year NAMES, sortBy-pinned, padded so a short fixture
        still formats every leg (a repeated year is diffed twice, never skipped)."""
        items = (get_json(base, f"/Years?userId={user_id}&sortBy=SortName"
                                f"&sortOrder=Ascending&limit={n}", token)
                 or {}).get("Items") or []
        names = [i.get("Name") for i in items if i.get("Name")]
        return (names + [names[-1]] * n)[:n] if names else ["0"] * n

    years = first_years(3)
    def users_by_id():
        """`user GUID -> username` for this server.

        The two servers create their accounts independently, so the same person
        has a different GUID on each. Any body that carries a raw user id is
        therefore undiffable as bytes but perfectly diffable as identity.
        """
        return {u["Id"]: u.get("Name") for u in (get_json(base, "/Users", token) or [])}

    artist = first_named("/Artists")
    lyric_ids = lyric_seed_ids(base, token, user_id)
    channels = (get_json(base, f"/LiveTv/Channels?userId={user_id}&limit=1", token) or {}).get("Items") or []
    listings_providers = (get_json(base, "/System/Configuration/livetv", token) or {}).get("ListingProviders") or []
    return {
        "users_by_id": users_by_id(),
        "channel": channels[0]["Id"] if channels else "",
        # The `PlaylistsFolder` — the one seed whose ancestor chain reaches the
        # `AggregateFolder`.
        "playlists_folder": playlists_folder_id(),
        # The `UserRootFolder` — the parent whose browse takes C#'s
        # `folder.GetChildren` branch instead of a query.
        "root": user_root_id(),
        # The fixture's XMLTV listings provider, per server: Jellyfin and
        # Ferrofin each mint their own id, and both the mapping-options and
        # lineups reads take it as a query parameter.
        "listings_provider": listings_providers[0].get("Id") or "" if listings_providers else "",
        "user": user_id,   # item() reads c["user"]
        "u": user_id,       # user() URL templates use {u}
        "genre": first_name("/Genres"),
        "studio": first_name("/Studios"),
        "person": first_name("/Persons"),
        "series": first_id("Series"),
        # Kind-correct seeds for the /Similar aliases and the invariant probe.
        "album_id": first_id("MusicAlbum"),
        "movie": first_id("Movie"),
        "episode": first_id("Episode"),
        # Path-derived, so these resolve to the SAME id on both servers (checked
        # live: Movie/Series/Season/Episode/MusicAlbum/Audio and the Person all
        # match byte-for-byte). That is what lets the kind-fanned rows below
        # compare like for like instead of each server's own arbitrary first item
        # — the "sweep single-item align" failure these probes exist to replace.
        "season": first_id("Season"),
        "audio_id": first_id("Audio"),
        "person_id": first_named("/Persons").get("Id") or "",
        "task": first_task(),
        "device": first_device(),
        "artist": urllib.parse.quote(artist.get("Name") or ""),
        "artist_id": artist.get("Id") or "",
        "musicgenre": first_name("/MusicGenres"),
        # The fixture's production years, by SortName so both servers pick the
        # same three (a Year's SortName is the zero-padded value, so this is
        # numeric order). Names, not ids — but the ids agree anyway, since a
        # by-name id is MD5(TypeFullName + metadata path).
        "year1": years[0],
        "year2": years[1],
        "year3": years[2],
        # The three tracks `seed_lyrics` writes to, in LYRIC_SEEDS order.
        "lyric_lrc": lyric_ids[0],
        "lyric_elrc": lyric_ids[1],
        "lyric_txt": lyric_ids[2],
    }


def first_program_in_window(base, token, window):
    """The id of the earliest airing inside `window`, or `""`.

    Ordered by `StartDate,SortName` so a tie at the window edge resolves the
    same way on both servers, exactly as the `/LiveTv/Programs` legs do.
    """
    url = "/LiveTv/Programs?limit=1&sortBy=StartDate,SortName&sortOrder=Ascending"
    if window:
        url += f"&minStartDate={window[0]}&maxStartDate={window[1]}"
    items = (get_json(base, url, token) or {}).get("Items") or []
    return items[0].get("Id") or "" if items else ""


def shared_guide_window(bases_tokens):
    """The `[MinStartDate, MaxStartDate)` BOTH servers' guides cover, as ISO-8601 Z.

    Each server holds a ROLLING guide window anchored to its own last refresh:
    `GuideManager.RefreshChannelsInternal` keeps `[now - 1h, now - 1h + GuideDays)`
    and drops the rest (v10.11.8 GuideManager.cs:215-231). Two independent
    instances therefore never hold the same window — a container restart moves
    one of them by hours — so an UNPINNED programme leg compares Ferrofin's
    window against Jellyfin's and reports the offset as a body divergence,
    which it is not.

    Pinning the window is the honest instrument here, and it is narrow: it is
    the intersection of what the two servers actually hold, measured from their
    own answers rather than hard-coded, so it shrinks when the servers disagree
    instead of hiding the disagreement. What stays compared inside it is every
    field of every programme. Guide-window ANCHORING itself is per-instance
    refresh state owned by the guide-refresh and scheduled-task rows, not by
    this op.

    Returns `None` when either server holds no guide, so the caller can leave
    the legs unpinned rather than invent a window.
    """
    edges = []
    for base, token in bases_tokens:
        lo = hi = None
        for order, pick in (("Ascending", "lo"), ("Descending", "hi")):
            body = get_json(base, "/LiveTv/Programs?limit=1&sortBy=StartDate"
                                  f"&sortOrder={order}", token) or {}
            items = body.get("Items") or []
            if not items:
                return None
            if pick == "lo":
                lo = items[0].get("StartDate")
            else:
                hi = items[0].get("StartDate")
        if not lo or not hi:
            return None
        edges.append((lo, hi))
    # ISO-8601 Z strings of one fixed shape sort lexicographically, which is
    # exactly the comparison the intersection needs.
    start, end = max(e[0] for e in edges), min(e[1] for e in edges)
    return (start, end) if start < end else None


def run(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    jt, ju = bring_up(jellyfin_url, "jellyfin")
    hc, jc = resolve_named(ferrofin_url, ht, hu), resolve_named(jellyfin_url, jt, ju)
    # `similar_invariants` holds each server to ITS OWN documented algorithm.
    hc["server"], jc["server"] = "ferrofin", "jellyfin"

    # The guide window both servers hold, so the programme legs ask for the same
    # airings on both rather than for each server's own rolling window.
    window = shared_guide_window([(ferrofin_url, ht), (jellyfin_url, jt)])
    if window is None:
        print("  live tv guide window: one server holds no programmes; the POST "
              "/LiveTv/Programs legs run UNPINNED", file=sys.stderr)
    for ctx in (hc, jc):
        ctx["guide_from"], ctx["guide_to"] = window or (None, None)
    # The programme seed for the `GET /Items/{itemId}` extra leg, taken from
    # INSIDE the shared window and ordered, so both servers name the same
    # airing. `""` (no overlap, or no guide) makes the leg skip loudly rather
    # than compare two different programmes — the same guard `{channel}` has.
    for ctx, base, token in ((hc, ferrofin_url, ht), (jc, jellyfin_url, jt)):
        ctx["program"] = first_program_in_window(base, token, window)

    pairs = correlate(path_id_map(ferrofin_url, ht, hu), path_id_map(jellyfin_url, jt, ju))

    # Write-then-read-back setup for the DisplayPreferences row (see
    # `seed_display_preferences`). Both servers get the identical body.
    hseed = seed_display_preferences(ferrofin_url, ht, hc)
    jseed = seed_display_preferences(jellyfin_url, jt, jc)
    if hseed != jseed:
        print(f"  display-preferences seed status differs: H={hseed} J={jseed}",
              file=sys.stderr)

    # The lyric row seeds and reaps itself — see `multi(..., seed=, reap=)`.

    rows = {}

    def agg_method(legs):
        """The honest method for an aggregate of diffed (jbody, hbody, compared) legs.

        None when no leg compared anything (untested); `empty-corpus` when every
        leg was two EMPTY results (an `Items: []` envelope agreeing on its own
        zeros, or a bare `[]` agreeing on nothing at all — see
        `verification.is_empty_result`); `body-diff` as soon as one leg compared
        real content.
        """
        seen = [m for m in (verification.read_method(j, h, c) for j, h, c in legs)
                if m is not None]
        if not seen:
            return None
        return (verification.BODY_DIFF if verification.BODY_DIFF in seen
                else verification.EMPTY_CORPUS)

    def record(op, clean, total, buckets, method, note=None, compared=None, caveats=None):
        """`method` is HOW the row was verified, from `verification.METHODS`, and it
        is written into the results row. There is no default: gen-ledger.py counts
        only `body-diff` in the headline, so a row that agreed on named invariants
        ("property"), or on two empty result envelopes ("empty-corpus"), must say so
        rather than borrowing a claim it did not earn. `method=None` means the probe
        compared nothing at all — recorded untested, never verified.

        `compared`, when given, is the PER-LEG count of non-volatile LEAF
        comparisons the row actually performed, rendered leg by leg next to the
        leg count. Per-leg and not a total, because the total is what hides the
        problem: "3/3 clean" reads as three times the evidence when one of the
        three legs asserted `[]` and compared nothing at all, and "3/3 clean, 19
        leaves" still does. "3/3 legs clean (14+5+0 leaves compared)" cannot.
        Without any count the page said "1/1 clean" for a row that compared 984
        fields and for a row that compared one, and a reader could not tell a
        thick body diff from a thin one without opening this file.

        `caveats` is what this row did NOT compare, in plain words. It rides on
        the row (not only in a comment in this file) so `gen-ledger.py` can print
        it under the row: a green row with a projected-away field is honest only
        while the reader can see which field."""
        if total == 0 or (method is None and not any(buckets.values())):
            rows[op] = {"deep_verified": None, "classification": "",
                        "verification_method": None,
                        "note": note or "no comparable response (nothing was compared)"}
            return
        n = sum(len(buckets[k]) for k in ("mismatch", "missing", "extra"))
        if n == 0:
            detail = {verification.BODY_DIFF: "",
                      verification.PROPERTY: " (named invariants agreed; bodies not diffed)",
                      verification.EMPTY_CORPUS: " (both result sets EMPTY; only the envelope"
                                                 " zeros compared — handler logic unexercised)",
                      }.get(method, "")
            leaves = leaf_note(compared)
            rows[op] = {"deep_verified": True, "classification": "ok",
                        "verification_method": method,
                        "note": f"{clean}/{total} legs clean{leaves}" + detail
                                + (f"; {note}" if note else "")}
            if caveats:
                rows[op]["caveats"] = list(caveats)
        else:
            sample = "; ".join(f"{m['path']}(J={m.get('j')} H={m.get('h')})"
                               for m in buckets["mismatch"][:3])
            # Dedup diff paths across the correlated items (strip the per-item [key] prefix) so the
            # detail lists each divergent FIELD once — the actionable enumeration for a fix.
            def field_paths(bucket):
                seen = {}
                for m in bucket:
                    p = re.sub(r"^\[[^\]]*\]\.?", "", m["path"])
                    seen.setdefault(p, m)
                return seen
            rows[op] = {"deep_verified": False,
                        "classification": "flagged: read diff vs Jellyfin (verify)",
                        "verification_method": method or verification.BODY_DIFF,
                        "note": f"{clean}/{total} legs clean{leaf_note(compared)}; "
                                f"mismatch:{len(buckets['mismatch'])} "
                                f"missing:{len(buckets['missing'])} extra:{len(buckets['extra'])} | {sample}"
                                + (f" | {note}" if note else ""),
                        "diffs": {
                            "missing": sorted(field_paths(buckets["missing"])),
                            "extra": sorted(field_paths(buckets["extra"])),
                            "mismatch": sorted(field_paths(buckets["mismatch"])),
                        }}
            if caveats:
                rows[op]["caveats"] = list(caveats)

    for ep in READS:
        if ep["kind"] == "invariant":
            hf = ep["fn"](ferrofin_url, ht, hc)
            jf = ep["fn"](jellyfin_url, jt, jc)
            buckets = {"mismatch": [], "missing": [], "extra": []}
            for key in sorted(set(hf) | set(jf)):
                h, j = hf.get(key), jf.get(key)
                if key not in hf:
                    buckets["missing"].append({"path": key, "j": j, "h": None})
                elif key not in jf:
                    buckets["extra"].append({"path": key, "j": None, "h": h})
                elif h != j or h is False:
                    # `h is False` also fails a fact BOTH servers get wrong —
                    # agreeing on a broken invariant is not parity.
                    buckets["mismatch"].append({"path": key, "j": j, "h": h})
            n = sum(len(v) for v in buckets.values())
            record(ep["op"], 1 if n == 0 else 0, 1, buckets, ep["method"],
                   compared=[len(set(jf) | set(hf))])
        elif ep["kind"] == "multi":
            if ep.get("seed"):
                hstat = ep["seed"](ferrofin_url, ht, hc)
                jstat = ep["seed"](jellyfin_url, jt, jc)
                if hstat != jstat:
                    print(f"  {ep['op']} seed statuses differ: H={hstat} J={jstat}",
                          file=sys.stderr)
            agg = {"mismatch": [], "missing": [], "extra": []}
            legs = []
            clean = tested = 0
            try:
                method = ep.get("http_method", "GET")
                for leg in ep["legs"]:
                    hs, hb = token_get(ferrofin_url, leg["url"](hc), ht, method)
                    js, jb = token_get(jellyfin_url, leg["url"](jc), jt, method)
                    tested += 1
                    # A leg that does not answer 200-with-JSON on both sides is a
                    # RESULT, not a reason to skip: `continue`-ing here made a
                    # status divergence (exactly what the type=MusicArtist and
                    # type=TvChannel legs exist to catch) silently invisible.
                    if hs != js:
                        agg["mismatch"].append(
                            {"path": f"{leg['tmpl']} :: status", "j": js, "h": hs})
                        continue
                    if hb is None or jb is None:
                        agg["mismatch"].append(
                            {"path": f"{leg['tmpl']} :: body",
                             "j": "no JSON" if jb is None else "json",
                             "h": "no JSON" if hb is None else "json"})
                        continue
                    if leg["project"]:
                        hb, jb = leg["project"](hb), leg["project"](jb)
                    n, b, compared = diff_stats(jb, hb)
                    legs.append((jb, hb, compared))
                    if n == 0:
                        clean += 1
                    for k in agg:
                        agg[k].extend(b[k])
            finally:
                # Whatever happened above, the seeded state comes back off both
                # servers — an aborted run must not leave the pair asymmetric.
                if ep.get("reap"):
                    ep["reap"](ferrofin_url, ht, hc)
                    ep["reap"](jellyfin_url, jt, jc)
            record(ep["op"], clean, tested, agg, agg_method(legs),
                   compared=[c for _j, _h, c in legs],
                   caveats=ep.get("caveats"))
        elif ep["kind"] == "posted":
            agg = {"mismatch": [], "missing": [], "extra": []}
            legs = []
            clean = tested = 0
            dropped = []
            unavailable = []
            proven = set()          # tags whose control returned content on BOTH servers
            for leg in ep["legs"]:
                hs = js = 0
                hb = jb = None
                if not (leg["requires"] and leg["requires"] not in proven):
                    # Both servers get the identical body, in the same run, back
                    # to back, so an upstream provider sees one state.
                    for _attempt in range(MB_RETRIES if leg["retry_empty"] else 1):
                        hs, hb = token_post(ferrofin_url, leg["url"], ht, leg["body"](hc))
                        time.sleep(MB_PACE)
                        js, jb = token_post(jellyfin_url, leg["url"], jt, leg["body"](jc))
                        # Retry while EITHER side is empty on a 200 — the
                        # rate-limiter signature, which lands on ONE server as
                        # often as on both (musicbrainz.org answers per request,
                        # not per pass). Settle on both-non-empty; a non-200 is
                        # a RESULT and is never retried. What leaves this loop is
                        # then unambiguous: both empty is the limiter, one empty
                        # after every retry is a persistent divergence and gets
                        # diffed like anything else.
                        if not leg["retry_empty"] or hs != 200 or js != 200 or (hb and jb):
                            break
                        time.sleep(MB_PACE)
                outcome = post_leg_outcome(leg, hs, hb, js, jb, proven)
                if outcome == "uncontrolled":
                    dropped.append(f"{leg['url']} (gate assertion not credited: its positive "
                                   f"control {leg['requires']!r} did not return content)")
                    continue
                if outcome == "status":
                    tested += 1
                    agg["mismatch"].append(
                        {"path": f"{leg['url']} :: status", "j": js, "h": hs})
                    continue
                if outcome == "unavailable":
                    unavailable.append(f"{leg['url']} (both HTTP {hs}, no JSON body)")
                    continue
                if outcome == "rate-limited":
                    dropped.append(f"{leg['url']} (H={len(hb)} J={len(jb)}, both empty)")
                    continue
                tested += 1
                n, b, compared = diff_stats(jb, hb)
                legs.append((jb, hb, compared))
                if n == 0:
                    clean += 1
                    if leg["tag"] and hb and jb:
                        proven.add(leg["tag"])
                for k in agg:
                    agg[k].extend(b[k])
            notes = []
            if dropped:
                notes.append("no comparable response on: " + "; ".join(dropped))
            if unavailable:
                notes.append("no body to compare on: " + "; ".join(unavailable))
            record(ep["op"], clean, tested, agg, agg_method(legs),
                   note="; ".join(notes) or None,
                   compared=[c for _, _, c in legs])
        elif ep["kind"] in ("plain", "user"):
            path = ep["url"](hc if ep["kind"] == "user" else {})
            jpath = ep["url"](jc if ep["kind"] == "user" else {})
            hs, hb = token_get(ferrofin_url, path, ht)
            js, jb = token_get(jellyfin_url, jpath, jt)
            if hb is None or jb is None:
                # Say WHICH side failed. "both empty/non-200" was written even when
                # only Ferrofin 500'd against a Jellyfin 200 — the loudest possible
                # divergence, reported as an absence of evidence.
                record(ep["op"], 0, 0, {"mismatch": [], "missing": [], "extra": []}, None,
                       note=f"no comparable response (H={hs} J={js})")
                continue
            if ep.get("project"):
                hb, jb = ep["project"](hb, hc), ep["project"](jb, jc)
            n, buckets, compared = diff_stats(jb, hb)
            record(ep["op"], 1 if n == 0 else 0, 1, buckets,
                   agg_method([(jb, hb, compared)]), compared=[compared])
        else:  # item — aggregate over correlated pairs
            agg = {"mismatch": [], "missing": [], "extra": []}
            legs = []
            clean = tested = 0
            # A declared `extra_seeds` key that either server cannot resolve is a
            # LOST LEG, not a clean run with fewer legs. Dropping it silently is
            # how the one probe that reaches the `AggregateFolder` could vanish
            # and leave the row reading BETTER than before (fewer legs, fewer
            # diffs). Every other failure mode in this loop records something;
            # so does this one.
            extra = []
            for key in ep.get("extra_seeds", ()):
                hid, jid = hc.get(key) or "", jc.get(key) or ""
                if hid and jid:
                    extra.append((hid, jid))
                    continue
                tested += 1
                agg["mismatch"].append({
                    "path": f"[extra_seeds:{key}] :: unresolved",
                    "j": jid or "(absent)", "h": hid or "(absent)"})
            for hid, jid in list(pairs) + extra:
                hs, hb = token_get(ferrofin_url, ep["url"](hc, hid), ht)
                js, jb = token_get(jellyfin_url, ep["url"](jc, jid), jt)
                if hs != js:
                    # Same blind spot the multi branch had: a status divergence
                    # is the loudest kind of divergence and must not vanish
                    # into a `continue`.
                    tested += 1
                    agg["mismatch"].append({"path": f"[{hid}] :: status", "j": js, "h": hs})
                    continue
                if hb is None or jb is None:
                    continue
                tested += 1
                n, b, compared = diff_stats(jb, hb)
                legs.append((jb, hb, compared))
                if n == 0:
                    clean += 1
                else:
                    for k in agg:
                        agg[k].extend(b[k])
            for url_of in ep.get("extra") or ():
                hu, ju = url_of(hc), url_of(jc)
                if hu is None or ju is None:
                    # Unseeded on at least one server: skip rather than compare
                    # a collapsed URL. `tested` does not move, so the row's
                    # "n/m legs clean" note shows the leg did not run.
                    print(f"    ! {ep['op']}: an extra leg has no seed on "
                          f"{'ferrofin' if hu is None else 'jellyfin'}; skipped",
                          file=sys.stderr)
                    continue
                hs, hb = token_get(ferrofin_url, hu, ht)
                js, jb = token_get(jellyfin_url, ju, jt)
                if hs != js:
                    tested += 1
                    agg["mismatch"].append({"path": f"[{hu}] :: status", "j": js, "h": hs})
                    continue
                if hb is None or jb is None:
                    continue
                tested += 1
                n, b, compared = diff_stats(jb, hb)
                legs.append((jb, hb, compared))
                if n == 0:
                    clean += 1
                else:
                    for k in agg:
                        agg[k].extend(b[k])
            record(ep["op"], clean, tested, agg, agg_method(legs),
                   compared=[c for _j, _h, c in legs])

    return rows, len(pairs)


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL", "http://localhost:18097")
    rows, npairs = run(ferrofin, jellyfin)
    out = {"generated_by": "suite/parity/reads.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "correlated_items": npairs, "rows": rows}
    with open(os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
                          "suite/parity/reads-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"] is True
             and v["verification_method"] == verification.BODY_DIFF)
    other = collections.Counter(v["verification_method"] for v in rows.values()
                                if v["deep_verified"] is True
                                and v["verification_method"] != verification.BODY_DIFF)
    print(f"wrote parity/reads-results.json — {len(rows)} read ops, {ok} deep-verified "
          f"(bodies diffed), {dict(other)} verified another way "
          f"(correlated {npairs} items by Path)")


def selfcheck():
    from parity_diff import diff_counts as dc
    # clean vs dirty diff
    assert dc({"A": 1, "Id": "x"}, {"A": 1, "Id": "y"})[0] == 0    # Id volatile → clean
    assert dc({"A": 1}, {"A": 2})[0] == 1                          # real mismatch
    # ChildCount is scrubbed ONLY on the types whose upstream value is
    # `Random.Shared.Next(1, 10)` (DtoService.cs:649-656) — and stays comparable
    # everywhere else, so a Season that lost its episodes still diffs.
    assert dc({"Type": "CollectionFolder", "ChildCount": 4},
              {"Type": "CollectionFolder", "ChildCount": 1})[0] == 0
    assert dc({"Type": "UserView", "ChildCount": 2},
              {"Type": "UserView", "ChildCount": 9})[0] == 0
    assert dc({"Type": "Season", "ChildCount": 4},
              {"Type": "Season", "ChildCount": 1})[0] == 1, \
        "ChildCount must still diff on a real folder"
    assert dc({"Type": "UserRootFolder", "ChildCount": 4},
              {"Type": "UserRootFolder", "ChildCount": 3})[0] == 1, \
        "the user root is not ICollectionFolder/UserView — its ChildCount is real"
    # `UserData.Key` is COMPARED, not masked — that is what caught Ferrofin
    # answering with the item guid on every row — and the one exemption is the
    # degenerate case where both sides are bare GUIDs (a Live TV programme,
    # whose id is minted per scan).
    assert dc({"UserData": {"Key": "Year-2020"}},
              {"UserData": {"Key": "ed00b5b8-bc89-dd6e-2a09-006c4d9c5309"}})[0] == 1, \
        "a derived key against a guid is a real mismatch"
    assert dc({"UserData": {"Key": "Year-2020"}},
              {"UserData": {"Key": "Year-2021"}})[0] == 1
    assert dc({"UserData": {"Key": "f95eb75c-8a0b-0843-d0ee-267d9cfa7ce4"}},
              {"UserData": {"Key": "d79aab57-f0cb-5d8a-aaa9-39d31ca2937e"}})[0] == 0, \
        "two per-scan guids are what Id is already volatile for"
    # `DisplayPreferencesId` is compared too: it is MD5(type FullName), the same
    # 32 hex digits on both servers for a given kind.
    assert dc({"DisplayPreferencesId": "ff93de5e82fcf2878bb0087b4854a1a5"},
              {"DisplayPreferencesId": "ed00b5b8bc89dd6e2a09006c4d9c5309"})[0] == 1
    # array align by Path across divergent ids
    j = {"Items": [{"Path": "/m/a.mkv", "Id": "j1", "Name": "A"}]}
    h = {"Items": [{"Path": "/m/a.mkv", "Id": "h1", "Name": "A"}]}
    assert dc(j, h)[0] == 0, "Path-aligned items with divergent Ids should be clean"
    # correlation intersects by Path and caps
    hm = {"/m/a": "h1", "/m/b": "h2", "/m/c": "h3"}
    jm = {"/m/b": "j2", "/m/c": "j3", "/m/d": "j4"}
    global CORRELATE_LIMIT
    pairs = correlate(hm, jm)
    assert pairs == [("h2", "j2"), ("h3", "j3")], pairs
    # every op key is a canonical METHOD /path
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(
        os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
        "contracts/jellyfin-openapi-*.json")))[-1]))
    # A read row's verb is whatever the CONTRACT gives that path — almost always
    # GET, but `POST /Plugins/{pluginId}/Manifest` is `GetPluginManifest`, a
    # read behind a POST. Accepting only GET here is what made that row
    # unprobeable, so the check is now "this method+path is in the spec", not
    # "this path is a GET".
    valid = {f"{m.upper()} {p}" for p, item in spec["paths"].items() for m in item
             if m.lower() in ("get", "post", "put", "delete", "patch", "head", "options")}
    bad = [ep["op"] for ep in READS if ep["op"] not in valid]
    assert not bad, f"read op-keys not in spec: {bad}"
    # ...and a non-GET row must actually DECLARE its verb, or it silently probes
    # the wrong endpoint with the right name.
    # NB `http_method`, not `method`: an `invariant()` row already uses `method`
    # for its VERIFICATION method ("property"). Reusing that key here made every
    # invariant row look like it was probing with a verb called "property".
    #
    # Two kinds carry their verb intrinsically instead of declaring it, and each
    # is checked on its own terms below rather than waved through:
    #   * `posted` — the runner ALWAYS issues a POST for these, so the claim to
    #     test is that the op key says POST too;
    #   * `invariant` — the probe function issues its own requests, with whatever
    #     verbs the invariant needs (a write row posts and then GETs the value
    #     back), so no single verb on the row could describe it.
    mismatched = [ep["op"] for ep in READS
                  if ep["kind"] not in ("posted", "invariant")
                  and ep["op"].split(" ", 1)[0] != ep.get("http_method", "GET")]
    assert not mismatched, f"row op verb != probe method: {mismatched}"
    not_post = [ep["op"] for ep in READS
                if ep["kind"] == "posted" and not ep["op"].startswith("POST ")]
    assert not not_post, f"posted rows whose op key is not a POST: {not_post}"
    # Every `posted` row's legs must be well-formed and buildable from a
    # populated context, and only a leg whose empty answer would be an upstream
    # artefact may set `retry_empty` — never a leg whose assertion IS `[]`.
    posted_rows = [ep for ep in READS if ep["kind"] == "posted"]
    assert posted_rows, "the posted mechanism has no rows"
    # every {placeholder} in a user() URL must be a key resolve_named() produces (guards the
    # {u} vs "user" KeyError). Format each with a fully-populated context; a KeyError fails here.
    ctx = {"user": "U", "u": "U", "genre": "G", "studio": "S", "person": "P", "series": "SE",
           "task": "T", "device": "D", "artist": "A", "artist_id": "AID", "musicgenre": "MG",
           "channel": "CH", "program": "PRG", "album_id": "ALB", "movie": "MOV", "episode": "EP",
           "listings_provider": "LP", "playlists_folder": "PLF", "root": "ROOT",
           "lyric_lrc": "L1", "lyric_elrc": "L2", "lyric_txt": "L3",
           "year1": "2020", "year2": "2021", "year3": "2022",
           # The shared guide window `run` measures; `None` is the "no overlap
           # to name" case, and the self-check must exercise it too.
           "guide_from": None, "guide_to": None,
           "season": "SEA", "audio_id": "AUD", "person_id": "PID",
           # user GUID -> username, for the projections that translate a
           # per-instance user id into a comparable identity.
           "users_by_id": {"U": "bench"}}
    # The context keys the self-check invents must be the ones resolve_named
    # actually produces, or this guard passes while the live run KeyErrors.
    import inspect
    produced = set(re.findall(r'"(\w+)":', inspect.getsource(resolve_named).split("return {", 1)[1]))
    assert produced <= set(ctx), f"self-check context missing resolve_named keys: {produced - set(ctx)}"
    for ep in READS:
        if ep["kind"] == "user":
            ep["url"](ctx)  # raises KeyError if a placeholder has no context key
        elif ep["kind"] == "item":
            # The correlated template is formatted with an id at run time; the
            # `extra` legs are formatted from the context alone, so a bad
            # placeholder in one must fail HERE and not mid-run.
            for url_of in ep.get("extra") or ():
                assert url_of(ctx) is not None, f"{ep['op']}: extra leg unseeded in the self-check ctx"
                # …and an EMPTY seed yields None (skip), never a collapsed URL
                # that quietly compares a different endpoint.
                blank = dict(ctx)
                for k in list(blank):
                    if k != "user" and isinstance(blank[k], str):
                        blank[k] = ""
                assert url_of(blank) is None, f"{ep['op']}: an unseeded extra leg must be skipped, not collapsed"
        elif ep["kind"] == "multi":
            for leg in ep["legs"]:
                leg["url"](ctx)
        elif ep["kind"] == "posted":
            # The three assertions below are about the `RemoteSearch` family's body shape
            # and its rate-limiter machinery, NOT about `posted` in general. Scoped to that
            # family on purpose: applying them to every posted row made a Live TV row —
            # which mutates nothing, is never rate-limited and has no `SearchInfo` — abort
            # the whole self-test, so the layer could not run at all.
            remote_search = ep["op"].startswith("POST /Items/RemoteSearch")
            for leg in ep["legs"]:
                body = leg["body"](ctx)
                # A leg body is normally a DTO object. The two exceptions are
                # the binder-rejection legs on POST /LiveTv/Programs, whose
                # whole point is to post a body the model binder must REFUSE —
                # a JSON sequence where the DTO is an object. Allowing a list
                # here is not a loosened assertion: those legs assert a status,
                # and a leg that could not post a malformed body could not
                # assert anything about how a malformed body is answered.
                assert isinstance(body, (dict, list)), \
                    f"{ep['op']}: a leg body must be a JSON object or array"
                if remote_search:
                    assert "SearchInfo" in body, \
                        f"{ep['op']}: a RemoteSearch body without SearchInfo makes Jellyfin 500"
                    assert not (leg["retry_empty"] and not body["IncludeDisabledProviders"]), \
                        (f"{ep['op']}: a GATED leg asserts `[]`; retrying it away would turn the "
                         "assertion into an absence of evidence")
                    assert body["IncludeDisabledProviders"] or leg["requires"], \
                        (f"{ep['op']}: a GATED leg must name a positive control in `requires` — "
                         "otherwise a rate-limited provider satisfies its `[]` exactly as well "
                         "as a working gate does")
                else:
                    # `retry_empty` only ever means "a SHARED external rate limiter can make
                    # both sides empty". Nothing outside RemoteSearch has one, so setting it
                    # elsewhere would drop a real `[] vs []` divergence.
                    assert not leg["retry_empty"], \
                        (f"{ep['op']}: retry_empty models the MusicBrainz rate limiter; a row "
                         "with no shared external provider must not set it")
                json.dumps(body)
        if ep["kind"] == "posted":
            tags = {leg["tag"] for leg in ep["legs"] if leg["tag"]}
            needed = {leg["requires"] for leg in ep["legs"] if leg["requires"]}
            assert needed <= tags, \
                f"{ep['op']}: `requires` names a control this row does not run: {needed - tags}"
    # The `posted` leg-outcome rule, exercised on the exact shapes that used to
    # be laundered into a green row.
    control = {"retry_empty": True, "tag": "ctl", "requires": None}
    gate = {"retry_empty": False, "tag": None, "requires": "ctl"}
    plain_leg = {"retry_empty": True, "tag": None, "requires": None}
    # A Ferrofin 5xx against a Jellyfin 200 is a STATUS divergence, never a drop.
    assert post_leg_outcome(plain_leg, 500, None, 200, [{"Name": "x"}], set()) == "status"
    assert post_leg_outcome(control, 503, None, 200, [{"Name": "x"}], set()) == "status"
    # Both refused identically: no divergence, and no evidence either.
    assert post_leg_outcome(plain_leg, 500, None, 500, None, set()) == "unavailable"
    # Both 200-and-empty on a retry_empty leg is the shared rate limiter.
    assert post_leg_outcome(plain_leg, 200, [], 200, [], set()) == "rate-limited"
    # …but ONE side empty is a divergence, so it must reach the diff.
    assert post_leg_outcome(plain_leg, 200, [], 200, [{"Name": "x"}], set()) == "compare"
    assert post_leg_outcome(plain_leg, 200, [{"Name": "x"}], 200, [], set()) == "compare"
    # A gate leg is only credited when its control returned content this pass.
    assert post_leg_outcome(gate, 200, [], 200, [], set()) == "uncontrolled"
    assert post_leg_outcome(gate, 200, [], 200, [], {"ctl"}) == "compare"
    # …and a clean multi-leg row must SHOW the leg that compared nothing, rather
    # than folding it into a total that reads as evidence.
    assert leaf_note(None) == "" and leaf_note([]) == ""
    assert leaf_note([14, 5, 0]) == (" (14+5+0 leaves compared; the 0 is a leg "
                                     "whose ASSERTION is emptiness)")
    assert leaf_note([14, 5]) == " (14+5 leaves compared)"
    # An all-empty row does not repeat itself: `empty-corpus` already says it.
    assert leaf_note([0]) == " (0 leaves compared)"
    # The invariant rows must carry a callable, and the diff-shaped folding of
    # its facts must flag both a disagreement AND a fact both servers fail.
    assert all(callable(ep["fn"]) for ep in READS if ep["kind"] == "invariant")
    # ...and each ALIASED invariant row (the /{kind}/{itemId}/Similar family, one
    # C# method behind six routes) must own a DISTINCT alias, or three ledger
    # rows are one measurement of the same route. An invariant row that is not
    # part of an alias family carries no `alias` and is exempt — its op key is
    # already its identity.
    # ...and each /Similar invariant row must own a DISTINCT alias, or three
    # ledger rows are one measurement of the same route. (Rows that are not part
    # of that family — storage, trailers — carry no alias; they are one row per
    # op, which `rows` already keys uniquely.)
    aliases = [ep["fn"].alias for ep in READS
               if ep["kind"] == "invariant" and hasattr(ep["fn"], "alias")]
    assert len(aliases) == len(set(aliases)), f"invariant rows share an alias: {aliases}"
    # The allow-list is the `Similar` family plus each hand-written property row.
    # Naming them keeps a typo'd alias from silently colliding with a real one.
    assert set(aliases) <= set(SIMILAR_ALIASES) | {
        "LiveTvGuideInfo", "LiveTvRecordingGroup", "PluginsConfiguration",
        "PluginsConfigurationWrite", "PluginsManifest", "PluginsImage",
        "PluginsEnable", "PluginsDisable"}, aliases
    inv_ops = [ep["op"] for ep in READS if ep["kind"] == "invariant"]
    assert len(inv_ops) == len(set(inv_ops)), inv_ops
    # Every invariant row is stamped `property`, never the body-diff method the
    # ledger headline counts.
    assert all(ep["method"] == "property" for ep in READS if ep["kind"] == "invariant")
    # A projected user() row must be a real narrowing, not a body-eraser: the
    # projection has to keep every key it was given (so it cannot make a
    # divergence vanish by dropping a field) and has to actually change the
    # per-instance value it exists to translate.
    for ep in READS:
        if ep["kind"] != "user" or not ep.get("project"):
            continue
        probe = {"Services": [{"Name": "Emby"}], "IsEnabled": True, "EnabledUsers": ["U"]}
        out = ep["project"](probe, ctx)
        assert set(out) == set(probe), f'{ep["op"]}: projection changed the key set'
        assert out["EnabledUsers"] == ["bench"], f'{ep["op"]}: projection did not translate the id'
        # A user the map does not know must survive as its raw id, so an
        # unexpected extra account still shows up as a diff.
        stray = ep["project"]({**probe, "EnabledUsers": ["U", "X"]}, ctx)
        assert stray["EnabledUsers"] == ["X", "bench"], stray

    # A projected multi leg must project BOTH sides to the same key set, and
    # must not be able to project a body away to nothing.
    for ep in READS:
        if ep["kind"] != "multi":
            continue
        for leg in ep["legs"]:
            if leg["project"]:
                body = {"StartIndex": 7, "TotalRecordCount": 500,
                        "Items": [{"Name": "x"}, {"Name": "y"}]}
                got = leg["project"](dict(body))
                assert got and all(v is not None for v in got.values()), got
                if leg["project"] is with_item_order:
                    # ADDITIVE: keeps every original key and adds the order.
                    assert set(body) <= set(got), got
                    assert got["_ItemNameOrder"] == ["x", "y"], got
    # …and the order it adds must actually be able to fail: a list of plain
    # strings is the one array shape `parity_diff` compares POSITIONALLY, which
    # is the whole point of adding it.
    assert diff_stats(with_item_order({"Items": [{"Name": "x"}, {"Name": "y"}]}),
                      with_item_order({"Items": [{"Name": "y"}, {"Name": "x"}]}))[0] > 0, \
        "reordered Items must diff once ItemOrder is attached"
    assert diff_stats({"Items": [{"Name": "x"}, {"Name": "y"}]},
                      {"Items": [{"Name": "y"}, {"Name": "x"}]})[0] == 0, \
        "…and without it the key-aligned array is order-blind (the blind spot)"
    # An `extra_seeds` key must be declared on an `item` row and must be a key
    # `resolve_named` produces, or the live run can only ever record it as
    # unresolved.
    for ep in READS:
        for key in ep.get("extra_seeds", ()):
            assert ep["kind"] == "item", ep["op"]
            assert key in produced, f"{ep['op']}: extra seed {key!r} is not resolved"
    # Every projected row must NAME what it projected away. A projection is the
    # one way a "clean" row can be clean about less than it looks like, so an
    # undeclared one is a defect in itself.
    probe = {"StartIndex": 7, "TotalRecordCount": 500,
             "Items": [{"Name": "x", "Id": "1"}, {"Name": "y", "Id": "2"}]}

    def lossy(project):
        """True when the projection DROPS or REWRITES something the body had.

        An ADDITIVE projection (`with_item_order` only appends a derived
        `_ItemNameOrder`) compares everything the body carried and needs no
        caveat; a lossy one compares less, and that has to be declared.
        """
        got = project(dict(probe))
        return any(k not in got or got[k] != v for k, v in probe.items())

    undeclared = [ep["op"] for ep in READS
                  if ep["kind"] == "multi"
                  and any(leg["project"] and lossy(leg["project"]) for leg in ep["legs"])
                  and not ep.get("caveats")]
    assert not undeclared, f"lossy-projected rows with no declared caveats: {undeclared}"
    # The detector must actually be able to tell the two apart.
    assert lossy(years_page) and lossy(years_unordered) and not lossy(with_item_order)
    # `years_unordered` must normalise the SEQUENCE and nothing else: two pages
    # with the same years in different orders converge, one with a different
    # year SET, or a differing DTO field, still diverges.
    a = {"Items": [{"Name": "2020", "ChildCount": 5}, {"Name": "2022", "ChildCount": 1}],
         "TotalRecordCount": 2, "StartIndex": 0}
    b = {"Items": [{"Name": "2022", "ChildCount": 1}, {"Name": "2020", "ChildCount": 5}],
         "TotalRecordCount": 559, "StartIndex": 0}
    assert years_unordered(a) == years_unordered(b), "order alone must not diverge"
    c = {"Items": [{"Name": "2020", "ChildCount": 5}, {"Name": "2021", "ChildCount": 1}],
         "TotalRecordCount": 2, "StartIndex": 0}
    assert years_unordered(a) != years_unordered(c), "a different year SET must still diverge"
    d = {"Items": [{"Name": "2022", "ChildCount": 9}, {"Name": "2020", "ChildCount": 5}],
         "TotalRecordCount": 2, "StartIndex": 0}
    assert years_unordered(a) != years_unordered(d), "a differing DTO field must still diverge"
    # ...and the ORDERED projection must still catch an order divergence, so the
    # eight sortBy-pinned legs keep their teeth.
    assert years_page(a) != years_page(b), "years_page must still compare order"
    # The scheduled-tasks projection must keep the wire ORDER as an explicit
    # comparable (parity_diff aligns arrays by Name and would never see it) and
    # must drop ONLY LastExecutionResult.
    proj = tasks_projection([
        {"Key": "B", "Id": "id-b", "Name": "Bee", "IsHidden": False,
         "LastExecutionResult": {"Status": "x"}},
        {"Key": "A", "Id": "id-a", "Name": "Ay", "IsHidden": True},
    ])
    assert proj["Order"] == ["Bee", "Ay"], proj
    assert set(proj["Tasks"]) == {"A", "B"}, proj
    assert "LastExecutionResult" not in proj["Tasks"]["B"], proj
    assert proj["Tasks"]["B"]["IsHidden"] is False and proj["Tasks"]["A"]["Name"] == "Ay"
    # The task Id must survive the VOLATILE denylist under an alias, or the one
    # field that ADDRESSES a task is invisible to this row's diff.
    assert verification.parity_diff.VOLATILE.match("Id"), "the alias exists because of this"
    assert not verification.parity_diff.VOLATILE.match("WireId"), "…and must survive it"
    assert proj["Tasks"]["B"]["WireId"] == "id-b", proj
    assert proj["Tasks"]["A"]["WireId"] == "id-a", proj
    # Method derivation: two EMPTY result envelopes agreeing on their own zeros is
    # not the body-diff headline, and a pair that compared nothing at all is not a
    # verdict. Both are the shapes that silently inflated the count before.
    empty = {"Items": [], "TotalRecordCount": 0, "StartIndex": 0}
    n, _, compared = diff_stats(empty, dict(empty))
    assert n == 0 and compared == 2
    assert verification.read_method(empty, dict(empty), compared) == verification.EMPTY_CORPUS
    assert verification.read_method({}, {}, 0) is None
    assert verification.read_method({"Items": [{"Name": "x"}], "TotalRecordCount": 1},
                                    {"Items": [{"Name": "x"}], "TotalRecordCount": 1},
                                    2) == verification.BODY_DIFF
    hf, jf = {"a": True, "b": True, "c": False}, {"a": True, "b": False, "c": False}
    bad = [k for k in sorted(set(hf) | set(jf))
           if hf.get(k) != jf.get(k) or hf.get(k) is False]
    assert bad == ["b", "c"], f"invariant folding wrong: {bad}"
    print(f"ok: diff, Path-align, correlation, {len(READS)} read op-keys valid, "
          f"user/multi templates fillable, invariant folding, "
          f"{len(aliases)} distinct invariant aliases, projections total + declared")


if __name__ == "__main__":
    main()

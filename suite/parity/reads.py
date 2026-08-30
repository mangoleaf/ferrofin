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


def token_get(base, path, token):
    st, raw = http("GET", base + path, token)
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


def item(op, tmpl):
    # tmpl contains {u} and {i}; filled per server (own user + own correlated item id).
    return {"op": op, "kind": "item", "url": lambda c, i: tmpl.format(u=c["user"], i=i)}


def multi(op, legs):
    """Several URLs folded into ONE ledger row (rows are keyed by op, so a second
    entry would clobber the first). Every leg is diffed; the buckets are unioned.

    A leg is a URL template, or a `(template, project)` pair whose
    `project(body)` narrows what is compared. A projection is allowed ONLY where
    the rest of the body is provably non-comparable between two independent
    instances — the Suggestions paging leg is the one case (see there) — never to
    make a divergence disappear."""
    out = []
    for leg in legs:
        tmpl, project = leg if isinstance(leg, tuple) else (leg, None)
        out.append({"tmpl": tmpl,
                    "url": (lambda c, t=tmpl: t.format(**c)),
                    "project": project})
    return {"op": op, "kind": "multi", "legs": out}


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
    user("GET /Items", "/Items?userId={u}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=SortName&fields=Path"),
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
    ]),
    user("GET /Movies/Recommendations", "/Movies/Recommendations?userId={u}"),
    user("GET /Search/Hints", "/Search/Hints?userId={u}&searchTerm=a&limit=20"),
    # item-scoped — correlated by Path, each server queried with its own id
    item("GET /Items/{itemId}", "/Items/{i}?userId={u}&fields=Path,MediaSources,MediaStreams,Overview,Genres"),
    # A movie seed cannot be body-diffed (Random order + a deliberately different
    # candidate algorithm) — verified by properties instead, see
    # `similar_invariants`.
    invariant("GET /Items/{itemId}/Similar", similar_invariants_for("Items")),
    item("GET /Items/{itemId}/Ancestors", "/Items/{i}/Ancestors?userId={u}"),
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
    # Instant mixes are shuffled: the diff aligns by Name, so the SET of tracks is what is
    # compared (with the whole fixture under `limit`, both sides hold every track).
    user("GET /Artists/InstantMix", "/Artists/InstantMix?id={artist_id}&userId={u}&limit=100"),
    user("GET /MusicGenres/InstantMix", "/MusicGenres/InstantMix?name={musicgenre}&userId={u}&limit=100"),
    # Live TV (needs the tuner fixture): channels are keyed by Name across servers; the
    # airing programmes by Name too (the guide is identical on both).
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

    def users_by_id():
        """`user GUID -> username` for this server.

        The two servers create their accounts independently, so the same person
        has a different GUID on each. Any body that carries a raw user id is
        therefore undiffable as bytes but perfectly diffable as identity.
        """
        return {u["Id"]: u.get("Name") for u in (get_json(base, "/Users", token) or [])}

    artist = first_named("/Artists")
    channels = (get_json(base, f"/LiveTv/Channels?userId={user_id}&limit=1", token) or {}).get("Items") or []
    return {
        "users_by_id": users_by_id(),
        "channel": channels[0]["Id"] if channels else "",
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
    }


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

    pairs = correlate(path_id_map(ferrofin_url, ht, hu), path_id_map(jellyfin_url, jt, ju))

    # Write-then-read-back setup for the DisplayPreferences row (see
    # `seed_display_preferences`). Both servers get the identical body.
    hseed = seed_display_preferences(ferrofin_url, ht, hc)
    jseed = seed_display_preferences(jellyfin_url, jt, jc)
    if hseed != jseed:
        print(f"  display-preferences seed status differs: H={hseed} J={jseed}",
              file=sys.stderr)

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

    def record(op, clean, total, buckets, method, note=None, compared=None):
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
        leaves" still does. "3/3 legs clean (leaves 14+5+0)" cannot."""
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
            record(ep["op"], 1 if n == 0 else 0, 1, buckets, ep["method"])
        elif ep["kind"] == "multi":
            agg = {"mismatch": [], "missing": [], "extra": []}
            legs = []
            clean = tested = 0
            for leg in ep["legs"]:
                hs, hb = token_get(ferrofin_url, leg["url"](hc), ht)
                js, jb = token_get(jellyfin_url, leg["url"](jc), jt)
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
            record(ep["op"], clean, tested, agg, agg_method(legs))
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
                   agg_method([(jb, hb, compared)]))
        else:  # item — aggregate over correlated pairs
            agg = {"mismatch": [], "missing": [], "extra": []}
            legs = []
            clean = tested = 0
            for hid, jid in pairs:
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
            record(ep["op"], clean, tested, agg, agg_method(legs))
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
    # This layer is GET-shaped, but the `posted` rows are POSTs by contract, so
    # accept both verbs — and only for a path the spec actually declares with
    # that verb, so a typo still fails here.
    valid = {f"{m.upper()} {p}" for p, item in spec["paths"].items()
             for m in item if m in ("get", "post")}
    bad = [ep["op"] for ep in READS if ep["op"] not in valid]
    assert not bad, f"read op-keys not in spec: {bad}"
    # Every `posted` row's legs must be well-formed and buildable from a
    # populated context, and only a leg whose empty answer would be an upstream
    # artefact may set `retry_empty` — never a leg whose assertion IS `[]`.
    posted_rows = [ep for ep in READS if ep["kind"] == "posted"]
    assert posted_rows, "the posted mechanism has no rows"
    # every {placeholder} in a user() URL must be a key resolve_named() produces (guards the
    # {u} vs "user" KeyError). Format each with a fully-populated context; a KeyError fails here.
    ctx = {"user": "U", "u": "U", "genre": "G", "studio": "S", "person": "P", "series": "SE",
           "task": "T", "device": "D", "artist": "A", "artist_id": "AID", "musicgenre": "MG",
           "channel": "CH", "album_id": "ALB", "movie": "MOV", "episode": "EP",
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
                assert isinstance(body, dict), f"{ep['op']}: a leg body must be a dict"
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
    # ...and each invariant row must own a DISTINCT alias, or three ledger rows
    # are one measurement of the same route.
    aliases = [ep["fn"].alias for ep in READS if ep["kind"] == "invariant"]
    assert len(aliases) == len(set(aliases)), f"invariant rows share an alias: {aliases}"
    # The allow-list is the `Similar` family plus each hand-written property row.
    # Naming them keeps a typo'd alias from silently colliding with a real one.
    assert set(aliases) <= set(SIMILAR_ALIASES) | {
        "LiveTvGuideInfo", "LiveTvRecordingGroup"}, aliases
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
                got = leg["project"]({"StartIndex": 7, "TotalRecordCount": 500,
                                      "Items": [{"Name": "x"}]})
                assert got and all(v is not None for v in got.values()), got
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
          f"{len(aliases)} distinct invariant aliases, projections total")


if __name__ == "__main__":
    main()

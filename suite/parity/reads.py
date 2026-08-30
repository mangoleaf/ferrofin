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
import json
import os
import re
import time
import urllib.parse
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up          # noqa: E402
from parity_diff import diff_stats                  # noqa: E402
import verification                                  # noqa: E402

CORRELATE_LIMIT = 5   # item-scoped endpoints are exercised against this many Path-aligned items


def token_get(base, path, token):
    st, raw = http("GET", base + path, token)
    if st != 200 or not raw:
        return st, None
    try:
        return st, json.loads(raw)
    except ValueError:
        return st, None

# ---------------------------------------------------------------- endpoint set

def plain(op, url):
    return {"op": op, "kind": "plain", "url": lambda c: url}


def user(op, url):
    # url may reference {u} (user id) plus resolved per-server context keys (genre/studio/person/
    # year/series/season). By-name values are URL-encoded and identical across servers (same NFO);
    # series/season ids are per-server (same title on both → clean diff).
    return {"op": op, "kind": "user", "url": lambda c: url.format(**c)}


def item(op, tmpl, extra_seeds=()):
    # tmpl contains {u} and {i}; filled per server (own user + own correlated item id).
    #
    # `extra_seeds` names additional per-server context keys to probe with the
    # SAME template, appended to the Path-correlated media pairs. It exists for
    # seeds a media item can never reach: the playlists folder's ancestor chain
    # is the only one that climbs to the `AggregateFolder` (a movie's stops at
    # the `UserRootFolder`), so without it the whole aggregate-root model is
    # untested by this layer.
    return {"op": op, "kind": "item", "url": lambda c, i: tmpl.format(u=c["user"], i=i),
            "extra_seeds": tuple(extra_seeds)}


def multi(op, legs, seed=None, reap=None):
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
    return {"op": op, "kind": "multi", "legs": out, "seed": seed, "reap": reap}


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
    # (`if (repository.Enabled && repository.Url is not null)`); pointing it at a
    # dead URL proves the skip happens BEFORE the fetch — if either server tried
    # it, the row would stall rather than quietly pass.
    {"Name": "Parity Fixture Disabled", "Url": "http://livetv-source:8000/nope.json",
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

    The guid is bound as `Guid?`, so the N/D/B spellings must all resolve and
    anything else must be a 400 from model binding. The N (dashless) spelling is
    the one both servers emit and jellyfin-web echoes back.
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


READS = [
    plain("GET /System/Info", "/System/Info"),
    plain("GET /System/Endpoint", "/System/Endpoint"),
    plain("GET /Localization/Cultures", "/Localization/Cultures"),
    plain("GET /Users/Me", "/Users/Me"),
    plain("GET /Sessions", "/Sessions"),
    user("GET /UserViews", "/UserViews?userId={u}"),
    user("GET /Library/MediaFolders", "/Library/MediaFolders"),
    user("GET /Library/VirtualFolders", "/Library/VirtualFolders"),
    # Two legs, both order-checked (see `with_item_order`).
    #
    # Leg 2 is the `UserRootFolder` browse, and it is not a duplicate of
    # `GET /Items/Root`: C# `ItemsController.GetItems` answers it from
    # `folder.GetChildren(user, true)` instead of a query (ItemsController.cs:307,
    # 525-529), which is a code path nothing else in this layer reaches — no
    # sort, no paging, no item-type filter, and the aggregate's virtual children
    # APPENDED rather than sorted in. Measured on 10.11.8, `sortBy=…`,
    # `limit=…&startIndex=…` and `includeItemTypes=…` all leave the answer
    # unchanged, so the leg pins the branch as well as the order.
    multi("GET /Items", [
        ("/Items?userId={u}&recursive=true&includeItemTypes=Movie&limit=50&sortBy=SortName&fields=Path",
         with_item_order),
        ("/Items?userId={u}&parentId={root}&sortBy=SortName&sortOrder=Descending&limit=2&startIndex=1",
         with_item_order),
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
    ]),
    user("GET /Movies/Recommendations", "/Movies/Recommendations?userId={u}"),
    user("GET /Search/Hints", "/Search/Hints?userId={u}&searchTerm=a&limit=20"),
    # item-scoped — correlated by Path, each server queried with its own id
    item("GET /Items/{itemId}", "/Items/{i}?userId={u}&fields=Path,MediaSources,MediaStreams,Overview,Genres"),
    # A movie seed cannot be body-diffed (Random order + a deliberately different
    # candidate algorithm) — verified by properties instead, see
    # `similar_invariants`.
    invariant("GET /Items/{itemId}/Similar", similar_invariants_for("Items")),
    # …plus the PLAYLISTS FOLDER, whose single ancestor is the `AggregateFolder`
    # `LibraryManager.CreateRootFolder()` parents it to (LibraryManager.cs:855-885).
    # The five movie seeds all stop at the `UserRootFolder`, so this leg is what
    # actually exercises the aggregate hop.
    item("GET /Items/{itemId}/Ancestors", "/Items/{i}/Ancestors?userId={u}",
         extra_seeds=("playlists_folder",)),
    item("GET /Items/{itemId}/PlaybackInfo", "/Items/{i}/PlaybackInfo?userId={u}"),
    item("GET /Items/{itemId}/Images", "/Items/{i}/Images"),
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
    user("GET /LiveTv/Programs/Recommended", "/LiveTv/Programs/Recommended?userId={u}&isAiring=true&limit=5"),
    user("GET /LiveTv/Timers/Defaults", "/LiveTv/Timers/Defaults"),
    user("GET /LiveTv/Info", "/LiveTv/Info"),
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

    artist = first_named("/Artists")
    lyric_ids = lyric_seed_ids(base, token, user_id)
    channels = (get_json(base, f"/LiveTv/Channels?userId={user_id}&limit=1", token) or {}).get("Items") or []
    listings_providers = (get_json(base, "/System/Configuration/livetv", token) or {}).get("ListingProviders") or []
    return {
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
        "task": first_task(),
        "device": first_device(),
        "artist": urllib.parse.quote(artist.get("Name") or ""),
        "artist_id": artist.get("Id") or "",
        "musicgenre": first_name("/MusicGenres"),
        # The three tracks `seed_lyrics` writes to, in LYRIC_SEEDS order.
        "lyric_lrc": lyric_ids[0],
        "lyric_elrc": lyric_ids[1],
        "lyric_txt": lyric_ids[2],
    }


def run(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    jt, ju = bring_up(jellyfin_url, "jellyfin")
    hc, jc = resolve_named(ferrofin_url, ht, hu), resolve_named(jellyfin_url, jt, ju)
    # `similar_invariants` holds each server to ITS OWN documented algorithm.
    hc["server"], jc["server"] = "ferrofin", "jellyfin"

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
        leg that compared anything was two empty result envelopes agreeing on
        their own zeros; `body-diff` as soon as one leg compared real content.
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

        `compared` is the number of leaf comparisons the diff actually performed,
        carried into the note the way the sweep layer carries it. Without it the
        page said "1/1 clean" for a row that compared 984 fields and for a row
        that compared one, and a reader could not tell a thick body diff from a
        thin one without opening this file."""
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
            depth = f"; {compared} field(s) compared" if compared is not None else ""
            rows[op] = {"deep_verified": True, "classification": "ok",
                        "verification_method": method,
                        "note": f"{clean}/{total} clean{depth}" + detail}
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
                        "note": f"{clean}/{total} clean; mismatch:{len(buckets['mismatch'])} "
                                f"missing:{len(buckets['missing'])} extra:{len(buckets['extra'])} | {sample}",
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
            record(ep["op"], 1 if n == 0 else 0, 1, buckets, ep["method"],
                   compared=len(set(jf) | set(hf)))
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
            finally:
                # Whatever happened above, the seeded state comes back off both
                # servers — an aborted run must not leave the pair asymmetric.
                if ep.get("reap"):
                    ep["reap"](ferrofin_url, ht, hc)
                    ep["reap"](jellyfin_url, jt, jc)
            record(ep["op"], clean, tested, agg, agg_method(legs),
                   compared=sum(c for _j, _h, c in legs))
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
            n, buckets, compared = diff_stats(jb, hb)
            record(ep["op"], 1 if n == 0 else 0, 1, buckets,
                   agg_method([(jb, hb, compared)]), compared=compared)
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
            record(ep["op"], clean, tested, agg, agg_method(legs),
                   compared=sum(c for _j, _h, c in legs))

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
    valid = {f"GET {p}" for p in spec["paths"]}
    bad = [ep["op"] for ep in READS if ep["op"] not in valid]
    assert not bad, f"read op-keys not in spec: {bad}"
    # every {placeholder} in a user() URL must be a key resolve_named() produces (guards the
    # {u} vs "user" KeyError). Format each with a fully-populated context; a KeyError fails here.
    ctx = {"user": "U", "u": "U", "genre": "G", "studio": "S", "person": "P", "series": "SE",
           "task": "T", "device": "D", "artist": "A", "artist_id": "AID", "musicgenre": "MG",
           "channel": "CH", "album_id": "ALB", "movie": "MOV", "episode": "EP",
           "listings_provider": "LP", "playlists_folder": "PLF", "root": "ROOT",
           "lyric_lrc": "L1", "lyric_elrc": "L2", "lyric_txt": "L3"}
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
    # The invariant rows must carry a callable, and the diff-shaped folding of
    # its facts must flag both a disagreement AND a fact both servers fail.
    assert all(callable(ep["fn"]) for ep in READS if ep["kind"] == "invariant")
    # ...and each ALIASED invariant row (the /{kind}/{itemId}/Similar family, one
    # C# method behind six routes) must own a DISTINCT alias, or three ledger
    # rows are one measurement of the same route. An invariant row that is not
    # part of an alias family carries no `alias` and is exempt — its op key is
    # already its identity.
    aliases = [ep["fn"].alias for ep in READS
               if ep["kind"] == "invariant" and hasattr(ep["fn"], "alias")]
    assert len(aliases) == len(set(aliases)), f"invariant rows share an alias: {aliases}"
    assert set(aliases) <= set(SIMILAR_ALIASES), aliases
    # Every invariant row is stamped `property`, never the body-diff method the
    # ledger headline counts.
    assert all(ep["method"] == "property" for ep in READS if ep["kind"] == "invariant")
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

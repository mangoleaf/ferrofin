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
import json
import os
import re
import urllib.parse
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up          # noqa: E402
from parity_diff import diff_counts                  # noqa: E402

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
    user("GET /MusicGenres", "/MusicGenres?userId={u}"),
    user("GET /MusicGenres/{genreName}", "/MusicGenres/{musicgenre}?userId={u}"),
    # Instant mixes are shuffled: the diff aligns by Name, so the SET of tracks is what is
    # compared (with the whole fixture under `limit`, both sides hold every track).
    user("GET /Artists/InstantMix", "/Artists/InstantMix?id={artist_id}&userId={u}&limit=100"),
    # `/MusicGenres/InstantMix` takes `id`, not `name` (C#
    # `GetInstantMixFromMusicGenreById([FromQuery, Required] Guid id)`). Probed with
    # `name=` both servers 400 on the missing `id`, the harness recorded "H=400 J=400"
    # as agreement, and the route was never actually compared.
    user("GET /MusicGenres/InstantMix",
         "/MusicGenres/InstantMix?id={musicgenre_id}&userId={u}&limit=100"),
    # Live TV (needs the tuner fixture): channels are keyed by Name across servers; the
    # airing programmes by Name too (the guide is identical on both).
    user("GET /LiveTv/Channels", "/LiveTv/Channels?userId={u}"),
    user("GET /LiveTv/Channels/{channelId}", "/LiveTv/Channels/{channel}?userId={u}"),
    user("GET /LiveTv/Programs", "/LiveTv/Programs?channelIds={channel}&isAiring=true&userId={u}"),
    user("GET /LiveTv/Programs/Recommended", "/LiveTv/Programs/Recommended?userId={u}&isAiring=true&limit=5"),
    user("GET /LiveTv/Info", "/LiveTv/Info"),
    user("GET /LiveTv/TunerHosts/Types", "/LiveTv/TunerHosts/Types"),
    # resolvable-path-param GETs the breadth sweep couldn't fill (needs a real id).
    user("GET /ScheduledTasks/{taskId}", "/ScheduledTasks/{task}"),
    user("GET /DisplayPreferences/{displayPreferencesId}",
         "/DisplayPreferences/usersettings?userId={u}&client=emby"),
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

    artist = first_named("/Artists")
    # By-name ids are per-server (each derives its own), like `artist_id`.
    musicgenre = first_named("/MusicGenres")
    channels = (get_json(base, f"/LiveTv/Channels?userId={user_id}&limit=1", token) or {}).get("Items") or []
    return {
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
        "task": first_task(),
        "device": first_device(),
        "artist": urllib.parse.quote(artist.get("Name") or ""),
        "artist_id": artist.get("Id") or "",
        "musicgenre": urllib.parse.quote(musicgenre.get("Name") or ""),
        "musicgenre_id": musicgenre.get("Id") or "",
    }


def run(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    jt, ju = bring_up(jellyfin_url, "jellyfin")
    hc, jc = resolve_named(ferrofin_url, ht, hu), resolve_named(jellyfin_url, jt, ju)
    # `similar_invariants` holds each server to ITS OWN documented algorithm.
    hc["server"], jc["server"] = "ferrofin", "jellyfin"

    pairs = correlate(path_id_map(ferrofin_url, ht, hu), path_id_map(jellyfin_url, jt, ju))
    rows = {}

    def record(op, clean, total, buckets, method="body-diff"):
        """`method` is HOW the row was verified, and it is written into the
        results row: "body-diff" means the ledger's headline claim (the
        responses themselves diffed clean), "property" means only the named
        invariants agreed. gen-ledger.py counts and renders the two separately
        so the headline keeps meaning what it says."""
        if total == 0:
            rows[op] = {"deep_verified": None, "classification": "",
                        "verification_method": method,
                        "note": "no comparable response (both empty/non-200)"}
            return
        n = sum(len(buckets[k]) for k in ("mismatch", "missing", "extra"))
        if n == 0:
            rows[op] = {"deep_verified": True, "classification": "ok",
                        "verification_method": method,
                        "note": f"{clean}/{total} clean"
                                + ("" if method == "body-diff"
                                   else " (properties agreed; bodies not diffed)")}
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
                        "verification_method": method,
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
            record(ep["op"], 1 if n == 0 else 0, 1, buckets, method=ep["method"])
        elif ep["kind"] == "multi":
            agg = {"mismatch": [], "missing": [], "extra": []}
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
                n, b = diff_counts(jb, hb)
                if n == 0:
                    clean += 1
                for k in agg:
                    agg[k].extend(b[k])
            record(ep["op"], clean, tested, agg)
        elif ep["kind"] in ("plain", "user"):
            path = ep["url"](hc if ep["kind"] == "user" else {})
            jpath = ep["url"](jc if ep["kind"] == "user" else {})
            hs, hb = token_get(ferrofin_url, path, ht)
            js, jb = token_get(jellyfin_url, jpath, jt)
            if hb is None or jb is None:
                record(ep["op"], 0, 0, {})
                continue
            n, buckets = diff_counts(jb, hb)
            record(ep["op"], 1 if n == 0 else 0, 1, buckets)
        else:  # item — aggregate over correlated pairs
            agg = {"mismatch": [], "missing": [], "extra": []}
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
                n, b = diff_counts(jb, hb)
                if n == 0:
                    clean += 1
                else:
                    for k in agg:
                        agg[k].extend(b[k])
            record(ep["op"], clean, tested, agg)
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
    ok = sum(1 for v in rows.values() if v["deep_verified"] is True)
    print(f"wrote parity/reads-results.json — {len(rows)} read ops, {ok} deep-verified "
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
           "musicgenre_id": "MGID"}
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
    # ...and each invariant row must own a DISTINCT alias, or three ledger rows
    # are one measurement of the same route.
    aliases = [ep["fn"].alias for ep in READS if ep["kind"] == "invariant"]
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
                got = leg["project"]({"StartIndex": 7, "TotalRecordCount": 500,
                                      "Items": [{"Name": "x"}]})
                assert got and all(v is not None for v in got.values()), got
    hf, jf = {"a": True, "b": True, "c": False}, {"a": True, "b": False, "c": False}
    bad = [k for k in sorted(set(hf) | set(jf))
           if hf.get(k) != jf.get(k) or hf.get(k) is False]
    assert bad == ["b", "c"], f"invariant folding wrong: {bad}"
    print(f"ok: diff, Path-align, correlation, {len(READS)} read op-keys valid, "
          f"user/multi templates fillable, invariant folding, "
          f"{len(aliases)} distinct invariant aliases, projections total")


if __name__ == "__main__":
    main()

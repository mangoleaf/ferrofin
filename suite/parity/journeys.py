#!/usr/bin/env python3
"""Layer-2 write journeys: verify write *effects*, not just status.

Each journey runs a real mutation sequence (setup → POST/PUT/DELETE → read-back)
against BOTH servers using each server's own ids, and checks the effect invariant
on the read-back (e.g. after a favorite POST, the item's UserData.IsFavorite is
true). A write op is `deep_verified` when its effect is confirmed on Ferrofin AND
Jellyfin behaves the same way. Where they diverge, the row is classified, not
silently passed — this is exactly how the harness surfaces real write gaps (e.g.
the rating-DELETE that never clears Likes).

Writes into an ephemeral container DB (docker `down -v` discards it) over a
read-only media mount — nothing on real disk is touched. Results go to
`parity/journey-results.json`; gen-ledger.py ingests them (feeds deep_verified for
write ops).

Run via sweep.sh (brings both servers up), or directly against provisioned servers:
  FERROFIN_URL=... JELLYFIN_URL=... parity/journeys.py
Offline self-check:
  parity/journeys.py --check
"""
import json
import re
import os
import shutil
import sys
import time
import urllib.parse
import uuid
import urllib.request
import urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from sweep import http, get_json, bring_up, container_read, ROOT, USER, PASS, CLIENT, opensubtitles_credentials   # reuse HTTP + provisioning
import verification   # the closed set of verification methods

class Same:
    """A journey step whose effect held on this server AND whose `evidence` must equal
    the other server's.

    A plain bool only asserts per-server self-consistency: each server is checked
    against what *it* was posted, so two servers that each faithfully round-trip
    *different* values both pass. Returning `Same(ok, evidence)` instead makes the
    runner compare the two servers' evidence as well, so the row is green only
    when the write took AND both servers ended up in the same state.

    `evidence` must be a value that is genuinely comparable across two independent
    instances — a settings object, a flag, a count, a projected read-back, a
    sha256 of served bytes. NEVER an id, a date, or anything per-instance.
    """

    __slots__ = ("ok", "evidence")

    def __init__(self, ok, evidence):
        self.ok = bool(ok)
        self.evidence = evidence

    def __bool__(self):
        return self.ok

    def __repr__(self):
        return f"{self.ok}(+evidence)"


def cross_server_ok(h_ok, j_ok):
    """True unless BOTH sides returned `Same` and their evidence disagrees.

    The whole cross-server rule, in one place, so the self-check exercises the code
    the runner actually runs rather than a restatement of it."""
    if isinstance(h_ok, Same) and isinstance(j_ok, Same):
        return h_ok.evidence == j_ok.evidence
    return True


def evidence_diff(h, j):
    """A short note naming where two evidence values differ: for dicts, the keys whose
    values disagree (with both values); otherwise a repr pair.

    RECURSES into a differing pair of dicts and names the inner keys instead of
    printing both whole objects. Evidence that carries a projected read-back is
    a dict of dicts, and the flat form rendered ~1.5 KB of identical fields to
    say that three of them differed — a note nobody reads is a divergence
    nobody sees."""
    if isinstance(h, dict) and isinstance(j, dict):
        bad = []
        for k in sorted(set(h) | set(j)):
            a, b = h.get(k), j.get(k)
            if a == b:
                continue
            if isinstance(a, dict) and isinstance(b, dict):
                bad.append(f"{k}{{{evidence_diff(a, b)}}}")
            else:
                bad.append(f"{k}: H={a!r} J={b!r}")
        return "; ".join(bad) or "(no key differs)"
    return f"H={h!r} J={j!r}"


def earned_method(op, h_ok, j_ok):
    """The method a row actually earned, which is never stronger than what ran.

    A row may DECLARE `body-diff` only if it returned `Same` on BOTH servers —
    i.e. something comparable really was compared across them. If it did not, the
    claim is downgraded to `effect` here rather than being taken on trust: the
    declaration is a promise, this is the check. (The blanket "journeys never
    body-diff" rule this replaces was right about plain booleans and wrong about a
    read-back that IS diffed against the other server's.)"""
    declared = journey_method(op)
    if declared == verification.BODY_DIFF and not (
            isinstance(h_ok, Same) and isinstance(j_ok, Same)):
        return verification.EFFECT
    return declared


# ---------------------------------------------------------------- how each row is verified
#
# Almost nothing in this layer diffs a body. The write's own response is normally
# discarded (`st, _ = http(...)`), the read-back pulls out one to three NAMED
# fields, and the two servers are combined by AND-ing two independent booleans —
# no value from Ferrofin is ever compared to the same value from Jellyfin. Those
# rows may NOT claim the ledger's `body-diff` headline. Each op declares which
# weaker thing it actually established:
#
#   effect        a write was applied and its effect confirmed on that server's
#                 own read-back (the favourite is set, the id is gone, the count
#                 moved, the created object identifies itself).
#   status-class  the request was accepted (`st < 300`) and NOTHING was read
#                 back. A handler that 204s and ignores the request passes.
#   property      a named property of a response body agreed (an MPEG-TS sync
#                 signature, a non-empty search result) — no effect, no diff.
#   body-diff     the read-back BODY itself — every field of the parsed DTO bar
#                 a named, justified per-instance handful — diffed against the
#                 other server's, which is what reads.py does. A named
#                 PROJECTION, however wide, does not earn this: the test is
#                 "start from the whole body and subtract", never "start from
#                 nothing and add". Claimed today by exactly the two ops in the
#                 BODY_DIFF block below (see SERIES_TIMER_PER_INSTANCE for the
#                 subtraction and why each entry is in it); `selfcheck()` holds
#                 that list to an explicit allowlist so a third claim takes a
#                 deliberate edit, and `earned_method` independently downgrades
#                 any declaring row that did not actually return `Same` on both
#                 servers.
#
# The ONE exception is enumerated in `JOURNEY_BODY_DIFF` below and nowhere else:
# a `Same(ok, evidence)` step whose evidence IS the whole raw response, so
# `cross_server_ok` compares the two servers' bodies to each other. That is the
# headline by the closed set's own definition, and it is an explicit allowlist
# rather than a default precisely so a new journey cannot drift into it.
#
# `selfcheck()` asserts this table covers every op key the journeys declare, so a
# new journey op cannot land in the ledger without saying how it was verified.
JOURNEY_METHOD = {op: verification.STATUS_CLASS for op in (
    # Bare `st < 300`: accepted, never read back.
    "POST /Playlists/{playlistId}",                     # rename never read back
    "POST /System/Ping",
    "POST /Items/{itemId}/ContentType",
    "POST /Items/{itemId}/Refresh",
    "POST /Library/Refresh",
    "POST /ScheduledTasks/Running/{taskId}",
    "DELETE /ScheduledTasks/Running/{taskId}",
    "DELETE /Items",                                    # the removal effect is the singular DELETE's
    "POST /QuickConnect/Authorize",
    # A status PAIR (real path 2xx, bogus path 4xx) — still only statuses.
    "POST /Environment/ValidatePath",
    # A registered service prefix resets (204), an unknown one is rejected (400).
    # `DefaultLiveTvService.ResetTuner` is `Task.CompletedTask` upstream, so the
    # success path genuinely has nothing to read back — the id VALIDATION is the
    # whole observable behaviour.
    "POST /LiveTv/Tuners/{tunerId}/Reset",
    # Remote-control commands fired at a session with no live receiver: the server
    # accepting them is all that is observable here.
    "POST /Sessions/Viewing",
    "POST /Sessions/Playing/Ping",
    "POST /Sessions/{sessionId}/Command/{command}",
    "POST /Sessions/{sessionId}/Command",
    "POST /Sessions/{sessionId}/Message",
    "POST /Sessions/{sessionId}/Playing",
    "POST /Sessions/{sessionId}/Playing/{command}",
    "POST /Sessions/{sessionId}/System/{command}",
    "POST /Sessions/{sessionId}/Viewing",
)}
JOURNEY_METHOD.update({op: verification.PROPERTY for op in (
    # The write's effect IS compared across the two servers, field for field —
    # but on a NAMED PROJECTION of the read-back DTO, not on the body. The
    # projection is `identified()`; read its docstring for exactly which fields
    # are in it and which are not. It is `property` and not `body-diff` for one
    # reason, stated plainly: `body-diff` means "every non-volatile field of the
    # parsed body", and a hand-listed tuple is not that, however long the tuple
    # gets. (It was recorded `body-diff` once. The projection had 16 entries,
    # `MergeBaseItemData` under `replaceData` touches more, and the row's own
    # docstring claimed "nothing here is dropped" — which was false.)
    "POST /Items/RemoteSearch/Apply/{itemId}",
    # A container signature, not an effect: 200 + video/mp2t + a 0x47 sync byte at
    # 0 and 188. Wrong PIDs, wrong channel or a black feed all match.
    "GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}",
    "GET /LiveTv/LiveRecordings/{recordingId}/stream",
    # A read whose bar is "non-empty on both", not a write effect.
    "GET /Items/{itemId}/RemoteSearch/Subtitles/{language}",
    "GET /Providers/Subtitles/Subtitles/{subtitleId}",
    # A read verified by PROPERTIES, not a body diff: the mix is ordered
    # `(ItemSortBy.Random, Ascending)` on both servers, so the page is
    # legitimately different on consecutive calls and only its shape and the
    # typed-lookup refusal are comparable. See `j_playlist_instant_mix`.
    "GET /Playlists/{itemId}/InstantMix",
    # The write's effect IS compared across the two servers, but on derived
    # properties (which ImageTags keys appeared, what media type the stored file is
    # served as) — the stored bytes come from two different origins and cannot be
    # diffed. See `j_remote_image_download`.
    "POST /Items/{itemId}/RemoteImages/Download",
    # A NAMED set of invariants, compared across the two servers: did exactly one
    # row appear, is its id derived rather than posted, did the create schedule
    # anything, does sortBy actually reorder. Counts and orderings, not a body.
    "POST /LiveTv/SeriesTimers",
    "DELETE /LiveTv/SeriesTimers/{timerId}",
    # `UpdateTimerAsync` takes four fields and discards the rest of the posted
    # body: the projection is those four plus "did the discarded ones survive".
    "POST /LiveTv/Timers/{timerId}",
    # Three named invariants carried across the two servers — the write's STATUS
    # (Ferrofin 403 from upstream master's Forbid guard, Jellyfin 10.11.8 204),
    # the first user's name, and whether the provisioned credentials still
    # authenticate. The row asserts the refusal, so it is RED on the Jellyfin leg
    # and stays red for as long as 10.11.8 lacks the guard: that is the
    # `jellyfin-bug` row in classifications.json, not a gap in the probe. See
    # `j_startup` for why the status is the only discriminator available here.
    "POST /Startup/User",
)})


# The effect rows: a write was issued against BOTH servers and its effect confirmed
# on each server's OWN read-back. Enumerated, not defaulted. `effect` used to be
# whatever `journey_method` returned for an op nobody had classified, which is the
# same shape of defect the whole stamping exercise exists to remove — a new journey
# op would inherit the strongest verdict this layer can issue without anyone
# deciding that it had earned it. `--check` now fails on an op that appears in no
# list below.
JOURNEY_METHOD.update({op: verification.EFFECT for op in (
    # A scripted login, then that login's own two activity rows read back — the
    # order, the severity and the ShortOverview are compared per server, never
    # against each other's bodies (Date/Id are per-run).
    "GET /System/ActivityLog/Entries",
    # The external-change webhooks (j_library_webhooks): each server is checked against
    # its OWN read-back — the item the reported path created/removed, the Overview the
    # matched external id refreshed — and the two servers' bodies are never compared.
    "POST /Library/Media/Updated",
    "POST /Library/Movies/Added",
    "POST /Library/Movies/Updated",
    "POST /Library/Series/Added",
    "POST /Library/Series/Updated",
    "DELETE /Audio/{itemId}/Lyrics",
    "DELETE /Auth/Keys/{key}",
    "DELETE /Collections/{collectionId}/Items",
    "DELETE /Devices",
    "DELETE /Items/{itemId}",
    "DELETE /Library/VirtualFolders",
    "DELETE /Library/VirtualFolders/Paths",
    "DELETE /LiveTv/ListingProviders",
    "DELETE /LiveTv/Recordings/{recordingId}",
    "DELETE /LiveTv/Timers/{timerId}",
    "DELETE /LiveTv/TunerHosts",
    "DELETE /PlayingItems/{itemId}",
    # Not a body diff and not merely a status class: the write is issued and the
    # EFFECT is read back on each server's own `/Plugins` — a non-removable
    # plugin must still be installed after its 204.
    "DELETE /Plugins/{pluginId}",
    "DELETE /Plugins/{pluginId}/{version}",
    "DELETE /Playlists/{playlistId}/Items",
    "DELETE /Playlists/{playlistId}/Users/{userId}",
    "DELETE /Sessions/{sessionId}/User/{userId}",
    "DELETE /UserFavoriteItems/{itemId}",
    "DELETE /UserItems/{itemId}/Rating",
    "DELETE /UserPlayedItems/{itemId}",
    "DELETE /Users/{userId}",
    "DELETE /Videos/ActiveEncodings",
    "DELETE /Videos/{itemId}/AlternateSources",
    "DELETE /Videos/{itemId}/Subtitles/{index}",
    "GET /Backup",
    "GET /Backup/Manifest",
    "GET /Devices/Options",
    "GET /LiveTv/Programs/{programId}",
    "GET /LiveTv/Recordings/{recordingId}",
    "GET /LiveTv/Timers/{timerId}",
    "GET /Playlists/{playlistId}",
    "GET /Playlists/{playlistId}/Items",
    "GET /Playlists/{playlistId}/Users",
    "GET /Playlists/{playlistId}/Users/{userId}",
    "GET /QuickConnect/Connect",
    "GET /System/Configuration/{key}",
    "GET /Users/{userId}",
    "POST /Audio/{itemId}/Lyrics",
    "POST /Auth/Keys",
    "POST /ClientLog/Document",
    "POST /Collections",
    "POST /Collections/{collectionId}/Items",
    "POST /Devices/Options",
    "POST /DisplayPreferences/{displayPreferencesId}",
    "POST /Items/{itemId}",
    "POST /Items/{itemId}/PlaybackInfo",
    "POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}",
    "POST /Library/VirtualFolders",
    "POST /Library/VirtualFolders/LibraryOptions",
    "POST /Library/VirtualFolders/Name",
    "POST /Repositories",
    "POST /Library/VirtualFolders/Paths",
    "POST /Library/VirtualFolders/Paths/Update",
    "POST /LiveStreams/Close",
    "POST /LiveStreams/Open",
    "POST /LiveTv/ChannelMappings",
    "POST /LiveTv/ListingProviders",
    "POST /LiveTv/Timers",
    "POST /LiveTv/TunerHosts",
    "POST /MergeVersions/MergeEpisodes",
    "POST /MergeVersions/MergeMovies",
    "POST /MergeVersions/SplitEpisodes",
    "POST /MergeVersions/SplitMovies",
    "POST /PlayingItems/{itemId}",
    "POST /PlayingItems/{itemId}/Progress",
    "POST /Playlists",
    "POST /Playlists/{playlistId}/Items",
    "POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}",
    "POST /Playlists/{playlistId}/Users/{userId}",
    "POST /QuickConnect/Initiate",
    "POST /ScheduledTasks/{taskId}/Triggers",
    "POST /Sessions/Capabilities",
    "POST /Sessions/Capabilities/Full",
    "POST /Sessions/Logout",
    "POST /Sessions/Playing",
    "POST /Sessions/Playing/Progress",
    "POST /Sessions/Playing/Stopped",
    "POST /Sessions/{sessionId}/User/{userId}",
    "POST /Startup/Complete",
    "POST /Startup/Configuration",
    "POST /Startup/RemoteAccess",
    "POST /System/Configuration",
    "POST /System/Configuration/Branding",
    "POST /System/Configuration/{key}",
    "POST /UserFavoriteItems/{itemId}",
    "POST /UserItems/{itemId}/Rating",
    "POST /UserItems/{itemId}/UserData",
    "POST /UserPlayedItems/{itemId}",
    "POST /Users",
    "POST /Users/AuthenticateByName",
    "POST /Users/AuthenticateWithQuickConnect",
    "POST /Users/Configuration",
    "POST /Users/ForgotPassword",
    "POST /Users/ForgotPassword/Pin",
    "POST /Users/New",
    "POST /Users/Password",
    "POST /Users/{userId}/Policy",
    "POST /Videos/MergeVersions",
    "POST /Videos/{itemId}/Subtitles",
)})
JOURNEY_METHOD.update({op: verification.BODY_DIFF for op in (
    # The FIRST rows in this layer to earn the headline, and they earn it the way
    # reads.py does: the evidence is the whole parsed `SeriesTimerInfoDto`, with
    # exactly the four per-instance fields in SERIES_TIMER_PER_INSTANCE removed
    # and nothing else — not a hand-listed projection that grows a field at a
    # time. Two of those four are Id/ExternalId, and they are not waved through:
    # `derives_its_id` re-derives the id from the C# formula on each server
    # instead, which is a stronger check than comparing two random GUIDs could
    # ever be. `earned_method` still downgrades either row to `effect` if the
    # journey did not actually return `Same` on both servers.
    "GET /LiveTv/SeriesTimers/{timerId}",
    # Same body, read back after the update, plus the four whitelist invariants.
    "POST /LiveTv/SeriesTimers/{timerId}",
)})

# The enumerated body-diff exception. `j_remote_search_identify` is the only journey
# whose `Same.evidence` is the RAW response — `identify()` returns the candidate list
# untouched, every key, order included — so `cross_server_ok` compares Ferrofin's body
# to Jellyfin's directly, and does it with a bare `==` that has no VOLATILE denylist at
# all (stricter than parity_diff, not looser). `RemoteSearchResult` has 12 contract
# properties and not one is per-instance, which is why a whole-object comparison is
# available here and nowhere else in this layer.
#
# This is a LIST, never a rule: an op earns the headline by being named here after
# someone read its journey, which is the opposite of the silent default the stamping
# work removed.
JOURNEY_BODY_DIFF = frozenset((
    # The series-timer pair: the evidence is a whole parsed body minus the three
    # named per-instance fields (`series_timer_body`), not a hand-listed
    # projection, which is what lets it claim the headline.
    "GET /LiveTv/SeriesTimers/{timerId}",
    "POST /LiveTv/SeriesTimers/{timerId}",
    "POST /Items/RemoteSearch/BoxSet",
    "POST /Items/RemoteSearch/Movie",
    "POST /Items/RemoteSearch/Person",
    "POST /Items/RemoteSearch/Series",
    "POST /Items/RemoteSearch/Trailer",
))
JOURNEY_METHOD.update({op: verification.BODY_DIFF for op in JOURNEY_BODY_DIFF})

# MusicArtist is deliberately NOT in that list. Its evidence is a candidate list plus the
# library-gate assertions, and the candidate list is ORDER-normalised (see the row's
# comment: MusicBrainz orders tie-scored artists differently between two independent live
# queries against the SAME server). Every field of every candidate is still compared and
# the candidate set is still exact, but "whole raw body, order included" is not what this
# row does, so it does not claim that headline.
JOURNEY_METHOD["POST /Items/RemoteSearch/MusicArtist"] = verification.PROPERTY


def journey_method(op):
    """The declared method for a journey op. There is NO default: an op that no list
    in `JOURNEY_METHOD` names raises, and `--check` turns that into a hard failure
    before any results file is written."""
    try:
        return JOURNEY_METHOD[op]
    except KeyError:
        raise KeyError(
            f"{op!r} declares no verification_method — add it to JOURNEY_METHOD "
            f"(effect / status-class / property; journeys never body-diff)") from None



def q(base, path, token, user):
    return get_json(base, f"{path}{'&' if '?' in path else '?'}userId={user}", token)


def two_movies(base, token, user):
    b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes=Movie"
                       f"&limit=2&sortBy=SortName", token)
    return [i["Id"] for i in (b or {}).get("Items", [])]


def user_data(base, token, user, mid):
    return (q(base, f"/Items/{mid}", token, user) or {}).get("UserData", {}) or {}

# ---------------------------------------------------------------- journeys (per server → {op: effect_ok})

def credentials_still_valid(base):
    """Status of a fresh `POST /Users/AuthenticateByName` with the provisioned
    credentials — the only way to prove a password write did not clobber them.

    It MUST NOT reuse the harness's `DeviceId`: re-authenticating on a device
    that already holds a session revokes that session's token on both servers,
    and the run's own token is the one being revoked. Measured while writing
    this: with `http()`'s fixed `Client="parity", DeviceId="parity"` header,
    every request after this one 401'd — `POST /Startup/RemoteAccess`,
    `POST /Startup/Complete` and the whole playlist-share journey went red on
    both servers. A dedicated device id keeps the check to the one thing it is
    asking about, and its session is logged out again so the probe leaves no
    registration behind.
    """
    dev = 'Client="parity-reauth", Device="parity-reauth", DeviceId="parity-reauth", Version="1.0"'
    req = urllib.request.Request(
        f"{base}/Users/AuthenticateByName",
        data=json.dumps({"Username": USER, "Pw": PASS}).encode(),
        method="POST",
        headers={"Content-Type": "application/json", "Authorization": f"MediaBrowser {dev}"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            st, raw = r.status, r.read()
    except urllib.error.HTTPError as e:
        return e.code
    except (urllib.error.URLError, TimeoutError, ConnectionError):
        return 0
    try:
        tok = json.loads(raw)["AccessToken"]
    except (ValueError, KeyError):
        return st
    # Retire the session this probe just created, so a long-lived lab does not
    # accumulate one device registration per journeys run.
    logout = urllib.request.Request(
        f"{base}/Sessions/Logout", data=b"", method="POST",
        headers={"Content-Type": "application/json",
                 "Authorization": f'MediaBrowser Token="{tok}", {dev}'})
    try:
        urllib.request.urlopen(logout, timeout=30).close()
    except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError, ConnectionError):
        pass
    return st


def j_startup(base, token, user, _m, _m2):
    """The first-run wizard endpoints. Setup is complete on both servers by the time journeys
    run, but the controller's policy is FirstTimeSetupOrElevated — an admin can drive it after
    setup — so each POST is replayed with exactly the values the harness provisioned and its
    effect confirmed on the read-back (nothing actually changes). Must run FIRST: Startup/User
    rewrites "the first user", which is only the admin while no throwaway users exist."""
    r = {}
    cfg = {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"}
    st, _ = http("POST", f"{base}/Startup/Configuration", token, json.dumps(cfg))
    back = get_json(base, "/Startup/Configuration", token) or {}
    r["POST /Startup/Configuration"] = st < 300 and all(back.get(k) == v for k, v in cfg.items())
    # Post-setup the first user already has a password, and the two servers answer this write
    # DIFFERENTLY: Ferrofin ports upstream master, whose `Forbid` guard is security commit
    # 62a5ded920, and REFUSES with 403; Jellyfin 10.11.8 predates that commit, performs the
    # write, and silently re-sets the already-provisioned admin's password. That delta is
    # classified `jellyfin-bug` in classifications.json, with the C# for both trees.
    #
    # WHAT THIS ROW ASSERTS, and why it is the refusal itself: a post-setup
    # POST /Startup/User must be REFUSED. Ferrofin satisfies that, Jellyfin does not, so the
    # row is RED — which is the correct terminal state for a jellyfin-bug row, exactly as
    # `DELETE /Playlists/{playlistId}/Users/{userId}` is red in this same layer, and not a
    # probe weakness.
    #
    # TWO WRONG VERSIONS OF THIS ROW, both recorded so neither is rediscovered as an idea:
    #   * `st == 403` on both servers with NO evidence (the original). The status delta was
    #     real, but it lived only in the boolean, so journey-results.json said "H=True
    #     J=False" and named nothing. That is what let a hand-written "accepted" label stand
    #     in for a measurement.
    #   * `st in (204, 403)` plus "the admin was not clobbered" (batch F6's first attempt).
    #     TRUE BY CONSTRUCTION: the payload is the harness's OWN credentials, so Jellyfin's
    #     unguarded 204 re-sets the name to the name it already has and the password to the
    #     password it already has. Measured on the F6 pair — Jellyfin, a server with NO guard
    #     at all, returned evidence byte-identical to Ferrofin's and the row went GREEN. A
    #     probe a guardless server passes cannot be evidence that the guard is present, and
    #     it dropped the only Ferrofin-side discrimination the row had: a regression that
    #     deleted the guard would have kept it green.
    #
    # WHY THE PAYLOAD IS STILL THE HARNESS'S OWN CREDENTIALS: not to make the assertion pass
    # (it does not, on Jellyfin), but because Jellyfin ACTUALLY PERFORMS the write. Any other
    # Name or Password renames the lab's admin or changes its password for every later probe
    # on the pair, and StartupUserDto carries no third, harmless field. So "was the admin
    # clobbered" cannot separate the two servers without damaging the lab, and it is kept
    # only as a lab-safety invariant. The discriminator is the status, and the status is IN
    # the `Same` evidence so the divergence is named in journey-results.json rather than
    # living only in prose.
    #
    # Ferrofin's 403 is additionally pinned against MASTER — the only tree containing the fix,
    # which no 10.11.8 container can witness — by the unit tests in
    # crates/ferrofin-api/tests/startup.rs (404 / 403-with-ChangePassword-Times.Never /
    # Forbid-before-BadRequest ordering / empty-Password-column), transliterated from
    # upstream's own StartupControllerTests.cs.
    #
    # Jellyfin picks `Users.First()` from an unordered dictionary, so the call is only safe
    # while the admin is the ONLY user — a stray user from a failed cleanup would be the one
    # renamed/re-passworded instead. Guarded rather than assumed.
    if len(get_json(base, "/Users", token) or []) == 1:
        st, _ = http("POST", f"{base}/Startup/User", token, json.dumps({"Name": USER, "Password": PASS}))
        back = get_json(base, "/Startup/User", token) or {}
        reauth = credentials_still_valid(base)
        r["POST /Startup/User"] = Same(
            st == 403 and back.get("Name") == USER and reauth == 200,
            {"Status": st, "FirstUserName": back.get("Name"),
             "CredentialsStillValid": reauth == 200})
    else:
        r["POST /Startup/User"] = False   # not attempted: more than one user on the instance
    st, _ = http("POST", f"{base}/Startup/RemoteAccess", token,
                 json.dumps({"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False}))
    net = get_json(base, "/System/Configuration/network", token) or {}
    r["POST /Startup/RemoteAccess"] = st < 300 and net.get("EnableRemoteAccess") is True
    st, _ = http("POST", f"{base}/Startup/Complete", token, "")
    pub = get_json(base, "/System/Info/Public", None) or {}
    r["POST /Startup/Complete"] = st < 300 and pub.get("StartupWizardCompleted") is True
    return r


def j_favorites(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserFavoriteItems/{mid}?userId={user}", token, "")
    r["POST /UserFavoriteItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("IsFavorite") is True
    st, _ = http("DELETE", f"{base}/UserFavoriteItems/{mid}?userId={user}", token)
    r["DELETE /UserFavoriteItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("IsFavorite") is False
    return r


def j_played(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserPlayedItems/{mid}?userId={user}", token, "")
    r["POST /UserPlayedItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("Played") is True
    st, _ = http("DELETE", f"{base}/UserPlayedItems/{mid}?userId={user}", token)
    r["DELETE /UserPlayedItems/{itemId}"] = st < 300 and user_data(base, token, user, mid).get("Played") is False
    return r


def j_rating(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserItems/{mid}/Rating?userId={user}&likes=true", token, "")
    r["POST /UserItems/{itemId}/Rating"] = st < 300 and user_data(base, token, user, mid).get("Likes") is True
    st, _ = http("DELETE", f"{base}/UserItems/{mid}/Rating?userId={user}", token)
    # effect = the like is actually cleared (not just a 2xx). Catches "DELETE returns 200 but keeps Likes".
    r["DELETE /UserItems/{itemId}/Rating"] = st < 300 and user_data(base, token, user, mid).get("Likes") is None
    return r


def j_playlist(base, token, user, mid, m2):
    r = {}
    st, raw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "Parity PL", "Ids": [mid], "UserId": user}))
    pid = json.loads(raw).get("Id") if st < 300 and raw else None
    items = q(base, f"/Playlists/{pid}/Items", token, user) if pid else None
    r["POST /Playlists"] = bool(pid)
    r["GET /Playlists/{playlistId}/Items"] = bool(items and items.get("TotalRecordCount", 0) >= 1)
    if pid:
        st, _ = http("POST", f"{base}/Playlists/{pid}/Items?ids={m2}&userId={user}", token, "")
        after = q(base, f"/Playlists/{pid}/Items", token, user) or {}
        r["POST /Playlists/{playlistId}/Items"] = st < 300 and after.get("TotalRecordCount", 0) >= 2
        # Move the 2nd entry to index 0 and verify the order flips.
        items2 = after.get("Items") or []
        if len(items2) >= 2:
            st, _ = http("POST", f"{base}/Playlists/{pid}/Items/{items2[1]['PlaylistItemId']}/Move/0", token, "")
            moved = (q(base, f"/Playlists/{pid}/Items", token, user) or {}).get("Items") or [{}]
            r["POST /Playlists/{playlistId}/Items/{itemId}/Move/{newIndex}"] = \
                st < 300 and moved[0].get("Id") == items2[1].get("Id")
        entry = (after.get("Items") or [{}])[0].get("PlaylistItemId")
        st, _ = http("DELETE", f"{base}/Playlists/{pid}/Items?entryIds={entry}&userId={user}", token)
        rem = q(base, f"/Playlists/{pid}/Items", token, user) or {}
        r["DELETE /Playlists/{playlistId}/Items"] = st < 300 and rem.get("TotalRecordCount", 99) < after.get("TotalRecordCount", 0)
        # GET /Playlists/{playlistId} returns a PlaylistDto {ItemIds, OpenAccess, Shares};
        # the playlist still holds its original movie, so ItemIds is non-empty.
        got = get_json(base, f"/Playlists/{pid}", token) or {}
        r["GET /Playlists/{playlistId}"] = len(got.get("ItemIds") or []) >= 1
        st, _ = http("POST", f"{base}/Playlists/{pid}?name=Renamed", token, "{}")
        r["POST /Playlists/{playlistId}"] = st < 300
        http("DELETE", f"{base}/Items/{pid}", token)   # cleanup
    return r


def j_playlist_instant_mix(base, token, user, mid, _m2):
    """`GET /Playlists/{itemId}/InstantMix` — the typed lookup and a real mix.

    The fixture holds **zero** playlists, so the positive leg of this route had
    never been exercised by any layer: the breadth sweep fed it `any_item` (a
    movie), Jellyfin 404'd, Ferrofin answered 200, and the difference was
    recorded as "a harmless superset". It is not — it is a missing type guard.
    Every other route on `InstantMixController` resolves its seed with
    `GetItemById<BaseItem>`; this one alone uses `GetItemById<Playlist>`, and
    `LibraryManager.GetItemById<T>` returns `null` when the item is not a `T`
    (`if (item is T typedItem) return typedItem; return null;`), so a
    non-playlist id is a 404 upstream.

    Three facts, all comparable across servers:

    1. a movie id on this route is a 404;
    2. the mix is all-`Audio` with a self-consistent `TotalRecordCount`;
    3. the mix holds the WHOLE audio library — every track, not merely "some".

    (3) is the assertion with teeth, and it is exact rather than "non-empty":
    a `Playlist` has no `Genres`, so `GetInstantMixFromPlaylist` calls
    `GetInstantMixFromGenres(item.Genres)` with an EMPTY list, `GenreIds` is
    empty, and the query is left unfiltered — every audio item under the 200-row
    cap comes back (v10.11.8 `InstantMixController.cs` +
    `MusicManager.GetInstantMixFromGenreIds`). So the count is determined by the
    fixture, and a server returning one arbitrary track — which "non-empty"
    would have passed — fails.

    The ORDER of the mix is deliberately not asserted: C#
    `GetInstantMixFromGenreIds` sorts `(ItemSortBy.Random, Ascending)` and so
    does Ferrofin now, so consecutive calls legitimately differ. The playlist is
    created and deleted inside the journey, symmetrically on both servers.
    """
    r = {}
    library = (q(base, "/Items?includeItemTypes=Audio&recursive=true"
                       "&limit=1000&sortBy=SortName", token, user) or {}).get("Items") or []
    # The whole audio library, capped where the C# caps the mix (`limit: 200`).
    expected = min(len(library), 200)
    tracks = [i["Id"] for i in library[:3]]
    if not tracks:
        r["GET /Playlists/{itemId}/InstantMix"] = False
        return r
    st, raw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "Parity Mix PL", "Ids": tracks, "UserId": user,
                               "MediaType": "Audio"}))
    pid = json.loads(raw).get("Id") if st < 300 and raw else None
    try:
        mix_ok = False
        if pid:
            mix = q(base, f"/Playlists/{pid}/InstantMix?limit=200", token, user) or {}
            items = mix.get("Items") or []
            mix_ok = (len(items) == expected
                      and mix.get("TotalRecordCount") == len(items)
                      and all(i.get("Type") == "Audio" for i in items))
        # The type guard: a MOVIE id on the playlists route is not found.
        guard_ok = http("GET", f"{base}/Playlists/{mid}/InstantMix?userId={user}", token)[0] == 404
        r["GET /Playlists/{itemId}/InstantMix"] = bool(mix_ok and guard_ok)
    finally:
        if pid:
            http("DELETE", f"{base}/Items/{pid}", token)
    return r


#: OMDb's in-tree plugin guid — the `Id` override on
#: `MediaBrowser.Providers/Plugins/Omdb/Plugin.cs`, so it is present on any
#: stock Jellyfin AND on Ferrofin, and its installed version is the server's.
#: OMDb rather than TMDb on purpose: TMDb is the id an earlier reviewer flipped
#: to `Status: "Restart"` on this pair, and a journey should not be reading a
#: field somebody else perturbed.
OMDB_PLUGIN_ID = "a628c0da-fac5-4c7e-9d1a-7134223f14c8"
SERVER_PLUGIN_VERSION = "10.11.8.0"


def j_plugin_uninstall(base, token, _user, _m, _m2):
    """`DELETE /Plugins/{id}` and `/Plugins/{id}/{version}` on a SHARED plugin id.

    Both rows carried "the two servers share no plugin id, so there is nothing to
    diff". They now share five — Jellyfin's in-tree metadata providers — and the
    behaviour underneath is not what the note assumed either: a plugin reporting
    `CanUninstall: false` is refused IN SILENCE. `InstallationManager.
    UninstallPlugin` logs "Attempt to delete non removable plugin … ignoring
    request" and returns, while `PluginsController` answers `204` anyway
    (v10.11.8). Every plugin compiled into Jellyfin's own tree takes that path,
    so the honest expectation is 204-and-still-installed, which Ferrofin used to
    answer 400.

    The version-bearing form additionally resolves through
    `GetPlugin(id, version)`, whose `Version.Equals` compares all four
    components — so a three-component spelling of the installed version is a
    miss, and a non-`Version` string fails the `[FromRoute] Version` model
    binder with a 400 before the action runs.

    This journey MUTATES NOTHING: every leg is the documented no-op, and the
    read-back asserts the plugin is still installed on each server afterwards.
    """
    r = {}
    installed = lambda: any(p.get("Id", "").replace("-", "").lower()
                            == OMDB_PLUGIN_ID.replace("-", "")
                            for p in (get_json(base, "/Plugins", token) or []))
    if not installed():
        # No shared plugin on this server: report the rows unverified rather
        # than passing an assertion about an absent thing.
        return r
    bare = http("DELETE", f"{base}/Plugins/{OMDB_PLUGIN_ID}", token)[0]
    r["DELETE /Plugins/{pluginId}"] = bare == 204 and installed()
    exact = http("DELETE", f"{base}/Plugins/{OMDB_PLUGIN_ID}/{SERVER_PLUGIN_VERSION}", token)[0]
    wrong = http("DELETE", f"{base}/Plugins/{OMDB_PLUGIN_ID}/9.9.9.9", token)[0]
    short = http("DELETE", f"{base}/Plugins/{OMDB_PLUGIN_ID}/10.11.8", token)[0]
    unparseable = http("DELETE", f"{base}/Plugins/{OMDB_PLUGIN_ID}/notaversion", token)[0]
    r["DELETE /Plugins/{pluginId}/{version}"] = (
        exact == 204 and wrong == 404 and short == 404 and unparseable == 400 and installed())
    return r


def j_collection(base, token, user, mid, m2):
    r = {}
    import urllib.parse
    st, raw = http("POST", f"{base}/Collections?name=ParityCol&ids={mid}", token, "{}")
    cid = json.loads(raw).get("Id") if st < 300 and raw else None
    r["POST /Collections"] = bool(cid)
    if cid:
        st, _ = http("POST", f"{base}/Collections/{cid}/Items?ids={m2}", token, "{}")
        after = q(base, f"/Items?parentId={cid}", token, user) or {}
        r["POST /Collections/{collectionId}/Items"] = st < 300 and after.get("TotalRecordCount", 0) >= 2
        st, _ = http("DELETE", f"{base}/Collections/{cid}/Items?ids={m2}", token)
        rem = q(base, f"/Items?parentId={cid}", token, user) or {}
        r["DELETE /Collections/{collectionId}/Items"] = st < 300 and rem.get("TotalRecordCount", 99) < after.get("TotalRecordCount", 0)
        http("DELETE", f"{base}/Items/{cid}", token)   # cleanup
    return r


def j_users(base, token, user, _m, _m2):
    r = {}
    st, raw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "parityuser", "Password": "Parity!123"}))
    uid = json.loads(raw).get("Id") if st < 300 and raw else None
    r["POST /Users/New"] = bool(uid)
    if uid:
        got = get_json(base, f"/Users/{uid}", token)
        r["GET /Users/{userId}"] = bool(got and got.get("Id") == uid)
        pol = dict((got or {}).get("Policy", {})); pol["IsAdministrator"] = True
        st, _ = http("POST", f"{base}/Users/{uid}/Policy", token, json.dumps(pol))
        after = get_json(base, f"/Users/{uid}", token) or {}
        r["POST /Users/{userId}/Policy"] = st < 300 and (after.get("Policy") or {}).get("IsAdministrator") is True
        st, _ = http("DELETE", f"{base}/Users/{uid}", token)
        gone = http("GET", f"{base}/Users/{uid}", token)[0]
        r["DELETE /Users/{userId}"] = st < 300 and gone >= 400
    return r


def j_item_edit(base, token, user, mid, _m2):
    """The metadata editor's save. Two things are read back, because they are two
    different writes: a Tags edit (a column on the item row) and the external ids
    (their own `BaseItemProviders` table, which C# assigns wholesale —
    `item.ProviderIds = request.ProviderIds`). The id half is asserted since Ferrofin
    was measured silently DROPPING it: same DTO to both servers, both 204, read-back
    `{"Tvdb": "..."}` on Jellyfin and `{}` on Ferrofin. The original ids are posted
    back at the end so the corpus other rows diff is left as it was found."""
    r = {}
    dto = q(base, f"/Items/{mid}", token, user)
    if dto:
        before_ids = dto.get("ProviderIds") or {}
        dto["Tags"] = list(dict.fromkeys((dto.get("Tags") or []) + ["parity-test"]))
        # A key that no fixture item carries and no remote provider can resolve, plus
        # an EMPTY value that both servers must strip rather than store.
        dto["ProviderIds"] = {**before_ids, "TvMaze": "990101", "Zap2It": ""}
        st, _ = http("POST", f"{base}/Items/{mid}", token, json.dumps(dto))
        back = q(base, f"/Items/{mid}?fields=Tags,ProviderIds", token, user) or {}
        ids = back.get("ProviderIds") or {}
        r["POST /Items/{itemId}"] = Same(
            st < 300 and "parity-test" in (back.get("Tags") or [])
            and ids.get("TvMaze") == "990101" and "Zap2It" not in ids,
            {"tag edit read back": "parity-test" in (back.get("Tags") or []),
             "external id read back": ids.get("TvMaze") == "990101",
             "empty id value stripped": "Zap2It" not in ids})
        dto["ProviderIds"] = before_ids
        http("POST", f"{base}/Items/{mid}", token, json.dumps(dto))   # restore
    return r


def j_api_keys(base, token, user, _m, _m2):
    r = {}
    http("POST", f"{base}/Auth/Keys?app=parity-probe", token, "")
    keys = (get_json(base, "/Auth/Keys", token) or {}).get("Items") or []
    mine = next((k for k in keys if k.get("AppName") == "parity-probe"), None)
    r["POST /Auth/Keys"] = mine is not None
    if mine:
        st, _ = http("DELETE", f"{base}/Auth/Keys/{mine['AccessToken']}", token)
        after = (get_json(base, "/Auth/Keys", token) or {}).get("Items") or []
        r["DELETE /Auth/Keys/{key}"] = st < 300 and not any(k.get("AppName") == "parity-probe" for k in after)
    return r


def j_user_item_data(base, token, user, mid, _m2):
    r = {}
    st, _ = http("POST", f"{base}/UserItems/{mid}/UserData?userId={user}", token,
                 json.dumps({"Rating": 8.0, "Played": True}))
    ud = user_data(base, token, user, mid)
    r["POST /UserItems/{itemId}/UserData"] = st < 300 and ud.get("Rating") == 8.0 and ud.get("Played") is True
    return r


def j_display_prefs(base, token, user, _m, _m2):
    r = {}
    path = f"/DisplayPreferences/usersettings?userId={user}&client=parity"
    dto = get_json(base, path, token)
    if dto is not None:
        dto.setdefault("CustomPrefs", {})["parityProbe"] = "1"
        st, _ = http("POST", f"{base}{path}", token, json.dumps(dto))
        back = get_json(base, path, token) or {}
        r["POST /DisplayPreferences/{displayPreferencesId}"] = \
            st < 300 and (back.get("CustomPrefs") or {}).get("parityProbe") == "1"
    return r


def j_scheduled_task_triggers(base, token, user, _m, _m2):
    r = {}
    tasks = get_json(base, "/ScheduledTasks", token) or []
    if tasks:
        tid = tasks[0]["Id"]
        triggers = [{"Type": "IntervalTrigger", "IntervalTicks": 36_000_000_000}]
        st, _ = http("POST", f"{base}/ScheduledTasks/{tid}/Triggers", token, json.dumps(triggers))
        back = get_json(base, f"/ScheduledTasks/{tid}", token) or {}
        got = [t.get("Type") for t in (back.get("Triggers") or [])]
        r["POST /ScheduledTasks/{taskId}/Triggers"] = st < 300 and "IntervalTrigger" in got
    return r


def j_device_options(base, token, user, _m, _m2):
    r = {}
    devices = (get_json(base, "/Devices", token) or {}).get("Items") or []
    dev = next((d for d in devices if d.get("Id")), None)
    if dev:
        st, _ = http("POST", f"{base}/Devices/Options?id={dev['Id']}", token,
                     json.dumps({"CustomName": "ParityRenamed"}))
        after = (get_json(base, "/Devices", token) or {}).get("Items") or []
        renamed = next((d for d in after if d.get("Id") == dev["Id"]), {})
        r["POST /Devices/Options"] = st < 300 and renamed.get("CustomName") == "ParityRenamed"
        # Read the options back by id (needs a device that has options set — just did).
        opts = get_json(base, f"/Devices/Options?id={dev['Id']}", token) or {}
        r["GET /Devices/Options"] = opts.get("CustomName") == "ParityRenamed"
    return r


def my_session(base, token):
    sessions = get_json(base, "/Sessions", token) or []
    return next((s["Id"] for s in sessions if s.get("Client") == "parity"),
                sessions[0]["Id"] if sessions else None)


def j_playstate(base, token, user, mid, _m2):
    r = {}
    ticks = 6_000_000_000
    st, _ = http("POST", f"{base}/Sessions/Playing", token,
                 json.dumps({"ItemId": mid, "PlayMethod": "DirectPlay"}))
    sid = my_session(base, token)
    now = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                if s.get("Id") == sid), None) or {}
    r["POST /Sessions/Playing"] = st < 300 and now.get("Id") == mid
    st, _ = http("POST", f"{base}/Sessions/Playing/Progress", token,
                 json.dumps({"ItemId": mid, "PositionTicks": ticks, "PlayMethod": "DirectPlay"}))
    ps = next((s.get("PlayState") for s in (get_json(base, "/Sessions", token) or [])
               if s.get("Id") == sid), None) or {}
    r["POST /Sessions/Playing/Progress"] = st < 300 and ps.get("PositionTicks") == ticks
    st, _ = http("POST", f"{base}/Sessions/Playing/Stopped", token,
                 json.dumps({"ItemId": mid, "PositionTicks": ticks}))
    stopped = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                    if s.get("Id") == sid), None)
    r["POST /Sessions/Playing/Stopped"] = st < 300 and stopped is None
    return r


def j_capabilities(base, token, user, _m, _m2):
    r = {}
    sid = my_session(base, token)
    if sid:
        st, _ = http("POST", f"{base}/Sessions/Capabilities/Full?id={sid}", token,
                     json.dumps({"PlayableMediaTypes": ["Video"], "SupportedCommands": ["DisplayMessage"],
                                 "SupportsMediaControl": True}))
        caps = next((s.get("Capabilities") for s in (get_json(base, "/Sessions", token) or [])
                     if s.get("Id") == sid), None) or {}
        r["POST /Sessions/Capabilities/Full"] = st < 300 and caps.get("SupportsMediaControl") is True
    return r


def j_user_config(base, token, user, _m, _m2):
    r = {}
    cfg = (get_json(base, f"/Users/{user}", token) or {}).get("Configuration") or {}
    cfg["PlayDefaultAudioTrack"] = not cfg.get("PlayDefaultAudioTrack", True)
    st, _ = http("POST", f"{base}/Users/Configuration?userId={user}", token, json.dumps(cfg))
    back = (get_json(base, f"/Users/{user}", token) or {}).get("Configuration") or {}
    r["POST /Users/Configuration"] = st < 300 and back.get("PlayDefaultAudioTrack") == cfg["PlayDefaultAudioTrack"]
    return r


def j_system_config(base, token, user, _m, _m2):
    r = {}
    cfg = get_json(base, "/System/Configuration", token)
    if cfg is not None:
        cfg["EnableFolderView"] = not cfg.get("EnableFolderView", False)
        st, _ = http("POST", f"{base}/System/Configuration", token, json.dumps(cfg))
        back = get_json(base, "/System/Configuration", token) or {}
        r["POST /System/Configuration"] = st < 300 and back.get("EnableFolderView") == cfg["EnableFolderView"]
    return r


def j_playlist_share(base, token, user, mid, _m2):
    r = {}
    _, praw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "SharePL", "Ids": [mid], "UserId": user}))
    pid = json.loads(praw).get("Id") if praw else None
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "shareprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if pid and uid:
        st, _ = http("POST", f"{base}/Playlists/{pid}/Users/{uid}", token,
                     json.dumps({"CanEdit": True}))
        shared = (get_json(base, f"/Playlists/{pid}/Users", token) or [])
        r["POST /Playlists/{playlistId}/Users/{userId}"] = st < 300 and any(s.get("UserId") == uid for s in shared)
        # Read the share back both as a list and by id (GET endpoints).
        r["GET /Playlists/{playlistId}/Users"] = any(s.get("UserId") == uid for s in shared)
        one = get_json(base, f"/Playlists/{pid}/Users/{uid}", token) or {}
        r["GET /Playlists/{playlistId}/Users/{userId}"] = one.get("CanEdit") is True
        st, _ = http("DELETE", f"{base}/Playlists/{pid}/Users/{uid}", token)
        after = (get_json(base, f"/Playlists/{pid}/Users", token) or [])
        r["DELETE /Playlists/{playlistId}/Users/{userId}"] = st < 300 and not any(s.get("UserId") == uid for s in after)
        # Second symptom of the same upstream defect, recorded rather than asserted.
        # `PlaylistsController.RemoveUserFromPlaylist` specifies `share is null ->
        # NotFound("User permissions not found")`, so a share the DELETE really removed
        # must 404 on a repeat. Ferrofin does. Jellyfin's DELETE is a reference-equality
        # no-op (`PlaylistManager.RemoveUserFromShares` re-fetches a DIFFERENT Playlist
        # instance and calls `shares.Remove(share)` on a `PlaylistUserPermissions` with no
        # equality override — v10.11.8 L641-648 / master L669-676, byte-identical), so the
        # share survives and the repeat finds it again and answers 204 forever.
        #
        # Kept OUT of the row's boolean deliberately: folding it in would make the row
        # ASSERT the divergence instead of measuring the shared invariant, and would let a
        # future Ferrofin regression to a blind 204 pass by matching Jellyfin. The
        # underscore prefix keeps it out of the op loop (it is evidence, not a contract op);
        # the divergence itself is classified `jellyfin-bug` in classifications.json.
        r["_note:playlist_share_repeat_delete_status"] = http(
            "DELETE", f"{base}/Playlists/{pid}/Users/{uid}", token)[0]
        http("DELETE", f"{base}/Users/{uid}", token)   # cleanup
        http("DELETE", f"{base}/Items/{pid}", token)
    return r


def j_item_delete(base, token, user, mid, _m2):
    r = {}
    # Create a throwaway box-set and delete it (never touches real media).
    _, raw = http("POST", f"{base}/Collections?name=DeleteMe&ids={mid}", token, "{}")
    cid = json.loads(raw).get("Id") if raw else None
    if cid:
        st, _ = http("DELETE", f"{base}/Items/{cid}", token)
        gone = http("GET", f"{base}/Items/{cid}?userId={user}", token)[0]
        r["DELETE /Items/{itemId}"] = st < 300 and gone >= 400
    return r


def j_capabilities_query(base, token, user, _m, _m2):
    r = {}
    sid = my_session(base, token)
    if sid:
        st, _ = http("POST", f"{base}/Sessions/Capabilities?id={sid}&playableMediaTypes=Audio", token, "")
        caps = next((s.get("Capabilities") for s in (get_json(base, "/Sessions", token) or [])
                     if s.get("Id") == sid), None) or {}
        r["POST /Sessions/Capabilities"] = st < 300 and caps.get("PlayableMediaTypes") == ["Audio"]
    return r


def j_environment_validate(base, token, user, _m, _m2):
    r = {}
    ok, _ = http("POST", f"{base}/Environment/ValidatePath", token,
                 json.dumps({"Path": "/media/synth/movies", "ValidateWritable": False}))
    bad, _ = http("POST", f"{base}/Environment/ValidatePath", token,
                  json.dumps({"Path": "/no/such/parity/path", "ValidateWritable": False}))
    # A real path validates (2xx); a bogus one is rejected (>=400).
    r["POST /Environment/ValidatePath"] = ok < 300 and bad >= 400
    return r


def j_merge_versions(base, token, user, mid, m2):
    r = {}
    st, _ = http("POST", f"{base}/Videos/MergeVersions?ids={mid},{m2}", token, "")
    survivor = None
    for cand in (mid, m2):
        it = q(base, f"/Items/{cand}?fields=MediaSources", token, user)
        if it and len(it.get("MediaSources") or []) >= 2:
            survivor = cand
            break
    r["POST /Videos/MergeVersions"] = st < 300 and survivor is not None
    if survivor:
        st, _ = http("DELETE", f"{base}/Videos/{survivor}/AlternateSources", token)
        after = q(base, f"/Items/{survivor}?fields=MediaSources", token, user) or {}
        r["DELETE /Videos/{itemId}/AlternateSources"] = st < 300 and len(after.get("MediaSources") or []) <= 1
    return r


def j_playing_items(base, token, user, mid, _m2):
    r = {}
    sid = my_session(base, token)
    ticks = 7_000_000_000
    st, _ = http("POST", f"{base}/PlayingItems/{mid}?playMethod=DirectPlay", token, "")
    now = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                if s.get("Id") == sid), None) or {}
    r["POST /PlayingItems/{itemId}"] = st < 300 and now.get("Id") == mid
    st, _ = http("POST", f"{base}/PlayingItems/{mid}/Progress?positionTicks={ticks}", token, "")
    ps = next((s.get("PlayState") for s in (get_json(base, "/Sessions", token) or [])
               if s.get("Id") == sid), None) or {}
    r["POST /PlayingItems/{itemId}/Progress"] = st < 300 and ps.get("PositionTicks") == ticks
    st, _ = http("DELETE", f"{base}/PlayingItems/{mid}?positionTicks={ticks}", token)
    cleared = next((s.get("NowPlayingItem") for s in (get_json(base, "/Sessions", token) or [])
                    if s.get("Id") == sid), None)
    r["DELETE /PlayingItems/{itemId}"] = st < 300 and cleared is None
    return r


def root_collection_folders(base, token, user):
    """The library rows the CLIENT sees at the root, as {name: id}.

    Two DB-derived surfaces, unioned: a root `/Items` browse and `/UserViews`.
    Deliberately NOT `/Library/VirtualFolders`, which upstream builds by walking
    the user-views DIRECTORY (`LibraryManager.GetVirtualFolders`) — so it is the
    one surface a stale row cannot corrupt, and asserting on it is how a real
    orphan-view leak stayed green here for a whole campaign.
    """
    out = {}
    items = (q(base, "/Items", token, user) or {}).get("Items", []) or []
    for it in items:
        if it.get("Type") == "CollectionFolder":
            out[it.get("Name")] = it.get("Id")
    for v in ((get_json(base, f"/UserViews?userId={user}", token) or {}).get("Items", []) or []):
        if v.get("Type") == "CollectionFolder":
            out.setdefault(v.get("Name"), v.get("Id"))
    return out


def j_virtualfolder_rename(base, token, user, _m, _m2):
    """Rename a library and assert the ROOT VIEW converges — on both servers.

    Renaming moves the library's directory. Upstream's
    `LibraryStructureController.RenameVirtualFolder` moves the directory and
    nothing else; the stale row is deleted by the remove leg of
    `LibraryManager.ValidateTopLibraryFolders` ("If the user has somehow deleted
    the collection directory, remove the metadata from the database" — identical
    in v10.11.8 and master). Either way the converged root holds exactly ONE row
    for the library.

    This used to assert only `any(f["Name"] == new for f in /Library/VirtualFolders)`,
    which is filesystem-derived and therefore passes on both servers even when one
    of them has left a phantom `CollectionFolder` behind in `/Items` + `/UserViews`.
    So the assertions here are:
      * after the rename: no root row still carries the OLD name,
      * after the restore: no root row carries the NEW name,
      * and the root's CollectionFolder COUNT is unchanged end to end.
    """
    r = {}
    folders = get_json(base, "/Library/VirtualFolders", token) or []
    if not folders:
        return r
    old = folders[0].get("Name") or ""
    new = f"{old} Renamed"
    qo, qn = urllib.parse.quote(old), urllib.parse.quote(new)
    before = root_collection_folders(base, token, user)

    st, _ = http("POST", f"{base}/Library/VirtualFolders/Name?name={qo}&newName={qn}", token, "")
    listed = any(f.get("Name") == new for f in (get_json(base, "/Library/VirtualFolders", token) or []))
    mid = root_collection_folders(base, token, user)
    renamed_cleanly = (
        st < 300
        and listed
        and old not in mid                 # the vacated name must not linger as a row
        and new in mid                     # …and the new name must be materialized
        and len(mid) == len(before)        # no row gained, none lost
    )

    restored_cleanly = False
    if listed:  # restore original name so library state is unchanged
        http("POST", f"{base}/Library/VirtualFolders/Name?name={qn}&newName={qo}", token, "")
        end = root_collection_folders(base, token, user)
        restored_cleanly = (
            new not in end and old in end and len(end) == len(before)
        )
    r["POST /Library/VirtualFolders/Name"] = renamed_cleanly and restored_cleanly
    return r


def j_repositories(base, token, user, _m, _m2):
    """Replace the package repositories and read the effect back on BOTH surfaces.

    Upstream backs `/Repositories` and `/System/Configuration.PluginRepositories`
    with ONE field:

        [HttpGet("Repositories")]  => Ok(_serverConfigurationManager.Configuration.PluginRepositories…)
        [HttpPost("Repositories")] => Configuration.PluginRepositories = repositoryInfos; SaveConfiguration();

    Ferrofin used to keep a second, private copy in `{config_dir}/plugins/state.json`,
    so the write landed where `/System/Configuration` and `/Packages` could not see
    it. A `/Repositories`-only read-back passes in that state and proves nothing —
    hence both surfaces are asserted, and they must agree with each other.

    Restores the server's own prior list at the end, so the pair is left as found.
    """
    r = {}
    before = get_json(base, "/Repositories", token)
    before = before if isinstance(before, list) else []
    wanted = [
        {"Name": "Parity Journey A", "Url": "http://livetv-source:8000/manifest.json",
         "Enabled": True},
        {"Name": "Parity Journey B", "Url": "http://livetv-source:8000/manifest-b.json",
         "Enabled": False},
    ]
    st, _ = http("POST", f"{base}/Repositories", token, json.dumps(wanted))

    listed = get_json(base, "/Repositories", token)
    in_config = (get_json(base, "/System/Configuration", token) or {}).get("PluginRepositories")

    def normalized(rows):
        return [
            {"Name": x.get("Name"), "Url": x.get("Url"), "Enabled": x.get("Enabled")}
            for x in (rows or [])
        ]

    applied = (
        st < 300
        and normalized(listed) == wanted
        and normalized(in_config) == wanted   # the one store, not two
    )

    # Restore, and confirm the restore also reached both surfaces.
    st_back, _ = http("POST", f"{base}/Repositories", token, json.dumps(before))
    restored = (
        st_back < 300
        and normalized(get_json(base, "/Repositories", token)) == normalized(before)
        and normalized((get_json(base, "/System/Configuration", token) or {})
                       .get("PluginRepositories")) == normalized(before)
    )
    if not restored:
        print(f"  WARN: {base}: could not restore the repository list")
    r["POST /Repositories"] = applied and restored
    return r


def j_virtualfolder_crud(base, token, user, _m, _m2):
    """Full VirtualFolders lifecycle on a throwaway library: create → add/update/remove a
    media path → toggle a library option → delete. Each step verifies its effect via
    GET /Library/VirtualFolders, and the library is removed at the end so shared state is
    untouched (it runs last, so it can't perturb the other journeys)."""
    r = {}
    name = "ParityCRUD"
    qn = urllib.parse.quote(name)
    tv = "/media/synth/tv"
    qtv = urllib.parse.quote(tv)

    def find():
        for f in get_json(base, "/Library/VirtualFolders", token) or []:
            if f.get("Name") == name:
                return f
        return None

    if find():  # leftover from a prior aborted run
        http("DELETE", f"{base}/Library/VirtualFolders?name={qn}&refreshLibrary=false", token)

    st, _ = http("POST", f"{base}/Library/VirtualFolders?name={qn}&collectionType=movies"
                         f"&paths=%2Fmedia%2Fsynth%2Fmovies&refreshLibrary=false", token, "{}")
    created = find()
    r["POST /Library/VirtualFolders"] = st < 300 and created is not None
    if not created:
        return r
    lib_id = created.get("ItemId")

    st, _ = http("POST", f"{base}/Library/VirtualFolders/Paths?refreshLibrary=false", token,
                 json.dumps({"Name": name, "PathInfo": {"Path": tv}}))
    r["POST /Library/VirtualFolders/Paths"] = st < 300 and tv in ((find() or {}).get("Locations") or [])

    # Update the added path's info (no observable location change) — verify 204 + lib intact.
    st, _ = http("POST", f"{base}/Library/VirtualFolders/Paths/Update?refreshLibrary=false", token,
                 json.dumps({"Name": name, "PathInfo": {"Path": tv}}))
    r["POST /Library/VirtualFolders/Paths/Update"] = st < 300 and find() is not None

    # Toggle a library option and verify it round-trips through GET. Keyed by `Id` alone,
    # which is the only key a real client has (`UpdateLibraryOptionsDto` is `{Guid Id,
    # LibraryOptions}` — no Name), and asserted on the WHOLE options object, not just the
    # flipped flag: C# replaces the options wholesale, so anything the server silently
    # drops or rewrites on the way through is a divergence this row should catch.
    #
    # The read-back is ALSO handed to the runner as cross-server evidence. The round-trip
    # alone is per-server self-consistency — each server is compared against what it was
    # posted, and the posted object is derived from that same server's own read — so two
    # servers whose LibraryOptions *defaults* differ would both pass it. Comparing the
    # resulting objects is what catches a default divergence (both libraries here are
    # created by the same request, so their options must agree key for key).
    opts = (find() or {}).get("LibraryOptions") or {}
    opts["EnablePhotos"] = not opts.get("EnablePhotos", True)
    st, _ = http("POST", f"{base}/Library/VirtualFolders/LibraryOptions", token,
                 json.dumps({"Id": lib_id, "LibraryOptions": opts}))
    got = (find() or {}).get("LibraryOptions")
    r["POST /Library/VirtualFolders/LibraryOptions"] = Same(st < 300 and got == opts, got)

    st, _ = http("DELETE", f"{base}/Library/VirtualFolders/Paths?name={qn}&path={qtv}"
                          f"&refreshLibrary=false", token)
    r["DELETE /Library/VirtualFolders/Paths"] = st < 300 and tv not in ((find() or {}).get("Locations") or [])

    st, _ = http("DELETE", f"{base}/Library/VirtualFolders?name={qn}&refreshLibrary=false", token)
    r["DELETE /Library/VirtualFolders"] = st < 300 and find() is None
    return r


def auth_device(base, username, pw, device):
    """AuthenticateByName under a distinct DeviceId, returning the full result (which
    carries SessionInfo.Id + AccessToken). A dedicated DeviceId avoids colliding with —
    and destroying — the harness's own DeviceId='parity' session."""
    hdr = {
        "Content-Type": "application/json",
        "Authorization": f'MediaBrowser Client="parityctl", Device="parityctl", '
                         f'DeviceId="{device}", Version="1.0"',
    }
    req = urllib.request.Request(f"{base}/Users/AuthenticateByName",
                                 data=json.dumps({"Username": username, "Pw": pw}).encode(),
                                 method="POST", headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=30) as rr:
            return json.loads(rr.read())
    except (urllib.error.HTTPError, urllib.error.URLError, ValueError, TimeoutError):
        return {}


def j_sessions(base, token, user, mid, _m2):
    """Session remote-control surface. Prior journeys authenticate throwaway users on the
    shared DeviceId='parity', which destroys that session — so this journey stands up its
    OWN session (a throwaway user under a dedicated DeviceId) and takes the session id from
    the auth response, then drives every control against it with the admin token. Cleans up."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "sessctl", "Password": "Parity!123"}))
    ctl_uid = json.loads(uraw).get("Id") if uraw else None
    if not ctl_uid:
        return r
    auth = auth_device(base, "sessctl", "Parity!123", "parity-sessctl")
    sid = (auth.get("SessionInfo") or {}).get("Id")
    ctl_tok = auth.get("AccessToken")
    if not sid or not ctl_tok:
        http("DELETE", f"{base}/Users/{ctl_uid}", token)
        return r
    # The controlled session advertises remote-control support so commands are accepted.
    http("POST", f"{base}/Sessions/Capabilities/Full", ctl_tok, json.dumps({
        "PlayableMediaTypes": ["Video", "Audio"],
        "SupportedCommands": ["Mute", "Unmute", "DisplayMessage", "GoHome", "Play", "Pause", "Stop"],
        "SupportsMediaControl": True,
        "SupportsPersistentIdentifier": True,
    }))

    # Fire-and-accept controls (admin token → the controllable session): the server accepts
    # the request even with no live receiver on the other end.
    controls = [
        ("POST /Sessions/Viewing", f"/Sessions/Viewing?sessionId={sid}&itemId={mid}", ""),
        ("POST /Sessions/Playing/Ping", "/Sessions/Playing/Ping?playSessionId=parity", ""),
        ("POST /Sessions/{sessionId}/Command/{command}", f"/Sessions/{sid}/Command/Mute", ""),
        ("POST /Sessions/{sessionId}/Command", f"/Sessions/{sid}/Command",
         json.dumps({"Name": "DisplayMessage", "Arguments": {"Header": "H", "Text": "T"}})),
        ("POST /Sessions/{sessionId}/Message", f"/Sessions/{sid}/Message",
         json.dumps({"Text": "hi", "Header": "H", "TimeoutMs": 500})),
        ("POST /Sessions/{sessionId}/Playing", f"/Sessions/{sid}/Playing?playCommand=PlayNow&itemIds={mid}", ""),
        ("POST /Sessions/{sessionId}/Playing/{command}", f"/Sessions/{sid}/Playing/Pause", ""),
        ("POST /Sessions/{sessionId}/System/{command}", f"/Sessions/{sid}/System/GoHome", ""),
        ("POST /Sessions/{sessionId}/Viewing", f"/Sessions/{sid}/Viewing?itemType=Movie&itemId={mid}&itemName=X", ""),
    ]
    for op, path, body in controls:
        st, _ = http("POST", f"{base}{path}", token, body)
        r[op] = st < 300

    # Additional-user add/remove, observed on the session's AdditionalUsers.
    def additional():
        s = next((x for x in (get_json(base, "/Sessions", token) or []) if x.get("Id") == sid), {})
        return [a.get("UserId") for a in (s.get("AdditionalUsers") or [])]
    st, _ = http("POST", f"{base}/Sessions/{sid}/User/{user}", token, "")
    r["POST /Sessions/{sessionId}/User/{userId}"] = st < 300 and user in additional()
    st, _ = http("DELETE", f"{base}/Sessions/{sid}/User/{user}", token)
    r["DELETE /Sessions/{sessionId}/User/{userId}"] = st < 300 and user not in additional()

    # Logout revokes the calling token — do it to the throwaway token so the harness survives.
    st, _ = http("POST", f"{base}/Sessions/Logout", ctl_tok, "")
    dead = http("GET", f"{base}/Users/Me", ctl_tok)[0]
    r["POST /Sessions/Logout"] = st < 300 and dead == 401

    http("DELETE", f"{base}/Users/{ctl_uid}", token)   # cleanup
    return r


# ---------------------------------------------------------------- external-change webhooks
#
# The synthetic media tree both containers bind-mount read-only at /media/synth
# (suite/perf/docker-compose.yml mounts ./fixtures/media into BOTH servers). The
# webhook journey writes its probe folders here, host-side, because "a file appeared
# on disk" is the only precondition these routes exist to answer.
WEBHOOK_MEDIA = os.path.join(ROOT, "suite/perf/fixtures/media")
# The debounce both servers apply before acting on a reported path
# (ServerConfiguration.LibraryMonitorDelay). The journey lowers it to this and restores
# the server's own value afterwards; at the stock 60 s every wait below would have to be
# a minute longer.
WEBHOOK_DELAY = 1
# How long a reported change may take to become visible. Measured on the parity lab at
# LibraryMonitorDelay=1: a create lands in 2–20 s (Ferrofin ~2 s, Jellyfin ~15 s) and an
# id-matched refresh in ~2 s, so 45 s is ~2x the slowest observed. The negative control
# instead waits WEBHOOK_SETTLE and asserts NOTHING happened: that one is a floor (too
# short and it would pass by not having looked yet), which is why it is not the same
# number.
WEBHOOK_EFFECT_TIMEOUT = 45
WEBHOOK_SETTLE = 12


def _webhook_leg(base):
    """A per-server suffix for the probe folders, so the Ferrofin leg and the Jellyfin leg
    never share a path: each leg creates, reports and removes only its own."""
    return str(urllib.parse.urlparse(base).port or "x")


def _webhook_nfo(path, tag, title, id_type, id_value, plot):
    """Write the Kodi/XBMC sidecar both servers read: the external id the webhook selects
    on, and a `<plot>` that is the marker a refresh is proven by."""
    with open(path, "w", encoding="utf-8") as f:
        f.write('<?xml version="1.0" encoding="utf-8"?>\n'
                f"<{tag}><title>{title}</title><year>1999</year>"
                f'<uniqueid type="{id_type}" default="true">{id_value}</uniqueid>'
                f"<plot>{plot}</plot></{tag}>\n")


def _webhook_report(base, token, path, update_type):
    st, _ = http("POST", f"{base}/Library/Media/Updated", token,
                 json.dumps({"Updates": [{"Path": path, "UpdateType": update_type}]}))
    return st


def _webhook_item(base, token, user, name, kind):
    b = get_json(base, f"/Items?userId={user}&Recursive=true&IncludeItemTypes={kind}"
                       f"&Fields=Overview,ProviderIds&searchTerm={name}", token) or {}
    return next((i for i in b.get("Items", []) if i.get("Name") == name), None)


def _webhook_wait(base, token, user, name, kind, pred, timeout=WEBHOOK_EFFECT_TIMEOUT):
    """Poll until `pred(item)` holds (item is None while it does not exist). Returns the
    last item seen and whether the predicate ever held."""
    end = time.time() + timeout
    while True:
        it = _webhook_item(base, token, user, name, kind)
        if pred(it):
            return it, True
        if time.time() >= end:
            return it, False
        time.sleep(3)


def _webhook_probe(base, token, user, kind, route, folder, mkv_src, nfo_name, tag,
                   id_type, id_value, id_param):
    """One provider-id webhook pair (Movies or Series), start to finish, on ONE server.

    Creates a probe folder on disk, makes it an item with `POST /Library/Media/Updated`,
    then proves that `/Library/<route>/Updated` and `/Library/<route>/Added` refresh THAT
    item when the external id matches and leave it alone when it does not. The probe's
    external id is deliberately unresolvable (no such title exists at TMDB/TVDB), so a
    library whose "Metadata downloaders" are enabled cannot overwrite the marker and the
    `<plot>` in the sidecar stays the only thing that can change the Overview.

    Returns `(created_ok, deleted_ok, {"Updated": Same, "Added": Same})` — the caller
    binds those to their op keys, which stay spelled out there so `--check` can scrape
    them.
    """
    ops = {}
    name = os.path.basename(folder).split(" (")[0]
    media_path = "/media/synth/" + os.path.relpath(folder, WEBHOOK_MEDIA).replace(os.sep, "/")
    if kind == "Movie":
        media_path += f"/{os.path.basename(folder)}.mkv"
    shutil.rmtree(folder, ignore_errors=True)
    # Self-heal: a run that was killed mid-probe leaves its item in the library, and a
    # stale row would fail the create assertion below for the wrong reason (the item is
    # there, but carrying the previous run's marker). Report the removal of whatever is
    # left and wait for it to go before building the probe again. Costs one query when
    # there is nothing to clean, which is the normal case.
    if _webhook_item(base, token, user, name, kind) is not None:
        _webhook_report(base, token, media_path, "Deleted")
        _webhook_wait(base, token, user, name, kind, lambda it: it is None,
                      WEBHOOK_EFFECT_TIMEOUT)
    leaf = os.path.join(folder, "Season 01") if kind == "Series" else folder
    os.makedirs(leaf)
    shutil.copyfile(mkv_src, os.path.join(
        leaf, f"{name} S01E01.mkv" if kind == "Series" else f"{os.path.basename(folder)}.mkv"))
    nfo = os.path.join(folder, nfo_name)
    try:
        _webhook_nfo(nfo, tag, name, id_type, id_value, "PARITY-BASE")
        st_c = _webhook_report(base, token, media_path, "Created")
        made, ok = _webhook_wait(base, token, user, name, kind, lambda it: it is not None,
                                 WEBHOOK_EFFECT_TIMEOUT + 30)
        created_ok = (st_c < 300 and ok
                      and (made.get("ProviderIds") or {}).get(id_type.capitalize()) == id_value
                      and made.get("Overview") == "PARITY-BASE")
        if created_ok:
            for op, marker in (("Updated", "PARITY-UPDATED"), ("Added", "PARITY-ADDED")):
                # Let the previous step's scan go quiet, then read the Overview the
                # negative control is measured against. Taking the CURRENT value rather
                # than a literal is what makes the control survive the second pass (whose
                # baseline is the first pass's marker) and any ingest still finishing.
                time.sleep(WEBHOOK_SETTLE)
                baseline = (_webhook_item(base, token, user, name, kind) or {}).get("Overview")
                _webhook_nfo(nfo, tag, name, id_type, id_value, marker)
                # NEGATIVE control: an id that matches nothing must refresh nothing. This
                # is the leg that fails a handler which ignores the selector and rescans.
                st_n, _ = http("POST", f"{base}/Library/{route}/{op}?{id_param}=0", token, "")
                time.sleep(WEBHOOK_SETTLE)
                held = (_webhook_item(base, token, user, name, kind) or {}).get("Overview") == baseline
                st_p, _ = http("POST", f"{base}/Library/{route}/{op}?{id_param}={id_value}", token, "")
                _, hit = _webhook_wait(base, token, user, name, kind,
                                       lambda it: bool(it) and it.get("Overview") == marker)
                ops[op] = Same(
                    st_n < 300 and st_p < 300 and held and hit,
                    {"unmatched id changed nothing": held, "matched id refreshed the item": hit})
    finally:
        shutil.rmtree(folder, ignore_errors=True)
        st_d = _webhook_report(base, token, media_path, "Deleted")
        _, gone = _webhook_wait(base, token, user, name, kind, lambda it: it is None,
                                WEBHOOK_EFFECT_TIMEOUT + 30)
    return created_ok, (st_d < 300 and gone), ops


def j_library_webhooks(base, token, user, _m, _m2):
    """The Sonarr/Radarr external-change webhooks: `/Library/Media/Updated` (by path) and
    `/Library/{Movies,Series}/{Added,Updated}` (by external id).

    The effect that can FAIL is a real one: a folder that exists on disk becomes a library
    item ONLY because the POST reported it, stops being one when the removal is reported,
    and an edited NFO reaches the item ONLY when the reported external id matches. Every
    fixture library has `EnableRealtimeMonitor` false on both servers, so no OS watcher can
    manufacture the same green, and each id-selecting op is paired with a NEGATIVE control
    (an id that matches nothing) that must leave the item untouched.

    Settings this depends on, named because they change the timing and the corpus, not the
    verdict: `ServerConfiguration.LibraryMonitorDelay` (the debounce, lowered to 1 s here
    and restored), and the library's "Metadata downloaders" checkboxes (the probe's
    external id is unresolvable, so no remote fetcher can overwrite the marker).

    Each server is only ever checked against its OWN read-back: `effect`, never a body diff.
    """
    r = {}
    movies_root = os.path.join(WEBHOOK_MEDIA, "movies")
    tv_root = os.path.join(WEBHOOK_MEDIA, "tv")
    src = next((os.path.join(d.path, f) for d in os.scandir(movies_root) if d.is_dir()
                for f in sorted(os.listdir(d.path)) if f.endswith(".mkv")), None) \
        if os.path.isdir(movies_root) else None
    if src is None or not os.path.isdir(tv_root):
        return r   # not the synthetic lab: claim nothing rather than fail for the wrong reason
    leg = _webhook_leg(base)
    cfg = get_json(base, "/System/Configuration", token) or {}
    prev_delay = cfg.get("LibraryMonitorDelay")
    if prev_delay is not None:
        cfg["LibraryMonitorDelay"] = WEBHOOK_DELAY
        http("POST", f"{base}/System/Configuration", token, json.dumps(cfg))
    try:
        m_made, m_gone, m_ops = _webhook_probe(
            base, token, user, "Movie", "Movies",
            os.path.join(movies_root, f"Parityhookm{leg} (1999)"),
            src, "movie.nfo", "movie", "imdb", f"tt99{leg}", "imdbId")
        r["POST /Library/Movies/Updated"] = m_ops.get("Updated", False)
        r["POST /Library/Movies/Added"] = m_ops.get("Added", False)
        s_made, s_gone, s_ops = _webhook_probe(
            base, token, user, "Series", "Series",
            os.path.join(tv_root, f"Parityhooks{leg}"),
            src, "tvshow.nfo", "tvshow", "tvdb", f"99{leg}", "tvdbId")
        r["POST /Library/Series/Updated"] = s_ops.get("Updated", False)
        r["POST /Library/Series/Added"] = s_ops.get("Added", False)
        # The path-addressed webhook's own row: it is what created and removed both probes,
        # plus the two rejections C# raises from inside the loop (a null and an empty path,
        # `ArgumentException` → 400). The 400 BODIES are not compared: Jellyfin answers
        # text/plain "Error processing request.", Ferrofin a JSON error envelope.
        st_null, _ = http("POST", f"{base}/Library/Media/Updated", token,
                          json.dumps({"Updates": [{"UpdateType": "Modified"}]}))
        st_empty, _ = http("POST", f"{base}/Library/Media/Updated", token,
                           json.dumps({"Updates": [{"Path": "", "UpdateType": "Modified"}]}))
        r["POST /Library/Media/Updated"] = Same(
            m_made and m_gone and s_made and s_gone and st_null == 400 and st_empty == 400,
            {"reported file became an item": m_made and s_made,
             "reported removal pruned it": m_gone and s_gone,
             "path-less update rejected": st_null == 400 and st_empty == 400})
    finally:
        if prev_delay is not None:
            cfg["LibraryMonitorDelay"] = prev_delay
            http("POST", f"{base}/System/Configuration", token, json.dumps(cfg))
    return r


def j_system_and_refresh(base, token, user, mid, _m2):
    """Status-effect writes with no observable read-back: ping, item/library refresh
    triggers, and a content-type override. The differential still confirms both servers
    accept the identical request the same way. Runs last so a queued rescan can't perturb
    the other journeys."""
    r = {}
    st, _ = http("POST", f"{base}/System/Ping", token, "")
    r["POST /System/Ping"] = st < 300
    st, _ = http("POST", f"{base}/Items/{mid}/ContentType?contentType=movies", token, "")
    r["POST /Items/{itemId}/ContentType"] = st < 300
    st, _ = http("POST", f"{base}/Items/{mid}/Refresh?metadataRefreshMode=Default"
                         f"&imageRefreshMode=None", token, "")
    r["POST /Items/{itemId}/Refresh"] = st < 300
    st, _ = http("POST", f"{base}/Library/Refresh", token, "")
    r["POST /Library/Refresh"] = st < 300
    return r


def j_config_writes(base, token, user, _m, _m2):
    """Branding config round-trips through both the dedicated endpoint and the generic
    keyed endpoint, read back via GET /System/Configuration/{key}. Restores it after."""
    r = {}
    disc = "Parity disclaimer"
    st, _ = http("POST", f"{base}/System/Configuration/Branding", token,
                 json.dumps({"LoginDisclaimer": disc, "CustomCss": "", "SplashscreenEnabled": False}))
    got = get_json(base, "/System/Configuration/branding", token) or {}
    r["POST /System/Configuration/Branding"] = st < 300 and got.get("LoginDisclaimer") == disc
    r["GET /System/Configuration/{key}"] = got.get("LoginDisclaimer") == disc
    disc2 = "Parity disclaimer 2"
    st, _ = http("POST", f"{base}/System/Configuration/branding", token,
                 json.dumps({"LoginDisclaimer": disc2, "CustomCss": "", "SplashscreenEnabled": False}))
    got2 = get_json(base, "/System/Configuration/branding", token) or {}
    r["POST /System/Configuration/{key}"] = st < 300 and got2.get("LoginDisclaimer") == disc2
    http("POST", f"{base}/System/Configuration/Branding", token,   # restore
         json.dumps({"LoginDisclaimer": "", "CustomCss": "", "SplashscreenEnabled": False}))
    return r


def j_scheduled_run(base, token, user, _m, _m2):
    """Start then stop a scheduled task. Picks a task that is not a heavy library
    scan/refresh so the immediate stop leaves nothing running."""
    r = {}
    tasks = get_json(base, "/ScheduledTasks", token) or []
    heavy = ("scan", "refresh", "library", "metadata", "thumbnail", "trickplay")
    task = next((t for t in tasks if not any(h in (t.get("Name", "").lower()) for h in heavy)), None)
    task = task or (tasks[0] if tasks else None)
    tid = task.get("Id") if task else None
    if tid:
        st, _ = http("POST", f"{base}/ScheduledTasks/Running/{tid}", token, "")
        r["POST /ScheduledTasks/Running/{taskId}"] = st < 300
        st, _ = http("DELETE", f"{base}/ScheduledTasks/Running/{tid}", token)
        r["DELETE /ScheduledTasks/Running/{taskId}"] = st < 300
    return r


def j_playbackinfo_post(base, token, user, mid, _m2):
    st, raw = http("POST", f"{base}/Items/{mid}/PlaybackInfo?userId={user}", token, "{}")
    ok = False
    if st < 300 and raw:
        try:
            ok = len(json.loads(raw).get("MediaSources") or []) >= 1
        except ValueError:
            ok = False
    return {"POST /Items/{itemId}/PlaybackInfo": ok}


def j_active_encodings(base, token, user, _m, _m2):
    # No live transcode for this device/session → an idempotent 2xx no-op on both servers.
    st, _ = http("DELETE", f"{base}/Videos/ActiveEncodings?deviceId=parity&playSessionId=none", token)
    return {"DELETE /Videos/ActiveEncodings": st < 300}


def j_clientlog(base, token, user, _m, _m2):
    st, _ = http("POST", f"{base}/ClientLog/Document", token, "parity client log line")
    return {"POST /ClientLog/Document": st < 300}


def j_authenticate(base, token, user, _m, _m2):
    """Authenticate a throwaway user by name and confirm an access token comes back."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "authprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        _, araw = http("POST", f"{base}/Users/AuthenticateByName", None,
                       json.dumps({"Username": "authprobe", "Pw": "Parity!123"}))
        tok2 = json.loads(araw).get("AccessToken") if araw else None
        r["POST /Users/AuthenticateByName"] = bool(tok2)
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


def j_user_update(base, token, user, _m, _m2):
    """Update a throwaway user via POST /Users?userId= and read the change back."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "updprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        u = get_json(base, f"/Users/{uid}", token) or {}
        u["Name"] = "updprobe2"
        st, _ = http("POST", f"{base}/Users?userId={uid}", token, json.dumps(u))
        after = get_json(base, f"/Users/{uid}", token) or {}
        r["POST /Users"] = st < 300 and after.get("Name") == "updprobe2"
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


def j_devices_delete(base, token, user, _m, _m2):
    """Register a device via a throwaway login under a dedicated DeviceId, then delete it
    and confirm it drops off GET /Devices — without touching the harness's own device."""
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "delprobe", "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        auth = auth_device(base, "delprobe", "Parity!123", "parity-deldev")
        if auth.get("AccessToken"):
            def has_dev():
                items = (get_json(base, "/Devices", token) or {}).get("Items") or []
                return any(d.get("Id") == "parity-deldev" for d in items)
            present = has_dev()
            st, _ = http("DELETE", f"{base}/Devices?id=parity-deldev", token)
            r["DELETE /Devices"] = present and st < 300 and not has_dev()
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


def j_bulk_item_delete(base, token, user, mid, _m2):
    """DELETE /Items (bulk, by ids query) — delete a throwaway playlist item and confirm
    it is gone. Uses a created playlist so no fixture media is touched."""
    r = {}
    _, praw = http("POST", f"{base}/Playlists", token,
                   json.dumps({"Name": "BulkDelPL", "Ids": [mid], "UserId": user}))
    pid = json.loads(praw).get("Id") if praw else None
    if pid:
        # Both servers accept the bulk delete. The removal *effect* is deep-verified by the
        # singular DELETE /Items/{itemId}; a read-back here is unreliable because Jellyfin
        # deletes asynchronously (the item lingers a beat) while Ferrofin deletes synchronously.
        st, _ = http("DELETE", f"{base}/Items?ids={pid}", token)
        r["DELETE /Items"] = st < 300
        http("DELETE", f"{base}/Items/{pid}", token)  # ensure cleanup even if bulk lagged
    return r


SUBTITLE_REFRESH_WAIT_S = 10   # Jellyfin lists an uploaded stream only once its refresh ran


def external_subtitles(base, token, user, mid):
    """(index, language) of the item's EXTERNAL subtitle streams (uploaded files)."""
    item = q(base, f"/Items/{mid}?fields=MediaStreams", token, user) or {}
    return [(s["Index"], s.get("Language")) for s in item.get("MediaStreams") or []
            if s.get("Type") == "Subtitle" and s.get("IsExternal") and "Index" in s]


def external_subtitle_indexes(base, token, user, mid):
    return [i for i, _ in external_subtitles(base, token, user, mid)]


def j_subtitles_upload(base, token, user, mid, _m2):
    """Upload an external subtitle → it appears as an external subtitle stream → delete it by
    index → it is gone. (Delete only ever targets external streams; the embedded eng track
    the fixture carries stays.) The upload is "fra", and every external fra stream is reaped
    at the end whatever happened, so a stale file from an aborted run cannot mask the next."""
    import base64
    r = {}
    before = set(external_subtitle_indexes(base, token, user, mid))
    srt = "1\n00:00:00,000 --> 00:00:01,000\nParity\n"
    body = json.dumps({
        "Language": "fra", "Format": "srt", "IsForced": False,
        "IsHearingImpaired": False,
        "Data": base64.b64encode(srt.encode()).decode(),
    })
    st, _ = http("POST", f"{base}/Videos/{mid}/Subtitles", token, body)
    added = []
    for _ in range(SUBTITLE_REFRESH_WAIT_S):
        added = [i for i in external_subtitle_indexes(base, token, user, mid) if i not in before]
        if added:
            break
        time.sleep(1)
    r["POST /Videos/{itemId}/Subtitles"] = st < 300 and bool(added)
    if added:
        st, _ = http("DELETE", f"{base}/Videos/{mid}/Subtitles/{added[0]}", token)
        gone = added[0] not in external_subtitle_indexes(base, token, user, mid)
        r["DELETE /Videos/{itemId}/Subtitles/{index}"] = st < 300 and gone
    for i, lang in external_subtitles(base, token, user, mid):   # reap, whichever path ran
        if lang == "fra":
            http("DELETE", f"{base}/Videos/{mid}/Subtitles/{i}", token)
    return r


LYRIC_REFRESH_WAIT_S = 10   # Jellyfin serves an uploaded lyric only once its refresh ran

#: A synced .lrc: metadata tags (which must NOT surface as LyricDto.Metadata),
#: three timestamped lines, one of them carrying enhanced-LRC word time tags.
PARITY_LRC = ("[ar:Parity Artist]\n[ti:Parity Title]\n[al:Parity Album]\n"
              "[00:01.00]First line\n"
              "[00:05.50]<00:05.50>Second <00:06.00>line\n"
              # Text BEFORE the first word tag. `LrcTimedTextUtils` seeds its tag
              # list with the LINE's start, so this line owes a cue at position 0
              # carrying [00:08.00] — a shape a word-tag-first corpus can never
              # demand, and one Ferrofin used to drop.
              "[00:08.00]Third <00:09.00>line\n"
              "[00:12.25]Fourth line\n")
#: What both servers must parse PARITY_LRC into, per line: Text, Start-in-ticks,
#: and the word cues as (Position, EndPosition, Start, End). Asserting the CUES
#: and not just the text is what makes this read-back a real check on the
#: enhanced-LRC parser rather than a "it came back 200".
PARITY_LRC_LINES = [
    ("First line", 10000000, []),
    # The tag index lands AFTER the space the parser inserts at a boundary, so
    # the first cue of a `<tag>word <tag>word` line ends at 7, not 6.
    ("Second line", 55000000, [(0, 7, 55000000, 60000000),
                               (7, 11, 60000000, 80000000)]),
    ("Third line", 80000000, [(0, 6, 80000000, 90000000),
                              (6, 10, 90000000, 122500000)]),
    ("Fourth line", 122500000, []),
]


def last_audio(base, token, user):
    """The LAST audio item by Path — deterministic and identical on both servers,
    and deliberately disjoint from the first three tracks reads.py seeds."""
    b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes=Audio"
                       f"&limit=500&fields=Path", token)
    by_path = {i["Path"]: i["Id"] for i in (b or {}).get("Items") or [] if i.get("Path")}
    return by_path[sorted(by_path)[-1]] if by_path else ""


def lyric_lines(doc):
    """(Text, Start, cues) per line of a LyricDto, shaped like PARITY_LRC_LINES."""
    return [(ln.get("Text"), ln.get("Start"),
             [(c.get("Position"), c.get("EndPosition"), c.get("Start"), c.get("End"))
              for c in ln.get("Cues") or []])
            for ln in (doc or {}).get("Lyrics") or []]


def stored_lyric_lines(base, token, aid):
    """The item's stored lyric lines (see `lyric_lines`), or None when it has none."""
    st, raw = http("GET", f"{base}/Audio/{aid}/Lyrics", token)
    if st != 200:
        return None
    try:
        doc = json.loads(raw)
    except ValueError:
        return None
    return lyric_lines(doc)


def await_lyric(base, token, aid, present):
    """Poll the read-back until the lyric is there (present=True) or gone."""
    for _ in range(LYRIC_REFRESH_WAIT_S):
        got = stored_lyric_lines(base, token, aid)
        if (got is not None) == present:
            return got
        time.sleep(1)
    return stored_lyric_lines(base, token, aid)


def j_lyrics(base, token, user, mid, _m2):
    """Upload a .lrc to an audio item -> it reads back as the parsed, timestamped
    lyric -> delete it -> the item has no lyrics again. Also pins the two status
    contracts the controller owes and that Ferrofin used to break: a file whose
    extension no lyric parser claims is refused (400, nothing stored), and a
    NON-audio id is a 404 on this route (`mid` is a movie) rather than an accepted
    write. The lyric is reaped whatever happened, so a leftover from an aborted run
    cannot mask the next.

    The read-back is asserted line by line AND cue by cue against
    `PARITY_LRC_LINES`, whose document deliberately includes a line with text
    before its first word tag - the shape whose position-0 cue Ferrofin used to
    drop.

    This layer only ever compares a server against its OWN read-back, so both ops
    are `effect` rows. The cross-server body diff of the parsed LyricDto - the
    thing that actually pins Metadata/Cues/blank-line handling - is reads.py's
    `GET /Audio/{itemId}/Lyrics` row, off the identical seeds it uploads."""
    r = {}
    aid = last_audio(base, token, user)
    if not aid:
        return r
    pending = False        # True while an uploaded lyric is still on this server
    try:
        http("DELETE", f"{base}/Audio/{aid}/Lyrics", token)      # start from clean
        st, raw = http("POST", f"{base}/Audio/{aid}/Lyrics?fileName=parity.lrc",
                       token, PARITY_LRC)
        pending = st == 200
        try:
            posted = json.loads(raw)
        except ValueError:
            posted = {}
        echoed = lyric_lines(posted)
        stored = await_lyric(base, token, aid, True)
        # An extension no parser claims must be refused outright, not coerced.
        bad = http("POST", f"{base}/Audio/{aid}/Lyrics?fileName=parity.foo", token, "hello")[0]
        r["POST /Audio/{itemId}/Lyrics"] = (st == 200 and echoed == PARITY_LRC_LINES
                                            and stored == PARITY_LRC_LINES and bad == 400)

        st = http("DELETE", f"{base}/Audio/{aid}/Lyrics", token)[0]
        gone = await_lyric(base, token, aid, False) is None
        # A movie id is not an audio item: every lyric route 404s on it.
        movie = http("DELETE", f"{base}/Audio/{mid}/Lyrics", token)[0]
        pending = not gone
        r["DELETE /Audio/{itemId}/Lyrics"] = st < 300 and gone and movie == 404
    finally:
        # Reap anything the run did not manage to remove. A GET that is not 200
        # does NOT prove the file is gone: Jellyfin's DELETE only unlinks
        # resolved MediaStreamType.Lyric rows and its GET reads those same rows,
        # so before the queued refresh lands the read 404s while the file is
        # still on disk. Breaking out on that 404 is how a lyric gets stranded
        # inside the container for good. Wait for it to become VISIBLE, then
        # delete, then confirm — and say so if it will not go.
        if pending:
            if await_lyric(base, token, aid, True) is not None:
                http("DELETE", f"{base}/Audio/{aid}/Lyrics", token)
            if await_lyric(base, token, aid, False) is not None:
                print(f"  WARNING: {base} still holds a lyric on {aid} — asymmetric "
                      f"state on a shared pair; remove it before the next run",
                      file=sys.stderr)
    return r


def j_merge_versions_controller(base, token, user, mid, m2):
    """The /MergeVersions/* controller (no documented params/body). Probe each with the
    ids query the sibling /Videos/MergeVersions uses; records acceptance so the differential
    reveals whether both servers implement it identically."""
    r = {}
    for op, path in [
        ("POST /MergeVersions/MergeMovies", f"/MergeVersions/MergeMovies?ids={mid},{m2}"),
        ("POST /MergeVersions/MergeEpisodes", f"/MergeVersions/MergeEpisodes?ids={mid},{m2}"),
        ("POST /MergeVersions/SplitMovies", f"/MergeVersions/SplitMovies?id={mid}"),
        ("POST /MergeVersions/SplitEpisodes", f"/MergeVersions/SplitEpisodes?id={mid}"),
    ]:
        st, _ = http("POST", f"{base}{path}", token, "")
        r[op] = st < 300
    return r


def j_quickconnect(base, token, user, _m, _m2):
    """Full QuickConnect handshake: the device initiates → the admin authorizes the code →
    the device polls Connect (now authenticated) → the device exchanges the secret for an
    access token. The device runs under its OWN DeviceId so that exchanging the secret
    issues a token for *its* session, not the harness's DeviceId='parity' session (which
    the rest of the run still needs)."""
    r = {}
    dev_hdr = {
        "Content-Type": "application/json",
        "Authorization": f'MediaBrowser Token="{token}", Client="parityqc", '
                         f'Device="parityqc", DeviceId="parity-qc", Version="1.0"',
    }

    def dev(method, path, body=None):
        req = urllib.request.Request(base + path, data=(body.encode() if body else None),
                                     method=method, headers=dev_hdr)
        try:
            with urllib.request.urlopen(req, timeout=30) as rr:
                return rr.status, rr.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            return 0, b""

    st, raw = dev("POST", "/QuickConnect/Initiate")
    init = json.loads(raw) if st < 300 and raw else {}
    secret, code = init.get("Secret"), init.get("Code")
    r["POST /QuickConnect/Initiate"] = bool(secret and code)
    if not (secret and code):
        return r
    # The admin authorizes the code with the harness token (an admin action).
    st, _ = http("POST", f"{base}/QuickConnect/Authorize?code={code}&userId={user}", token, "")
    r["POST /QuickConnect/Authorize"] = st < 300
    # The device polls Connect with its secret; after authorize it is Authenticated.
    st, raw = dev("GET", f"/QuickConnect/Connect?secret={secret}")
    conn = json.loads(raw) if st < 300 and raw else {}
    r["GET /QuickConnect/Connect"] = conn.get("Authenticated") is True
    st, raw = dev("POST", "/Users/AuthenticateWithQuickConnect", json.dumps({"Secret": secret}))
    tok2 = json.loads(raw).get("AccessToken") if st < 300 and raw else None
    r["POST /Users/AuthenticateWithQuickConnect"] = bool(tok2)
    return r


def j_users_password(base, token, user, _m, _m2):
    r = {}
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "pwprobe", "Password": "Old!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if uid:
        st, _ = http("POST", f"{base}/Users/Password?userId={uid}", token,
                     json.dumps({"CurrentPw": "Old!123", "NewPw": "New!456"}))
        # Effect: authenticating with the NEW password succeeds.
        auth = http("POST", f"{base}/Users/AuthenticateByName", token,
                    json.dumps({"Username": "pwprobe", "Pw": "New!456"}))[0]
        r["POST /Users/Password"] = st < 300 and auth == 200
        http("DELETE", f"{base}/Users/{uid}", token)   # cleanup
    return r


def j_forgot_password(base, token, user, _m, _m2):
    """The local password-reset flow on a throwaway user. ForgotPassword answers with a PIN
    challenge whose PIN is written to a file on the server host; the harness reads that file
    out of the container (docker compose exec) and redeems it. The effect (port of
    DefaultPasswordResetProvider.RedeemPasswordResetPin) is that the PIN BECOMES the user's
    password, so the read-back is a login with it. Requests come from the docker bridge (a
    private range), which both servers treat as local — the gate for this flow."""
    r = {}
    # A stale fpprobe from an aborted run would make Users/New fail and leave both ops
    # silently untested: remove it first.
    for u in get_json(base, "/Users", token) or []:
        if u.get("Name") == "fpprobe":
            http("DELETE", f"{base}/Users/{u['Id']}", token)
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": "fpprobe", "Password": "Fp!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if not uid:
        return r
    st, raw = http("POST", f"{base}/Users/ForgotPassword", None,
                   json.dumps({"EnteredUsername": "fpprobe"}))
    try:
        challenge = json.loads(raw)
    except ValueError:
        challenge = {}
    pin_file = challenge.get("PinFile") or ""
    r["POST /Users/ForgotPassword"] = (st == 200 and challenge.get("Action") == "PinCode"
                                       and bool(pin_file) and bool(challenge.get("PinExpirationDate")))
    pin = None
    contents = container_read(base, pin_file) if pin_file else None
    if contents:
        try:
            pin = json.loads(contents).get("Pin")
        except ValueError:
            pin = None
    if pin:
        st, raw = http("POST", f"{base}/Users/ForgotPassword/Pin", None, json.dumps({"Pin": pin}))
        try:
            result = json.loads(raw)
        except ValueError:
            result = {}
        login = auth_device(base, "fpprobe", pin, "parity-fpprobe")
        r["POST /Users/ForgotPassword/Pin"] = (st == 200 and result.get("Success") is True
                                               and result.get("UsersReset") == ["fpprobe"]
                                               and bool(login.get("AccessToken")))
    else:
        r["POST /Users/ForgotPassword/Pin"] = False   # PIN file unreadable (no docker access?)
    http("DELETE", f"{base}/Users/{uid}", token)   # cleanup
    return r


LIVETV_OPS = [
    "POST /LiveTv/TunerHosts", "POST /LiveTv/ListingProviders",
    "POST /LiveStreams/Open", "GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}",
    "POST /LiveStreams/Close", "GET /LiveTv/Programs/{programId}", "POST /LiveTv/Timers",
    "GET /LiveTv/Timers/{timerId}", "POST /LiveTv/Timers/{timerId}",
    "GET /LiveTv/LiveRecordings/{recordingId}/stream",
    "GET /LiveTv/Recordings/{recordingId}", "DELETE /LiveTv/Timers/{timerId}",
    "DELETE /LiveTv/Recordings/{recordingId}",
]
SERIES_TIMER_OPS = [
    "POST /LiveTv/SeriesTimers", "GET /LiveTv/SeriesTimers/{timerId}",
    "POST /LiveTv/SeriesTimers/{timerId}", "DELETE /LiveTv/SeriesTimers/{timerId}",
]
# The fields a series-timer body diff CANNOT carry across two independent
# instances, and nothing else. Both servers mint the timer's external id as a
# fresh `Guid.NewGuid()` per create (DefaultLiveTvService.cs:265) and publish its
# MD5 as `Id` (LiveTvDtoService.cs:119) — so Id/ExternalId are random by
# construction, and `derives_its_id` below checks the DERIVATION instead of
# waving them through. `ServerId` is the instance's own SystemId.
#
# `ExternalChannelId` was in this tuple and has been PUT BACK IN THE DIFF. The
# reason given for dropping it — "it embeds MD5(tuner URL), which differs
# because the two servers reach the fixture tuner on different container hosts"
# — was simply false, and measuring it said so: both servers' /System/Configuration/livetv
# configure the m3u tuner with the identical `Url` "/media/synth/livetv/channels.m3u"
# (a container-local PATH, not a host URL), both publish the same LiveTvChannel
# guids, and a series timer created on each carries the same
# `ExternalChannelId` = m3u_5581ab8b…26b. A field that agrees is a field the
# diff must carry; excluding it hid nothing today and would have hidden a real
# channel-identity divergence tomorrow.
#
# Everything else in the DTO — Name, Overview, ChannelId, ChannelName,
# ExternalChannelId, ProgramId, ExternalProgramId, Days, DayPattern, the dates,
# every padding/keep/record flag, ServiceName, Type, ImageTags — stays in the
# diff. This is scoped to this probe on purpose: parity_diff.VOLATILE is global
# and must not be widened for it.
SERIES_TIMER_PER_INSTANCE = ("Id", "ExternalId", "ServerId")
# How far ahead a programme must start before this journey will build a series
# timer on it. "Has not started yet" is not enough: the journey runs the two
# servers one after the other and takes tens of seconds per side, so a programme
# three minutes out is New on the first server and already recording on the
# second — measured, 2026-08-30 16:57Z, on the 17:00 airing: Ferrofin still had
# a `New` child and Jellyfin had none, and the hand-cancel leg could not run
# there. The fixture guide is HOURLY, so ten minutes costs at most one candidate
# programme and buys a subject that cannot start mid-journey.
SERIES_TIMER_MIN_LEAD_S = 600
#: The client-chosen name the third series-timer create posts. Lower-case on purpose,
#: where every fixture programme title is capitalised: that is what makes the
#: name-ordering legs a collation probe rather than a tautology (InvariantCulture puts
#: "apple…" first, code-point order puts "Parity…" first). It must NOT match any
#: programme title in the guide — see `j_livetv_series_timers`' docstring.
RENAMED_SERIES_TIMER_NAME = "apple parity g5"
RECORDING_START_WAIT_S = 60   # the recorder opens the tuner stream + the Recordings folder refreshes
RECORDING_POLL_S = 5
STREAM_PREFIX_BYTES = 16384   # ~87 TS packets: enough for is_mpegts, cheap to pull
STREAM_READ_TIMEOUT_S = 30


def read_prefix(base, path, token, n=STREAM_PREFIX_BYTES, timeout=STREAM_READ_TIMEOUT_S):
    """(status, content-type, first n bytes) of a progressive/endless response — a live tuner
    stream or an in-progress recording never ends, so only a prefix is read."""
    hdr = {"Authorization": f'MediaBrowser Token="{token}", {CLIENT}'}
    req = urllib.request.Request(base + path, headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.headers.get("Content-Type", ""), r.read(n)
    except urllib.error.HTTPError as e:
        return e.code, "", b""
    except (urllib.error.URLError, TimeoutError, ConnectionError, OSError):
        return 0, "", b""


def is_mpegts(body):
    return body[:1] == b"\x47" and len(body) > 188 and body[188:189] == b"\x47"


def mpegts_response(st, ct, body):
    return st == 200 and ct.split(";")[0].strip().lower() == "video/mp2t" and is_mpegts(body)


def timer_update_leg(base, token, user, ch):
    """`POST /LiveTv/Timers/{timerId}` on a timer of its own, for a programme that has
    NOT started yet.

    It cannot ride on the journey's main timer: that one records the programme airing
    right now, so its recorder fires within a second of the create and
    `DefaultLiveTvService.UpdateTimerAsync` (v10.11.8 DefaultLiveTvService.cs:342-363)
    then refuses to touch it — "// Only update if not currently active". Measured on the
    lab pair: the same leg was green on one run and red on the next with Jellyfin's
    paddings unchanged, because its capture had started and Ferrofin's had not. Racing
    the recorder is not a parity signal, so this leg brings its own quiet timer.

    What it asserts is the C# whitelist: the FOUR padding fields are taken and the rest
    of the posted body — Name, Priority, StartDate, and a Status=Cancelled that must not
    cancel anything — is discarded. Plus: an id nothing matches must not MINT a timer.
    Only the "no phantom" half of that is compared across the two servers; upstream's
    intended answer there is `ResourceNotFoundException` (:346-349) but
    `LiveTvDtoService.GetTimerInfo` dereferences a null series timer first
    (LiveTvDtoService.cs:453-458), so Jellyfin really answers 500 where Ferrofin answers
    404 — a Jellyfin bug, not a status to agree on."""
    programs = (get_json(base, f"/LiveTv/Programs?channelIds={ch}&userId={user}", token)
                or {}).get("Items") or []
    now = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
    tid = None
    # A programme a leftover series timer already scheduled is a 400 ("a scheduled
    # recording already exists for this program") on both servers, so try a few.
    for prog in [p for p in programs if (p.get("StartDate") or "") > now][:5]:
        defaults = get_json(base, f"/LiveTv/Timers/Defaults?programId={prog['Id']}", token) or {}
        st, _ = http("POST", f"{base}/LiveTv/Timers", token, json.dumps(defaults))
        if st >= 300:
            continue
        timers = (get_json(base, f"/LiveTv/Timers?channelId={ch}", token) or {}).get("Items") or []
        found = next((t for t in timers if t.get("ProgramId") == prog["Id"]), None)
        if found:
            tid = found.get("Id")
            break
    if not tid:
        return False
    try:
        got = get_json(base, f"/LiveTv/Timers/{tid}", token) or {}
        upd = dict(got)
        upd.update(PrePaddingSeconds=300, PostPaddingSeconds=600,
                   IsPrePaddingRequired=True, IsPostPaddingRequired=True,
                   Name="parity-update-must-be-ignored", Priority=42,
                   Status="Cancelled", StartDate="2027-01-01T00:00:00.0000000Z")
        st, _ = http("POST", f"{base}/LiveTv/Timers/{tid}", token, json.dumps(upd))
        back = get_json(base, f"/LiveTv/Timers/{tid}", token) or {}
        # ExternalId is stripped from the ghost body on purpose: leave it in and
        # Jellyfin resolves it to the REAL timer and updates that one instead.
        ghost = "00000000000000000000000000009999"
        ghost_body = {k: v for k, v in upd.items() if k not in ("Id", "ExternalId")}
        http("POST", f"{base}/LiveTv/Timers/{ghost}", token,
             json.dumps(dict(ghost_body, Id=ghost)))
        return Same(st < 300, {
            "PrePaddingSeconds": back.get("PrePaddingSeconds"),
            "PostPaddingSeconds": back.get("PostPaddingSeconds"),
            "IsPrePaddingRequired": back.get("IsPrePaddingRequired"),
            "IsPostPaddingRequired": back.get("IsPostPaddingRequired"),
            "NameUnchanged": back.get("Name") == got.get("Name"),
            "PriorityUnchanged": back.get("Priority") == got.get("Priority"),
            "StartDateUnchanged": back.get("StartDate") == got.get("StartDate"),
            "StatusNotTheCancelledWePosted": back.get("Status") != "Cancelled",
            # NOT "is 404": `get_json` answers None for any non-200 AND for a 200
            # whose body is `null`, which is literally what Jellyfin returns here.
            # All this establishes is that no timer exists to read — see the
            # docstring for why the status itself is not a parity signal.
            "UnknownIdHasNoTimerToRead": get_json(base, f"/LiveTv/Timers/{ghost}", token) in (None, {}),
        })
    finally:
        http("DELETE", f"{base}/LiveTv/Timers/{tid}", token)


def j_livetv(base, token, user, _m, _m2):
    """The Live TV flow on the M3U fixture tuner: PlaybackInfo on a channel hands out an
    OpenToken → LiveStreams/Open returns a live media source → its LiveStreamFiles stream
    is a real MPEG-TS → Close revokes it. Then a timer on the programme airing now starts
    a recording → the in-progress recording streams → timer and recording are deleted.
    Every op starts False so an early exit (no channels, no programme, no timer) leaves a
    flagged row, never a missing one."""
    r = dict.fromkeys(LIVETV_OPS, False)
    channels = (get_json(base, f"/LiveTv/Channels?userId={user}", token) or {}).get("Items") or []
    # Provisioning (sweep.provision_livetv) added the tuner host and the listings provider;
    # their effect is what this journey runs on: channels from the tuner, programmes from
    # the guide.
    # EFFECT, at one remove: the POST is issued by sweep.provision_livetv (the tuner
    # host must exist before this layer runs at all), and what is confirmed here is
    # its effect on each server's own read-back — no tuner host, no channels. The
    # row is not a read of the POST's response and never was; the note says so.
    r["POST /LiveTv/TunerHosts"] = bool(channels)
    if not channels:
        return r
    ch = channels[0]["Id"]
    programs = (get_json(base, f"/LiveTv/Programs?channelIds={ch}&isAiring=true&userId={user}", token)
                or {}).get("Items") or []
    # Same shape: the listings provider is provisioned upstream and its effect is
    # the guide having programmes on this server.
    r["POST /LiveTv/ListingProviders"] = bool(programs)
    # --- live stream -------------------------------------------------------------------
    _, raw = http("POST", f"{base}/Items/{ch}/PlaybackInfo?userId={user}", token, json.dumps({}))
    try:
        sources = json.loads(raw).get("MediaSources") or []
    except ValueError:
        sources = []
    token_src = next((s for s in sources if s.get("OpenToken")), None)
    live = {}
    if token_src:
        st, raw = http("POST", f"{base}/LiveStreams/Open", token, json.dumps({
            "OpenToken": token_src["OpenToken"], "UserId": user, "ItemId": ch,
            "PlaySessionId": "parity-livetv", "EnableDirectPlay": True, "EnableDirectStream": True}))
        try:
            live = json.loads(raw).get("MediaSource") or {}
        except ValueError:
            live = {}
        r["POST /LiveStreams/Open"] = st == 200 and bool(live.get("LiveStreamId"))
    stream_id = live.get("LiveStreamId") or ""
    path = live.get("Path") or ""
    if "/LiveTv/LiveStreamFiles/" in path:
        stream_id = path.split("/LiveTv/LiveStreamFiles/", 1)[1].split("/", 1)[0]
    if stream_id:
        r["GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}"] = mpegts_response(
            *read_prefix(base, f"/LiveTv/LiveStreamFiles/{stream_id}/stream.ts", token))
    if live.get("LiveStreamId"):
        st, _ = http("POST", f"{base}/LiveStreams/Close?liveStreamId={urllib.parse.quote(live['LiveStreamId'])}",
                     token, "")
        gone = read_prefix(base, f"/LiveTv/LiveStreamFiles/{stream_id}/stream.ts", token, timeout=10)[0]
        r["POST /LiveStreams/Close"] = st < 300 and gone != 200
    # --- timer → in-progress recording -------------------------------------------------
    if not programs:
        return r
    prog = programs[0]["Id"]
    got = get_json(base, f"/LiveTv/Programs/{prog}?userId={user}", token) or {}
    r["GET /LiveTv/Programs/{programId}"] = got.get("Id") == prog
    defaults = get_json(base, f"/LiveTv/Timers/Defaults?programId={prog}", token) or {}
    st, _ = http("POST", f"{base}/LiveTv/Timers", token, json.dumps(defaults))
    timers = (get_json(base, f"/LiveTv/Timers?channelId={ch}", token) or {}).get("Items") or []
    timer = next((t for t in timers if t.get("ProgramId") == prog), None) or (timers[0] if timers else None)
    r["POST /LiveTv/Timers"] = st < 300 and timer is not None
    if not timer:
        return r
    tid = timer.get("Id")
    rec = None
    try:
        got = get_json(base, f"/LiveTv/Timers/{tid}", token) or {}
        r["GET /LiveTv/Timers/{timerId}"] = got.get("Id") == tid
        r["POST /LiveTv/Timers/{timerId}"] = timer_update_leg(base, token, user, ch)
        for _ in range(RECORDING_START_WAIT_S // RECORDING_POLL_S):
            recs = (get_json(base, f"/LiveTv/Recordings?isInProgress=true&userId={user}", token)
                    or {}).get("Items") or []
            if recs:
                rec = recs[0]
                break
            time.sleep(RECORDING_POLL_S)
        if rec:
            rid = rec["Id"]
            # An in-progress recording is served through /LiveTv/LiveRecordings/{id}/stream,
            # keyed by Jellyfin's INTERNAL timer id (not the timer DTO's hashed id): the only
            # way a client learns it is PlaybackInfo on the recording item, whose media
            # source carries that URL as EncoderPath (the direct Path is the growing file).
            _, raw = http("POST", f"{base}/Items/{rid}/PlaybackInfo?userId={user}", token, json.dumps({}))
            try:
                paths = [(m.get("EncoderPath") or "") + " " + (m.get("Path") or "")
                         for m in json.loads(raw).get("MediaSources") or []]
            except ValueError:
                paths = []
            live_path = next((p for p in paths if "/LiveTv/LiveRecordings/" in p), "")
            if live_path:
                key = live_path.split("/LiveTv/LiveRecordings/", 1)[1].split("/", 1)[0]
                r["GET /LiveTv/LiveRecordings/{recordingId}/stream"] = mpegts_response(
                    *read_prefix(base, f"/LiveTv/LiveRecordings/{key}/stream", token))
            r["GET /LiveTv/Recordings/{recordingId}"] = (
                (get_json(base, f"/LiveTv/Recordings/{rid}?userId={user}", token) or {}).get("Id") == rid)
    finally:
        # Whatever happened above, the timer (and with it the recording in progress) goes.
        st, _ = http("DELETE", f"{base}/LiveTv/Timers/{tid}", token)
        left = (get_json(base, f"/LiveTv/Timers?channelId={ch}", token) or {}).get("Items") or []
        r["DELETE /LiveTv/Timers/{timerId}"] = st < 300 and all(t.get("Id") != tid for t in left)
        if rec:
            st, _ = http("DELETE", f"{base}/LiveTv/Recordings/{rec['Id']}", token)
            still = get_json(base, f"/LiveTv/Recordings/{rec['Id']}?userId={user}", token)
            r["DELETE /LiveTv/Recordings/{recordingId}"] = st < 300 and not still
    return r


LIVETV_ADMIN_OPS = [
    "POST /LiveTv/Tuners/{tunerId}/Reset", "POST /LiveTv/ChannelMappings",
    "DELETE /LiveTv/ListingProviders", "DELETE /LiveTv/TunerHosts",
]
# MD5 of the UTF-16LE bytes of "Jellyfin.LiveTv.DefaultLiveTvService" rendered as
# a .NET Guid "N" — the service key `LiveTvManager.ResetTuner` splits `tunerId`
# on (LiveTvManager.cs:1233-1245). A prefix that names no registered service is
# `ArgumentException("Service not found.")` → 400.
LIVETV_SERVICE_KEY = "af999c25a00715699361240d4c6c7a53"
LIVETV_ADMIN_POLL_S = 5
LIVETV_ADMIN_WAIT_S = 120   # Jellyfin drains/rebuilds the guide via a QUEUED task


def _livetv_config(base, token):
    cfg = get_json(base, "/System/Configuration/livetv", token) or {}
    return (cfg.get("TunerHosts") or []), (cfg.get("ListingProviders") or [])


def _livetv_counts(base, token, user):
    """(channels, programmes) as this server reports them right now, or -1 for a
    count this server would not answer.

    -1 means "the read failed", NEVER "zero": a request that 500s or times out
    while a guide refresh holds the writer must not be scored as "the guide is
    empty" or "the channels are gone". `_wait_until` discards such a sample.
    """
    ch = get_json(base, "/LiveTv/Channels?limit=0&enableTotalRecordCount=true"
                        f"&userId={user}", token) or {}
    pr = get_json(base, "/LiveTv/Programs?limit=0&enableTotalRecordCount=true"
                        f"&userId={user}", token) or {}
    return ch.get("TotalRecordCount", -1), pr.get("TotalRecordCount", -1)


def _wait_until(base, token, user, predicate):
    """Polls (channels, programmes) until `predicate` holds on a sample both reads
    answered. Jellyfin does this work on a queued scheduled task and Ferrofin
    inline, so the wait is what makes the two comparable rather than a race.

    A sample carrying a -1 is discarded rather than tested: judging a verdict on
    a read that failed is how a probe reports a server bug that never happened."""
    counts = (-1, -1)
    for _ in range(LIVETV_ADMIN_WAIT_S // LIVETV_ADMIN_POLL_S):
        counts = _livetv_counts(base, token, user)
        if min(counts) >= 0 and predicate(*counts):
            return True, counts
        time.sleep(LIVETV_ADMIN_POLL_S)
    return False, counts


def _settle_guide(base, token, user, channels, programs):
    """Waits for the guide to finish rebuilding — hygiene, not an assertion.

    Jellyfin rebuilds on a QUEUED scheduled task, so "programmes are listed again"
    can be true while the task is still half way through. Handing the lab back in
    that state would make whatever runs next read a partial guide, so the journey
    waits for the pre-journey counts to come back before it returns. The verdict is
    already decided; this only refuses to leave a mess."""
    _wait_until(base, token, user, lambda ch, pr: ch >= channels and pr >= programs)


def _refresh_guide(base, token):
    tasks = get_json(base, "/ScheduledTasks", token) or []
    guide = next((t for t in tasks if t.get("Key") == "RefreshGuide"), None)
    if guide:
        http("POST", f"{base}/ScheduledTasks/Running/{guide['Id']}", token, "")


def j_livetv_admin(base, token, user, _m, _m2):
    """Tuner-host and listings-provider administration, on the fixture's own tuner.

    Runs AFTER j_livetv (and after every Live TV read probe in reads.py) because it
    mutates the configuration those depend on, and it puts back everything it takes
    away: the listings provider and the tuner host are re-added from the bodies read
    at the top, and the journey does not return until channels AND programmes are
    listed again on this server.

    Every op starts False, so an early exit (no tuner fixture, no channels) leaves a
    flagged row rather than a missing one."""
    r = dict.fromkeys(LIVETV_ADMIN_OPS, False)
    tuners, providers = _livetv_config(base, token)
    if not tuners or not providers:
        return r
    tuner, provider = tuners[0], providers[0]
    tuner_id, provider_id = tuner.get("Id") or "", provider.get("Id") or ""
    base_channels, base_programs = _livetv_counts(base, token, user)
    if base_channels <= 0 or base_programs <= 0:
        return r

    # --- reset a tuner (no mutation: the C# success path is Task.CompletedTask) ----
    good, _ = http("POST", f"{base}/LiveTv/Tuners/{LIVETV_SERVICE_KEY}_1/Reset", token, "")
    bad, _ = http("POST", f"{base}/LiveTv/Tuners/nosuchservice/Reset", token, "")
    r["POST /LiveTv/Tuners/{tunerId}/Reset"] = good == 204 and bad == 400

    # --- map a tuner channel onto the OTHER channel's listings --------------------
    opts = get_json(base, f"/LiveTv/ChannelMappingOptions?providerId={provider_id}", token) or {}
    tuner_channels = opts.get("TunerChannels") or []
    if len(tuner_channels) >= 2:
        target = tuner_channels[1]["Id"]                       # the channel being re-pointed
        onto = tuner_channels[0].get("ProviderChannelId") or ""  # the other channel's guide id
        st, raw = http("POST", f"{base}/LiveTv/ChannelMappings", token, json.dumps({
            "ProviderId": provider_id, "TunerChannelId": target, "ProviderChannelId": onto}))
        try:
            mapped = json.loads(raw)
        except ValueError:
            mapped = {}
        # The response is the RESOLVED row: the id asked for, the provider channel
        # asked for, and the provider channel's NAME — which a handler that merely
        # echoes the request body cannot produce.
        posted_ok = (st == 200 and mapped.get("Id") == target
                     and mapped.get("ProviderChannelId") == onto
                     and bool(mapped.get("ProviderChannelName")))
        # EFFECT, on this server's own read-back: the pair is stored, and the tuner
        # channel now reports the guide channel it was pointed at.
        after = get_json(base, f"/LiveTv/ChannelMappingOptions?providerId={provider_id}", token) or {}
        stored = after.get("Mappings") or []
        moved = next((c for c in (after.get("TunerChannels") or []) if c.get("Id") == target), {})
        applied = (any(p.get("Name") == target and p.get("Value") == onto for p in stored)
                   and moved.get("ProviderChannelId") == onto)
        # RESTORE: re-posting the identical pair is the C# toggle — `channelMappingExists`
        # suppresses the re-add after the unconditional removal (ListingsManager.cs:233-249),
        # so the mapping list goes back to empty. Verified live against Jellyfin 10.11.8.
        http("POST", f"{base}/LiveTv/ChannelMappings", token, json.dumps({
            "ProviderId": provider_id, "TunerChannelId": target, "ProviderChannelId": onto}))
        restored = get_json(base, f"/LiveTv/ChannelMappingOptions?providerId={provider_id}", token) or {}
        r["POST /LiveTv/ChannelMappings"] = (posted_ok and applied
                                             and not (restored.get("Mappings") or []))

    # --- remove the listings provider: the guide drains, the channels stay --------
    # The id is CASE-FLIPPED on purpose: both C# deletes filter with
    # StringComparison.OrdinalIgnoreCase, and a server matching the stored key
    # byte-for-byte would 204 and delete nothing.
    st, _ = http("DELETE", f"{base}/LiveTv/ListingProviders?id={provider_id.upper()}", token)
    gone = not _livetv_config(base, token)[1]
    drained, counts = _wait_until(base, token, user, lambda ch, pr: pr == 0)
    channels_survived = counts[0] == base_channels
    # RESTORE the provider (Jellyfin mints a NEW id here — `index == -1` — so nothing
    # may cache the old one) and wait for the guide to come back.
    http("POST", f"{base}/LiveTv/ListingProviders?validateListings=false", token, json.dumps(provider))
    _refresh_guide(base, token)
    back, _ = _wait_until(base, token, user, lambda ch, pr: pr > 0)
    r["DELETE /LiveTv/ListingProviders"] = (st == 204 and gone and drained
                                            and channels_survived and back)

    # --- remove the tuner host ---------------------------------------------------
    # Asserted on the CONFIG read-back, not on the channel count: C# DeleteTunerHost
    # only rewrites the configuration and (unlike DeleteListingsProvider) queues no
    # guide refresh, so Jellyfin's channel items linger until the next one, while
    # Ferrofin's cascade away with the host row. The configuration is what both
    # servers agree the delete means.
    #
    # That narrowing hides a REAL difference unless the difference is written down
    # somewhere the ledger reads, so it is: `classifications.json` carries
    # `DELETE /LiveTv/TunerHosts` as an accepted divergence with `scope: side-path`
    # — a note gen-ledger renders in its own section next to this row's verdict,
    # that can never be counted as this row's verdict, and that can never absorb a
    # future red. Change one, change the other.
    st, _ = http("DELETE", f"{base}/LiveTv/TunerHosts?id={tuner_id.upper()}", token)
    tuner_gone = not _livetv_config(base, token)[0]
    # RESTORE the tuner host and rebuild the guide behind it.
    http("POST", f"{base}/LiveTv/TunerHosts", token, json.dumps(tuner))
    _refresh_guide(base, token)
    whole, _ = _wait_until(base, token, user,
                           lambda ch, pr: ch >= base_channels and pr > 0)
    r["DELETE /LiveTv/TunerHosts"] = st == 204 and tuner_gone and whole
    _settle_guide(base, token, user, base_channels, base_programs)
    return r


def dotnet_md5_guid_n(text):
    """Jellyfin's `string.GetMD5().ToString("N")`: MD5 over UTF-16LE bytes, read back
    as a .NET `Guid` (Data1/Data2/Data3 are little-endian) and printed without dashes.

    This is `LiveTvDtoService.GetInternalSeriesTimerId` (v10.11.8 LiveTvDtoService.cs:417-421)
    when fed `"Emby" + externalId + "4"` lowercased. Having it here is what lets the
    journey assert that a server DERIVED its series-timer id rather than minting one —
    the alternative would be to drop Id from the comparison and call that verified."""
    import hashlib
    d = hashlib.md5(text.encode("utf-16-le")).digest()
    return (bytes([d[3], d[2], d[1], d[0], d[5], d[4], d[7], d[6]]) + d[8:]).hex()


def derives_its_id(dto):
    """True when this series timer's Id is the MD5 upstream derives from its ExternalId."""
    ext = dto.get("ExternalId") or ""
    return bool(ext) and dto.get("Id") == dotnet_md5_guid_n(("Emby" + ext + "4").lower())


def series_timer_body(dto):
    """The read-back DTO with exactly the per-instance fields dropped — see
    SERIES_TIMER_PER_INSTANCE for why each one, and why nothing else is dropped."""
    return {k: v for k, v in (dto or {}).items() if k not in SERIES_TIMER_PER_INSTANCE}


def series_timer_ids(base, token, query=""):
    items = (get_json(base, f"/LiveTv/SeriesTimers{query}", token) or {}).get("Items") or []
    return [t.get("Id") for t in items]


def children_of(base, token, series_timer_id):
    """The timers a series timer has scheduled, as `GET /LiveTv/Timers` publishes them.

    That view excludes `Completed` on both servers (`GetTimersAsync`, v10.11.8
    DefaultLiveTvService.cs:392-403), so it is the right lens for "what is still
    going to record" and the wrong one for "what rows exist" — which is exactly why
    the Completed-child leak in `CancelSeriesTimerAsync` needed a unit test rather
    than a journey leg."""
    return [t for t in ((get_json(base, "/LiveTv/Timers", token) or {}).get("Items") or [])
            if t.get("SeriesTimerId") == series_timer_id]


def j_livetv_series_timers(base, token, user, _m, _m2):
    """The series-timer lifecycle on the fixture tuner: pick three FUTURE programmes with
    different titles from the guide, create a series timer from each programme's own
    `Timers/Defaults` body, read one back, update it, delete it, and confirm it and the
    timers it scheduled are gone — then clean the other two up.

    THE NAME IS NOT A FREE PARAMETER, and an earlier version of this probe did not know
    that. `GetTimersForSeries` (v10.11.8 DefaultLiveTvService.cs:803-821, the `query.Name` line is
    820) builds the
    fan-out query with `ExternalSeriesId = seriesTimer.SeriesId` and then, when that is
    empty, `query.Name = seriesTimer.Name`. The XMLTV fixture publishes no series id, so
    the fan-out matches programmes BY NAME on both servers. Overwriting `defaults["Name"]`
    before the create therefore makes the series timer match NOTHING — measured on the
    pair 2026-08-31, identically on both servers: unmodified defaults schedule 7 showings
    (1 New, 6 Cancelled), renamed defaults schedule 0. The old probe renamed both creates
    and then asserted the fan-out on one of them, so `posted_name_kept` and `fan_out` were
    mutually exclusive and the row could not pass on ANY server, Jellyfin included. That
    was a broken probe, not a Ferrofin gap.

    So the two roles are split across different series timers, and BOTH are still measured:
      * the first two creates leave `Name` exactly as `Timers/Defaults` published it, and
        carry the fan-out;
      * a third create on a THIRD programme posts a client-chosen `Name`, and carries
        `posted_name_kept` plus the positive form of the rule above — a renamed series
        timer with no SeriesId schedules nothing. The third programme is a different one
        on purpose: `CreateSeriesTimer` re-parents any existing timer with the same
        `ProgramId` onto the new series timer (:263-305), so building the renamed one on
        a programme another series timer already owns would STEAL a showing from it and
        make both rows lie.

    The assertions here are the ones that were missing, and each catches a real bug:
      * three creates from three different programmes leave THREE rows with DIFFERENT ids.
        `Timers/Defaults` hands every programme the same constant Id and clients post
        it straight back, so a server that honours it collapses every series timer
        onto one row and silently destroys the previous one.
      * the created Id is the MD5 the C# derives from a freshly minted ExternalId
        (`derives_its_id`), not the posted one and not a random GUID in DB casing.
      * creating a series timer SCHEDULES something: at least one timer carrying
        SeriesTimerId. A series timer that records nothing passes every status check.
      * …and a series timer whose Name matches no programme schedules NOTHING, which is
        the same rule read from the other side.
      * editing the series timer does not RESURRECT a showing the user cancelled by
        hand — the `IsManual` contract. Ferrofin wrote that flag on INSERT only, so
        cancelling a child (always an UPDATE) never raised it and the next edit put
        the showing back to New.
      * the update keys on the BODY's id, not the route's, exactly as upstream does.

    Every op starts False so an early exit (no channels, no future programmes) leaves a
    flagged row, never a missing one."""
    r = dict.fromkeys(SERIES_TIMER_OPS, False)
    channels = (get_json(base, f"/LiveTv/Channels?userId={user}", token) or {}).get("Items") or []
    if not channels:
        return r
    ch = channels[0]["Id"]
    programs = (get_json(base, f"/LiveTv/Programs?channelIds={ch}&userId={user}", token)
                or {}).get("Items") or []
    # A programme that has already ended schedules nothing (`MinEndDate = UtcNow` in
    # `GetTimersForSeries`), so the fan-out assertion would pass vacuously on it —
    # and one that is airing RIGHT NOW is worse: its timer fires the moment the
    # series timer is created, so the earliest showing races the recorder through
    # New → InProgress → Completed while the journey is still reading. Only a
    # programme SERIES_TIMER_MIN_LEAD_S out is a stable subject; see that constant
    # for why "has not started yet" was not a wide enough margin.
    now = time.strftime("%Y-%m-%dT%H:%M:%S",
                        time.gmtime(time.time() + SERIES_TIMER_MIN_LEAD_S))
    future = [p for p in programs if (p.get("StartDate") or "") > now]
    picked, seen = [], set()
    for p in future:                      # three DIFFERENT titles: three independent series
        if p.get("Name") not in seen:
            seen.add(p.get("Name"))
            picked.append(p)
        if len(picked) == 3:
            break
    if len(picked) < 3:
        return r
    created = []
    try:
        # --- create, three times, from three different programmes' defaults ----------
        # The first two keep the name `Timers/Defaults` published (so they fan out over
        # the guide); the third posts a client-chosen one. See the docstring for why the
        # two roles cannot live on the same series timer.
        create_ok, evidence = True, {}
        # `RENAMED_SERIES_TIMER_NAME` is deliberately lower-case where the fixture's
        # programme titles are capitalised: it is what makes the name-ordering legs
        # below a real collation probe. `StringComparison.InvariantCulture` sorts
        # "apple parity g5" BEFORE "Parity Show …"; code-point order sorts 'P' (U+0050)
        # before 'a' (U+0061) and answers the other way round.
        for n, prog in enumerate(picked):
            before = set(series_timer_ids(base, token))
            defaults = get_json(base, f"/LiveTv/Timers/Defaults?programId={prog['Id']}", token) or {}
            defaults["Priority"] = 3 + n * 5          # non-default: the create must DISCARD it
            if n == 2:
                # A client-chosen Name, which the create must KEEP:
                # `LiveTvDtoService.GetSeriesTimerInfo` binds `Name = dto.Name`
                # (v10.11.8 LiveTvDtoService.cs:499) and neither
                # `DefaultLiveTvService.CreateSeriesTimer` (:263-309) nor
                # `UpdateSeriesTimerAsync`'s whitelist (:314-334) ever writes it, so
                # create is the ONLY place a name can be set.
                defaults["Name"] = RENAMED_SERIES_TIMER_NAME
            st, _ = http("POST", f"{base}/LiveTv/SeriesTimers", token, json.dumps(defaults))
            fresh = [i for i in series_timer_ids(base, token) if i not in before]
            created += fresh
            dto = get_json(base, f"/LiveTv/SeriesTimers/{fresh[0]}", token) if fresh else {}
            evidence[f"created{n}"] = {
                "status_ok": st < 300,
                "exactly_one_new_row": len(fresh) == 1,
                "id_is_not_the_posted_defaults_id": bool(fresh) and fresh[0] != defaults.get("Id"),
                "id_is_derived_from_external_id": derives_its_id(dto or {}),
                "external_channel_id_set": bool((dto or {}).get("ExternalChannelId")),
                "program_id": (dto or {}).get("ProgramId") == prog["Id"],
                # `LiveTvManager.CreateSeriesTimer` overwrites the posted Priority
                # with the standing defaults' ("// Set priority from default
                # values", LiveTvManager.cs:1145-1147). The body above posted a
                # non-default one, so a server that honours it fails here — and
                # would then also make the sort leg below meaningless.
                "posted_priority_discarded": (dto or {}).get("Priority") == 0,
                # For n in (0, 1) this says the create did not INVENT a name; for
                # n == 2 it says the create honoured the client's.
                "posted_name_kept": (dto or {}).get("Name") == defaults["Name"],
            }
            create_ok = create_ok and all(evidence[f"created{n}"].values())
        evidence["three_distinct_ids"] = len(set(created)) == 3
        create_ok = create_ok and evidence["three_distinct_ids"]
        if len(created) < 3:
            r["POST /LiveTv/SeriesTimers"] = Same(False, evidence)
            return r
        sid, other, renamed = created[0], created[1], created[2]
        # The other side of `GetTimersForSeries`' name rule (DefaultLiveTvService.cs
        # :812-821): with `SeriesId` empty — which is every timer in this XMLTV fixture —
        # the fan-out query is `Name = seriesTimer.Name`, so a series timer the client
        # renamed matches no programme and schedules nothing at all. Measured identical
        # on both servers. This leg is what keeps the split above honest: without it,
        # moving `posted_name_kept` onto its own timer would have quietly dropped the
        # only evidence that the name is what the fan-out keys on.
        evidence["renamed_series_timer_schedules_nothing"] = \
            len(children_of(base, token, renamed)) == 0
        create_ok = create_ok and evidence["renamed_series_timer_schedules_nothing"]
        # …and the create SCHEDULED something: a series timer that records nothing
        # passes every status check ever written, which is how this went unnoticed.
        #
        # The invariant compared is the shape upstream produces, not the raw count.
        # Every airing of one title in this fixture hashes to the same `ShowId`
        # (`XmlTvListingsProvider.cs:186-206` — no episode info, so it is MD5(title)),
        # so `SearchForDuplicateShowIds` (DefaultLiveTvService.cs:681-707) records the
        # EARLIEST showing and cancels the rest. The raw count is deliberately left out
        # of the comparison: it is "future airings of this title at the instant the
        # request landed", and the two servers are called seconds apart, so it moves by
        # one across an hour boundary in the fixture's hourly guide. `>= 2` still gates
        # that the fan-out really walked the guide rather than scheduling the one
        # programme it was handed.
        children = children_of(base, token, sid)
        recordable = [t for t in children if t.get("Status") != "Cancelled"]
        earliest = min(children, key=lambda t: t.get("StartDate") or "", default=None)
        evidence["fan_out"] = {
            "scheduled_more_than_one_showing": len(children) >= 2,
            "exactly_one_showing_is_recordable": len(recordable) == 1,
            "the_recordable_one_is_the_earliest":
                bool(earliest) and earliest.get("Status") != "Cancelled",
        }
        create_ok = create_ok and all(evidence["fan_out"].values())
        r["POST /LiveTv/SeriesTimers"] = Same(create_ok, evidence)

        # --- read one back: the whole body, diffed against the other server ------------
        dto = get_json(base, f"/LiveTv/SeriesTimers/{sid}", token) or {}
        r["GET /LiveTv/SeriesTimers/{timerId}"] = Same(
            dto.get("Id") == sid and derives_its_id(dto), series_timer_body(dto))

        # --- update: the C# whitelist, and nothing else --------------------------------
        ghost = "0000000000000000000000000000dead"
        posted = dict(dto)
        posted.update(PrePaddingSeconds=120, PostPaddingSeconds=240,
                      IsPrePaddingRequired=True, Priority=7, KeepUpTo=3,
                      Days=["Saturday", "Sunday"], DayPattern="Daily",
                      Name="RENAMED", Overview="RENAMED")

        # Before the edit: a person cancels the ONE showing this series timer was
        # going to record. The edit below must not put it back. `CancelTimerAsync`
        # cancels manually (v10.11.8 DefaultLiveTvService.cs:199-203), which flags the
        # surviving row `IsManual` (:176-180); the next fan-out copies that flag onto
        # the candidate (:745) and both arms that could revive it are guarded on it
        # (`ShouldCancelTimerForSeriesTimer`'s first arm :646, and the
        # `else if (!existingTimer.IsManual)` at :751). Ferrofin wrote `IsManual` on
        # INSERT only, so cancelling a child — always an UPDATE, because a child keeps
        # its SeriesTimerId and so is updated rather than deleted — never raised it.
        # Measured on the pair before the fix: Jellyfin left the cancelled showing
        # `Cancelled` and promoted the next one, Ferrofin set it back to `New` and
        # would have recorded it. The create leg above has already established that
        # `recordable` holds exactly one timer on both servers.
        victim = recordable[0].get("Id") if len(recordable) == 1 else None
        if victim:
            http("DELETE", f"{base}/LiveTv/Timers/{victim}", token)

        st, _ = http("POST", f"{base}/LiveTv/SeriesTimers/{sid}", token, json.dumps(posted))
        updated = get_json(base, f"/LiveTv/SeriesTimers/{sid}", token) or {}

        hand_cancel = {"ran": False, "recordable_children": len(recordable)}
        if victim:
            kids_after = children_of(base, token, sid)
            same_showing = next((t for t in kids_after if t.get("Id") == victim), None)
            hand_cancel = {
                "ran": True,
                "the_cancelled_showing_is_not_new_again":
                    same_showing is not None and same_showing.get("Status") != "New",
                "another_showing_took_its_place":
                    any(t.get("Status") == "New" and t.get("Id") != victim
                        for t in kids_after),
            }

        # The row is keyed on the BODY's id, not the route's: `LiveTvController`
        # (v10.11.8 LiveTvController.cs:933-937) hands the manager the body alone and
        # never reads `timerId`, and `UpdateSeriesTimerAsync` matches on `info.Id`
        # (DefaultLiveTvService.cs:314). So a body carrying a FOREIGN id, posted to a
        # perfectly valid route, must change nothing. This leg exists because Ferrofin
        # used to fall back to the route id when the body id matched no row — strictly
        # more lenient than upstream, and invisible to the ghost leg below (which
        # misses on both ids at once and so cannot tell the two rules apart).
        http("POST", f"{base}/LiveTv/SeriesTimers/{sid}", token,
             json.dumps(dict(posted, Id=ghost, ExternalId="",
                             PrePaddingSeconds=999, PostPaddingSeconds=888)))
        after_foreign = get_json(base, f"/LiveTv/SeriesTimers/{sid}", token) or {}

        # An id nothing matches must write nothing: no ghost row, no readable row after.
        http("POST", f"{base}/LiveTv/SeriesTimers/{ghost}", token,
             json.dumps(dict(posted, Id=ghost, ExternalId="")))

        after = get_json(base, f"/LiveTv/SeriesTimers/{sid}", token) or {}
        r["POST /LiveTv/SeriesTimers/{timerId}"] = Same(st < 300 and bool(after), {
            "body": series_timer_body(after),
            # Named here as well as inside the body so a failure says WHICH rule broke:
            # the whitelist keeps Name/Overview, and DayPattern is recomputed from Days
            # (`GetDayPattern`, LiveTvDtoService.cs:360-387) — [Sat,Sun] is Weekends even
            # though the posted body said Daily.
            "name_not_updatable": after.get("Name") == dto.get("Name"),
            "overview_not_updatable": after.get("Overview") == dto.get("Overview"),
            "day_pattern_recomputed": after.get("DayPattern") == "Weekends",
            "foreign_body_id_changed_nothing":
                series_timer_body(after_foreign) == series_timer_body(updated),
            # Deliberately NOT named "is 404": `get_json` returns None for any
            # non-200, and what this leg establishes is only "no row exists to
            # read". The status itself is not compared here because upstream's is
            # a Jellyfin bug — see `timer_update_leg`'s docstring.
            "unknown_id_has_no_row_to_read":
                get_json(base, f"/LiveTv/SeriesTimers/{ghost}", token) in (None, {}),
            "hand_cancelled_showing_survives_the_edit": hand_cancel,
        })

        # --- sortBy is honoured, not silently dropped ---------------------------------
        # This runs AFTER the update, which is the only way a series timer's Priority
        # can move (create discards it): `sid` is now Priority 7 and `other` is still
        # the default 0. Upstream's ASCENDING Priority arm is OrderByDescending(Priority)
        # (LiveTvManager.cs:925-926) — the inversion is upstream's, ported verbatim — so
        # the two orders are each other's reverse and both differ from the default
        # name order. Names are compared, not ids, so this is cross-server evidence.
        # Without distinct priorities the three lists would be identical and the leg
        # would pass on a server that drops sortBy on the floor, which is the bug.
        def order_of(query, ids):
            items = (get_json(base, f"/LiveTv/SeriesTimers{query}", token) or {}).get("Items") or []
            return [t.get("Name") for t in items if t.get("Id") in ids]
        pair = (sid, other)
        r["POST /LiveTv/SeriesTimers"] = Same(create_ok, dict(
            evidence,
            order_default=order_of("", pair),
            order_priority_asc=order_of("?sortBy=Priority", pair),
            order_priority_desc=order_of("?sortBy=Priority&sortOrder=Descending", pair),
            # The collation leg, and it is a DIFFERENT pair: `sid`/`other` both carry
            # the fixture's capitalised programme titles, which sort the same way under
            # either rule and so prove nothing. `renamed` carries the lower-case
            # `RENAMED_SERIES_TIMER_NAME`, so a server comparing by Unicode scalar
            # instead of CLDR root collation answers ["Parity Show …", "apple parity g5"]
            # ascending and ["apple parity g5", "Parity Show …"] descending — the exact
            # reverse of upstream on both.
            order_name_asc=order_of("", (sid, renamed)),
            order_name_desc=order_of("?sortOrder=Descending", (sid, renamed))))

        # --- delete: gone, its timers gone, and a second delete is not a silent 204 ----
        st, _ = http("DELETE", f"{base}/LiveTv/SeriesTimers/{sid}", token)
        again, _ = http("DELETE", f"{base}/LiveTv/SeriesTimers/{sid}", token)
        left = children_of(base, token, sid)
        r["DELETE /LiveTv/SeriesTimers/{timerId}"] = Same(st < 300, {
            "gone_from_the_list": sid not in series_timer_ids(base, token),
            # The literal status, not "get_json came back empty": `get_json`
            # answers None for ANY non-200, so the old form let a 500 pass as a
            # 404 and a regression from one to the other stayed green. Both
            # servers really do answer 404 here (`LiveTvController.GetSeriesTimer`
            # returns `NotFound()` on a null timer, v10.11.8 LiveTvController.cs:875-884),
            # so the code is comparable and is compared.
            "single_get_status": http("GET", f"{base}/LiveTv/SeriesTimers/{sid}", token)[0],
            "its_timers_are_gone": len(left) == 0,
            "second_delete_is_not_2xx": again >= 400,
        })
        created.remove(sid)
    finally:
        for leftover in created:
            http("DELETE", f"{base}/LiveTv/SeriesTimers/{leftover}", token)
    return r


def j_remote_subtitles(base, token, user, mid, _m2):
    """Remote subtitles through OpenSubtitles — only when credentials are configured (see
    sweep.opensubtitles_credentials). The fixture's first movie carries a real IMDb id in
    its NFO, so both servers search the same title: search → download the first hit → the
    item gains an external subtitle stream → the provider's own subtitle file is fetched.
    Cost: two provider downloads per server (the POST and the Providers GET both download),
    so four quota units per two-leg run. Jellyfin's plugin uses its own bundled API key,
    Ferrofin uses OPENSUBTITLES_API_KEY — an exhausted quota can therefore flag one side only."""
    if not opensubtitles_credentials():
        return {}
    r = {}
    hits = get_json(base, f"/Items/{mid}/RemoteSearch/Subtitles/eng", token) or []
    r["GET /Items/{itemId}/RemoteSearch/Subtitles/{language}"] = bool(hits)
    sub_id = hits[0].get("Id") if hits else None
    if not sub_id:
        return r
    before = set(external_subtitle_indexes(base, token, user, mid))
    st, _ = http("POST", f"{base}/Items/{mid}/RemoteSearch/Subtitles/{urllib.parse.quote(sub_id, safe='')}",
                 token, "")
    # Jellyfin answers 204 even when the download failed (it logs and swallows), and only
    # QUEUES the refresh that creates the stream row — the read-back below is the verdict.
    # The verdict wants the stream the search asked for: a new EXTERNAL stream tagged eng
    # (the provider id carries the language; a server that drops it lands an "und" stream).
    added = []
    for _ in range(SUBTITLE_REFRESH_WAIT_S):
        added = [i for i, lang in external_subtitles(base, token, user, mid)
                 if i not in before and lang == "eng"]
        if added:
            break
        time.sleep(1)
    r["POST /Items/{itemId}/RemoteSearch/Subtitles/{subtitleId}"] = st < 300 and bool(added)
    st, raw = http("GET", f"{base}/Providers/Subtitles/Subtitles/{urllib.parse.quote(sub_id, safe='')}", token)
    r["GET /Providers/Subtitles/Subtitles/{subtitleId}"] = st == 200 and bool(raw)
    for i, lang in external_subtitles(base, token, user, mid):   # reap, whichever path ran
        # Our own debris whatever language the server gave it, plus any external eng stream
        # (the fixture's own eng track is embedded, never external).
        if i not in before or lang == "eng":
            http("DELETE", f"{base}/Videos/{mid}/Subtitles/{i}", token)
    return r


# ---------------------------------------------------------------- Identify → Apply

def http_headers(method, url, token=None, body=None):
    """`sweep.http()` plus the response headers, keys lowercased.

    Only the artwork journey needs them: what a stored image is SERVED as is the
    observable half of "the download was typed from the response, not the URL",
    and header casing differs between the two stacks (axum lowercases, Kestrel
    does not), so the lookup must be case-insensitive."""
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["Authorization"] = f'MediaBrowser Token="{token}", {CLIENT}'
    req = urllib.request.Request(url, data=(body.encode() if isinstance(body, str) else body),
                                 method=method.upper(), headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, resp.read(), {k.lower(): v for k, v in resp.headers.items()}
    except urllib.error.HTTPError as e:
        return e.code, e.read(), {k.lower(): v for k, v in e.headers.items()}
    except (urllib.error.URLError, TimeoutError, ConnectionError) as e:
        return 0, str(e).encode(), {}


def movie_named(base, token, user, name):
    """The movie whose Name is exactly `name`, or None.

    Selection is BY NAME, never by sort position: the two servers do not hold the
    same movie list (Jellyfin's DVR journey leaves recordings behind that sort into
    the same range), so `limit=1&sortOrder=Descending` picks a DIFFERENT item on each
    side and the cross-server comparison silently stops comparing the same thing."""
    b = get_json(base, f"/Items?userId={user}&recursive=true&includeItemTypes=Movie"
                       f"&searchTerm={urllib.parse.quote(name)}&limit=10", token)
    return next((i["Id"] for i in (b or {}).get("Items", []) if i.get("Name") == name), None)


#: The read-back fields `POST /Items/RemoteSearch/Apply/{itemId}` can change,
#: derived from v10.11.8 `MetadataService.MergeBaseItemData` — every scalar it
#: assigns under `replaceData: true`, plus the four the endpoint's own contract
#: covers (`LockData`/`LockedFields`, and the ids the controller writes).
#:
#: This list is NOT the body. See `identified()` for what is left out and why the
#: row is recorded `property`.
IDENTIFY_READBACK = ("Name", "OriginalTitle", "ProductionYear", "PremiereDate", "ProviderIds",
                     "Overview", "Genres", "Taglines", "CommunityRating", "OfficialRating",
                     "RunTimeTicks", "LockData", "LockedFields",
                     # …the rest of MergeBaseItemData's replaceData assignments,
                     # which the first cut of this projection omitted — exactly
                     # the fields a port could silently skip and no probe see.
                     "EndDate", "IndexNumber", "ParentIndexNumber", "CustomRating",
                     "CriticRating", "Tags", "ProductionLocations", "ForcedSortName",
                     "PreferredMetadataLanguage", "PreferredMetadataCountryCode",
                     "AlbumArtist", "AlbumArtists")


def identified(dto):
    """The item's post-Apply state, projected into a CROSS-SERVER comparable shape.

    A PROJECTION, not a body — which is why the row is recorded `property` and not
    the `body-diff` headline. Being explicit about both halves, because the
    previous version of this docstring asserted "nothing here is dropped" and that
    was not true:

    COMPARED. Every field in `IDENTIFY_READBACK` (the whole of
    `MergeBaseItemData`'s `replaceData` assignment list, MetadataService.cs
    :1009-1176, plus `LockData`/`LockedFields`/`ProviderIds`), and four derived
    values whose raw form is per-instance and for no other reason:
      * `Studios` — the DTO entries carry each server's own GUID for the studio, so
        the NAMES are compared, which is the whole of what a provider supplies.
      * `People` — same GUID problem; the (Name, Type, Role) triples are compared,
        so a cleared or re-ordered cast still fails. Upstream deliberately KEEPS
        the cast here (`temp.People` is null, and `SaveItemAsync` skips a null
        people list), so this is a guard against Ferrofin clearing it, not a
        prediction that it will change.
      * `RemoteTrailers` — the URLs are compared; the C# replaces the whole list
        under `replaceData`.
      * `ImageTags` — the tag is an md5 of path+mtime, so it differs between two
        servers holding byte-identical artwork (measured). The KEY SET is
        compared, so an image that appeared, vanished, or landed under the wrong
        `ImageType` still fails. `BackdropImageTags` has the same tag problem, so
        the COUNT is compared.

    NOT COMPARED, and therefore not claimed: everything else the DTO carries —
    `MediaSources`/`MediaStreams`/`Chapters`/`Trickplay`/`UserData`/`ExternalUrls`
    and the rest. Those are the file's facts, not the merge's, they are owned by
    the `GET /Items/{itemId}` row, and two of them (`MediaStreams[].BitRate`,
    `MediaSources[].Bitrate`) carry a known open divergence that would pin this
    row red for someone else's bug. A whole-body diff belongs in reads.py, which
    has one.
    """
    out = {k: dto.get(k) for k in IDENTIFY_READBACK}
    out["Studios"] = sorted(s.get("Name") for s in (dto.get("Studios") or []))
    out["People"] = sorted((p.get("Name"), p.get("Type"), p.get("Role"))
                           for p in (dto.get("People") or []))
    out["RemoteTrailers"] = sorted(t.get("Url") for t in (dto.get("RemoteTrailers") or []))
    out["ImageTags"] = sorted((dto.get("ImageTags") or {}).keys())
    out["BackdropImageTags"] = len(dto.get("BackdropImageTags") or [])
    return out


#: How long to wait for a forced library scan to report itself finished. The
#: fixture's 552 items take ~3 s on both stacks; the ceiling is generous so a
#: loaded host produces a `scan_completed: False` row rather than a timeout.
SCAN_WAIT_S = 300


def scan_library_and_wait(base, token):
    """Run the library scan to completion on `base`; True only if it really ran.

    Driven through the `RefreshLibrary` SCHEDULED TASK, not `POST
    /Library/Refresh`, because the task is the only library-scan trigger that
    reports a completion signal on both stacks: `State` returns to `Idle` and
    `LastExecutionResult.EndTimeUtc` advances past the value captured before the
    start. `POST /Library/Refresh` is fire-and-forget on both, which is exactly
    how a read-back taken "after" a scan can be taken before it.

    Returns False on a timeout, on a missing task, or on a refused start — the
    caller puts that in its evidence, so a scan that did not happen can never be
    mistaken for a scan that changed nothing.
    """
    task = next((t for t in (get_json(base, "/ScheduledTasks", token) or [])
                 if t.get("Key") == "RefreshLibrary"), None)
    if not task:
        return False
    task_id, before = task["Id"], (task.get("LastExecutionResult") or {}).get("EndTimeUtc")
    if http("POST", f"{base}/ScheduledTasks/Running/{task_id}", token, "")[0] >= 300:
        return False
    deadline = time.time() + SCAN_WAIT_S
    while time.time() < deadline:
        time.sleep(2)
        now = get_json(base, f"/ScheduledTasks/{task_id}", token) or {}
        end = (now.get("LastExecutionResult") or {}).get("EndTimeUtc")
        if now.get("State") == "Idle" and end != before:
            return True
    return False


def j_remote_search_apply(base, token, user, _m, _m2):
    """`POST /Items/RemoteSearch/Apply/{itemId}` — the Identify dialog's "Apply".

    A real mutation, so the row is earned on the READ-BACK — and the read-back is
    compared against the OTHER server's, field for field, through `identified()`.
    That is a NAMED PROJECTION of the DTO and not the DTO, so the row is recorded
    `property`; `identified()`'s docstring lists what is in it and what is not.

    Two legs, on movies no other journey touches (`Movie 0497`/`Movie 0496`, chosen by
    name — see `movie_named`):

      1. UNLOCKED. The fixture's Movies library has every "Metadata downloaders" and
         "Image fetchers" box cleared (`LibraryOptions.TypeOptions[Movie]`), so no
         remote fetcher may run: v10.11.8 `ProviderManager.CanRefreshMetadata` →
         `BaseItemManager.IsMetadataFetcherEnabled` is an ALLOW-list, and an empty
         `MetadataFetchers` disables everything. Two things must happen anyway. The
         controller's own assignment — `item.ProviderIds = searchResult.ProviderIds`,
         commented "Since the refresh process won't erase provider Ids, we need to set
         this explicitly now" (`ItemLookupController.ApplySearchCriteria`) — and the
         `RemoveOldMetadata` wipe: `MetadataService.RefreshWithProviders` skips
         re-adding the item's own values, so merging the empty provider result under
         `ReplaceAllMetadata` CLEARS every provider-supplied field. Ferrofin failed
         this leg three ways in turn: it ignored the checkboxes and fetched TMDB; then,
         once gated, it dropped the chosen ids too; and it never cleared the old
         record's genres/studios/year.

      2. LOCKED. `item.IsLocked` makes `RefreshWithProviders` return before the merge,
         so the ids land and nothing else moves. Refusing the FETCH (right) and
         refusing the ID ASSIGNMENT (wrong) are separable only on this leg.

    …and then, for BOTH legs, the same read-back again after a library scan that
    this journey forces and WAITS FOR (`scan_library_and_wait`). That third
    observation is not decoration; it is the difference between a fact and a
    coincidence. An earlier version of this row took its read-back immediately and
    recorded it green, while the journeys suite's own `POST /Library/Refresh` was
    mid-flight; seconds later Ferrofin's scan re-applied `movie.nfo` over the item
    Apply had just cleared, and the row had certified a state that no longer
    existed. Upstream cannot do that: `BaseNfoProvider.HasChanged` is
    `nfoWriteTime - item.DateLastSaved > 1 minute`, so a sidecar older than the last
    save reports no change, `MetadataService.GetProviders` returns an empty provider
    list for the item, and the scan leaves it alone. Ferrofin's scan re-reads the
    sidecar unconditionally, so `unlocked_after_scan` diverges — see the
    `open-work (NOT accepted)` entry in classifications.json. Forcing the scan makes
    that divergence a deterministic red instead of a race whose outcome depends on
    when the suite happened to run.

    Deliberately NOT probed: a leg with the downloaders TICKED. It would rewrite both
    items from live TMDB, whose answer is not pinned in time, so the row would go red
    on an upstream synopsis edit. The gate is what this row is for.
    """
    r = {}
    unlocked = movie_named(base, token, user, "Movie 0497")
    locked = movie_named(base, token, user, "Movie 0496")
    if not (unlocked and locked):
        return r

    st, _ = http("POST", f"{base}/Items/RemoteSearch/Apply/{unlocked}?replaceAllImages=true",
                 token, json.dumps({"Name": "The Matrix", "ProviderIds": {"Tmdb": "603"},
                                    "ProductionYear": 1999,
                                    "SearchProviderName": "TheMovieDb"}))
    after_open = identified(q(base, f"/Items/{unlocked}", token, user) or {})

    dto = q(base, f"/Items/{locked}", token, user) or {}
    dto["LockData"] = True
    lock_st, _ = http("POST", f"{base}/Items/{locked}", token, json.dumps(dto))
    st2, _ = http("POST", f"{base}/Items/RemoteSearch/Apply/{locked}?replaceAllImages=true",
                  token, json.dumps({"Name": "Inception", "ProviderIds": {"Tmdb": "27205"},
                                     "ProductionYear": 2010,
                                     "SearchProviderName": "TheMovieDb"}))
    after_locked = identified(q(base, f"/Items/{locked}", token, user) or {})

    # Durability. `scan_completed` is IN the evidence and in `ok`, so a scan that
    # timed out or never started cannot pass as "the scan changed nothing".
    scanned = scan_library_and_wait(base, token)
    durable_open = identified(q(base, f"/Items/{unlocked}", token, user) or {})
    durable_locked = identified(q(base, f"/Items/{locked}", token, user) or {})

    ev = {"status": st, "locked_status": st2, "lock_accepted": lock_st < 300,
          "unlocked": after_open, "locked": after_locked,
          "scan_completed": scanned,
          "unlocked_after_scan": durable_open, "locked_after_scan": durable_locked}
    r["POST /Items/RemoteSearch/Apply/{itemId}"] = Same(
        st == 204 and st2 == 204 and scanned
        and (after_open.get("ProviderIds") or {}).get("Tmdb") == "603"
        and (after_locked.get("ProviderIds") or {}).get("Tmdb") == "27205"
        and after_locked.get("LockData") is True, ev)
    return r


def j_remote_image_download(base, token, user, _m, _m2):
    """`POST /Items/{itemId}/RemoteImages/Download` — "Choose Image"'s download-by-URL.

    Gated by admin elevation ONLY: v10.11.8 `RemoteImageController.DownloadRemoteImage`
    carries `[Authorize(Policy = Policies.RequiresElevation)]` and consults no library
    option, because `ProviderManager.SaveImage` is a raw GET of a caller-supplied URL,
    not a provider call. Unlike Refresh and Identify, the absence of a "Metadata
    downloaders"/"Image fetchers" check here is CORRECT; adding one would be the
    divergence.

    Each server downloads from ITSELF (`http://127.0.0.1:8096/...`, the in-container
    listener) — the one source URL that is identical text on both sides and depends on
    no other container. That is also why the row is `property` and not `body-diff`: the
    two servers' stored bytes come from two different origins (each server's own
    re-encode of its own poster), so they cannot be diffed against each other. What IS
    compared across the two servers is the derived set below — statuses, which
    `ImageTags` keys appeared, and the media type each stored file is SERVED as, which
    is exactly what the two bugs this row exists for corrupted:

      * the stored file used to be typed from the URL's SUFFIX ("ends with .png ? png :
        jpeg"), so a PNG fetched from a URL carrying no `.png` was written as `.jpg`
        and served as `image/jpeg`. C# reads `response.Content.Headers.ContentType` and
        falls back to the URL PATH only when that is absent or
        `application/octet-stream` (`ProviderManager.SaveImage`);
      * and there was no `image/*` check at all, so a URL answering JSON was stored as
        the item's artwork and served back as an image. C# throws
        `Request returned '{contentType}' instead of an image type`.

    Reaped at the end so a re-run, and every layer after it, sees the item unchanged.
    """
    r = {}
    mid = movie_named(base, token, user, "Movie 0495")
    if not mid:
        return r
    # The server fetches this itself, from inside its own container.
    src = f"http://127.0.0.1:8096/Items/{mid}/Images/Primary"
    before = sorted(((q(base, f"/Items/{mid}", token, user) or {}).get("ImageTags") or {}).keys())

    def download(image_type, url):
        st, _ = http("POST", f"{base}/Items/{mid}/RemoteImages/Download?type={image_type}"
                             f"&imageUrl={urllib.parse.quote(url, safe='')}", token, "")
        return st

    # 1. happy path: a JPEG, from a URL with no extension at all.
    logo_st = download("Logo", src)
    # 2. the same picture re-encoded as PNG, again extensionless — the leg that used to
    #    land as `.jpg` and be served `image/jpeg`.
    thumb_st = download("Thumb", src + "?format=Png")
    # 3. a URL that answers JSON: must be refused, and must leave no artwork behind.
    art_st = download("Art", "http://127.0.0.1:8096/System/Info/Public")
    # 4. the argument checks (`type` and `imageUrl` are both required).
    no_type = http("POST", f"{base}/Items/{mid}/RemoteImages/Download"
                           f"?imageUrl={urllib.parse.quote(src, safe='')}", token, "")[0]
    no_url = http("POST", f"{base}/Items/{mid}/RemoteImages/Download?type=Logo", token, "")[0]
    # 5. an unknown item is 404 — a RANDOM guid, never the degenerate
    #    00000000-0000-0000-0000-000000000001, which makes Jellyfin 500 out of its own
    #    repository (an artefact of that id, not of this endpoint).
    unknown = http("POST", f"{base}/Items/{uuid.uuid4().hex}/RemoteImages/Download?type=Logo"
                           f"&imageUrl={urllib.parse.quote(src, safe='')}", token, "")[0]

    keys = sorted(((q(base, f"/Items/{mid}", token, user) or {}).get("ImageTags") or {}).keys())

    def served(image_type):
        st, _, headers = http_headers("GET", f"{base}/Items/{mid}/Images/{image_type}", token)
        return st, (headers.get("content-type") or "").split(";")[0].strip().lower()

    logo_served, logo_ct = served("Logo")
    thumb_served, thumb_ct = served("Thumb")
    art_served, _ = served("Art")

    ev = {"logo_status": logo_st, "thumb_status": thumb_st, "art_status_class": art_st // 100,
          "no_type_status": no_type, "no_url_status": no_url, "unknown_item_status": unknown,
          "added_keys": sorted(set(keys) - set(before)),
          "logo_served": logo_served, "logo_content_type": logo_ct,
          "thumb_served": thumb_served, "thumb_content_type": thumb_ct,
          "art_served": art_served}
    r["POST /Items/{itemId}/RemoteImages/Download"] = Same(
        logo_st == 204 and thumb_st == 204
        and "Logo" in keys and "Thumb" in keys
        # the refusal, and its consequence: no Art image exists afterwards
        and art_st >= 400 and "Art" not in keys and art_served == 404
        # …and the PNG kept its own type all the way through the store
        and logo_ct == "image/jpeg" and thumb_ct == "image/png"
        and no_type == 400 and no_url == 400 and unknown == 404, ev)

    for image_type in ("Logo", "Thumb", "Art"):
        if image_type in keys:
            http("DELETE", f"{base}/Items/{mid}/Images/{image_type}", token)
    return r


# ---------------------------------------------------------------- remote search ("Identify")

# The two servers must be asked the SAME question at the SAME moment, because the answers
# come from live TMDB. The runner cannot give us that: journeys() runs Ferrofin's ENTIRE
# suite before Jellyfin's, so a naive per-leg probe would separate the two legs by every
# other journey — minutes, not seconds, of TMDB popularity drift. So this journey queries
# BOTH servers itself, case by case, back-to-back, on whichever leg runs first, and the
# second leg reads its own server's answers out of the cache.
PAIR = {}              # base -> (peer_base, peer_token); registered by journeys()
_IDENTIFY_CACHE = {}   # base -> {case_key: (status, [rows])}

TMDB = "TheMovieDb"
OMDB = "The Open Movie Database"
MUSICBRAINZ = "MusicBrainz"

# MusicBrainz enforces a hard per-client rate limit and answers an over-rate query with an
# EMPTY list rather than an error, so two servers asked back-to-back can disagree for a
# reason that is not a parity fact. Cases named here are paced apart and retried while the
# answer is empty. An empty answer that SURVIVES the retries is reported and diffed as-is
# — never skipped, and never "compared only when both are non-empty".
# Measured on the lab pair: with both containers behind ONE egress IP, the limiter needs
# roughly 40 s of cumulative quiet before it answers the second server. 6 s of spacing with
# a linear backoff over 5 attempts clears that with margin. If both servers end up empty
# anyway, the row goes RED with `named_count: 0` on both sides — the honest outcome, since
# a search neither server answered is a search this row did not verify.
MB_PACED = {"ma_named"}
MB_SPACING_S = 6.0
MB_RETRIES = 5

# Every RemoteSearch this journey asserts: case -> (kind, SearchInfo, SearchProviderName).
IDENTIFY_CASES = {
    # -- Movie. Name+Year: TmdbMovieProvider forwards Year to /search/movie and every row
    #    carries PremiereDate + ProductionYear from the release date.
    "m_named":    ("Movie",  {"Name": "The Matrix", "Year": 1999}, TMDB),
    #    A `Tmdb` id short-circuits the name search: one row, with the IMDb id merged.
    "m_byid":     ("Movie",  {"ProviderIds": {"Tmdb": "603"}}, TMDB),
    #    The SAME id, whitespace-padded. `int.Parse(id, CultureInfo.InvariantCulture)`
    #    defaults to NumberStyles.Integer (AllowLeadingWhite|AllowTrailingWhite), so
    #    upstream still pins the title; a server whose parse rejects the padding falls
    #    through to the name search — and the deliberately-wrong Name here makes that
    #    visible as a WRONG title rather than as an empty list.
    "m_padded":   ("Movie",  {"ProviderIds": {"Tmdb": " 603 "}, "Name": "Zzz Not A Movie"}, TMDB),
    #    An `Imdb` id resolves through TMDB's /find.
    "m_byimdb":   ("Movie",  {"ProviderIds": {"Imdb": "tt0133093"}}, TMDB),
    "m_bogus":    ("Movie",  {"Name": "zzqqxxnotamovie12345"}, TMDB),
    "m_unfilt":   ("Movie",  {"Name": "The Matrix", "Year": 1999}, None),
    # -- Series. TmdbSeriesProvider leaves SearchSeriesAsync's year at 0, so a WRONG Year
    #    must not narrow the search; its mappers set PremiereDate and never ProductionYear;
    #    and the by-id branch carries Tmdb + Imdb + Tvdb.
    "s_named":    ("Series", {"Name": "Breaking Bad"}, TMDB),
    "s_year":     ("Series", {"Name": "Breaking Bad", "Year": 2019}, TMDB),
    "s_byid":     ("Series", {"ProviderIds": {"Tmdb": "1396"}}, TMDB),
    "s_padded":   ("Series", {"ProviderIds": {"Tmdb": " 1396 "}, "Name": "Zzz Not A Show"}, TMDB),
    "s_bogus":    ("Series", {"Name": "zzzqqxnotarealshowxyz123"}, TMDB),
    "s_unfilt":   ("Series", {"Name": "Breaking Bad"}, None),
    # -- BoxSet. TmdbBoxSetProvider short-circuits on tmdbId > 0 (one row, and no Overview
    #    on any search DTO) and otherwise searches /search/collection by name.
    "b_named":    ("BoxSet", {"Name": "The Lord of the Rings Collection"}, TMDB),
    "b_byid":     ("BoxSet", {"ProviderIds": {"Tmdb": "119"},
                              "Name": "The Matrix Collection"}, TMDB),
    "b_padded":   ("BoxSet", {"ProviderIds": {"Tmdb": " 119 "},
                              "Name": "The Matrix Collection"}, TMDB),
    "b_bogus":    ("BoxSet", {"Name": "zzqxwv nonexistent collection 99812"}, TMDB),
    # -- Person. Only the by-id branch takes a language upstream (`GetPersonAsync(id,
    #    language, countryCode, …)`); `SearchPersonAsync(name, ct)` takes none.
    "p_named":    ("Person", {"Name": "Tom Hanks"}, TMDB),
    "p_fr":       ("Person", {"ProviderIds": {"Tmdb": "31"}, "MetadataLanguage": "fr",
                              "MetadataCountryCode": "FR"}, TMDB),
    "p_en":       ("Person", {"ProviderIds": {"Tmdb": "31"}, "MetadataLanguage": "en",
                              "MetadataCountryCode": "US"}, TMDB),
    "p_padded":   ("Person", {"ProviderIds": {"Tmdb": " 31 "}, "Name": "Zzz Nobody"}, TMDB),
    "p_bogus":    ("Person", {"Name": "zzqqxxnotarealpersonzzz"}, TMDB),
    "p_noprov":   ("Person", {"Name": "Tom Hanks"}, "NoSuchProvider"),
    # -- Trailer. OMDb is the only Trailer fetcher on either side.
    "t_named":    ("Trailer", {"Name": "Inception", "Year": 2010}, OMDB),
    "t_bogus":    ("Trailer", {"Name": "zzqxwvyunlikelytitle12345"}, OMDB),
    # -- MusicArtist. MusicBrainz is the only ArtistInfo fetcher on either side. UNSCOPED
    #    (no ItemId), so upstream builds its dummy reference item with default
    #    LibraryOptions and every fetcher is enabled — the leg that proves the two servers
    #    agree on the search itself, before the library gate is brought into it below.
    #    NO bogus-term case here, deliberately: MusicBrainz answers an over-rate query with
    #    an empty list, so `[] == []` on a nonsense name would be satisfied by the rate
    #    limiter as readily as by the search, and an assertion that cannot fail is worse
    #    than no assertion. It would also burn the quota the leg below actually needs.
    "ma_named":   ("MusicArtist", {"Name": "Radiohead"}, MUSICBRAINZ),
}


def remote_search(base, token, kind, search_info, provider=None,
                  item_id=None, include_disabled=None):
    """POST /Items/RemoteSearch/{kind}; returns (status, results-list).

    `item_id` scopes the search to an existing row, which is what makes upstream read that
    item's library options and drop the fetchers its "Metadata downloaders" list leaves
    unticked (ProviderManager.cs:787 -> CanRefreshMetadata:462). `include_disabled` is the
    short-circuit that puts them back."""
    body = {"SearchInfo": search_info}
    if provider:
        body["SearchProviderName"] = provider
    if item_id:
        body["ItemId"] = item_id
    if include_disabled is not None:
        body["IncludeDisabledProviders"] = include_disabled
    st, raw = http("POST", f"{base}/Items/RemoteSearch/{kind}", token, json.dumps(body))
    try:
        out = json.loads(raw) if raw else []
    except ValueError:
        out = []
    return st, (out if isinstance(out, list) else [])


def identify_responses(base, token):
    """Every IDENTIFY_CASES answer for `base`, with both servers queried case-adjacently.

    The first leg to ask drives the whole probe: for each case it hits its own server and
    then, within milliseconds, the peer registered in PAIR, and caches both. The second leg
    then reads its OWN server's recorded answers. Each leg's evidence is still its own
    server's response — the runner's cross-server equality does exactly as much work as
    before — but the two responses being compared were produced seconds apart instead of
    being separated by an entire journey suite."""
    if base in _IDENTIFY_CACHE:
        return _IDENTIFY_CACHE[base]
    legs = [(base, token)]
    peer = PAIR.get(base)
    if peer and peer[0]:
        legs.append(peer)
    per_leg = {b: {} for b, _ in legs}
    for case, (kind, info, provider) in IDENTIFY_CASES.items():
        paced = case in MB_PACED
        for attempt in range(MB_RETRIES if paced else 1):
            for b, t in legs:
                if paced:
                    time.sleep(MB_SPACING_S)
                per_leg[b][case] = remote_search(b, t, kind, info, provider)
            if not paced or all(per_leg[b][case][1] for b, _ in legs):
                break
            # Back off before re-asking: an empty list here is far more often the rate
            # limiter than a real answer, and the retries are what let a genuinely empty
            # answer be reported honestly instead of being written off as throttling.
            time.sleep(MB_SPACING_S * (attempt + 1))
    _IDENTIFY_CACHE.update(per_leg)
    return _IDENTIFY_CACHE[base]


def identify(results):
    """Every candidate, WHOLE — the raw JSON object, every key, order preserved.

    Nothing is projected away. `RemoteSearchResult` has exactly 12 properties in the
    vendored contract (Name, SearchProviderName, ProviderIds, ImageUrl, Overview,
    PremiereDate, ProductionYear, IndexNumber, IndexNumberEnd, ParentIndexNumber,
    AlbumArtist, Artists) and not one of them is per-instance — no ids, no timestamps, no
    paths — so a whole-object comparison is both safe across two independent servers and
    the strictest thing available. It deliberately includes key PRESENCE: a server that
    emits `"Overview": null` where the other omits the key fails the row, which is what
    this batch's BoxSet fix (stop emitting Overview at all) actually changed, and what a
    `dict.get()` projection would have missed. It also catches a field neither side is
    supposed to send, which an explicit field list cannot."""
    return results


def j_remote_search_identify(base, token, user, _m, _m2):
    """The Identify flow: POST /Items/RemoteSearch/{Movie,Series,BoxSet,Person,Trailer}.

    These are POSTs by contract but SEARCHES by behaviour — v10.11.8
    `ItemLookupController` only calls `IProviderManager.GetRemoteSearchResults` and returns
    Ok, so nothing is mutated and there is no read-back. They live here because sweep.py is
    GET/HEAD-only and reads.py correlates by Path.

    They hit live remote providers, so the assertions are built to survive drift: the two
    servers are asked each question back-to-back (see `identify_responses` — the runner's
    own per-leg ordering is NOT relied on, and must not be), and every claim is either
    pinned by ProviderId or is a structural invariant.

    DELIBERATELY NOT COMPARED, and why:
      * Rows from any provider other than the one pinned by `SearchProviderName`. The
        provider SETS differ by owner-accepted design (CLAUDE.md "Current scope"):
        Ferrofin's OMDb is inert without FERROFIN_OMDB_KEY while Jellyfin 10.11.8 ships a
        hardcoded key (MediaBrowser.Providers/Plugins/Omdb/OmdbProvider.cs:257), and
        Ferrofin compiles TheTVDB in where the oracle has no TVDB plugin installed.
        Comparing totals would encode those as a permanent red. Provider identity is still
        asserted: `tmdb_answers` goes false if the TMDB provider stops answering an
        unfiltered search on either side, and every pinned assertion below is scoped to
        TheMovieDb, so answering from the WRONG provider fails the row.
      * A NON-NUMERIC provider id. Upstream's `int.Parse`/`Convert.ToInt32` throws and
        ProviderManager's catch-all swallows the whole provider, so Jellyfin answers [];
        Ferrofin declines to port the crash and falls through to the remaining branches.
        Measured and recorded as an accepted divergence in classifications.json rather
        than probed here, because asserting it would freeze a Jellyfin bug into the gate.
    Everything else IS compared: for every case below the FULL candidate list is compared
    as raw JSON, element-for-element, key-for-key, order and count included (see
    `identify`). If this row ever flakes on TMDB ranking drift, the fix is to pin the
    search harder (by ProviderId), never to widen the comparison.
      * `POST /Items/RemoteSearch/Trailer` cannot be verified without an OMDb key: OMDb is
        the ONLY provider implementing `IRemoteMetadataProvider<Trailer, TrailerInfo>` in
        v10.11.8, so a keyless Ferrofin can only ever answer []. The row is still probed
        (both statuses, both bogus-term answers, both whole-body projections) so the
        divergence is measured rather than assumed; see classifications.json. NOTE for
        whoever sets a key: the Trailer row is the one that exercises OMDb's IndexNumber /
        ParentIndexNumber echo and its `Artists` list, and `identify` compares those,
        because it compares the whole object.

    Needs outbound TMDB reachability from both containers; with none, both sides return []
    and the rows fail rather than passing vacuously.
    """
    r = {}
    a = identify_responses(base, token)
    rows = {case: a[case][1] for case in IDENTIFY_CASES}
    status = {case: a[case][0] for case in IDENTIFY_CASES}

    def pinned(case, tmdb_id):
        """The one candidate an id-pinned search must return, named by its Tmdb id."""
        got = rows[case]
        return len(got) == 1 and (got[0].get("ProviderIds") or {}).get("Tmdb") == tmdb_id

    # ---- Movie ---------------------------------------------------------------
    ev = {"named": identify(rows["m_named"]),
          "byid": identify(rows["m_byid"]),
          "padded": identify(rows["m_padded"]),
          "byimdb": identify(rows["m_byimdb"]),
          "bogus_empty": rows["m_bogus"] == [],
          "tmdb_answers": TMDB in {x.get("SearchProviderName") for x in rows["m_unfilt"]}}
    r["POST /Items/RemoteSearch/Movie"] = Same(
        bool(rows["m_named"]) and pinned("m_byid", "603") and pinned("m_padded", "603")
        and bool(rows["m_byimdb"])
        and all(x.get("PremiereDate") for x in rows["m_named"])
        and ev["bogus_empty"] and ev["tmdb_answers"], ev)

    # ---- Series --------------------------------------------------------------
    ev = {"named": identify(rows["s_named"]),
          "year_ignored": "1396" in {(x.get("ProviderIds") or {}).get("Tmdb")
                                     for x in rows["s_year"]},
          "byid": identify(rows["s_byid"]),
          "padded": identify(rows["s_padded"]),
          "bogus_empty": rows["s_bogus"] == [],
          "tmdb_answers": TMDB in {x.get("SearchProviderName") for x in rows["s_unfilt"]}}
    r["POST /Items/RemoteSearch/Series"] = Same(
        bool(rows["s_named"]) and pinned("s_byid", "1396") and pinned("s_padded", "1396")
        and ev["year_ignored"]
        and all(x.get("PremiereDate") for x in rows["s_named"])
        and not any(x.get("ProductionYear") for x in rows["s_named"])
        and ev["bogus_empty"] and ev["tmdb_answers"], ev)

    # ---- BoxSet --------------------------------------------------------------
    ev = {"named": identify(rows["b_named"]),
          "byid": identify(rows["b_byid"]),
          "padded": identify(rows["b_padded"]),
          "bogus_empty": rows["b_bogus"] == []}
    r["POST /Items/RemoteSearch/BoxSet"] = Same(
        bool(rows["b_named"]) and pinned("b_byid", "119") and pinned("b_padded", "119")
        and ev["bogus_empty"], ev)

    # ---- Person --------------------------------------------------------------
    # `bio_localized` compares each server against ITSELF, so it is drift-proof: it is true
    # only when the fr and en biographies of the SAME person differ, which is the effect of
    # the language reaching /person/{id}. The fr and en rows are ALSO cross-compared whole
    # (`byid_fr`/`byid_en`), so the biography TEXT the language fix changes is diffed, not
    # merely asserted to be two different strings.
    fr_bio = rows["p_fr"][0].get("Overview") if rows["p_fr"] else None
    en_bio = rows["p_en"][0].get("Overview") if rows["p_en"] else None
    ev = {"named": identify(rows["p_named"]),
          "byid_en": identify(rows["p_en"]),
          "byid_fr": identify(rows["p_fr"]),
          "padded": identify(rows["p_padded"]),
          "bio_localized": bool(fr_bio) and bool(en_bio) and fr_bio != en_bio,
          "bogus_empty": rows["p_bogus"] == [],
          "provider_filter_empty": rows["p_noprov"] == []}
    r["POST /Items/RemoteSearch/Person"] = Same(
        bool(rows["p_named"]) and pinned("p_en", "31") and pinned("p_fr", "31")
        and pinned("p_padded", "31") and ev["bio_localized"]
        and ev["bogus_empty"] and ev["provider_filter_empty"], ev)

    # ---- Trailer -------------------------------------------------------------
    ev = {"status": status["t_named"],
          "named": identify(rows["t_named"]),
          "bogus_empty": rows["t_bogus"] == []}
    r["POST /Items/RemoteSearch/Trailer"] = Same(
        status["t_named"] == 200 and bool(rows["t_named"]) and ev["bogus_empty"], ev)

    # ---- MusicArtist ---------------------------------------------------------
    # Two halves. (a) the UNSCOPED search, whole-object compared like every case above.
    #     ONE deliberate relaxation, and it is a relaxation of ORDER only: the candidate
    #     lists are sorted by (Name, MusicBrainzArtist id) before comparison, because
    #     MusicBrainz returns tie-SCORED artists in an order that two independent live
    #     queries genuinely do not agree on (measured: 6 rows of the 25 swap between
    #     back-to-back queries against the SAME server). Every field of every candidate is
    #     still compared, and the candidate SET is still compared exactly — a row present
    #     on one side and not the other still fails.
    # (b) the SCOPED search, which is the deterministic half and the one this row exists
    #     for: with `ItemId` set, upstream resolves the item's library and drops every
    #     fetcher its "Metadata downloaders" list leaves unticked (ProviderManager.cs:787
    #     -> GetMetadataProvidersInternal:440 -> CanRefreshMetadata:462 ->
    #     BaseItemManager.IsMetadataFetcherEnabled). On the synthetic fixture that list is
    #     empty for MusicArtist, so the correct answer is [] with NO outbound request at
    #     all — no rate limiter can reach this assertion. `IncludeDisabledProviders` is
    #     the short-circuit (:474) that puts the provider back.
    #
    # Resolving the id is itself the detector for the MusicArtistResolver port: a server
    # whose artists are accessed-by-name rows carry no TopParentId, are invisible to this
    # user-scoped recursive query, and there is no id to scope with. That MUST fail the
    # row — never skip it — which is what `artist_resolved` records.
    def artist_by_path(b, t):
        q = ("/Items?userId=%s&recursive=true&includeItemTypes=MusicArtist&fields=Path"
             "&sortBy=Path" % user)
        found = (get_json(b, q, t) or {}).get("Items") or []
        return (found[0].get("Id"), found[0].get("Path")) if found else (None, None)

    def sorted_candidates(rowset):
        return sorted(rowset, key=lambda x: (x.get("Name") or "",
                                             (x.get("ProviderIds") or {}).get("MusicBrainzArtist")
                                             or ""))

    artist_id, artist_path = artist_by_path(base, token)
    scoped = forced = None
    scoped_status = forced_status = None
    if artist_id:
        scoped_status, scoped = remote_search(base, token, "MusicArtist", {"Name": "Radiohead"},
                                              MUSICBRAINZ, item_id=artist_id)
        for attempt in range(MB_RETRIES):
            time.sleep(MB_SPACING_S)
            forced_status, forced = remote_search(base, token, "MusicArtist",
                                                  {"Name": "Radiohead"}, MUSICBRAINZ,
                                                  item_id=artist_id, include_disabled=True)
            if forced:
                break
            time.sleep(MB_SPACING_S * (attempt + 1))
    ev = {"named": sorted_candidates(rows["ma_named"]),
          # Surfaced so a reader can tell a real disagreement from the rate limiter at a
          # glance: BOTH counts zero means neither server got an answer.
          "named_count": len(rows["ma_named"]),
          "forced_count": len(forced or []),
          "artist_resolved": bool(artist_id),
          "artist_path": artist_path,
          # Both statuses are evidence, not just gate inputs: the runner compares the whole
          # dict across servers, so a status that diverges between the two fails the row
          # even when both bodies happen to be empty.
          "scoped_status": scoped_status,
          "forced_status": forced_status,
          # The gate: the library ticks no MusicArtist metadata fetcher, so a scoped
          # search must answer [] even though the unscoped one answers. `[]` alone is NOT
          # enough — `remote_search` yields [] for an unparseable body too, so a 401/404/
          # 500 would satisfy a bare `scoped == []`. Pinning 200 is what makes this a
          # statement about the fetcher gate rather than about the server being reachable.
          "gate_drops_the_fetcher": scoped_status == 200 and scoped == [],
          # …and lifting the gate must put the SAME provider back.
          "include_disabled_restores": forced_status == 200 and bool(forced)
          and {x.get("SearchProviderName") for x in forced} == {MUSICBRAINZ}}
    r["POST /Items/RemoteSearch/MusicArtist"] = Same(
        bool(artist_id) and bool(rows["ma_named"])
        and ev["gate_drops_the_fetcher"] and ev["include_disabled_restores"], ev)
    return r


def j_backup(base, token, user, _m, _m2):
    """Manifest + list against the PINNED backup — the Jellyfin-authored zip
    seeded into each server's config volume with the snapshot (data/backups/).
    Backup/Create is NOT exercised here: on a real-size library Jellyfin's
    Database backup serializes every entity row-by-row while holding the
    pessimistic exclusive DB lock — hours, with every concurrent request
    500ing "database is locked" (measured 2026-09-01, plan §D0) — which
    wedged every layer after journeys. The create op runs LAST of everything,
    bounded, in terminal.py. The listing must contain the pinned artifact and
    the Manifest route must read the same manifest back by its path — on BOTH
    servers, which is the drop-in story itself: Ferrofin serving a backup
    Jellyfin wrote."""
    r = {}
    listed = get_json(base, "/Backup", token) or []
    pinned = next((m for m in listed if m.get("Path")), None)
    r["GET /Backup"] = pinned is not None
    if pinned:
        manifest = get_json(
            base, "/Backup/Manifest?path=" + urllib.parse.quote(pinned["Path"]), token) or {}
        r["GET /Backup/Manifest"] = (manifest == pinned
                                     and bool(manifest.get("BackupEngineVersion"))
                                     and bool(manifest.get("DateCreated")))
    return r


def j_activity_log(base, token, user, _m, _m2):
    """Log a throwaway user in and check the pair of activity entries the login writes.

    `GET /System/ActivityLog/Entries` cannot be body-diffed — `Date` and `Id` are
    per-run — but for a SCRIPTED action the rest is fully determined, and this is
    what the C# consumers specify:

      * `SessionManager.AuthenticateNewSessionInternal` awaits `LogSessionActivity`
        (which raises `SessionStarted`) BEFORE publishing the authentication
        result, so the feed reads SessionStarted then AuthenticationSucceeded.
        Ferrofin wrote AuthenticationSucceeded from the HTTP handler and spawned
        the SessionStarted write, so the pair came out backwards.
      * `AuthenticationSucceededLogger` / `SessionStartedLogger` both set
        `ShortOverview = string.Format(LabelIpAddressValue, RemoteEndPoint)`,
        i.e. "IP address: <ip>". Ferrofin passed `RemoteEndPoint = null` on the
        authenticate request, so every entry's ShortOverview was null.
      * Both are `LogLevel.Information` (only `AuthenticationFailed` is Error).

    Run identically on both servers, so it is a property assertion, not a body
    diff — the row is stamped `effect`."""
    r = {}
    name = "actlogprobe"
    _, uraw = http("POST", f"{base}/Users/New", token,
                   json.dumps({"Name": name, "Password": "Parity!123"}))
    uid = json.loads(uraw).get("Id") if uraw else None
    if not uid:
        return r
    try:
        auth = auth_device(base, name, "Parity!123", "parity-actlog")
        if not auth.get("AccessToken"):
            return r
        entries = (get_json(base, "/System/ActivityLog/Entries?limit=10", token)
                   or {}).get("Items") or []
        # The newest AuthenticationSucceeded for this probe user, and whatever
        # the feed put immediately after it (the feed is DateCreated DESC, so
        # "after" is "written before").
        idx = next((i for i, e in enumerate(entries)
                    if e.get("Type") == "AuthenticationSucceeded"
                    and name in (e.get("Name") or "")), None)
        ok = idx is not None and idx + 1 < len(entries)
        if ok:
            succeeded, started = entries[idx], entries[idx + 1]
            ip_re = re.compile(r"^IP address: \S+$")
            ok = (started.get("Type") == "SessionStarted"
                  and name in (started.get("Name") or "")
                  and succeeded.get("Severity") == "Information"
                  and started.get("Severity") == "Information"
                  and bool(ip_re.match(succeeded.get("ShortOverview") or ""))
                  and bool(ip_re.match(started.get("ShortOverview") or "")))
        r["GET /System/ActivityLog/Entries"] = bool(ok)
    finally:
        http("DELETE", f"{base}/Users/{uid}", token)
    return r


JOURNEYS = [j_startup,   # first: see its docstring
            j_favorites, j_played, j_rating, j_playlist, j_playlist_instant_mix,
            j_plugin_uninstall, j_collection, j_users, j_item_edit,
            j_api_keys, j_user_item_data, j_display_prefs, j_scheduled_task_triggers,
            j_device_options, j_playstate, j_capabilities, j_user_config, j_system_config,
            j_playlist_share, j_item_delete, j_capabilities_query, j_environment_validate,
            j_merge_versions, j_playing_items, j_virtualfolder_rename,
            j_users_password, j_virtualfolder_crud, j_sessions, j_config_writes,
            j_repositories,
            j_scheduled_run, j_playbackinfo_post, j_active_encodings, j_clientlog,
            j_authenticate, j_user_update, j_devices_delete, j_bulk_item_delete,
            j_subtitles_upload, j_lyrics, j_quickconnect,
            # Writes probe folders into the shared media mount, so it runs late (after the
            # journeys that read the corpus) but before the destructive merge journey.
            j_library_webhooks, j_system_and_refresh,
            j_forgot_password, j_backup, j_livetv, j_livetv_series_timers,
            # Mutates the tuner/listings configuration the Live TV reads depend
            # on, so it runs after every other Live TV leg.
            j_livetv_admin, j_remote_subtitles,
            j_activity_log,
            j_remote_image_download,
            j_remote_search_identify,
            # Destructive-ish: rewrites the metadata of two movies it owns outright
            # (Movie 0497/0496, by name), and locks one of them.
            j_remote_search_apply,
            # Destructive: merges/splits the shared movies, so it must run LAST so its
            # mutations can't corrupt the items other journeys read/refresh.
            j_merge_versions_controller]

# ---------------------------------------------------------------- run

def run_all(base, token, user):
    mids = two_movies(base, token, user)
    if len(mids) < 2:
        raise SystemExit(f"{base}: need >=2 movies, got {len(mids)}")
    out = {}
    for jn in JOURNEYS:
        try:
            out.update(jn(base, token, user, mids[0], mids[1]))
        except Exception as e:   # a journey blowing up marks its ops failed, doesn't abort the rest
            out[f"_error:{jn.__name__}"] = str(e)
    return out


def journeys(ferrofin_url, jellyfin_url):
    ht, hu = bring_up(ferrofin_url, "ferrofin")
    jt = ju = None
    if jellyfin_url:
        # Both servers are authenticated BEFORE either suite runs, so a journey whose
        # oracle is a live third party (j_remote_search_identify) can ask them the same
        # question back-to-back instead of inheriting the leg-after-leg ordering below.
        # bring_up is idempotent, so hoisting it changes nothing else.
        jt, ju = bring_up(jellyfin_url, "jellyfin")
    PAIR.clear()
    _IDENTIFY_CACHE.clear()
    PAIR[ferrofin_url] = (jellyfin_url, jt)
    if jellyfin_url:
        PAIR[jellyfin_url] = (ferrofin_url, ht)
    h = run_all(ferrofin_url, ht, hu)
    j = {}
    if jellyfin_url:
        j = run_all(jellyfin_url, jt, ju)

    rows = {}
    for op in sorted(k for k in h if not k.startswith("_")):
        h_ok = h.get(op)
        j_ok = j.get(op)
        if jellyfin_url:
            agreed = cross_server_ok(h_ok, j_ok)
            deep = bool(h_ok and j_ok) and agreed
            method = earned_method(op, h_ok, j_ok)
            if h_ok and j_ok and not agreed:
                rows[op] = {"deep_verified": False,
                            "classification": "flagged: the write took on both servers but "
                                              "they ended in DIFFERENT states (verify)",
                            "verification_method": method,
                            "note": evidence_diff(h_ok.evidence, j_ok.evidence)}
                continue
            if h_ok and not j_ok:
                cls = "flagged: Jellyfin read-back differed (verify: oracle setup or Ferrofin extra)"
            elif not h_ok and j_ok:
                cls = "flagged: Ferrofin read-back did not reflect the write (verify: real gap vs read-back method)"
            elif not h_ok:
                cls = "flagged: write effect not observed on either server (likely corpus/setup)"
            else:
                cls = "ok"
            detail = {
                verification.BODY_DIFF: "read-back bodies diffed against each other",
                # `Same` rows compare a NAMED PROJECTION of each server's
                # read-back against the other's; plain-bool rows compare nothing
                # across servers at all. Saying "bodies not diffed" for both hid
                # which of the two a row was.
                verification.PROPERTY: ("named properties compared across the two servers; "
                                        "bodies NOT diffed"),
                verification.EFFECT: ("each server checked against its OWN read-back only; "
                                      "nothing compared across the two"),
                verification.STATUS_CLASS: "status only; nothing read back",
            }.get(method, "bodies not diffed")
            # A row that returned `Same` on both servers collected comparable
            # evidence even when one leg's assertion failed, and that evidence is
            # the only place the divergence is NAMED. Rendering it only in the
            # `agreed is False` branch above threw it away for every mixed row —
            # `POST /Startup/User` reported "H=True J=False" and said nothing
            # about the 403-vs-204 that made it so.
            ev = ""
            if isinstance(h_ok, Same) and isinstance(j_ok, Same) and h_ok.evidence != j_ok.evidence:
                ev = f" [evidence: {evidence_diff(h_ok.evidence, j_ok.evidence)}]"
            rows[op] = {"deep_verified": deep, "classification": cls,
                        "verification_method": method,
                        "note": f"H={h_ok} J={j_ok} ({method}; {detail}){ev}"}
        else:
            # No oracle: the row rests on Ferrofin alone, which is not a parity
            # verdict at all — there is nothing to have agreed with. Recorded
            # UNTESTED with the Ferrofin-only observation kept in the note; the
            # old code called this `deep_verified`, which then defaulted into the
            # ledger's body-diff headline with Jellyfin never contacted.
            rows[op] = {"deep_verified": None, "classification": "",
                        "verification_method": None,
                        "note": f"H={h_ok} — no Jellyfin oracle, so no parity verdict "
                                f"(Ferrofin-only run)"}
    # The `_` namespace never becomes a row. `_error:<journey>` records a journey
    # that blew up; `_note:<name>` records a measurement kept deliberately OUT of
    # a row's assertion (see the repeat-DELETE evidence in `j_playlist_share`) —
    # and a note is reported for BOTH servers, because its whole value is the
    # side-by-side.
    errors = {k: v for k, v in h.items() if k.startswith("_error")}
    notes = {k[len("_note:"):]: {"ferrofin": h.get(k), "jellyfin": j.get(k)}
             for k in sorted(set(h) | set(j)) if k.startswith("_note:")}
    return rows, errors, notes


def main():
    if "--check" in sys.argv:
        selfcheck()
        return
    ferrofin = os.environ.get("FERROFIN_URL", "http://localhost:18096")
    jellyfin = os.environ.get("JELLYFIN_URL")
    rows, errors, notes = journeys(ferrofin, jellyfin)
    out = {"generated_by": "suite/parity/journeys.py", "last_verified": os.environ.get("PARITY_STAMP", ""),
           "errors": errors, "notes": notes, "rows": rows}
    with open(os.path.join(ROOT, "suite/parity/journey-results.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True)
        f.write("\n")
    ok = sum(1 for v in rows.values() if v["deep_verified"])
    import collections
    by = collections.Counter(v["verification_method"] for v in rows.values()
                             if v["deep_verified"] is True)
    print(f"wrote parity/journey-results.json — {len(rows)} write ops, {ok} verified {dict(by)}"
          + (f", errors: {list(errors)}" if errors else ""))


def selfcheck():
    # The combine logic: deep_verified only when the effect holds on BOTH servers, and —
    # for a `Same` step — only when both servers ended in the same state.
    def combine(h_ok, j_ok):
        return bool(h_ok and j_ok) and cross_server_ok(h_ok, j_ok)
    assert combine(True, True) is True
    assert combine(True, False) is False   # Jellyfin disagrees → not verified
    assert combine(False, True) is False   # real Ferrofin gap → not verified
    # …and for a `Same` step, only when both servers ended in the same state.
    assert combine(Same(True, {"ProductionYear": 2020}), Same(True, {"ProductionYear": None})) is False
    assert combine(Same(True, {"ProductionYear": None}), Same(True, {"ProductionYear": None})) is True
    assert combine(Same(False, {"a": 1}), Same(True, {"a": 1})) is False
    # …and the note names the INNER key, not the whole projection.
    assert evidence_diff({"after": {"Year": 2020, "Name": "M"}},
                         {"after": {"Year": None, "Name": "M"}}) \
        == "after{Year: H=2020 J=None}"
    assert evidence_diff({"a": 1}, {"a": 1}) == "(no key differs)"
    # A MIXED row (one leg's assertion failed) still carries comparable evidence,
    # and the runner must name it: `POST /Startup/User` is red BECAUSE the two
    # servers' statuses differ, and a note that says only "H=True J=False" hides
    # the very fact the row exists to record.
    assert evidence_diff({"Status": 403, "FirstUserName": "bench"},
                         {"Status": 204, "FirstUserName": "bench"}) \
        == "Status: H=403 J=204"
    # A row may only KEEP a declared body-diff when both sides really compared.
    # Two rows declare it (the allowlist below pins which), but the guard itself is
    # exercised on a SYNTHETIC declaration so it is tested even when the real
    # claimants change: it must upgrade nothing and downgrade a plain-bool row.
    diffed = "GET /System/Info"
    assert diffed not in JOURNEY_METHOD
    JOURNEY_METHOD[diffed] = verification.BODY_DIFF
    try:
        assert earned_method(diffed, Same(True, 1), Same(True, 1)) == verification.BODY_DIFF
        assert earned_method(diffed, True, True) == verification.EFFECT
        assert earned_method(diffed, Same(True, 1), True) == verification.EFFECT
    finally:
        del JOURNEY_METHOD[diffed]
    # …and the ops that claim the headline are exactly the ones whose evidence is
    # a whole parsed body minus a named per-instance list — not a hand-listed
    # projection. This used to be a blanket "no journey op may claim body-diff";
    # it is an allowlist now rather than a deletion, so adding a third one is a
    # deliberate edit here and not a quiet upgrade at the call site.
    assert {op for op, m in JOURNEY_METHOD.items()
            if m == verification.BODY_DIFF} == set(JOURNEY_BODY_DIFF)
    # The derivation helper is the reason Id/ExternalId may be left out of that
    # body diff at all, so it is checked against the C# oracle here: the constant
    # `GET /LiveTv/Timers/Defaults` publishes is GetInternalSeriesTimerId("").
    assert dotnet_md5_guid_n("emby4") == "eb075d6a62e2edc6b764a304633d33c0"
    assert derives_its_id({"ExternalId": "8279078f967a44c4a96656331ebc08d2",
                           "Id": dotnet_md5_guid_n("emby8279078f967a44c4a96656331ebc08d24")})
    assert not derives_its_id({"ExternalId": "", "Id": "eb075d6a62e2edc6b764a304633d33c0"})
    assert not derives_its_id({"ExternalId": "abc", "Id": "abc"})
    # …and the body projection drops exactly the three named per-instance fields,
    # nothing else. `ExternalChannelId` is deliberately NOT among them: it was,
    # on a rationale that measured false, and it is back in the diff.
    assert series_timer_body({"Id": 1, "ExternalId": 2, "ServerId": 4,
                              "ExternalChannelId": "m3u_abc",
                              "Name": "n", "Priority": 0}) \
        == {"ExternalChannelId": "m3u_abc", "Name": "n", "Priority": 0}
    assert "ExternalChannelId" not in SERIES_TIMER_PER_INSTANCE
    # Two servers that each faithfully round-trip DIFFERENT defaults are not parity.
    assert combine(Same(True, {"EnableRealtimeMonitor": True}),
                   Same(True, {"EnableRealtimeMonitor": False})) is False
    assert combine(Same(True, {"EnableRealtimeMonitor": False}),
                   Same(True, {"EnableRealtimeMonitor": False})) is True
    assert combine(Same(False, {"a": 1}), Same(True, {"a": 1})) is False
    assert evidence_diff({"a": 1, "b": 2}, {"a": 1, "b": 3}) == "b: H=2 J=3"
    assert evidence_diff({"a": 1}, {"a": 1}) == "(no key differs)"

    # `identify` compares WHOLE candidate objects, so key presence counts. A row that
    # emits a null where the other omits the key must NOT compare equal — that asymmetry
    # is exactly what the BoxSet Overview fix changed.
    assert identify([{"Name": "X"}]) != identify([{"Name": "X", "Overview": None}])
    assert identify([{"Name": "X"}]) == identify([{"Name": "X"}])
    # ...and order and count count too.
    assert identify([{"Name": "A"}, {"Name": "B"}]) != identify([{"Name": "B"}, {"Name": "A"}])
    assert identify([{"Name": "A"}]) != identify([{"Name": "A"}, {"Name": "A"}])

    # Every journey advertises only op keys that exist in the vendored spec.

    # Every field the contract gives RemoteSearchResult is inside that comparison, because
    # nothing is projected away. Asserted against the spec so a contract bump that adds a
    # property cannot silently escape the diff.
    import glob
    spec = json.load(open(sorted(glob.glob(os.path.join(ROOT, "contracts/jellyfin-openapi-*.json")))[-1]))
    rsr = set(spec["components"]["schemas"]["RemoteSearchResult"]["properties"])
    sample = {k: None for k in rsr}
    assert identify([sample])[0].keys() == rsr, "identify() must carry every contract field"

    # `identify_responses` asks BOTH servers each case back-to-back, not leg-after-leg:
    # the interleaving is what makes an element-for-element diff of live TMDB answers
    # honest, so it is checked here rather than asserted in a docstring.
    global remote_search
    real, order = remote_search, []
    try:
        remote_search = lambda b, t, k, i, p=None: (order.append((b, k)) or (200, [{"Name": b}]))
        PAIR.clear(); _IDENTIFY_CACHE.clear()
        PAIR["F"] = ("J", "jtok")
        PAIR["J"] = ("F", "ftok")
        got_f = identify_responses("F", "ftok")
        got_j = identify_responses("J", "jtok")            # served from the cache
    finally:
        remote_search = real
    assert len(order) == 2 * len(IDENTIFY_CASES), order
    # Adjacent pairs, one case at a time — never all of F's then all of J's.
    assert [b for b, _ in order] == ["F", "J"] * len(IDENTIFY_CASES), order
    # ...and the adjacent pair is the SAME case on both servers.
    kinds = [k for _, k in order]
    assert kinds[0::2] == kinds[1::2] == [c[0] for c in IDENTIFY_CASES.values()], kinds
    # Each leg still reports its OWN server's answers, so the cross-server equality the
    # runner performs is doing real work.
    assert got_f["m_byid"][1] == [{"Name": "F"}] and got_j["m_byid"][1] == [{"Name": "J"}]
    PAIR.clear(); _IDENTIFY_CACHE.clear()

    # Every journey advertises only op keys that exist in the vendored spec.
    valid = {f"{m.upper()} {p}" for p, it in spec["paths"].items() for m in it if m in
             ("get", "post", "put", "delete", "patch")}
    class Rec(dict):
        pass
    # Dry-run each journey with a stub http that records nothing but returns benign values.
    declared = set()
    for jn in JOURNEYS:
        # Introspect by running against a no-op recorder base is overkill; instead assert the
        # op-key literals in each function source are spec paths.
        import inspect
        import re
        src = inspect.getsource(jn)
        for line in src.splitlines():
            if 'r["' in line:
                key = line.split('r["', 1)[1].split('"]', 1)[0]
                # `_`-prefixed keys are the runner's own namespace (`_error:…`,
                # `_note:…`): journeys() filters them out of `rows`, so they are
                # evidence carried alongside the run and are NOT contract ops.
                if not key.startswith("_"):
                    declared.add(key)
        # …plus op keys carried in a data table rather than an `r["…"]` literal —
        # the nine remote-control rows are assigned in a loop, so scraping only
        # `r["` left them unvalidated against the spec AND unstamped.
        declared.update(re.findall(r'"((?:GET|POST|PUT|DELETE|PATCH) /[^"\s]*)"', src))
    missing = sorted(k for k in declared if k not in valid)
    assert not missing, f"journey op-keys not in spec: {missing}"
    # Every declared op must resolve to a method inside the closed set, and NO
    # journey op may claim the body-diff headline — this layer never diffs a body.
    for k in declared:
        m = journey_method(k)
        assert m in verification.VALID, f"{k}: unknown verification method {m!r}"
        assert (m != verification.BODY_DIFF) or k in JOURNEY_BODY_DIFF, \
            f"{k}: only the enumerated JOURNEY_BODY_DIFF ops may claim the headline"
    assert JOURNEY_BODY_DIFF <= declared, JOURNEY_BODY_DIFF - declared
    stale = sorted(k for k in JOURNEY_METHOD if k not in declared)
    assert not stale, f"JOURNEY_METHOD names ops no journey declares: {stale}"
    undeclared = sorted(k for k in declared if k not in JOURNEY_METHOD)
    assert not undeclared, (f"{len(undeclared)} journey op(s) declare no "
                            f"verification_method: {undeclared}")
    import collections
    by = collections.Counter(journey_method(k) for k in declared)
    print(f"ok: combine logic, {len(declared)} journey op-keys all valid spec paths, "
          f"methods {dict(by)}")


if __name__ == "__main__":
    main()

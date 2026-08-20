#!/usr/bin/env python3
"""Extra WRITE endpoint rows for the fast loop only.

``suite/perf/endpoints.py`` is the gate's table: adding rows there changes what
``perf-gate.sh`` measures and what ``suite/perf-baseline.json`` must carry. The
write path needs more coverage than the gate's three write rows while it is
being investigated, so the extra rows live here and are merged in by ``hit.sh``
(fast loop only, never the gate).

Same entry shape as ``endpoints.py`` (see its docstring). Every row is
**state-preserving** under repetition, so a 10s window at 500/s leaves the
database where it found it:

* ``playstate_*`` report position 0 on ``{writeItemId}`` — the same row the
  gate's ``playstate_progress`` uses.
* ``userdata_*`` set a fixed value, so the Nth request writes what the 1st did.
* ``playlist_add_item`` re-adds an id the playlist already holds (an upsert that
  keeps the existing edge), ``playlist_move_item`` moves it to index 0 where it
  already is.
* ``displayprefs_post`` writes back a fixed preferences blob.
"""

from endpoints import _e

WRITE_ENDPOINTS = [
    # ── playstate reporting: what a playing client fires every few seconds.
    _e("playstate_start", "/Sessions/Playing", method="POST", ok=204,
       body={"ItemId": "{writeItemId}", "PositionTicks": 0, "CanSeek": True,
             "PlayMethod": "DirectPlay"}),
    _e("playstate_stopped", "/Sessions/Playing/Stopped", method="POST", ok=204,
       body={"ItemId": "{writeItemId}", "PositionTicks": 0,
             "PlaySessionId": "micro-fastloop"}),
    _e("playstate_ping", "/Sessions/Playing/Ping?playSessionId=micro-fastloop",
       method="POST", ok=204, body={}),

    # ── user-data updates (favourite / played / rating).
    _e("userdata_favorite_set", "/UserFavoriteItems/{writeItemId}", method="POST"),
    _e("userdata_played_set", "/UserPlayedItems/{writeItemId}", method="POST"),
    _e("userdata_rating_set", "/UserItems/{writeItemId}/Rating?likes=true", method="POST"),

    # ── playlist mutation.
    _e("playlist_add_item", "/Playlists/{playlistId}/Items?ids={writeItemId}&userId={userId}",
       method="POST", ok=204),
    _e("playlist_move_item", "/Playlists/{playlistId}/Items/{writeItemId}/Move/0",
       method="POST", ok=204),

    # ── display preferences (jellyfin-web writes these on layout changes).
    _e("displayprefs_post", "/DisplayPreferences/usersettings?userId={userId}&client=emby",
       method="POST", ok=204,
       body={"Id": "3CE5B65D-E116-D731-65D1-EFC4A30EC35C", "Client": "emby",
             "SortBy": "SortName", "SortOrder": "Ascending",
             "RememberIndexing": False, "RememberSorting": False,
             "PrimaryImageHeight": 250, "PrimaryImageWidth": 250,
             "ScrollDirection": "Horizontal", "ShowBackdrop": True,
             "ShowSidebar": False,
             "CustomPrefs": {"chromecastVersion": "stable",
                             "skipForwardLength": "30000",
                             "enableNextVideoInfoOverlay": "True",
                             "skipBackLength": "10000"}}),
]

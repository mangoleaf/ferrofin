#!/usr/bin/env python3
"""The round trip's Live TV check (gl-1556): Ferrofin's tuner, guide, timers and a favourite channel come back
in Jellyfin, before and after Jellyfin's first guide refresh.

    livetv.py fixture DIR                                          an M3U tuner and an XMLTV guide, into DIR
    livetv.py setup --base-url URL --data-dir DIR --out FILE       in Ferrofin: the tuner and the guide, a guide
                                                                   refresh, a timer, a series timer and a favourite
                                                                   channel; what it answers, to FILE
    livetv.py check --base-url URL --data-dir DIR --ferrofin FILE  in Jellyfin: its answers equal FILE's before its
                                                                   first guide refresh and after it

Both servers read the fixture at /livetv (run.sh mounts it there), so the tuner's path, which every channel's and
programme's id is derived from, is the same on both. The guide starts the next day at 18:00 UTC, so no airing ends
during a run; one of its programmes is in +01:00, whose id embeds the offset. The answers compared are each
timer's and series timer's ids, name and times, the channels' ids, the favourite channels' names, and the
programmes' ids. Credentials come from the copy (api.py), and the favourite is the account smoke.sh probes as
(answers.smoke_user). Exit status 0; 1 with one line per difference on stdout.
"""

import argparse
import datetime
import json
from pathlib import Path
import sys
import time
import urllib.parse

import answers
import api

#: Where run.sh mounts the fixture in both servers.
MOUNT = "/livetv"
#: The most programmes one request asks for: the fixture has 8, and a capture that holds fewer than the server
#: counts fails rather than compare part of the guide.
PROGRAMME_PAGE = 500
#: How often the guide refresh's state is polled, in seconds: api.py's interval for its waits.
POLL_SECONDS = api.POLL_SECONDS
#: The answers compared, per timer and series timer.
TIMER_KEYS = ("Id", "ExternalId", "ChannelId", "ExternalChannelId", "ProgramId", "ExternalProgramId",
              "SeriesTimerId", "ExternalSeriesTimerId", "Name", "StartDate", "EndDate", "Status")
SERIES_KEYS = ("Id", "ExternalId", "Name", "ChannelId", "ProgramId", "ExternalProgramId")


def fixture(directory, now=None):
    """Writes tuner.m3u (two channels) and guide.xml (six airings of a series, a film, one programme in +01:00)
    into `directory`, the guide from 18:00 UTC on the day after `now`."""
    now = now or datetime.datetime.now(datetime.timezone.utc)
    day = (now + datetime.timedelta(days=1)).replace(hour=18, minute=0, second=0, microsecond=0)
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "tuner.m3u").write_text(
        "#EXTM3U\n"
        '#EXTINF:-1 tvg-id="news.test" tvg-name="Test News" tvg-chno="101",Test News\n'
        "http://127.0.0.1:9/news.ts\n"
        '#EXTINF:-1 tvg-id="films.test" tvg-name="Test Films" tvg-chno="102",Test Films\n'
        "http://127.0.0.1:9/films.ts\n")

    def stamp(at, offset="+0000"):
        if offset == "+0100":
            at = at + datetime.timedelta(hours=1)
        return at.strftime("%Y%m%d%H%M%S ") + offset

    programmes = []
    for round_ in range(6):
        start = day + datetime.timedelta(hours=3 * round_)
        programmes.append(
            f'  <programme start="{stamp(start)}" stop="{stamp(start + datetime.timedelta(hours=1))}" channel="news.test">\n'
            f"    <title>Quiz Night</title>\n    <sub-title>Round {round_ + 1}</sub-title>\n"
            f'    <category>Game show</category>\n    <episode-num system="xmltv_ns">0.{round_}.</episode-num>\n'
            "    <new />\n  </programme>\n")
    film = day + datetime.timedelta(hours=2)
    programmes.append(
        f'  <programme start="{stamp(film)}" stop="{stamp(film + datetime.timedelta(hours=2))}" channel="films.test">\n'
        "    <title>The Long Road</title>\n    <category>Movie</category>\n    <date>1999</date>\n  </programme>\n")
    mix = day + datetime.timedelta(hours=14)
    programmes.append(
        f'  <programme start="{stamp(mix, "+0100")}" stop="{stamp(mix + datetime.timedelta(hours=1), "+0100")}" '
        'channel="films.test">\n    <title>Morning Mix</title>\n  </programme>\n')
    (directory / "guide.xml").write_text(
        '<?xml version="1.0" encoding="UTF-8"?>\n<tv>\n'
        '  <channel id="news.test"><display-name>Test News</display-name></channel>\n'
        '  <channel id="films.test"><display-name>Test Films</display-name></channel>\n'
        + "".join(programmes) + "</tv>\n")


def ok(status, body, what):
    """`body` when `status` is a success, else a CheckError naming `what`."""
    if status not in (200, 204):
        raise api.CheckError(f"{what}: status {status}")
    return body


def refresh_guide(server, timeout):
    """Runs the RefreshGuide task and waits until it finished again."""
    tasks = server.request("/ScheduledTasks")
    task = next((t for t in tasks if isinstance(t, dict) and t.get("Key") == "RefreshGuide"), None)
    if task is None:
        raise api.CheckError("no RefreshGuide task")
    before = (task.get("LastExecutionResult") or {}).get("EndTimeUtc")
    ok(*server.send("POST", f"/ScheduledTasks/Running/{task['Id']}"), "starting the guide refresh")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(POLL_SECONDS)
        task = server.request(f"/ScheduledTasks/{task['Id']}")
        last = task.get("LastExecutionResult") or {}
        if task.get("State") == "Idle" and last.get("EndTimeUtc") != before:
            if last.get("Status") != "Completed":
                raise api.CheckError(f"the guide refresh ended {last.get('Status')}")
            return
    raise api.CheckError(f"the guide refresh did not finish within {timeout:.0f} s")


def capture(server, user):
    """What the server answers on Live TV, as `user` (an id)."""
    query = urllib.parse.urlencode
    timers = ok(*server.send("GET", "/LiveTv/Timers"), "the timers")["Items"]
    series = ok(*server.send("GET", "/LiveTv/SeriesTimers"), "the series timers")["Items"]
    channels = ok(*server.send("GET", "/LiveTv/Channels?" + query({"UserId": user})), "the channels")["Items"]
    favourites = ok(*server.send("GET", "/LiveTv/Channels?" + query({"UserId": user, "IsFavorite": "true"})),
                    "the favourite channels")["Items"]
    programmes = capture_programmes(server, user)
    return {
        "timers": sorted(({k: t.get(k) for k in TIMER_KEYS} for t in timers), key=json.dumps),
        "series_timers": sorted(({k: t.get(k) for k in SERIES_KEYS} for t in series), key=json.dumps),
        "channels": sorted(c.get("Id") for c in channels),
        "favourite_channels": sorted(c.get("Name") for c in favourites),
        "programmes": sorted(p.get("Id") for p in programmes),
    }


def differences(ferrofin, jellyfin, when):
    """One line per answer of `jellyfin` (captured `when`) that is not `ferrofin`'s."""
    lines = []
    for key, expected in ferrofin.items():
        got = jellyfin.get(key)
        if got != expected:
            lines.append(f"Live TV {when}: {key} differ: Ferrofin {json.dumps(expected)}, Jellyfin {json.dumps(got)}")
    return lines


def setup(server, user, timeout):
    """The tuner and the guide at MOUNT, a guide refresh, a timer on the film, a series timer on the series and
    the news channel a favourite of `user`."""
    ok(*server.send("POST", "/LiveTv/TunerHosts", {"Type": "m3u", "Url": f"{MOUNT}/tuner.m3u",
                                                    "FriendlyName": "Round trip", "TunerCount": 2}), "adding the tuner")
    ok(*server.send("POST", "/LiveTv/ListingProviders?ValidateListings=false&ValidateLogin=false",
                    {"Type": "xmltv", "Path": f"{MOUNT}/guide.xml", "EnableAllTuners": True}), "adding the guide")
    refresh_guide(server, timeout)
    programmes = capture_programmes(server, user)
    for name, path in (("The Long Road", "/LiveTv/Timers"), ("Quiz Night", "/LiveTv/SeriesTimers")):
        programme = next((p for p in programmes if p.get("Name") == name), None)
        if programme is None:
            raise api.CheckError(f"the guide refresh stored no programme {name}")
        defaults = ok(*server.send("GET", f"/LiveTv/Timers/Defaults?programId={programme['Id']}"),
                      f"the timer defaults for {name}")
        ok(*server.send("POST", path, defaults), f"the timer on {name}")
    channels = ok(*server.send("GET", "/LiveTv/Channels?" + urllib.parse.urlencode({"UserId": user})),
                  "the channels")["Items"]
    news = next((c for c in channels if c.get("Name") == "Test News"), None)
    if news is None:
        raise api.CheckError("no channel Test News")
    ok(*server.send("POST", f"/UserFavoriteItems/{news['Id']}?userId={user}"), "the favourite channel")


def capture_programmes(server, user):
    """The guide's programmes, as `user`: all of them, or a CheckError."""
    query = urllib.parse.urlencode({"UserId": user, "Limit": PROGRAMME_PAGE})
    page = ok(*server.send("GET", "/LiveTv/Programs?" + query), "the programmes")
    if page.get("TotalRecordCount", len(page["Items"])) != len(page["Items"]):
        raise api.CheckError(f"the programmes: {page.get('TotalRecordCount')} counted, {len(page['Items'])} listed")
    return page["Items"]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("mode", choices=("fixture", "setup", "check"))
    parser.add_argument("directory", nargs="?")
    parser.add_argument("--base-url")
    parser.add_argument("--data-dir")
    parser.add_argument("--out")
    parser.add_argument("--ferrofin")
    parser.add_argument("--timeout", type=float, default=600)
    args = parser.parse_args(argv)
    if args.mode == "fixture":
        fixture(args.directory)
        return 0
    try:
        server = api.Server(args.base_url, args.data_dir)
        _, user = answers.smoke_user(args.data_dir)
        if args.mode == "setup":
            setup(server, user, args.timeout)
            Path(args.out).write_text(json.dumps(capture(server, user), indent=1, sort_keys=True))
            return 0
        ferrofin = json.loads(Path(args.ferrofin).read_text())
        lines = differences(ferrofin, capture(server, user), "before Jellyfin's first guide refresh")
        refresh_guide(server, args.timeout)
        lines += differences(ferrofin, capture(server, user), "after Jellyfin's first guide refresh")
    except (api.CheckError, OSError, ValueError, KeyError, TypeError) as error:
        print(f"Live TV: {error}")
        return 1
    for line in lines:
        print(line)
    return 1 if lines else 0


if __name__ == "__main__":
    sys.exit(main())

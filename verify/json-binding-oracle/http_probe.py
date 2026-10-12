#!/usr/bin/env python3
"""Probe value conversion on a disposable Jellyfin/Ferrofin server.

The auth file contains token and user_id. This overwrites that user's configuration
and named encoding/metadata settings; use only a disposable test installation.
"""
import argparse
import json
import urllib.error
import urllib.request
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("url")
parser.add_argument("auth", type=Path)
parser.add_argument("output", type=Path)
args = parser.parse_args()
base = args.url.rstrip("/")
auth = json.loads(args.auth.read_text())
headers = {
    "Content-Type": "application/json",
    "Authorization": (
        'MediaBrowser Client="JSON oracle", Device="Local", '
        'DeviceId="json-number-oracle", Version="1", Token="' + auth["token"] + '"'
    ),
}
rows = []


def probe(path, body, expected, content_type="application/json", method="POST"):
    request = urllib.request.Request(
        base + path, body.encode() if method == "POST" else None,
        {**headers, "Content-Type": content_type}, method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            status = response.status
            response.read()
    except urllib.error.HTTPError as error:
        status = error.code
        error.read()
    row = {
        "path": path.replace(auth["user_id"], "{user_id}"),
        "body": body, "status": status, "expected": expected,
    }
    if content_type != "application/json":
        row["content_type"] = content_type
    if method != "POST":
        row["method"] = method
    rows.append(row)
    print(row)
    assert status == expected, row


def read(path):
    request = urllib.request.Request(base + path, headers=headers)
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.load(response)


encoding = "/System/Configuration/encoding"
probe(encoding, '{"EncodingThreadCount":"3","DownMixAudioBoost":"1.5","DownMixStereoAlgorithm":1}', 204)
config = read(encoding)
expected = {"EncodingThreadCount": 3, "DownMixAudioBoost": 1.5, "DownMixStereoAlgorithm": "Dave750"}
assert {key: config[key] for key in expected} == expected
rows[-1]["observed"] = expected
probe(encoding, '{"EncodingThreadCount":" 3"}', 500)
probe(encoding, '{"EnableThrottling":"true"}', 500)
probe(encoding, '[]', 500)
metadata = "/System/Configuration/metadata"
probe(metadata, '{"useFileCreationTimeForDateAdded":false}', 204)
assert read(metadata)["UseFileCreationTimeForDateAdded"] is True
rows[-1]["observed"] = {"UseFileCreationTimeForDateAdded": True}
user = "/Users/" + auth["user_id"]
configuration = user + "/Configuration"
probe(configuration, '{"SubtitleMode":"3","SubtitleLanguagePreference":1.50}', 204)
config = read(user)["Configuration"]
expected = {"SubtitleMode": "None", "SubtitleLanguagePreference": "1.50"}
assert {key: config[key] for key in expected} == expected
rows[-1]["observed"] = expected
probe(configuration, '{"PlayDefaultAudioTrack":"true"}', 400)
probe(configuration, '{"SubtitleMode":999}', 204)
assert read(user)["Configuration"]["SubtitleMode"] == 999
rows[-1]["observed"] = {"SubtitleMode": 999}
probe(configuration, '{"SubtitleLanguagePreference":{}}', 400)
probe(configuration, '{"SubtitleLanguagePreference":[]}', 400)
probe(configuration, 'null', 400)
probe(configuration, '', 400)
probe(configuration, '{}', 415, "text/plain")
for literal in ["NaN", "Infinity", "-Infinity"]:
    probe(encoding, json.dumps({"DownMixAudioBoost": literal}), 204)
    probe(encoding, "", 400, method="GET")
probe(encoding, '{"DownMixAudioBoost":2}', 204)
args.output.write_text(json.dumps(rows, indent=2) + "\n")

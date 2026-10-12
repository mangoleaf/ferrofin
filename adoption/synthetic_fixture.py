"""Build meaningful adoption state through Jellyfin's own API."""

import base64
from io import BytesIO
import json
import re
import time
import urllib.error
import urllib.parse
import urllib.request

from PIL import Image

from metadata import Api as MetadataApi, CheckError


PASSWORD = "Synthetic-adoption-only-123!"
USERS = ("synthetic-admin", "synthetic-adult", "synthetic-child")
LIBRARIES = (("Synthetic Movies", "movies", "movies"),
             ("Synthetic Private", "movies", "private"),
             ("Synthetic Shows", "tvshows", "shows"),
             ("Synthetic Music", "music", "music"))


class Api:
    def __init__(self, base, token=None, user=None, device=None):
        self.base, self.token, self.user = base.rstrip("/"), token, user
        self.device = device or f"synthetic-{user or 'setup'}"

    def call(self, method, path, body=None, allowed=(200, 204), raw=None, binary=False, **query):
        query = {key: value for key, value in query.items() if value is not None}
        url = self.base + path + ("?" + urllib.parse.urlencode(query) if query else "")
        data = raw if raw is not None else json.dumps(body).encode() if body is not None else None
        auth = ('MediaBrowser Client="Jellyfin Web", Device="Synthetic adoption", '
                f'DeviceId="{self.device}", Version="1"')
        if self.token:
            auth += f', Token="{self.token}"'
        headers = {"Authorization": auth}
        if data is not None:
            headers["Content-Type"] = "image/png" if raw is not None else "application/json"
        request = urllib.request.Request(url, data=data, method=method, headers=headers)
        for attempt in range(60):
            try:
                with urllib.request.urlopen(request, timeout=60) as response:
                    payload, code = response.read(), response.status
            except urllib.error.HTTPError as error:
                code, payload = error.code, error.read()
                error.close()
            if code != 503:
                break
            time.sleep(1)
        if code not in allowed:
            route = re.sub(r"/[0-9a-fA-F-]{32,36}(?=/|$)", "/<id>", path)
            raise CheckError(f"synthetic fixture {method} {route} returned {code}")
        if code >= 400:
            return {"status": code}
        return payload if binary else json.loads(payload) if payload else None

    def get(self, path, **query):
        return self.call("GET", path, **query)

    def post(self, path, body=None, **query):
        return self.call("POST", path, body, **query)

    def request(self, path, method="GET"):
        return self.call(method, path)

    scan = MetadataApi.scan

    def login(self, name):
        for attempt in range(60):
            try:
                device = "synthetic-login-" + name
                response = Api(self.base, device=device).post("/Users/AuthenticateByName", {"Username": name, "Pw": PASSWORD})
                return Api(self.base, response["AccessToken"], response["User"]["Id"], device=device)
            except OSError:
                # Jellyfin may restart once after an upgrade. Authentication is
                # safe to retry; fixture mutations are deliberately not retried.
                if attempt == 59:
                    raise
                time.sleep(1)

    def ready(self, timeout=300):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                self.get("/System/Info/Public")
                return
            except (OSError, CheckError):
                time.sleep(1)
        raise CheckError("synthetic server did not become ready")

    def idle(self, timeout=300):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if all(t.get("State") == "Idle" for t in self.get("/ScheduledTasks")):
                return
            time.sleep(1)
        raise CheckError("synthetic server tasks did not finish")


def seed(base):
    api = Api(base)
    api.ready()
    api.post("/Startup/Configuration", {"UICulture": "en-US", "MetadataCountryCode": "US",
                                        "PreferredMetadataLanguage": "en"})
    api.get("/Startup/User")
    api.post("/Startup/User", {"Name": USERS[0], "Password": PASSWORD})
    api.post("/Startup/RemoteAccess", {"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False})
    api.post("/Startup/Complete")
    api = api.login(USERS[0])
    api.post("/Auth/Keys", app="Synthetic adoption")
    for name in USERS[1:]:
        api.post("/Users/New", {"Name": name, "Password": PASSWORD})
    types = ("Movie", "Series", "Season", "Episode", "MusicArtist", "MusicAlbum", "Audio")
    options = {
        "EnableRealtimeMonitor": False, "EnableInternetProviders": False,
        "EnableChapterImageExtraction": False, "ExtractChapterImagesDuringLibraryScan": False,
        "EnableTrickplayImageExtraction": False, "ExtractTrickplayImagesDuringLibraryScan": False,
        "EnableLUFSScan": False, "SaveLocalMetadata": False, "MetadataSavers": [],
        "AutomaticRefreshIntervalDays": 0, "PreferredMetadataLanguage": "en",
        "MetadataCountryCode": "US", "SubtitleDownloadLanguages": ["eng"],
        "DisabledSubtitleFetchers": [], "RequirePerfectSubtitleMatch": True,
        "SaveSubtitlesWithMedia": False, "SkipSubtitlesIfEmbeddedSubtitlesPresent": True,
        "SkipSubtitlesIfAudioTrackMatches": False,
        "TypeOptions": [{"Type": kind, "MetadataFetchers": [], "ImageFetchers": [],
                         "MetadataFetcherOrder": ["The Open Movie Database", "TheMovieDb"],
                         "ImageFetcherOrder": ["Fanart", "TheMovieDb"]} for kind in types],
    }
    for name, kind, folder in LIBRARIES:
        api.post("/Library/VirtualFolders", {"LibraryOptions": {
            **options, "PathInfos": [{"Path": "/media/" + folder}]}},
            name=name, collectionType=kind, refreshLibrary="false")
    api.scan(300, interval=1)
    api.idle()
    users = {row["Name"]: row for row in api.get("/Users")}
    libraries = {row["Name"]: row for row in api.get("/Library/VirtualFolders")}
    child = users[USERS[2]]
    ratings = api.get("/Localization/ParentalRatings")
    pg = next(row["Value"] for row in ratings if row["Name"] == "PG")
    for name in USERS[1:]:
        user = users[name]
        policy = user["Policy"]
        policy.update({"EnableContentDownloading": False, "EnableContentDeletion": False,
                       "EnableRemoteControlOfOtherUsers": False, "EnableAllFolders": name != USERS[2]})
        if name == USERS[2]:
            policy.update({"MaxParentalRating": pg, "EnabledFolders": [
                libraries[name]["ItemId"] for name, _, folder in LIBRARIES if folder != "private"]})
        api.post(f"/Users/{user['Id']}/Policy", policy)
        configuration = user["Configuration"]
        configuration.update({"AudioLanguagePreference": "fra", "SubtitleLanguagePreference": "eng",
                              "SubtitleMode": "Always", "PlayDefaultAudioTrack": False,
                              "RememberAudioSelections": True, "RememberSubtitleSelections": True})
        api.post("/Users/Configuration", configuration, userId=user["Id"])

    rows = api.get("/Items", userId=api.user, recursive="true", limit=1000,
                   fields="Path,ProviderIds,MediaSources,Genres,Studios,Tags")["Items"]
    movies = {r["Name"]: r for r in rows if r["Type"] == "Movie"}
    episodes = sorted((r for r in rows if r["Type"] == "Episode"),
                      key=lambda r: (r["ParentIndexNumber"], r["IndexNumber"]))
    tracks = sorted((r for r in rows if r["Type"] == "Audio"), key=lambda r: r["Path"])
    if len(episodes) != 6 or len(tracks) != 6 or len(movies) < 5:
        raise CheckError("synthetic media was not fully scanned")

    adult = users[USERS[1]]["Id"]
    movie = movies["Blue Orchard"]["Id"]
    edited = api.get(f"/Items/{movie}", userId=api.user)
    edited.update({"Name": "Manually edited orchard", "Overview": "A deliberate manual description.",
                   "ForcedSortName": "000 synthetic manual order", "LockData": True,
                   "LockedFields": ["Name", "Overview", "Genres"],
                   "Genres": ["Synthetic manual genre"], "ProviderIds": {"Tmdb": "999999991", "Imdb": "tt99999991"}})
    api.post(f"/Items/{movie}", edited)
    picture = BytesIO()
    Image.new("RGB", (100, 150), "cyan").save(picture, format="PNG")
    api.call("POST", f"/Items/{movie}/Images/Primary", raw=base64.b64encode(picture.getvalue()))

    # Different accounts deliberately disagree on the same movie and series.
    for index, item in enumerate([movies["Amber Harbor"]["Id"], *[r["Id"] for r in episodes[:3]]]):
        api.post(f"/UserPlayedItems/{item}", userId=adult, datePlayed=f"2026-01-0{index+1}T12:00:00Z")
    api.post(f"/UserPlayedItems/{episodes[0]['Id']}", userId=child["Id"], datePlayed="2026-01-02T12:00:00Z")
    for user, item, ticks in [(adult, movie, 2_000_000_000), (adult, episodes[3]["Id"], 3_000_000_000),
                               (child["Id"], movies["Amber Harbor"]["Id"], 1_000_000_000)]:
        api.post(f"/UserItems/{item}/UserData", {"Played": False, "PlaybackPositionTicks": ticks,
                 "LastPlayedDate": "2026-01-06T12:00:00Z"}, userId=user)
    api.post(f"/UserFavoriteItems/{tracks[0]['Id']}", userId=adult)

    playlist = api.post("/Playlists", {"Name": "Synthetic mixed order", "MediaType": "Audio",
        "Ids": [tracks[3]["Id"], tracks[0]["Id"], tracks[2]["Id"]], "UserId": adult,
        "Users": [{"UserId": child["Id"], "CanEdit": False}], "IsPublic": False})["Id"]
    private_playlist = api.post("/Playlists", {"Name": "Synthetic private playlist", "MediaType": "Video",
        "Ids": [movie, movies["Amber Harbor"]["Id"]], "UserId": adult, "IsPublic": False})["Id"]
    collection = api.post("/Collections", name="Synthetic collection", ids=",".join(
        [movies["Amber Harbor"]["Id"], movie]), isLocked="true")["Id"]
    # The initial full collection refresh can reload an empty collection.xml.
    # The normal add-members action settles membership after creation finishes.
    api.post(f"/Collections/{collection}/Items", ids=",".join([movies["Amber Harbor"]["Id"], movie]))
    api.idle()
    manifest = {"version": 1, "users": {name: users[name]["Id"] for name in USERS},
            "libraries": {name: row["ItemId"] for name, row in libraries.items()},
            "locked_movie": movie, "allowed_movie": movies["Amber Harbor"]["Id"],
            "rated_movie": movies["Crimson Signal"]["Id"],
            "private_movie": movies["Hidden Meadow"]["Id"],
            "versions_movie": movies["Twin Horizon"]["Id"],
            "playlist": playlist, "private_playlist": private_playlist, "collection": collection}

    from user_accounts import seed as seed_accounts
    seed_accounts(api, manifest)
    return manifest

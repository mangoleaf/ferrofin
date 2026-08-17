# Upgrading Ferrofin

Operator-facing notes for upgrades that need a manual step or change behavior
in a way the release's feature list doesn't make obvious. Newest first.

`CHANGELOG.md` is generated from commit messages and lists *what* changed;
this file explains *what you have to do about it*.

## Unreleased — per-library metadata/image fetcher enforcement

Per-library **Metadata downloaders** / **Image fetchers** selections are now
enforced during the library scan (previously they were saved but ignored), and
five built-in fetchers are newly named in a library's options:

- TheTVDB
- FanArt
- MusicBrainz
- TheAudioDB
- Embedded Image Extractor

**Who is affected:** anyone running Ferrofin before this release, plugins or
not. A library whose options were last saved by an older Ferrofin cannot have
these five names in its saved fetcher lists, and "not in the saved list" now
means "disabled" — so after the upgrade that library silently stops fetching
TVDB/fanart/MusicBrainz/TheAudioDB metadata and stops extracting embedded cover
art.

**What to do:** open each existing library's settings in the dashboard and
click **Save** once. The UI re-saves the full current fetcher list, which
includes the newly named fetchers, and they resume on the next scan.

**Not affected:** libraries migrated from a real Jellyfin database (their saved
lists already use Jellyfin's fetcher names, which match), and any library
created after the upgrade (it starts with everything enabled).

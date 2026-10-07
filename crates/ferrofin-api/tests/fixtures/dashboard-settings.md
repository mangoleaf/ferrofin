# Dashboard save payload fixtures

Source: Jellyfin Web `1e507c588f353482a00333f84e36ddb7c8fc8221`, reviewed from
`/home/mango/dev/3rdparty/jellyfin-web` on 2026-10-07. The JSON contains manually
chosen form values with the member names, nesting, and value types emitted by
these current Web save actions; it is not generated from Ferrofin DTO defaults.

| Fixture | Web source under `src/` |
|---|---|
| `server` | `apps/dashboard/routes/settings/index.tsx`, `libraries/display.tsx`, `libraries/metadata.tsx`, `playback/resume.tsx`, `playback/streaming.tsx`, `playback/trickplay.tsx`, `logs/index.tsx` |
| `branding` | `apps/dashboard/routes/branding/index.tsx` |
| `network` | `apps/dashboard/routes/networking/index.tsx` |
| `encoding` | `apps/dashboard/routes/playback/transcoding.tsx` |
| `metadata` | `apps/dashboard/routes/libraries/display.tsx` |
| `xbmcmetadata` | `apps/dashboard/routes/libraries/nfo.tsx` |
| `livetv` | `apps/dashboard/routes/livetv/recordings.tsx` |
| `library` | `components/libraryoptionseditor/libraryoptionseditor.js`, `components/imageOptionsEditor/imageOptionsEditor.js` |

The main fixture combines the fields changed by the listed actions. Web fetches
the current full server document before assigning those fields. The transport
test supplies the fields to the real POST binder, then reads them back. Named
fixtures cover Web's form/reducer values; encoding excludes the read-only
`EncoderAppPathDisplay`. Library creation and ID-based update both exercise the
complete editor payload, including provider lists and nested image options.

One historical constant is intentionally ignored: Web still emits
`EnableArchiveMediaFiles: false`. It has no editable control and is absent from
Jellyfin v12.0-rc7's `LibraryOptions` (`4910aafa1a`). Ferrofin discards it just as
that server does. Do not turn it into a pretend archive-media capability.

These tests prove transport fidelity, not that each setting has a runtime
consumer. The living dashboard review tracks the separate consumer findings.
When updating the Web pin, recheck the action objects, controlled input names,
provider-list helpers and nested payloads before updating this fixture.

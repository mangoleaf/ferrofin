# S38: encoder selection honours the saved path, and ffprobe is found like Jellyfin finds it

`MediaEncoder.SetFFmpegPath` chooses ffmpeg from the command line or
environment, then the encoding configuration's saved `EncoderAppPath`, then
`$PATH`. Ferrofin's bootstrap had the middle rung written but never wired —
startup always passed "no saved path" — so an adopted Jellyfin server whose
`encoding.xml` named a custom ffmpeg silently ran whatever was on `$PATH`.
Its ffprobe derivation also differed from upstream's.

Source oracle: Jellyfin `4910aafa1a`, `MediaEncoder.SetFFmpegPath`
(`MediaEncoder.cs:180-238`) and `FfprobePathRegex`; the existing evidence note
is `brain/knowledge/DASHBOARD_SETTINGS_S38_ENCODER_BOOTSTRAP.md`.

## Behavior

- **Selection order** is now `--ffmpeg` / `FERROFIN_FFMPEG_PATH` → the saved
  `EncoderAppPath` → `ffmpeg` on `$PATH`. The saved value is read before
  discovery from `{config}/users/named/encoding.json` (the live store) or, on a
  server Ferrofin has not yet saved for, the adopted Jellyfin
  `{config}/encoding.xml`. A missing, blank, malformed or null value is "not
  configured"; `EncoderAppPathDisplay` is never mistaken for the selector. The
  failure message and log label name the encoding configuration.
- **ffprobe derivation** is `FfprobePathRegex`: the whole final path component
  becomes `ffprobe` and only a trailing `.ext` survives, `/` and `\` both ending
  a directory — `jellyfin-ffmpeg` and `ffmpeg-static` → `ffprobe`, `ffmpeg.exe`
  → `ffprobe.exe`, `ffmpeg-6.1` → `ffprobe.1`. The old rule replaced the
  substring `ffmpeg` inside the name (`jellyfin-ffprobe`, which does not exist
  beside a `jellyfin-ffmpeg`).
- **Unchanged, and a recorded difference.** A host without a usable ffmpeg
  still boots with transcoding disabled (documented in the repository guide),
  where Jellyfin aborts startup with "Failed to find valid ffmpeg"; Ferrofin
  also still validates ffprobe's version where Jellyfin's check is nonfatal, and
  has no `FFmpeg:novalidation` bypass. Whether to match those startup rules is
  the owner's call (the guide lists the accepted divergences), so S38 is closed
  for selection and derivation only; the lifecycle rules above remain an open
  decision.

## Verification

- Unit: the derivation for every shape above, including Windows separators and
  dotted names; the saved path from JSON, from XML (entities decoded, the
  display field ignored, self-closing and blank = unconfigured), JSON winning over
  an XML that names another, and every unusable JSON shape.
- Production binary, previous baseline vs updated, started without `--ffmpeg`
  with `EncoderAppPath` saved to a symlink named `my-ffmpeg` (its sibling
  `ffprobe` beside it):

  | | encoder in use (`EncoderAppPathDisplay`) |
  |---|---|
  | before | `/usr/bin/ffmpeg` — the saved path ignored |
  | after | **the saved `…/tools/my-ffmpeg`**, ffprobe found beside it |

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for the server
package passed: server 237. Real FFmpeg tests enabled; the WASM guest build
remains disabled. The server is exempt from the coverage gate (composition
root); the selection and derivation logic it contains is tested above. Only the
server crate changed.

# S08: embedded images from videos

The "Embedded Image Extractor" image fetcher was advertised for videos but only
worked for audio files. A movie's own file can now supply its Primary,
Backdrop and Logo.

Source oracle: Jellyfin `4910aafa1a`,
`MediaBrowser.Providers/MediaInfo/EmbeddedImageProvider.cs` (`Supports`,
`GetSupportedImages`, `GetImage`, `GetEmbeddedImage`) and
`MediaEncoder.ExtractVideoImage(…, imageStream, imageStreamIndex, targetFormat)`.

## Behavior

- **Types.** An episode offers Primary only; a movie, video, music video or
  trailer offers Primary, Backdrop and Logo. Each runs when the item lacks that
  image and the library's image limit for the type is above zero.
- **Source choice.** An attachment whose file name contains a fragment
  (case-insensitive) wins; otherwise the first embedded-image stream whose
  comment contains one; a Primary with no labelled match takes the first
  embedded-image stream. Fragments — Primary: `poster folder cover default
  movie show`; Backdrop: `backdrop background art`; Logo: `logo`.
- **Format.** An attachment's comes from its MIME type (else its file
  extension), a stream's from its codec (`bmp`, `gif`, `png`, `webp`; anything
  else, including `mjpeg`, is `jpg`).
- **Eligibility.** No `.strm` shortcut, non-file protocol, placeholder or DVD
  structure.
- **Extraction.** New `MediaEncoder::extract_image_stream` maps the stream by
  index (`-map 0:N`, one frame) into the temp directory in the chosen format,
  trying the I-frame path first and the standard one second, as
  `ExtractImage` does. The default implementation reports it unsupported so
  frame-only encoders and fakes are unaffected. The picture is stored as
  `{metadata}/library/{id}/{primary|backdrop|logo}.{ext}`; an existing file is
  reused unless images are being replaced.
- **Attachments from the same pass.** The scan persists attachments after the
  artwork step, so the probe that just ran hands its attachment rows to the
  extractor directly; a refresh without a probe reads the stored rows.
- **Checkbox.** Unticking "Embedded Image Extractor" for the type skips all of
  this.

## Verification

- Unit: the source choice (attachment over stream, MIME over extension, codec
  formats, Primary fallback, no fallback for other types, empty names) and the
  per-kind type list.
- Scan (`library_scan_video_embedded_images`): a movie probed with a "Cover"
  stream, a "Backdrop art" stream and a `Studio-LOGO.png` attachment gets
  `primary.jpg`, `backdrop.png` and `logo.png` with the right bytes and the
  right stream indexes; an unchanged rescan extracts nothing; a library whose
  image fetchers omit the extractor gets nothing.
- Encoder: the I-frame attempt failing then the standard one succeeding, the
  failure message carrying ffmpeg's tail, and no stray temp files.
- Real ffmpeg (`FERROFIN_FFMPEG_TESTS=1`): a Matroska file with a PNG
  attachment yields a valid 32×24 PNG for its stream index; a non-picture
  stream errors and leaves nothing behind. ffmpeg 9 rejects `-vsync`, so the
  test supplies the probed version as the server does.
- Production binary, previous baseline vs updated, a real MKV with three PNG
  attachments (`cover`, `backdrop`, `logo`), one library with defaults and one
  with the extractor unticked:

  | | default library | extractor unticked |
  |---|---|---|
  | before | no images | no images |
  | after | **Primary, Logo, 1 Backdrop** | no images |

- Scan cost, 300 empty movies, three runs each (debug builds): 1.84/1.95/1.93 s
  before, 1.95/1.83/2.01 s after — no difference beyond noise. Items with no
  embedded pictures add only the lookups above.

## Limitations

- Stored width, height and blur hash are filled by the image pass that
  follows, as for other local images.
- A Blu-ray or other non-DVD disc structure is treated like a file, as
  upstream's `Supports` does.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on a changed crate passed: mediaencoding 952, hls 117, core
2,441, extensions 96, livetv 296, wasm 62, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
two over-long functions and later runs a file-extension comparison and a
similar-names lint in new tests; the extraction was split into helpers
(`embedded_image_eligible`, `stored_media_source`,
`extract_video_embedded_images`), the lints fixed and Clippy rerun clean, with
the new core and encoder tests rerun. The package and coverage results above
were taken just before those refactors. Line coverage over each crate's own
sources: **mediaencoding 94.20%**, **core 95.08%**, both above the 80% gate;
traits and server composition are coverage-exempt. The instrumented core build
took `target` to 42.7 GiB, above the 30 GiB cap; each coverage cache was
removed right after its numbers were taken, leaving 24.0 GiB with about
494 GiB free.

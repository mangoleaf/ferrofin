# B03: the splashscreen preview, formats and first upload

`GET /Branding/Splashscreen` had three defects around the dashboard's custom
splashscreen page, and a fresh install could not upload one at all.

Source oracle: Jellyfin `4910aafa1a`, `ImageController.GetSplashscreen`,
`UploadCustomSplashscreen` and `GetImageResult`.

## Behavior

- **Administrator preview.** Upstream is `!SplashscreenEnabled &&
  !User.IsInRole(Administrator)` → 404. Ferrofin refused everyone while the
  splashscreen was disabled, so the dashboard could not preview an uploaded
  image before enabling it. The route now reads the optional caller; an API key
  or a user whose policy is administrator gets the file, anyone else (signed in
  or anonymous) still gets 404. The role test is shared with `RequireAdmin`.
- **`format` and `tag`.** The route used to serve the stored bytes untouched,
  ignoring both. It now goes through the same tail as every image route: the
  image processor converts to the requested `format` (an explicit format wins,
  otherwise the client's `Accept` list), and a `tag` earns
  `Cache-Control: public, max-age=31536000, immutable` and an `ETag`
  (`GetImageResult` with a 365-day cache duration), including `304` on a
  matching `If-None-Match`.
- **First upload.** The upload wrote to `<data>/splashscreen-upload.<ext>`
  without creating `<data>`, which does not exist in a fresh Ferrofin data
  directory, so the first upload failed with a 500 (`No such file or
  directory`). The directory is now created first (Jellyfin's data path exists
  from startup).

## Verification

- API tests (`branding.rs`, with caller stubs for key, regular user and
  anonymous): a disabled splashscreen is served to the key and 404s for a
  regular user and an anonymous caller; enabled, everyone gets it; an untagged
  response is `public`, a tagged one immutable with its ETag. The existing
  upload/get/delete, disabled, missing-file and bad-content-type tests pass.
- Production binary, previous baseline (data directory pre-created so its upload
  works) vs updated, a 16×16 red PNG uploaded with the splashscreen disabled:

  | | admin GET | anonymous GET | `?format=jpg` | `?tag=abc` caching |
  |---|---|---|---|---|
  | before | **404** | 404 | 404 | 404 |
  | after | **200** | 404 | **200 `image/jpeg`** (`ffd8ff…`) | **immutable** + `"abc"` |

  Before, the disabled flag hid format and caching behavior too; the code ignored
  both regardless. On a fresh data directory the previous binary's upload
  returned 500; the updated one returns 204.

## Limitations

- The upload still writes the decoded bytes without validating that they are an
  image, as upstream does.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on api passed: api 931, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. The first Clippy run flagged
the field order of a struct constructor; it was fixed and Clippy and the
branding tests rerun clean, while the package and coverage results were taken
just before. Line coverage over api's own sources: **87.81%**, above the 80%
gate; traits and server composition are coverage-exempt. The instrumented build
took `target` to 36.7 GiB, above the 30 GiB cap; its cache was removed right
after the numbers were taken, leaving 24.2 GiB with about 494 GiB free.

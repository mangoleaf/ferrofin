# G02: verify the live server UI culture

`UICulture` was already connected to a live configuration reader. This change adds
regression coverage showing that an existing localization service observes a
saved culture immediately and recomputes media-stream labels/display titles.
No production behavior changed.

The tested server-default lookup uses the configured culture, then English, then
the phrase key. This follows the fallback rules in Jellyfin v12.0-rc7's
`LocalizationManager` (`4910aafa1a`). Explicit culture lookups retain precedence
over the server default. Web's own client-language preference is a separate
surface from this server setting.

## Verification

The new media-stream regression and all 38 localization tests pass. Formatting,
strict workspace Clippy, and the server build pass.

Real HTTP checks generated and scanned a two-second video with one default audio
stream, with remote metadata/image providers disabled. Without restarting, saving
`UICulture=de` produces `Standard`, `fr` produces `Par défaut`, and an unknown or
empty culture produces `Default`. Both `LocalizedDefault` and the composed
`DisplayTitle` change on the next playback-info request. The before and after
builds both pass, confirming the original supported classification.

For the same disposable media fixture, authenticated curl playback-info GETs used
10 warmups and 50 samples. Median latency was **1.623 → 1.542 ms**, p95
**2.637 → 3.115 ms**. These are noisy shared-host measurements of unchanged
production code, not a performance improvement claim. Full workspace tests,
doctests and per-crate coverage run at the end of the first 20 findings.

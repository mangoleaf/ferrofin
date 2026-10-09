# Dashboard settings: first implementation batch

This batch covers the first 20 findings, **C01–C04, G01–G09 and U01–U07**,
in order on `fix/dashboard-settings`, based on
`782ca9f28cc938bf15b5a55b425c435673d2f392`. Each finding has its own signed-off
commit and validation record in this directory, without a co-author trailer.
The canonical living checklist is
`brain/knowledge/JELLYFIN_WEB_DASHBOARD_SETTINGS_REVIEW.md`; it remains ignored
and shared with the main checkout through the worktree's `brain` link.

The changes connect saved configuration to its runtime consumers, validate and
atomically publish updates, honor cache/metadata paths and concurrency limits,
and correct account, session and authentication-provider behavior. Findings that
were already supported received targeted verification and regression coverage.
U07's original "Supported" assessment proved too broad: ordinary requests and
password login bypassed the user remote-access setting. That gap is fixed here.
The remaining dashboard findings retain their individual status in the checklist.

The UI reference is local Jellyfin Web
`1e507c588f353482a00333f84e36ddb7c8fc8221`; the behavior oracle is the repository's
pinned Jellyfin `v12.0-rc7`, `4910aafa1a`. Live checks used disposable data and
accounts. The primary checkout's code and build targets were left untouched.

## Final checks

All required checks pass on the final implementation:

- `cargo fmt --all --check`.
- Strict workspace Clippy with all targets and features.
- `cargo nextest run --workspace`: **7,772 passed, 5 skipped**.
- `cargo test --workspace --doc`: **3 passed**, no failures.
- Debug and release server builds, plus the individual live HTTP checks recorded
  beside each finding.

Each coverage gate ran one crate at a time and excluded other workspace crates
from its report. All ten changed, non-exempt crates exceed the 80% line gate:

| Crate | Line coverage |
|---|---:|
| `ferrofin-util` | 96.10% |
| `ferrofin-common` | 87.18% |
| `ferrofin-model` | 86.86% |
| `ferrofin-drawing` | 91.41% |
| `ferrofin-mediaencoding` | 93.74% |
| `ferrofin-providers` | 93.20% |
| `ferrofin-livetv` | 93.83% |
| `ferrofin-extensions` | 94.87% |
| `ferrofin-api` | 87.19% |
| `ferrofin-core` | 93.85% |

Traits and the server composition root have the repository's documented coverage
exemption. The final workspace run includes the SQL-boundary check, real-server
restart/restore tests, and contract route checks. Two issues caught during the
batch run were corrected before the passing rerun: the remote-login fixture now
explicitly grants remote access after checking rejection, and the provider-ID
write lives in a persistence module.

## Combined browse verification

The repository's `bench/screens.js` functional shape pass ran against both native
release binaries built with Rust 1.98.1. Docker daemon access is unavailable to
this account. Separate disposable configuration copies shared a private,
read-only copy of the generated benchmark media; original benchmark data was
preserved. Servers used CPUs 8–15 and k6 used CPUs 16–19. Plugins were disabled,
startup tasks drained, and a 30-second settle preceded the functional pass.

The baseline is the branch base; the after build includes all 20 findings. Both
returned **146 successful responses** (144 HTTP 200 and two HTTP 204), with
identical statuses, item counts, field sets and image byte counts in the shape
pass. Library counts also matched: 3,001 movies, 250 series, 7,490 episodes,
296 artists, 800 albums and 8,000 songs. This verifies consistency across the
combined changes on the repository's realistic browse fixture.

A combined latency window could **not** be measured: three attempts failed the
unchanged 90% initial-idle requirement at **83.8%, 80.1% and 23.7% idle**.
The functional pass deliberately evaluates no latency results. Docker/cgroup
memory measurements were also unavailable. Per-finding before/after HTTP timings
and resource-limit observations remain in the individual records; they are
shared-host observations, not optimization claims. The pending combined latency
comparison needs a quiet host; its prepared method is the same pinned cores,
separate 30-second warmups and 120-second windows at five screens per second.

The [validation summary](dashboard-settings-batch-01-validation.json) preserves
coverage, test totals, binary hashes and the functional comparison. Local raw
logs are `/tmp/ferrofin-dashboard-final-*.log`; disposable shape results are under
`target/dashboard-native-bench/{before-shape,after-shape}/`. This record closes
the earlier batch-end test and coverage notes and explicitly records the
remaining performance-measurement limitation.

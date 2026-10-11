# TV03: the M3U playlist is fetched with the tuner's user agent

The tuner's **User agent** setting reached only the stream's required HTTP
headers. The playlist request itself always presented Ferrofin's own product
agent, so a provider whose playlist URL only answers a particular client failed
before any channel was listed.

Source oracle: Jellyfin `4910aafa1a`, `M3uParser.GetListingsStream`
(`requestMessage.Headers.UserAgent.TryParseAdd(info.UserAgent)` for a non-empty
value) and `M3UTunerHost` (which already sends the agent for the stream).

## Behavior

- `SourceFetcher` gains `fetch_as(url, user_agent)`. The real fetcher sends
  the agent on `http(s)` requests; a local playlist path needs none. The default
  ignores it, so existing fakes keep working.
- The M3U tuner host fetches its playlist with `tuner.user_agent`.
- A blank value, or one that is not a valid header value, leaves the client's
  own agent in place (`TryParseAdd` drops what it cannot parse).
- The XMLTV listings fetch is unchanged: upstream's listings provider has no
  user-agent setting.

## Verification

- Unit: a one-shot HTTP server records the request's `User-Agent` — a
  configured value (trimmed) arrives, an absent, blank or invalid one leaves the
  product agent; the default `fetch_as` ignores the agent; the host passes the
  tuner's value through.
- Production binary, previous baseline vs updated, an M3U tuner with `UserAgent
  = "VLC/3.0 ferrofin-test"` served by a local HTTP server that records the
  header:

  | | playlist request `User-Agent` |
  |---|---|
  | before | `Ferrofin/0.4.1` |
  | after | **`VLC/3.0 ferrofin-test`** |

## Limitations

- A configured agent is sent as a single `User-Agent` value; upstream's
  `TryParseAdd` also accepts product/comment lists, which reqwest passes through
  as the same single string.

## Gates

`cargo fmt --check`, strict workspace Clippy (all targets/features,
`-D warnings`), the SQL boundary test, and nextest plus doctests for every
package depending on livetv passed: livetv 299, server 230. Real FFmpeg tests
enabled; the WASM guest build remains disabled. Line coverage over livetv's own
sources: **93.82%**, above the 80% gate; traits and server composition are
coverage-exempt. The instrumented build was cleaned right after the numbers
were taken, leaving `target` at about 24 GiB with about 494 GiB free.

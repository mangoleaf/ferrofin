# Configuration

Every runtime setting below is listed in jellyfin-web under **Dashboard → Plugins → Ferrofin
Settings → Settings** ([Seeing what is set](#seeing-what-is-set)). That page edits the ones it may
change and saves them to `config.toml` ([Editing settings from the
dashboard](#editing-settings-from-the-dashboard)), the last of the three places below; the first two
are set where the server is started.

Ferrofin is configured three ways, **highest precedence first**:

1. **CLI flags** — `--data-dir`, `--bind`, `--port`, `--published-url`, `--ffmpeg`, `--config`.
2. **`FERROFIN_*` environment variables** — the full surface, below.
3. **`config.toml`** — an optional file at `{data_dir}/config.toml` (or `--config`/
   `FERROFIN_CONFIG_FILE`). Each key is its variable's stem, lower-cased and prefix-stripped
   (`FERROFIN_BIND_ADDR` → `bind_addr`), except the six in the table below. The whole file is
   optional; a missing one is not an error. It is TOML and must be named `*.toml`: any other
   name (`config.json`, `config.yaml`, …) is refused.

| Variable | `config.toml` key |
|---|---|
| `FERROFIN_LOG` | `log_level` |
| `FERROFIN_OMDB_KEY` | `omdb_api_key` |
| `FERROFIN_TVDB_KEY` | `tvdb_api_key` |
| `FERROFIN_TVDB_PIN` | `tvdb_subscriber_pin` |
| `FERROFIN_FANART_KEY` | `fanart_personal_api_key` |
| `FERROFIN_MUSICBRAINZ_URL` | `musicbrainz_base_url` |

Each setting has exactly one key; the variable's stem is not accepted for these six.
`FERROFIN_CONFIG_FILE` has no key: it names the file. A key the file cannot hold is ignored with
a warning at startup that names it, and a key spelled as its variable's stem (`tvdb_pin`) is
told the key to use (`tvdb_subscriber_pin`). A number or on/off variable whose value does not
parse (`FERROFIN_PORT=80g`) is ignored with a warning naming it, so what it would outrank
applies, and a blank one counts as unset; the rows that say so (`FERROFIN_DB_POOL`,
`FERROFIN_DB_STATEMENT_CACHE`, the cache lifetimes, `FERROFIN_TRICKPLAY_CONCURRENCY`, the login
backoff's numbers) stop the server on a bad value instead.

The file is `--config`, else `FERROFIN_CONFIG_FILE` (blank counts as unset), else `config.toml` in the data directory
that `--data-dir`, `FERROFIN_DATA_DIR` or the default location names, in that order. A
`data_dir` key in the file applies only to a file named by `--config` or `FERROFIN_CONFIG_FILE`:
a file found in the data directory cannot move it, so the key is ignored there, with a warning.
The startup log's `paths` line names the file (`config_file`) and whether it was found
(`config_file_found`).

Everything below has a working default except where noted; a bare `cargo run -p ferrofin-server`
boots.

### Seeing what is set

`GET /Ferrofin/Settings` (administrators only) reports every setting below with its readable
name (`Label`), its tab (`Group`), the value in use and its unit (`Unit`, when it has one), its
default, and where that value comes from: a flag, a variable, the Networking page (`port`,
`https_port` and `base_url` only), `config.toml` or the default. A value `config.toml` holds
that is not in use (the file
changed since the server started, or a flag, a variable or the Networking page outranks it) is
reported beside it. A choice lists its options (`Options`, each with its `Value`, `Label` and
`Note`) and says when the stored value fits none (`Custom: true`, the
value as written in `Value`); a setting may carry a longer `Note`; and each says where it is
stored (`Store`: `ConfigFile`, or `ServerConfiguration` for one the server configuration holds,
which applies at once, `Applies: Immediately`). Each setting also says
whether the Ferrofin settings page may change it, and when it may not, why: a flag or a variable
sets it (change those where the server is started), or it is one the web never changes (the
directories, the ffmpeg and ffprobe executables, the first-boot seed settings,
`disable_extensions`, `wasm_private_http_allow`, `image_private_http_allow`,
`subtitle_private_http_allow`), or one
jellyfin-web's own Networking and General pages cover. Secrets (the provider keys, the TheTVDB
PIN, the seed admin password) are reported in plain text, as Jellyfin's plugin settings are, and
never logged. The route belongs
to the **Ferrofin Settings** plugin under Dashboard → Plugins and answers `404` while that plugin is
disabled.

That plugin's settings page shows the report: open it from the plugin's Settings link or the
dashboard menu's **Ferrofin Settings** entry. It lists the settings in nine tabs (Network, Library,
Trickplay, Images, Providers, Performance, Logging, Plugins, System), the first open, in a pane
on the left (a row above the settings on a narrow screen). Each setting
is headed by a readable name, with its `config.toml` key, variable, flag and default on a small
line below, then its description. A badge beside the name says what sets it, when anything does
(`config.toml`, the Networking page, a variable or a flag), and secrets are marked as secrets.
Disabling the plugin hides the page and the menu entry.

### Editing settings from the dashboard

The same page edits the settings it may change, and saves them to `config.toml`: the file it
names at the top, chosen as [above](#configuration). When the server cannot write that file the
page says so beside its name, and a save fails with a message naming it.

Each setting is edited on one line: its name and description, with its field, its unit and
**Reset to default** beside them (below them on a narrow screen). The field shows the value
`config.toml` holds. When it holds none, the field shows the default, muted: as a number box's or
a text box's placeholder (a described default such as "8, capped at the cores" too), or as a
checkbox or an option set the way the default sets it. The field is the one for the setting's
kind: a checkbox for on or off, a number box with the setting's limits, an **Auto** checkbox with
a number box for a count that can be `auto`, a list of named options for a choice, a text box for
text, a URL or a path, and a text box with its items separated by commas for a list (so a list
here cannot hold a path with a comma in it). A secret is a plain text box, marked as a secret:
administrators see these values. A setting may carry a note longer than its description, shown
under it.

A choice (`log_format`: JSON or Text) shows the picked option's note below its row. A value
`config.toml` holds that fits no option (`log_format = "xml"`, which the server runs as JSON with
a warning) shows as **Custom** and stays as written: a save that does not pick another option
leaves it alone, and a save only takes one of the options.

A setting can also be stored in the server configuration that the Dashboard edits
(`GET /System/Configuration`) instead of `config.toml`. Such a setting is marked **Applies at
once**: Save writes it through the server's configuration and it applies with no restart, no
variable or flag sets it, and **Reset to default** puts back a new server's value. The two
[trickplay settings](#trickplay) are stored there.

**Reset to default** is active when `config.toml` holds a value or the field holds an edit. On a
value the file holds, it marks the key, and Save removes it, so the default applies after the
restart (**Undo reset** takes the mark back); on an edit alone, it drops the edit.

A search box and two filters sit above the tabs, since they reach every tab:

- the search matches a setting's name, description, `config.toml` key, variable and flag,
  ignoring case, and each word typed must match. It lists the matches of every tab, each tab's
  under its name with their count, and dims the tabs with none (so does **Changed**);
- **Changed** lists, from every tab, the settings something sets (a flag, a variable, the
  Networking page or `config.toml`) and those holding an edit not saved yet;
- **Hide read-only** leaves out the settings the page may not change, on a tab and in a list.

Opening a tab ends a search and **Changed**. Each tab shows how many edits it
holds that are not saved yet, and **Save** sends the changed settings of every tab at once
([`POST /Ferrofin/Settings`](#saving-from-the-web)). The server checks them, not the browser, and
a refused change shows the server's message next to the setting it names, with its tab marked and
opened (a search or **Changed** stays as it is), or at the top of the page when
it names none, such as a file that cannot be written. Nothing is saved then, and every edit stays
on the page to correct. Restarting with edits not yet saved asks first, since it drops them.

Flags and variables outrank `config.toml`, so a setting one of them sets has no field: the page
shows its value as text with a lock, and a note naming the flag or variable and where that is
changed. These have no field whatever sets them, each with its reason on the page:

- the directories (`data_dir`, `config_dir`, `cache_dir`, `web_dir`): changing one from the web
  could point the server at other files;
- `ffmpeg_path` and `ffprobe_path`: the web may not choose an executable the server runs;
- the first-boot seed settings (`library_roots`, `admin_user`, `admin_password`): they are used
  only when the database is created;
- `disable_extensions`: a measurement switch, set where a benchmark starts the server;
- `wasm_private_http_allow`: it lets the plugins it names reach private addresses, so it is set by
  hand in `config.toml` or its variable, and an admin session alone cannot grant it;
- `image_private_http_allow`: it lets image downloads reach the private addresses it lists, so it
  is set the same way, for the same reason;
- `subtitle_private_http_allow`: it lets OpenSubtitles download links reach the private addresses
  it lists, so it is set the same way, for the same reason;
- the settings jellyfin-web's own pages cover: `bind_addr`, `published_url`, `port`, `https_port`
  and `base_url` under Dashboard → Networking, `server_name` and `enable_metrics` under
  Dashboard → General;
- the OpenTelemetry variables, which `config.toml` has no key for.

A saved change [applies at the next restart](#when-a-change-applies). After a save the line below
the setting shows the saved value and the one in use, and a warning at the top of the page says that
a restart is required to apply the changed settings (it shows too while `config.toml` holds a value
edited by hand that is not in use yet). Its **Restart now** button runs the in-process restart, as
**Dashboard → Restart** does (`POST /System/Restart`), waits for the server to answer again and
shows the values now in effect. The settings the in-process restart keeps (`log_level`,
`log_format`, `metrics_scan_duration_buckets`) say so on their row before and after saving, and the
warning lists any waiting: they need a full process restart, so restart the container or service.
Once the restart has run and only those still wait, the warning has no **Restart now**, which
cannot apply them.

### When a change applies

A change to `config.toml` applies at the next restart, whether that is a restart of the server
process or the in-process restart that **Dashboard → Restart** (`POST /System/Restart`) and a
backup restore run: the in-process restart loads the configuration again. These settings are
held for the life of the process, so they need the process itself to restart:

- the directories: `data_dir`, `config_dir`, `cache_dir` (`web_dir` is not among them: the
  in-process restart serves the web client from its new value);
- `log_level` and `log_format` (logging is set up once per process);
- `metrics_scan_duration_buckets` (the metrics pipeline is set up once per process).

When one of them has changed, the in-process restart keeps its current value and logs a warning
naming it. When `config.toml` no longer loads (a syntax error, an invalid value), the in-process
restart logs an error naming the file and comes back on the configuration it was running; at a
process start the same file stops the server. Environment variables and flags are read from the
process, so a change to one applies only when the process restarts.

### Saving from the web

`POST /Ferrofin/Settings` (administrators only) saves changes to `config.toml`, and to the server
configuration for a setting stored there. The body is
`{"Changes": {"scan_probe_concurrency": 4, "log_level": null}}`: a value sets the key, and `null`
removes it, so the setting goes back to its default (a setting stored in the server configuration
gets a new server's value back). The save is all or nothing, and it only touches what it names:
comments, layout and every other key stay as they were, and a save that changes nothing stored in
the server configuration leaves it, and `GET /System/Configuration`, byte for byte as it was. A
removed key's line goes, with the comment at its end; the comment lines above it stay. An existing
file keeps its mode and, where the server may set them, its owner and group; a new one is created
`0600` (it may hold provider keys); and a linked file stays a link.

Every change is checked before anything is written, and a refused one answers `400` naming the
key: an unknown key, a setting that may not be changed here (with the reason
[the report](#seeing-what-is-set) gives, so a value a flag or a variable sets is refused, as one
saved underneath it would not apply), or an invalid value. The result is then loaded the way a
start loads it, under the server's environment and flags, before it replaces `config.toml`. Rules
that only warn at a start (`tvdb_cache_hours`, `tvdb_token_days`, `log_level`, `log_format`,
`metrics_scan_duration_buckets`, the provider time limits, `ffmpeg_analyze_duration`,
`ffmpeg_probe_size`) refuse the value here instead, so a
value the server would ignore is never saved. A URL setting must be an `http` or `https` URL, or
blank for its default, and is saved trimmed.

A file or directory that cannot be written answers `409` naming the path, and nothing changes.
That includes a read-only `config.toml` (`chmod a-w`), even where the directory would allow the
rename: the file is yours to lock. It also includes a `config.toml` that is itself a mount (one
file bound into a container), which cannot be renamed over: mount its directory instead. The
new `config.toml` is written and loaded before the server configuration is saved, and renamed into
place after, so a server configuration that cannot be saved leaves the file as it was too. Only a
rename that fails after the server configuration was saved (the file was found writable and
loaded just before) leaves that change applied and `config.toml` as it was. The
answer lists the `config.toml` keys saved (`Changed`), whether a restart is needed
(`RestartRequired`, only when one of those changed), which of them need the server process itself
restarted ([above](#when-a-change-applies), `ProcessRestartRequired`), and the keys stored in the
server configuration that changed and applied at once (`AppliedNow`). `GET /System/Info` reports
`HasPendingRestart: true` until the restart. The log names the keys, never their values. Like
the report, the route answers `404` while the **Ferrofin Settings** plugin is disabled.

## Paths

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_DATA_DIR` | `$XDG_DATA_HOME/ferrofin`, else `/var/lib/ferrofin` | Program-data root; derives the sub-dirs below and holds the database. |
| `FERROFIN_CONFIG_DIR` | `{data_dir}/config` | `system.json` + `branding.json`. |
| `FERROFIN_CACHE_DIR` | `{data_dir}/cache` | Cache root, including the transcode/segment cache. |
| `FERROFIN_WEB_DIR` | `{data_dir}/web` | Built jellyfin-web `dist/` served at `/web`. Absent → API-only. |
| `FERROFIN_CONFIG_FILE` | `{data_dir}/config.toml` | Path to the optional bootstrap `config.toml`. |

A relative data, configuration, cache or web directory (`--data-dir ./data`) is resolved against the working directory once, at startup, as Jellyfin does: the server uses, logs and reports the absolute path (keeping any `..`). A blank one stops the server, naming it.

The data, configuration and cache directories must be different directories: startup checks each against its Jellyfin marker file (`.jellyfin-data`, `.jellyfin-config`, `.jellyfin-cache`) and stops when two of them are one directory (see `docs/UPGRADING.md`).

The database file is `{data_dir}/ferrofin.db`. For drop-in adoption Ferrofin also opens a
Jellyfin `jellyfin.db` (root or `data/jellyfin.db`) found in the data dir — see [Migrating from Jellyfin](../README.md#migrating-from-jellyfin).

## Network & identity

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_BIND_ADDR` | `0.0.0.0` | Address the HTTP listener binds to. |
| `FERROFIN_PORT` | `8096` | HTTP port. It and the `--port` flag outrank **Local HTTP port number** on Dashboard → Networking, which outranks the `config.toml` key `port` unless it holds the default, `8096`. A change on the Networking page applies after a restart, and the dashboard shows the restart as pending. The container image sets `FERROFIN_PORT=8096`, so in a container the Networking page's port applies only once the variable is set blank (`FERROFIN_PORT=`), and the port mapping must follow it. |
| `FERROFIN_HTTPS_PORT` | `8920` | HTTPS port (parity value). Served beside the HTTP port when the dashboard's Networking settings enable HTTPS and the PKCS#12 (`.pfx`) certificate at their certificate path loads; see [HTTPS](#https). It outranks **Local HTTPS port number** on Dashboard → Networking, which outranks the `config.toml` key `https_port` unless it holds the default, `8920`. A change on the Networking page applies after a restart. When the Networking page gives both ports the same number, it sets neither (Jellyfin resets both to the defaults). |
| `FERROFIN_PUBLISHED_URL` | auto-detect | Public base URL advertised to clients. |
| `FERROFIN_BASE_URL` | none | URL path prefix the server is mounted under. It outranks **Base URL** on Dashboard → Networking, which outranks the `config.toml` key `base_url`. A blank value is set: it serves at the root and still outranks the Networking page. A change on the Networking page applies after a restart, and the dashboard shows the restart as pending. |
| `FERROFIN_SERVER_NAME` | host name | Server name reported to clients. |
| `FERROFIN_LOG` | `info` | Log filter, `RUST_LOG`/`EnvFilter` syntax; `RUST_LOG` wins when set. The `config.toml` key is `log_level`. |
| `FERROFIN_LOG_FORMAT` | `json` | Stdout log lines: `json` (one JSON object per line, with the request's `trace_id`, for a log pipeline) or `text` (human-readable, for running the server by hand). Any other value logs a warning and is `json`. The log file under `{data_dir}/log` is always plain text. The `config.toml` key is `log_format`. |
| `FERROFIN_QUICK_CONNECT_PENDING_PER_IP` | `10` | How many unauthorized Quick Connect requests one client IP may hold at once. `POST /QuickConnect/Initiate` needs no sign-in, so past this cap the IP's oldest pending request is dropped and its code stops working. Jellyfin has no cap. Behind a reverse proxy that is not in **Known proxies**, every client shares the proxy's IP, so the cap then counts all clients together. The cap is per address, so an IPv6 client can spread requests across its prefix. `1` to `1000` (a value outside logs a warning and is clamped); `0` means the default. The `config.toml` key is `quick_connect_pending_per_ip`. |
| `FERROFIN_DETECT_NETWORK_CHANGE` | `true` | Listen for the operating system's network changes (an address added or removed, a link or a route changed) and read the network interfaces again 2 seconds after each one, as Jellyfin's `DetectNetworkChange` does. The interfaces decide the server's own local URL (Live TV's stream URLs use it) and the addresses it advertises; a burst of changes is read once. Ferrofin keeps listening on the addresses it bound at startup, as Jellyfin does. On Linux this is a netlink socket, which needs no privileges and works in a container's own network namespace; on macOS a routing socket and on Windows an IP Helper notification. `false` reads the interfaces only at startup, at a network settings save and when a client's URL is worked out. Accepts `1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`. The `config.toml` key is `detect_network_change`. |
| `FERROFIN_LOGIN_BACKOFF` | `true` | Ferrofin's login backoff (Jellyfin has none): repeated failed logins make an account's and a client address's next attempts wait, as the four settings below describe. `false` turns it off. The `config.toml` key is `login_backoff`. |
| `FERROFIN_LOGIN_BACKOFF_ATTEMPTS` | `5` | Failed logins an account, or a client address, may make before its next attempts are made to wait. The first wait is 1 s, and each further failure doubles it up to `FERROFIN_LOGIN_BACKOFF_MAX_DELAY_SECS`. The backoff never disables the account. A successful login clears the account's count, not the address's. It covers `POST /Users/AuthenticateByName`, `POST /Users/{userId}/Authenticate` and the current password when a user changes theirs. Behind a reverse proxy that is not in **Known proxies**, every client shares the proxy's address, so one guesser's failures make every login through it wait, and can get them refused as wrong passwords. Add the proxy to Known proxies. The count is per address, so an IPv6 client can spread attempts across its prefix. An account is only made to wait, never refused, so the guesses one account can take grow with the number of addresses guessing it: about two a minute each at the defaults. 3 to 20; any other value stops the server. The `config.toml` key is `login_backoff_attempts`. |
| `FERROFIN_LOGIN_BACKOFF_MAX_DELAY_SECS` | `30` | The longest wait, in seconds, the login backoff gives an attempt. Attempts that arrive together queue one wait apart. From one address, an attempt that would wait longer than this is answered as a wrong password without its password being checked. An account is never refused: its attempts wait this long at most, so a caller guessing at an account cannot keep its owner out. 5 to 300; any other value stops the server. The `config.toml` key is `login_backoff_max_delay_secs`. |
| `FERROFIN_LOGIN_BACKOFF_WINDOW_MINS` | `15` | How long, in minutes, the login backoff remembers an account's or an address's failed logins after the last one, once its wait is over. 1 to 1440; any other value stops the server. The `config.toml` key is `login_backoff_window_mins`. |
| `FERROFIN_LOGIN_BACKOFF_MAX_TRACKED` | `100000` | The most accounts and addresses the login backoff remembers at once, which bounds its memory against a caller cycling through names or addresses: about 50 bytes a key, up to twice that between sweeps, so 5 to 10 MB at the default. Past it, the ones not yet made to wait are forgotten first. 1000 to 10000000; any other value stops the server. The `config.toml` key is `login_backoff_max_tracked`. |
| `FERROFIN_SESSION_IDLE_HOURS` | `24` | How long, in hours, a session with no WebSocket that plays nothing is kept after its last request. Once a minute the server ends every such session idle for longer, as when a session's last WebSocket closes: it leaves `GET /Sessions`, and for a signed-in user the activity log notes the disconnect. A client that returns gets a new session with the same id and its device's reported capabilities, so only what the ended one held (additional users, the item it was viewing, its last queue and play state) starts over, and for a signed-in user the activity log notes it as online again. An ended session also stops counting toward a user's maximum number of active sessions, which Jellyfin counts until it restarts. Jellyfin keeps such a session until the server restarts, so a client that sends a new device id on every run would grow memory without end. jellyfin-web's dashboard already hides sessions idle for more than 16 minutes. `1` to `168` (a value outside logs a warning and is clamped). The `config.toml` key is `session_idle_hours`. |

### HTTPS

Ferrofin serves HTTPS itself, as Jellyfin does, from the dashboard's **Networking** settings
(`{config}/users/named/network.json`):

- **Enable HTTPS** with a **Custom SSL certificate path** to a PKCS#12 (`.pfx`/`.p12`) file holding
  the certificate and its private key, plus its password if it has one. At startup the server
  listens with HTTPS on its HTTPS port on every address it listens with HTTP on.
  If the certificate does not load, the server logs why and serves HTTP only.
  A certificate without a password must use the usual empty-password form, as OpenSSL writes
  it. The rarer NULL-password form, which .NET loads, does not load yet (gl-815). To rewrite
  a password-less file in the usual form:
  `openssl pkcs12 -in old.pfx -passin pass: -nodes -out all.pem`, then
  `openssl pkcs12 -export -in all.pem -passout pass: -out new.pfx` (delete `all.pem` after:
  it holds the private key unencrypted).
- HTTPS connections use HTTP/1.1; Jellyfin's HTTPS endpoints also offer HTTP/2 (gl-814).
- **Require HTTPS** answers every plain HTTP request with a `307` redirect to the same path on
  HTTPS, except `/health/live` and `/health/ready`. A request a known proxy forwarded with
  `X-Forwarded-Proto: https` counts as HTTPS.
- A new certificate path takes effect after a restart; the dashboard shows the restart as
  pending. A path to a missing file saves but is answered `404`.
- The ports are the Networking page's **Local HTTP port number** and **Local HTTPS port
  number**, below `FERROFIN_PORT` (or `--port`) and `FERROFIN_HTTPS_PORT` (see the table above).
  An adopted Jellyfin install keeps its ports from its first boot, unless a flag or a variable
  pins them (the container image sets `FERROFIN_PORT`). When a variable, a flag or
  `config.toml` leaves both on the same port, HTTPS is not served and the log says so.
- Adoption keeps Jellyfin's **Enable HTTPS** and **Require HTTPS**, but not its certificate
  path and password, which point into Jellyfin's filesystem.

A reverse proxy that terminates TLS needs none of this. List it under **Known proxies** instead.
Addresses published from each request (**Enable published server URI by request**) take the
proxy's `X-Forwarded-Proto` and `X-Forwarded-Host`.

## Admin seeding (fresh database only)

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_ADMIN_USER` | `admin` | Username of the admin seeded on first boot. |
| `FERROFIN_ADMIN_PASSWORD` | none (passwordless) | Admin password. Set it for a headless install. Unset, the admin is seeded **without a password** (as in Jellyfin) and signs in with an empty one until the setup wizard at `/web` sets it — finish the wizard before exposing the server. |

## ffmpeg

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_FFMPEG_PATH` | auto-discovered on `$PATH` | Path to `ffmpeg`. Absent ffmpeg only disables transcode. |
| `FERROFIN_FFPROBE_PATH` | auto-discovered on `$PATH` | Path to `ffprobe` (scan probes). |
| `FERROFIN_FFMPEG_IMG_EXTRACT_PERF_TRADEOFF` | `false` | Jellyfin's `FFmpeg:imgExtractPerfTradeoff`: trade frame quality for speed when extracting chapter images, video thumbnails and embedded pictures. On, the first attempt decodes keyframes only and takes the first one, instead of picking the most representative of 24 frames; extraction is faster, and the image may be less typical of the scene. The `config.toml` key is `ffmpeg_img_extract_perf_tradeoff`. |
| `FERROFIN_FFMPEG_ANALYZE_DURATION` | `200M` | Jellyfin's `FFmpeg:analyzeduration`: how long, in microseconds, ffmpeg and ffprobe analyse an input to find its streams when its source names no duration of its own (a Live TV channel's 3 seconds wins). It applies to every probe, scans included, to the media file of every transcode, and to an external subtitle a transcode reads. The value is in ffmpeg's syntax, as Jellyfin takes it: digits, then `K`, `M` or `G` for powers of 1000, or `Ki`, `Mi` or `Gi` for powers of 1024, so `200M` is 200 seconds. A smaller value starts a transcode of a slow or remote source (a `.strm` URL) sooner, because ffmpeg reads the input before the first segment on every job start, a restart after an idle job is ended included; it can also miss a stream that starts late, and a scan keeps what its probe found. `1M` to `600M` (a value outside logs a warning and is clamped; one that does not parse logs a warning and is ignored). Blank is the default (for a blank value Jellyfin drops the flag; Ferrofin does not); `5M` is ffmpeg's own default. The `config.toml` key is `ffmpeg_analyze_duration`. |
| `FERROFIN_FFMPEG_PROBE_SIZE` | `1G` | Jellyfin's `FFmpeg:probesize`: how many bytes of an input ffmpeg and ffprobe read to find its streams, on the same inputs as `FERROFIN_FFMPEG_ANALYZE_DURATION`, in the same syntax, with the same trade-off. `1M` to `4G` (a value outside logs a warning and is clamped; one that does not parse logs a warning and is ignored). Blank is the default (for a blank value Jellyfin drops the flag; Ferrofin does not); `5M` is ffmpeg's own default. The `config.toml` key is `ffmpeg_probe_size`. |
| `FERROFIN_MAX_TRANSCODES` | `0` | The most streaming jobs that re-encode the whole server runs at once, `0` to `1024` (a value outside logs a warning and is clamped). `0` means no cap. A job counts when it re-encodes any stream, video or audio, so a copied video with re-encoded audio counts and so does a music transcode; only a job copying every stream, a direct stream or remux, is free. It counts until its ffmpeg exits. Trickplay, chapter images, recordings and library analysis are other work and are not counted. Past the cap, a new job of a signed-in user first ends an idle job of that user on the same device (see `FERROFIN_MAX_TRANSCODES_PER_USER`). If that makes no room the request is refused with `503` and `Retry-After: 10`, before anything is started, and the stream can play once a job ends. Refusals log at `debug`. A job started with an API key counts here, and has no user whose jobs could be ended. Jellyfin has no such cap. The `config.toml` key is `max_transcodes`. |
| `FERROFIN_MAX_TRANSCODES_PER_USER` | `0` | The most streaming jobs that re-encode one signed-in user runs at once, `0` to `1024`, counted as for `FERROFIN_MAX_TRANSCODES`. `0` means no cap. The user comes from the sign-in, never the request's `userId`, so accounts shared between devices share this cap. An API key has no user and is outside it. Past the cap, the user's idle jobs on the same device are ended first, longest idle first and no more than needed, so "next episode" works while the last episode's job waits to be reaped; if none is idle the request is refused with `503` and `Retry-After: 10`. Idle means no request is reading from the job at that moment. The device is the request's `DeviceId`, which the stream URL from playback info carries from the client's sign-in; a request without one ends nothing. Another device's job is never ended, since a player between two segment requests is idle too: with a cap below the number of devices playing on one account, the next device is refused until a stream ends (a stopped one at once, an abandoned one when it is reaped, about a minute for HLS). Players that share one `DeviceId`, such as two tabs of one browser, count as one device and can still end each other's idle job. Jellyfin has no such cap. The `config.toml` key is `max_transcodes_per_user`. |

## Library & database

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_LIBRARY_ROOTS` | none | Folders a fresh install starts with as libraries, separated by the OS path-list separator (`:` on Linux); in `config.toml`, `library_roots` is an array of paths. On the boot that creates the database's administrator, each folder becomes a library named after it (`/media/Movies` → `Movies`; a name already taken gets a number, `Movies2`), with no content type (mixed content) and Jellyfin's default library options (real-time monitoring off), both changeable in the dashboard; then one library scan starts in the background. A folder that does not exist, is not a directory or is already a library's location is skipped with a warning. Every later boot, and an adopted Jellyfin database, ignores the setting, so a library removed in the dashboard does not come back. |
| `FERROFIN_DB_POOL` | `auto` | SQLite connection count, or `auto` (sizes to cores — the measured optimum). |
| `FERROFIN_DB_STATEMENT_CACHE` | `150` | How many prepared statements each SQLite connection keeps, in the reader and writer pools alike. A statement the cache no longer holds is prepared again the next time it runs. Every connection keeps its own statements, so a larger cache costs memory on each connection of the pool. The `IN (…)` lists are padded to a few lengths, so a browse of a library of 3,000 movies, 250 series and 800 albums runs 146 distinct read statements: on an 8-core server, 150 holds them all, so a repeat browse prepares none again, for 6 to 13 MiB more than 100. `100` to `1024`. The `config.toml` key is `db_statement_cache`. |
| `FERROFIN_ITEM_VALUES_WRITE_CHUNK` | `500` | How many items' genre, studio, tag and artist links one database transaction rewrites when a metadata edit saves many items at once: a lock set on a folder reaches every item under it, and a series', season's or album's tags and ratings reach its seasons, episodes or tracks. The server has one database writer, so a smaller value holds it for shorter stretches (other writes, such as playback progress, wait behind it) and a larger one finishes a big edit in fewer transactions. `1` writes each item's links in its own transaction, as Jellyfin's per-item saves of such an edit do. `1` to `5000` (a value outside logs a warning and is clamped). The `config.toml` key is `item_values_write_chunk`. |
| `FERROFIN_SCAN_PROGRESS_EVERY` | built-in | Items between scan-progress log lines, `0` to `100000` (a value outside logs a warning and is clamped); `0` turns them off. |
| `FERROFIN_PLAYBACK_METRICS_QUEUE` | `1024` | Depth of the playback-metrics write queue, in events. Each PlaybackInfo decision and playstate start/stop is handed to this queue and written by one background task, so a SQLite WAL checkpoint can never stall a playback request. A full queue drops the event with a warning rather than blocking — raise it if `playback-metrics queue full` appears in the log under a load spike. `64` to `65536` (a value outside logs a warning and is clamped); `0` means the default. Whatever this is set to, the rows the queue writes (one per decision, with its user and device) expire with the activity log: the "Optimize database" task deletes those older than the server configuration's `ActivityLogRetentionDays` (30 days by default; unset keeps them). |
| `FERROFIN_SCAN_PROBE_CONCURRENCY` | `8` (capped at cores) | ffprobe processes a library scan keeps in flight. The probe is the bulk of scan time, so this is the scan's main throughput knob: measured ~3.7× at `4` and ~10× at `16` over serial probing on a local SSD. Keep it low on a spinning disk or a network mount, where a wide window turns sequential reads into seek thrash. `1` to `64` (a value outside logs a warning and is clamped); `0` means the default. |
| `FERROFIN_SCAN_DIRECTORY_CACHE_RECORDS` | `100000` | How many directory-listing records one library scan keeps (a folder counts each entry plus one). A scan lists each folder once and every item in it reads that listing; once the scan holds this many records it drops them all and lists folders again as it reaches them. This bounds the memory a huge library's scan spends on listings. The default is Jellyfin's (`DirectoryService.MaxCachedRecords`). `1000` to `10000000` (a value outside, `0` included, logs a warning and is clamped). The `config.toml` key is `scan_directory_cache_records`. |
| `FERROFIN_AUTH_CACHE_TTL_SECS` | `30` | Seconds a resolved access token (its device, user and policy, or an API key's name, or the rejection of a token that matched nothing) is served from memory before the database is read again. Every change made through the server applies at once; this only bounds how long a change made outside it, in the database itself, goes unseen. `5` to `300`. The `config.toml` key is `auth_cache_ttl_secs`. |
| `FERROFIN_ROW_CACHE_TTL_SECS` | `30` | Seconds the per-user access check serves a user's permissions and preferences, the libraries' rows and options, and the views' and the playlists folder's rows, from memory. Queries scoped to the user's libraries, the home screen's views and every lookup of a library's physical folders read the same cached rows and options. Whether a Live TV tuner is configured, which decides whether Live TV is offered, is cached the same way. As for the auth cache, every change made through the server applies at once, and this only bounds changes made outside it (the database, a library's `options.json`). `5` to `300`. The `config.toml` key is `row_cache_ttl_secs`. |
| `FERROFIN_SCAN_DIRECTORY_CACHE_IDLE_SECS` | `300` | Seconds a library scan's directory listings survive unread before they are dropped. The default is Jellyfin's (`DirectoryService.IdleTimeoutMs`). `30` to `3600` (a value outside, `0` included, logs a warning and is clamped). The `config.toml` key is `scan_directory_cache_idle_secs`. |
| `FERROFIN_BOOK_ARCHIVE_MAX_ENTRIES` | `100000` | The most entries a comic archive (`.cbz`, `.cb7`, `.cbt`) may hold for its pages to set the book's runtime. A book's runtime is its pages × 10000 ticks, which is what jellyfin-web's comic and PDF readers report as the position: a comic counts its archive's entries other than folders, a PDF its pages, as Jellyfin does. The count reads only the archive's directory, never a page; an archive with more entries than this is not counted and its book keeps the runtime it has (one second for a new one). Every entry counts towards the limit, folders included. Jellyfin has no such limit. A book is counted when it is added, when its file changes, when its metadata is refreshed ("Search for missing metadata" or "Replace all metadata"), and on any scan while a `.cbz`, `.cb7`, `.cbt` or PDF still has the one-second runtime (as a book added by an older release does); a counted book whose file has not changed is not opened again, while one whose pages cannot be counted is opened again on each scan but not saved. `1000` to `10000000` (a value outside, `0` included, logs a warning and is clamped). The `config.toml` key is `book_archive_max_entries`. |
| `FERROFIN_TRICKPLAY_CONCURRENCY` | `auto` | How many videos generate trickplay images at once (each one's selected widths in turn: for each, an ffmpeg frame extraction, then tile packing). A video keeps its place until all of its widths are made, so with **Both** selected a scan makes each video's 320 and 640 sets together. A library scan generates several at once; the **Generate Trickplay Images** task goes one item at a time but shares this cap when it overlaps a scan. `auto` allows as many as the image-encoding limit: **Parallel image encoding limit** on the dashboard (`ParallelImageEncodingLimit`), else one per CPU. An integer sets a lower cap; `0`, a negative number, or a value above the auto value means `auto`. Each generation is an ffmpeg process with its own memory, and with hardware acceleration its own GPU memory (about 1 GiB for a 4K HEVC video on an NVIDIA GPU). Packing its tiles then takes about 60 MiB of server memory at the default tile grid, at 320 or 640, because a wider set packs fewer previews into each tile ([Trickplay](#trickplay)). Jellyfin generates one at a time; `1` does the same. The `config.toml` key `trickplay_concurrency` takes an integer or `"auto"`. |

## Trickplay

The **Trickplay** tab of the Ferrofin settings page holds two settings stored in the server
configuration: the `TrickplayOptions` that **Dashboard → Playback → Trickplay** edits. A save on
either page shows on the other and applies at once, with no restart. No variable, flag,
`config.toml` key or Helm value sets them. A new install starts on 320 and Optimize Disk Usage,
Jellyfin's own defaults, and an upgraded or adopted server keeps the values it has, so better
images are always an admin's choice. The tab's third setting, how many videos generate trickplay
images at once, is `FERROFIN_TRICKPLAY_CONCURRENCY` under [Library & database](#library--database).

| Setting | Default | Purpose |
|---|---|---|
| Preview Size | 320 | How wide the scrubbing previews are, in pixels: `WidthResolutions` `[320]`, `[640]`, or `[320, 640]` (**Both**, in either order). 320 is fast to fetch but small on a 4K screen. 640 is sharper on 4K, and phones and 1080p screens get a large preview too. Both makes the two, so each client picks by its screen, at two ffmpeg passes per video and the disk of both. Packing a tile takes about 60 MiB of server memory at either width, at the default tile grid. |
| Image Quality | Optimize Disk Usage | How much colour detail the previews keep: `Qscale` and the Ferrofin-only `FullChroma`. **Optimize Disk Usage** is Jellyfin's 4:2:0 at `Qscale` 4, with blocky colour around edges; **Optimize Quality** is 4:4:4 at `Qscale` 2, with full colour detail, and takes as long to generate. A video that trickplay's hardware acceleration (**Enable hardware decoding** and **Enable hardware accelerated MJPEG encoding** on the dashboard's Trickplay page) decodes or encodes on the GPU stays 4:2:0, so it costs the disk without the colour; one the GPU neither decodes nor encodes (a source it cannot decode, with a software MJPEG encoder) gets 4:4:4. `FullChroma` is left out of `GET /System/Configuration` while it is off, so the document is Jellyfin's unless an admin picks Optimize Quality. |

A value the dashboard holds that fits no option (a width typed by hand, a `Qscale` changed alone)
shows as **Custom**, and a save leaves it alone until an admin picks an option for that setting.
A change takes full effect after the next **Generate Trickplay Images** run, which replaces each
video's images that no longer match, keeping the old ones until the new ones are in place. That
run decodes every video again in the libraries with trickplay images on, at below-normal priority,
so a large library can take days. A hand edit of `Qscale` or JPEG quality on the dashboard is not
picked up that way and needs **Replace existing trickplay images**, as in Jellyfin. So does
picking an Image Quality option over a Custom value that differs from it only in `Qscale`.

Wider sets get a smaller tile grid, so a tile holds no more pixels than a 320 tile (until the grid
is down to 1 x 1). **Tile width** and **Tile height** on the dashboard's Trickplay page
(`TileWidth`, `TileHeight`, 10 x 10 by default) are the grid at 320 and below. Above 320, each is
multiplied by 320 / width, rounded down, at least 1: at the default, 480 gets 6 x 6, 640 gets 5 x 5
and 1280 gets 2 x 2. A client decodes a whole tile to show one preview, and a 640 tile at 10 x 10
is 6400 x 3600, about 92 MB decoded: more than a client may keep in its image cache. Wholphin was
seen decoding it again on every scrub, showing a black box meanwhile. Each tile now covers less
video, so a client fetches more of them, for about the same bytes in all. Jellyfin uses the
configured grid at every width. The next **Generate Trickplay Images** run replaces a set made at
another grid (by an earlier version, or by Jellyfin) with one at the new grid, so the first run
after upgrading decodes again every video with a set wider than 320.

Disk per hour of video, with the default 10-second interval and tile grid. The grid does not
change the bytes: re-packing real 640 tiles from 10 x 10 into 5 x 5 changed their size by less
than 0.1 %.

| Width | Optimize Disk Usage | Optimize Quality |
|---|---|---|
| 320 | 2.4 to 3.2 MiB | 3.6 to 4.7 MiB |
| 640 | 6.0 to 7.7 MiB | 10.2 to 13.0 MiB |
| Both | 320 and 640 together | 320 and 640 together |

Measured with the server's own writer and the release image's jellyfin-ffmpeg 8.1.3, software
decoding, on 10-minute excerpts of three films scaled to an hour: a 4K HDR HEVC Blu-ray at 2.40:1,
a 4K HDR AV1 web download at 16:9 (both tone-mapped) and a 1080p SDR HEVC Blu-ray. Another film
will land somewhere else, and a busier one higher. With **FFmpeg Threads** (`ProcessThreads`) at
4, a pass at one width over the three excerpts together (30 minutes of video) took about 4 minutes
on a running server: at 320, Optimize Disk Usage took 238 and 223 seconds and Optimize Quality
228; at 640, Optimize Disk Usage took 263; and both widths at Optimize Quality took 443 together
(640 at Optimize Quality was not timed alone). The first pass after the server started (320,
Optimize Disk Usage) took 358 seconds. FFmpeg Threads defaults to 1, so a stock server's times
will differ.

## Observability

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_ENABLE_METRICS` | `false` | Serve Prometheus `/metrics`. Off → the route is absent. |
| `FERROFIN_METRICS_SAMPLE_INTERVAL` | built-in | Seconds between process-metric samples, `1` to `300` (a value outside logs a warning and is clamped); `0` means the default. |
| `FERROFIN_METRICS_SCAN_DURATION_BUCKETS` | 1–2.5–5 per decade, `0.001` … `5000` | Bucket boundaries, in seconds, of the library-scan duration histograms (`ferrofin_library_scan_duration_seconds`, `ferrofin_library_scan_pass_duration_seconds`). The env var is comma-separated (`0.01,0.1,1,10,60,300,1800,7200`); the `config.toml` key `metrics_scan_duration_buckets` is an array (`[0.01, 0.1, 1, 10]`). A list that is empty, non-numeric, non-finite, not above zero or not strictly increasing is refused with a warning at startup and the default is used. |
| `FERROFIN_SHUTDOWN_TIMEOUT_SECS` | `30` | Seconds a graceful shutdown or in-process restart (`POST /System/Restart`, backup restore) waits for in-flight requests before aborting the remaining connections — ASP.NET's `ShutdownTimeout` default. `5` to `600` (a value outside logs a warning and is clamped). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | none | Environment only. Enable OTLP trace export (gRPC) to this endpoint (off by default). An `https://` endpoint, or a bare `host:port` unless `OTEL_EXPORTER_OTLP_INSECURE` (or `OTEL_EXPORTER_OTLP_TRACES_INSECURE`) is `true`, is reached over TLS, and the collector's certificate is checked against the system trust store; `SSL_CERT_FILE` or `SSL_CERT_DIR` replace that store. The startup log says whether export is on, or why the exporter could not be built. |
| `OTEL_TRACES_SAMPLER_ARG` | `0.25` | Environment only. Share of traces exported when OTLP export is on, `0` to `1`; a value outside is clamped, and one that is not a number uses the default. |

The `OTEL_*` variables are OpenTelemetry's standard ones, which every OpenTelemetry SDK reads, so
they are environment only on purpose: they have no `config.toml` key. A change to them applies
when the server process restarts.

## Provider request limits

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_PROVIDER_TIMEOUT_SECONDS` | `20` | Default per-attempt HTTP timeout, clamped to 10–60 seconds (a value outside logs a warning). Explicit request timeouts take precedence. The `config.toml` key is `provider_timeout_seconds`. |
| `FERROFIN_PROVIDER_MAX_WAIT_SECONDS` | `60` | Maximum pending pacing/quota wait before skipping a lookup, clamped to 30–300 seconds (a value outside logs a warning). Open circuits skip immediately until a probe is permitted. The `config.toml` key is `provider_max_wait_seconds`. |

A change applies at the next restart, the dashboard's in-process restart included.
See [provider rate limiting](PROVIDER_RATE_LIMITING.md) for retry and cooldown policy.

## Image limits

What an image costs in memory depends on its dimensions, not its file size, so these bound what
one image can cost. They cover every image Ferrofin turns into pixels: thumbnails, blur
hashes, photos, trickplay frames, collages and the splashscreen. An image over a limit is never
an error for a client: it gets the original file instead of a thumbnail. An image too large to
decode also has no blur hash, and a collage or the splashscreen leaves it out. Reading an image's
dimensions decodes nothing, so it needs no limit.

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_IMAGE_DECODE_MAX_MEGAPIXELS` | `200` | The largest image Ferrofin decodes, in megapixels (width × height), clamped to `50`–`1000` (a value outside logs a warning). The file's header gives the size, so a larger image costs no memory. The `config.toml` key is `image_decode_max_megapixels`. |
| `FERROFIN_IMAGE_DECODE_MAX_MEMORY_MB` | `512` | The memory one decode may take, in MiB, clamped to `256`–`2048` (a value outside logs a warning). An image takes its width × height × its bytes per pixel, which is 3 for an ordinary colour photo, so the default stops those at about 178 megapixels. Each decode has its own allowance, so the most that decoded images can hold together is this times the images being worked on at once: one per CPU at most, and for thumbnails **Parallel image encoding limit** on the dashboard (`ParallelImageEncodingLimit`) when that is lower. The `config.toml` key is `image_decode_max_memory_mb`. |
| `FERROFIN_IMAGE_RESIZE_BUDGET_MB` | `2048` | The working memory all resizes in flight share, in MiB, clamped to `512`–`8192` (a value outside logs a warning). A resize takes 16 bytes per pixel of the source's width by the output's height, whatever the file's format: a thumbnail 267 pixels tall of an image 6000 pixels wide takes about 24 MiB, and a 2160-pixel-tall view of an image 7680 pixels wide about 253 MiB. The default holds about 13000 by 10000. An image request's `backgroundColor`, `blur` or `foregroundLayer` shares it too, at 4 bytes per output pixel, 3 more when the output is a JPEG, and for a blur 64 per pixel of the output's longest side. The memory an encoder takes to write the file is outside the budget, for every image. Work that alone needs more serves the original file; work that fits but not beside the others waits for them. The `config.toml` key is `image_resize_budget_mb`. |

Raise a limit for photos or panoramas that come back at full size. Lower them on a host with
little memory: under a container's memory limit, keep the budget and the decode limit well below
it, or the server can reach the limit first. At log level `debug`, a thumbnail refused names the
image and the setting that refused it. A change applies at the next restart, the dashboard's
in-process restart included.

## Remote images

Artwork that lives at a URL, such as a channel logo from an M3U playlist, is downloaded the first
time a client asks for it. These settings bound what that can cost, and where a download may go.
One URL is downloaded by one request at a time, and the others wait for the result. As in
Jellyfin, a download never follows a redirect from https to http (logged at `debug`). It follows
at most 10 redirects, where Jellyfin follows 50.

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_IMAGE_DOWNLOAD_MAX_MB` | `64` | The largest remote image Ferrofin downloads, in MiB, clamped to `16`–`256` (a value outside logs a warning). A response that declares more is refused unread, and a read stops at the limit whatever the response declared. The limit covers everything downloaded from a metadata provider: channel logos, scan artwork, the **Download** button in the image editor, an LDAP user's picture given as a URL, and every provider's answer (TMDb, TheTVDB, OMDb, fanart.tv, TheAudioDB, MusicBrainz, ListenBrainz, LRCLIB, OpenSubtitles and the Studio Images list). For those answers this setting is only the size limit: the private-address rule of `FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW` applies to image downloads alone, and an OpenSubtitles download link follows its own, `FERROFIN_SUBTITLE_PRIVATE_HTTP_ALLOW`. The scan moves on to the provider's next image, saying so at log level `debug`. Each request of a download, each redirect it follows counting as one, also has to finish within `FERROFIN_PROVIDER_TIMEOUT_SECONDS`, so raising the limit may need that raised too. The `config.toml` key is `image_download_max_mb`. |
| `FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW` | none | The private hosts and networks image downloads may reach, comma-separated: host names (`nas.lan`), addresses (`192.168.1.20`, `fd00::5`) and networks in CIDR form (`192.168.1.0/24`). Every image download (channel and programme artwork, scan artwork, studio thumbs, the **Download** button in the image editor, an LDAP user's picture given as a URL) refuses loopback, private, link-local (cloud metadata services among them), CGNAT and reserved addresses, on its first request and on every redirect, unless this lists the host or its address, or the host is a configured Live TV tuner's, so tvheadend's `/imagecache/` logos and an HDHomeRun's keep working without it. A tuner's host is allowed by its name, on every port and path, whatever led there and at whatever address the name resolves to: a service on the same machine as a tuner is reachable too, and so is any address a tuner whose name's DNS someone else controls (a remote IPTV playlist, say) points that name at. A host name allows that host at any address; a network allows every address in it, and `0.0.0.0/0, ::/0` every address there is. An entry that is none of these logs a warning and allows nothing. The first refused download logs a warning naming this setting, and later ones log at `debug`. A guide's (XMLTV) host is not a tuner's: list it here when its artwork is on your network; list a host serving LDAP users' pictures here too (LDAP-Auth's `Url` profile image format, which Jellyfin fetches from anywhere). Behind an HTTP proxy (`HTTP_PROXY`, `HTTPS_PROXY` or `ALL_PROXY`), the server checks a host on its own DNS answer before the request, so it must be able to resolve it, and the proxy, which resolves the host again, is trusted to reach the same address. After a redirect, a response that names no image type is refused, where the URL's file extension names it otherwise (a tvheadend `/imagecache/` URL still counts as PNG, as without a redirect). Jellyfin checks none of this. The settings page shows it but does not change it: set it in `config.toml` or the variable. The `config.toml` key is `image_private_http_allow`. |
| `FERROFIN_IMAGE_NEGATIVE_CACHE_TTL_SECS` | `3600` | How long a logo URL that gave no image is left alone before it is tried again, in seconds, clamped to `300`–`86400` (a value outside logs a warning). That is a `404` or `403`, a response that is not an image, an empty one included, one over the limit, or one on a private address image downloads may not reach (`FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW`), which stays refused until the time is up even if a tuner on that host is added meanwhile. A connection failure, a timeout, another status or a provider cooldown is not remembered, so it cannot hide a good image. Until the time is up the client gets a `404`, and Ferrofin keeps the image's row, where Jellyfin removes it. The memory is held per process and a restart clears it. The `config.toml` key is `image_negative_cache_ttl_secs`. |

An image must look like one, whatever the response declares: JPEG, PNG, GIF, WebP, BMP, TIFF, ICO,
AVIF, HEIC, JPEG XL, JPEG 2000 or uncompressed SVG. Anything else is refused and never stored. A
change applies at the next restart, the dashboard's in-process restart included.

## Remote metadata provider keys

Every remote provider is compiled in and enabled per library through the fetcher
checkboxes, as in Jellyfin. TMDB, OMDb, TVDB, fanart.tv and TheAudioDB ship with
Jellyfin's shared keys. The settings below are optional overrides or additions.

| Variable | Purpose |
|---|---|
| `FERROFIN_OMDB_KEY` | Optional OMDb API key override; unset or blank uses Jellyfin's shared key. The `config.toml` key is `omdb_api_key`. |
| `FERROFIN_TVDB_KEY` | Optional TheTVDB API key override; unset or blank uses Jellyfin's shared key. The `config.toml` key is `tvdb_api_key`. |
| `FERROFIN_TVDB_PIN` | Optional TheTVDB subscriber PIN, for a paid subscription. The `config.toml` key is `tvdb_subscriber_pin`. |
| `FERROFIN_TVDB_CACHE_HOURS` | How long TheTVDB lookups are reused, in whole hours (default `1`, the TVDB plugin's `CacheDurationInHours`): an episode's TVDB id, and the season list of a series a season's TVDB id is read from. Within one library scan it also bounds how long a series' TheTVDB record and an episode's TheTVDB still are kept for the items after them: after that, the series' image pass and the episode's read them from TheTVDB again. A longer duration saves requests when a library is refreshed again within that time. The `config.toml` key is `tvdb_cache_hours`. `1` to `168`: a value outside logs a warning at startup and is clamped, and one that is not a whole number logs a warning and the default is used. |
| `FERROFIN_TVDB_TOKEN_DAYS` | How old TheTVDB's sign-in token may get before Ferrofin signs in again, in whole days (default `25`, the TVDB plugin's value). TheTVDB expires a token after a month, so the value must be from 1 to 29. The `config.toml` key is `tvdb_token_days`. Any other value is refused with a warning at startup and the default is used. |
| `FERROFIN_FANART_KEY` | Optional fanart.tv personal key, sent alongside the shared project key. The `config.toml` key is `fanart_personal_api_key`. |
| `FERROFIN_MUSICBRAINZ_URL` | MusicBrainz base URL override (self-hosted mirror). The `config.toml` key is `musicbrainz_base_url`. |
| `FERROFIN_STUDIOS_REPO_URL` | Studio-images repo URL override. |
| `FERROFIN_SUBTITLE_PRIVATE_HTTP_ALLOW` | The private hosts and networks an OpenSubtitles download link may reach, comma-separated, in the form `FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW` takes: host names (`mirror.lan`), addresses (`192.168.1.20`, `fd00::5`) and networks in CIDR form (`192.168.1.0/24`). Default: none. A subtitle download fetches the link OpenSubtitles answered with, and refuses loopback, private, link-local (cloud metadata services among them), CGNAT and reserved addresses, on the link and on every redirect from it, unless this lists the host or its address; the user gets an error naming this setting, and the first refusal logs a warning. `FERROFIN_IMAGE_PRIVATE_HTTP_ALLOW` does not apply to it, nor does a Live TV tuner's host. Behind an HTTP proxy (`HTTP_PROXY`, `HTTPS_PROXY` or `ALL_PROXY`), the server checks the host on its own DNS answer before the request, as for images, so it must be able to resolve the download host itself: where only the proxy can, subtitle downloads now fail. The OpenSubtitles plugin checks none of this. The settings page shows it but does not change it: set it in `config.toml` or the variable. The `config.toml` key is `subtitle_private_http_allow`. |

TMDB's `TmdbApiKey` plugin setting overrides its shared key. OpenSubtitles uses
Jellyfin's shared application key with the username/password configured in its
plugin settings; its `ApiKey` field is an optional override. The shared key does
not replace the account needed for subtitle downloads and for the subtitle
searches a scan or the scheduled task makes.

## Extensions

| Variable | Default | Purpose |
|---|---|---|
| `FERROFIN_DISABLE_EXTENSIONS` | `false` | Register none of the compiled-in extensions (Intro Skipper, File Transformation, Merge Versions, LDAP-Auth): their plugins, scheduled tasks and event hooks are gone. Meant for measurement, so a benchmark compares Ferrofin with a Jellyfin that has no plugins installed. The **Ferrofin Settings** plugin stays: it is the server's own settings, with no task or hook. Accepts `1`/`0`, `true`/`false`, `yes`/`no`, `on`/`off`. |

## WASM plugins (Tier 1b)

Limits for sandboxed WASM plugins loaded from `{data_dir}/plugins/*.wasm`
(see `EXTENSIONS.md`). Each takes the range its row names: a value outside logs a warning at startup
and is clamped. Zero is treated as unset; defaults apply.

| Variable | Purpose |
|---|---|
| `FERROFIN_WASM_CALL_TIMEOUT_SECS` | Per-guest-call deadline in seconds (default 30, `1` to `600`). A plugin call past the deadline is interrupted; repeated failures sideline the plugin until restart. The deadline covers the host functions the plugin calls too: `http-fetch`, the media extractions (`extract-audio`, `extract-frames`, `extract-subtitle-track`, including the wait for a decode slot) and the library reads (`query-items`, `next-up`, `media-info`) get only the time left in the call, and one cut at the deadline fails the call like any other overrun. Library writes run to completion. The `config.toml` key is `wasm_call_timeout_secs`. |
| `FERROFIN_WASM_CALL_GRACE_SECS` | How long, in seconds, a caller waits past a plugin call's deadline before the call counts as stuck (default 10, `2` to `600`). A stuck call, one blocked in a host function that does not end with the deadline, disables the plugin until restart: its caller and every call queued behind it fail, and the stuck thread is abandoned. The wait starts when the call starts running, not when it is queued. Loading a plugin at startup and validating one at install are timed the same way: a stuck plugin is skipped at startup and refused at install. The `config.toml` key is `wasm_call_grace_secs`. |
| `FERROFIN_WASM_MEMORY_LIMIT_MB` | Per-plugin linear-memory cap in MiB (default 128, `16` to `4096`). A `memory.grow` ceiling, never a reservation — small plugins use a few MiB. Also caps `http-fetch` response bodies. |
| `FERROFIN_WASM_EVENT_QUEUE_CAPACITY` | Per-plugin event queue depth (default 256, `16` to `65536`). A full queue drops events for that plugin only. Requests to a plugin's own URL space (`/Plugins/{id}/web/…`) may hold at most half of it, queued or running; past that, or when the queue is full, they get `503` at once instead of waiting. |
| `FERROFIN_WASM_STATE_LIMIT_MB` | Per-plugin key/value state cap in MiB (default 8, `1` to `256`). Settings and cursors fit easily; raise for stats-heavy plugins (e.g. playback reporting). Each plugin's state is also kept parsed in memory while the server runs (roughly twice the logical bytes when values dominate, since they are hex-encoded, plus per-entry overhead), so raising the cap raises that too. |
| `FERROFIN_WASM_IMAGE_DOWNLOAD_MB` | Cap on one plugin-artwork download in MiB (default 20, `1` to `256`). Posters/backdrops are single-digit MiB; anything larger is not artwork. |
| `FERROFIN_WASM_IMAGE_TIMEOUT_SECS` | Wall-clock bound on one plugin-artwork download in seconds (default 30, `5` to `300`). A CDN GET, not a transfer job. |
| `FERROFIN_WASM_WRITE_CONTENT_MB` | Cap on one plugin `write-lyrics`/`write-subtitles` payload in MiB (default 2, `1` to `64`). Settings-class writes, not media. |
| `FERROFIN_WASM_SUBTITLE_EXTRACT_MB` | Cap on one plugin-extracted subtitle track in MiB (default 10, `1` to `128`). Generous for SRT text. |
| `FERROFIN_WASM_ANALYSIS_CONCURRENCY` | Concurrent media-decode budget shared by all analysis plugins (`extract-audio`/`extract-frames`). Default: a quarter of the visible cores, at least one — analysis must never starve transcodes; a small NAS may want `1`, a big host more. `1` to `64`. |
| `FERROFIN_WASM_PRIVATE_HTTP_ALLOW` | Plugins allowed to `http-fetch` private/loopback/link-local destinations: comma-separated plugin UUIDs, or `*` for all. Default: denied for every plugin (public destinations are always allowed). Plugin UUIDs appear in `/Plugins` and the load log line. (Accepting plugin names here is a planned improvement.) |
| `FERROFIN_MAX_PLUGIN_DOWNLOAD_MB` | Size cap, in MiB, on a plugin downloaded from a plugin repository during an install (default 128, sized for interpreter-language guests that bundle their runtime; `8` to `1024`). An abuse guard, not a tuning knob: a larger download is refused. `0` means the default. |

## Build- and test-time only

Not runtime config, listed for completeness:

- `FERROFIN_REFRESH_PLUGIN_ASSETS=1` — re-fetch vendored extension settings pages during
  `cargo build -p ferrofin-extensions`.
- `FERROFIN_SCAN_BENCH=1` — run the opt-in library-scan measurement harness
  (`apps/ferrofin-server/tests/scan_bench.rs`); reports items/second for a generated
  fixture library. `FERROFIN_SCAN_BENCH_{MOVIES,SERIES,EPISODES}` size it,
  `FERROFIN_SCAN_BENCH_PROBE=1` turns on the real ffprobe, and
  `FERROFIN_SCAN_BENCH_{MEDIA,POSTER}` point it at real sample files.
- `FERROFIN_LDAP_TESTS=1` — run the LDAP-Auth directory test against OpenLDAP in
  containers (`crates/ferrofin-extensions/tests/ldap_directory.rs`; needs docker).
- `FERROFIN_WASM_GUEST_TESTS=1` — build `examples/wasm-hello` from source (needs the
  wasm32-wasip2 target) and run the end-to-end WASM plugin tests.

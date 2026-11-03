# noaa-weather-cli (`wx`)

Rainfall, radar, alerts, forecasts and current conditions from NOAA and NWS raw
data, at the terminal. No API keys, no account.

Unofficial; not affiliated with or endorsed by NOAA or the National Weather
Service.

`wx` reads the same grids the NWS draws its own rainfall maps from, so a total is
the value in the 1 km cell over a place, not a nearby airport's reading. It works
anywhere MRMS covers, which is the contiguous US and the edges of its neighbors.
Alerts, forecasts and conditions also work where NWS has a forecast grid outside
that, such as Alaska and Hawaii.

## Install

Download a binary from [Releases](https://github.com/piekstra/noaa-weather-cli/releases),
or build from source:

```sh
cargo install --git https://github.com/piekstra/noaa-weather-cli --locked
```

`wx self-update` fetches the newest release after that.

## Quick start

```sh
wx places add cabin 39.74,-104.99     # name a place, or pass coordinates directly

wx rain 24h --at cabin                # total at that place and across its region
wx series 3d --at cabin               # hour by hour; --step 15m for quarter hours
wx rain 45m --at cabin                # the last 45 minutes, radar only
wx map 7d -o week.png                 # color bands around your saved places, with a world file
wx gauges 7d                          # rain gauges beside MRMS for the same hours; today works too

wx radar --at cabin                   # the 2-minute rain rate, newest frames
wx alerts --at cabin                  # active NWS alerts; --area CO for a state
wx forecast --at cabin                # --hourly for hour by hour
wx conditions --at cabin              # the nearest station reporting something fresh
```

`wx status` shows what NOAA has published and what is cached. It needs no place.

## How fine the data is

| Data | Cell | Time step | Gauge-corrected | Latest |
|---|---|---|---|---|
| MRMS MultiSensor QPE, Pass 2 | 1 km | 1 hour | yes, the full gauge network | about 1 hour old |
| MRMS MultiSensor QPE, Pass 1 | 1 km | 1 hour | gauges reported so far | about 15 minutes old |
| MRMS RadarOnly QPE | 1 km | 15 minutes | no | a few minutes old |
| MRMS PrecipRate | 1 km | 2 minutes | no | a few minutes old |
| GHCN-Daily gauges | one gauge | 1 day | they are the gauges | 2 to 3 days old |
| CoCoRaHS gauges | one gauge | 1 day | they are the gauges | entered that morning |
| USGS and ASOS gauges | one gauge | 1 hour | they are the gauges | about an hour old |

1 km is the finest gridded rainfall NOAA publishes. Every total reports how many
of its hours came from each pass, so a provisional figure is never presented as
final. Gauge figures from CoCoRaHS, USGS and ASOS are read from those networks
directly, ahead of GHCN-Daily's checks, and are marked provisional.

## Windows

| Spec | Meaning |
|---|---|
| `15m`, `45m` | the last N minutes, radar only, in 15-minute steps |
| `6h`, `3d`, `2w` | the last N hours, days or weeks of available data |
| `today`, `yesterday` | local calendar days |
| `2026-10-28..2026-10-30` | an inclusive range of local dates |
| `2026-11-01T08:00..now` | a local time to the newest data |
| `2026-11-01T12:00Z..2026-11-02T00:00Z` | explicit UTC or any `±HH:MM` offset |

Windows other than `Nm` snap outward to whole hours. The resolved window is
always reported back.

## Where it looks

`--at` takes a saved place or `lat,lon`. The area downloaded is a box around the
`--at` points, or around your saved places, unless `--region
west,south,east,north` says otherwise. Commands that draw or list a whole area,
`map` and `gauges`, use your saved places or `--region`. Places live in
`${XDG_CONFIG_HOME:-~/.config}/wx/config.json`; `wx config path` prints the file. On
Windows `~` is `%USERPROFILE%`. Claude Desktop and launchd do not start from your shell, so
they do not see an `XDG_CONFIG_HOME` set in a shell profile and read `~/.config`.

Downloaded grids are cached in SQLite under your cache directory, so a repeated
question downloads nothing. A grid is dropped 92 days after it was downloaded, and the
file stops growing because SQLite reuses the space. `wx config set retain_days N`
changes the limit, and 0 keeps everything.

## Use with Claude

`wx mcp` serves four read-only tools to Claude over MCP: `wx_rain`, `wx_forecast`,
`wx_conditions` and `wx_alerts`. They read the same public data the commands do, with no
account or key. Maps, raw grids, gauges and radar frames stay on the command line.

Each tool takes `at`, a saved place's name or `lat,lon`, except that `wx_alerts` can take
a state or marine `area` instead. `wx_rain` takes a `window` in the grammar above, and
`step` (`1h` or `15m`) to add a bar per step. A total keeps its passes, and says when it can
still change.

**Claude Code.** Add the `noaa-weather-cli` plugin from the
[piekstra plugin marketplace](https://github.com/piekstra/claude-plugins):

```console
$ claude plugin marketplace add piekstra/claude-plugins
$ claude plugin install noaa-weather-cli@piekstra-plugins
```

or register the server yourself:

```console
$ claude mcp add wx -- wx mcp
```

**Claude Desktop.** Download `wx-<version>.mcpb` from the
[latest release](https://github.com/piekstra/noaa-weather-cli/releases/latest) and open it.
It asks for nothing and touches no keychain. To ask about places by name, install `wx` too
and run `wx places add`: the extension reads the same config file.

Any other MCP client that launches a local server can run `wx mcp`.

**Claude on the web and mobile.** claude.ai reaches tools through a remote connector. `wx`
keeps its cache and your saved places on your computer, so it serves the same tools from
your computer to a connector you deploy into your own Cloudflare account:

```console
$ wx mcp --new-token            # once: a bearer token, saved in the keychain and printed for the Worker
$ wx mcp --http --port 8770     # serve on 127.0.0.1 only
```

`--http` takes the token from `$WX_MCP_TOKEN` if it is set, and otherwise from the keychain.
The connector, [cli-common's `connector/`](https://github.com/piekstra/cli-common/tree/main/connector#tools-from-your-computer-home-modules),
reaches that port through a Cloudflare Tunnel in your account; its README walks through the
tunnel, the VPC service, a launchd agent that keeps the server running, and the
`HOME_WX_TOKEN` secret, with the threat model. `--http` binds 127.0.0.1 only, refuses to
start without the token, refuses any request whose `Host` is not loopback or that carries an
`Origin`, and serves only read-only tools, which is all `wx` has. Tool results pass through
Cloudflare and reach Claude ([`SECURITY.md`](SECURITY.md)). While the computer sleeps, the
tools answer that it is offline.

After `wx self-update`, restart the server: a running one keeps the old code, and its
launchd agent restarts it only when it exits. Its start line names the version.

```console
$ launchctl kickstart -k gui/$(id -u)/com.piekstra.wx-mcp-http
```

`self-update` does not reach a Claude Desktop extension, which runs its own copy. Install
the new release's `.mcpb` too.

## Output and exit codes

`--json` on any command prints one document tagged `<name>/v1` on stdout.
Diagnostics go to stderr. `wx info --json` lists capabilities. In `wx gauges --json`,
`reports` and `possible` count steps of `step_hours`: days for a daily gauge and hours
for an hourly one. `mrms_hours` gives the MRMS passes behind `mrms_mm`.

| Code | Meaning |
|---|---|
| 0 | ok |
| 2 | bad usage, including points too far apart to share one grid |
| 3 | `mcp --http` without a bearer token |
| 4 | not found, such as a window with no data yet, an area outside MRMS coverage, a point with no NWS forecast, or no nearby station with a recent observation |
| 5 | upstream failure from NOAA or NWS |

## As a library

The crate is also the library `noaa_weather`. A program embedding it skips the
CLI's dependencies:

```toml
noaa-weather-cli = { git = "https://github.com/piekstra/noaa-weather-cli", tag = "v0.4.0", default-features = false }
```

## Data

MRMS and GHCN-Daily come from the NOAA Open Data Dissemination Program on AWS.
Alerts, forecasts and observations come from the National Weather Service API.
Same-day gauges come from CoCoRaHS volunteers, the USGS Water Data API and the Iowa
Environmental Mesonet's ASOS archive.
[docs/api.md](docs/api.md) records each endpoint and its traps.

## License

MIT or Apache-2.0, at your option.

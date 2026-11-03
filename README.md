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
wx gauges 7d                          # rain gauges beside MRMS for the same hours

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

1 km is the finest gridded rainfall NOAA publishes. Every total reports how many
of its hours came from each pass, so a provisional figure is never presented as
final.

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
`${XDG_CONFIG_HOME:-~/.config}/wx/config.json`; `wx config path` prints the file.

Downloaded grids are cached in SQLite under your cache directory, so a repeated
question downloads nothing.

## Output and exit codes

`--json` on any command prints one document tagged `<name>/v1` on stdout.
Diagnostics go to stderr. `wx info --json` lists capabilities.

| Code | Meaning |
|---|---|
| 0 | ok |
| 2 | bad usage, including points too far apart to share one grid |
| 4 | not found, such as a window with no data yet, an area outside MRMS coverage, or a point with no NWS forecast |
| 5 | upstream failure from NOAA or NWS |

## As a library

The crate is also the library `noaa_weather`. A program embedding it skips the
CLI's dependencies:

```toml
noaa-weather-cli = { git = "https://github.com/piekstra/noaa-weather-cli", tag = "v0.1.0", default-features = false }
```

## Data

MRMS and GHCN-Daily come from the NOAA Open Data Dissemination Program on AWS.
Alerts, forecasts and observations come from the National Weather Service API.
[docs/api.md](docs/api.md) records each endpoint and its traps.

## License

MIT or Apache-2.0, at your option.

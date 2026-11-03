# AGENTS.md: noaa-weather-cli

Canonical agent entrypoint for this repo. `CLAUDE.md` is a one-line pointer here.

## What this is

A Rust CLI (`wx`) and library (`noaa_weather`) over NOAA and NWS raw data: MRMS
rainfall and radar grids on AWS, GHCN-Daily gauges on AWS, and `api.weather.gov`
for alerts, forecasts and observations. It conforms to the family spec
`piekstra-cli/1` from [cli-common](https://github.com/piekstra/cli-common).

Everything is anonymous public data: no credentials, no keychain, no `auth login`.
`auth status` exists because the spec requires it and reports `required: false`.

## Build and test

```sh
make verify     # fmt-check, clippy (with and without the cli feature), tests, smoke
make install    # signed, from the reviewed Cargo.lock
git config core.hooksPath .githooks   # pre-commit gitleaks and fmt
```

Tests are offline. `cargo test -- --ignored real_file` decodes a real MRMS file
you downloaded; see the test for the variable it reads.

## Layout

| File | What it owns |
|---|---|
| `src/main.rs` | The clap tree, output, exit-code mapping. No domain logic. |
| `src/lib.rs` | The library's public surface. |
| `src/window.rs` | The window grammar, shared by every caller. |
| `src/grid.rs` | `Region`, `Grid`, the MRMS domain, area snapping for map views. |
| `src/grib2.rs` | A small GRIB2 reader: grid template 3.0, PNG packing 5.41, nothing else. |
| `src/mrms.rs` | Product names, bucket keys, downloads, retries. |
| `src/engine.rs` | Planning which files cover a window, loading, summing. |
| `src/store.rs` | The SQLite cache. Grids are stored deflated. |
| `src/ghcn.rs` | GHCN-Daily gauges and the like-for-like comparison with MRMS. |
| `src/nws.rs` | `api.weather.gov`: points, forecasts, observations, alerts. |
| `src/ramp.rs` | The color scales and their validator results. |
| `src/render.rs` | PNG rendering. |
| `src/places.rs` | The config file: named places and settings. |
| `docs/api.md` | Every endpoint and its traps. Read before touching a client. |

## Rules

- **No personal data in tracked files.** Saved places belong in the user's config,
  never in a test, example, doc or commit message. Examples use a public city
  center or an invented place. `tests/repo_clean.rs` scans for address and phone
  shapes; it cannot recognize a real home's coordinates, so check that by eye.
- **No region or place defaults.** A command with no `--at`, `--region` or saved
  places is a usage error, not a guess. The tool works anywhere in coverage.
- **Library features.** `src/lib.rs` and everything it declares compile with
  `--no-default-features`. Family crates and clap stay behind the `cli` feature.
- **Never present provisional as final.** A total carries its pass counts. Do not
  drop them from a DTO or a human view.
- **A silently misread grid is the worst failure here.** `grib2.rs` rejects what
  it does not understand rather than decoding it approximately.
- **Be polite upstream.** Downloads go through the concurrency limit. The
  User-Agent names the repository, and nothing about the person running it.

## Output contract

`--json` prints exactly one document, tagged `<name>/v1`. Pass bare names to
`output::tagged` and `emit_one`; they append `/v1` themselves. Exit codes: 0 ok,
2 usage, 4 not found, 5 upstream.

## Update trigger

Revise `docs/api.md` when a new endpoint or trap turns up, and the Layout table
when a module is added.

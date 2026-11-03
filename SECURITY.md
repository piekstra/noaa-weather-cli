# Security policy

## Reporting a vulnerability

Report security issues privately, not in a public issue: use "Report a
vulnerability" on this repository's Security tab.

## Notes for this tool

`wx` handles no credentials. NOAA's buckets and `api.weather.gov` are anonymous:
no login, no key, no token, no cookie session. Nothing is written to the OS
keychain.

What it writes to disk:

| File | Holds |
|---|---|
| `${XDG_CONFIG_HOME:-~/.config}/wx/config.json` | Named places and non-secret settings |
| A SQLite database in the user cache directory | Downloaded grids, gauge reports, zone shapes |

Your named places can be sensitive, because they may be your home. They stay in
that config file and are sent upstream only as the coordinates of the requests
you make: a point lookup to `api.weather.gov` carries them, while MRMS and
GHCN-Daily downloads are whole-grid files and reveal only the time asked about.

What remains to care about:

- **Parsing untrusted files.** GRIB2 and Parquet are decoded from the network. The
  GRIB2 reader accepts only the templates MRMS uses and returns an error for
  anything else. Decompression stops at 128 MiB, and the embedded PNG must match the
  grid the file declares.
- **`wx api`.** It sends GET requests only, and only to `api.weather.gov`.
- **Dependency advisories.** CI runs `cargo audit` and `cargo deny`.
- **No secrets or personal data in the repo.** CI runs gitleaks, and
  `tests/repo_clean.rs` scans tracked files for address and phone shapes.

## Supported versions

Before 1.0, only the latest release receives fixes.

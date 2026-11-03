# Security policy

## Reporting a vulnerability

Report security issues privately, not in a public issue: use "Report a
vulnerability" on this repository's Security tab.

## Notes for this tool

`wx` needs no credential for any data. NOAA's buckets, `api.weather.gov` and the gauge
networks are anonymous: no login, no key, no cookie session. The one thing it can write to
the OS keychain is the bearer token of `wx mcp --http`, if you ask for one (below), and that
token signs in to nothing upstream.

What it writes to disk:

| File | Holds |
|---|---|
| `${XDG_CONFIG_HOME:-~/.config}/wx/config.json` (on Windows, `~` is `%USERPROFILE%`) | Named places and non-secret settings. With no home directory, `wx` refuses rather than using a temp directory. |
| `${XDG_CACHE_HOME:-~/.cache}/wx/weather.sqlite` | Downloaded grids, gauge reports, zone shapes |
| OS keychain (`piekstra.wx`, `mcp-http-token`) | Only after `wx mcp --new-token`: the bearer token of `mcp --http` |

Your named places can be sensitive, because they may be your home. They stay in
that config file and are sent upstream only as the coordinates of the requests
you make: a point lookup to `api.weather.gov` carries them, while MRMS and
GHCN-Daily downloads are whole-grid files and reveal only the time asked about.
The same-day gauge feeds see less than a place: CoCoRaHS and the Iowa Environmental
Mesonet receive the US states a region touches, and USGS a box of whole degrees
around it.

What remains to care about:

- **Parsing untrusted files.** GRIB2, Parquet, CSV and JSON are decoded from the
  network. A USGS next link contributes only its query, so a page cannot send the
  client to another host. The GRIB2 reader accepts only the templates MRMS uses and
  returns an error for anything else. Decompression stops at 128 MiB, and the embedded
  PNG must match the grid the file declares.
- **`wx api`.** It sends GET requests only, and only to `api.weather.gov`.
- **Dependency advisories.** CI runs `cargo audit` and `cargo deny`.
- **No secrets or personal data in the repo.** CI runs gitleaks, and
  `tests/repo_clean.rs` scans tracked files for address and phone shapes.

## MCP tools

`wx mcp` serves four read-only tools to Claude: `wx_rain`, `wx_forecast`, `wx_conditions`
and `wx_alerts`. No tool writes anything but the grid cache, and none takes a path or a URL.
The config is read on each call. On stdio nothing touches the keychain, and listing the
tools reads no file.

**What reaches Claude.** Tool results go to Claude as part of your conversation. A tool takes
a saved place by name and sends its coordinates only upstream, as a command would. No result
or error repeats them, but a forecast's nearest town and a station's name and distance say
roughly where the place is. An unknown name's error lists the names of your saved places, so
Claude can pick one. An NWS alert's text is third-party data; the server's instructions tell
the model to treat it as data, never as instructions.

`wx mcp --http --port <PORT>` serves the same tools for your own Cloudflare Worker, which
reaches it through your own Cloudflare Tunnel (cli-common's `connector/`). It binds 127.0.0.1
only, with no flag for another address, and refuses to start without a bearer token of 32 or
more characters, from `WX_MCP_TOKEN` or the keychain item `mcp-http-token` (`piekstra.wx`),
which `wx mcp --new-token` makes from 256 random bits and replaces only with `--overwrite`.
Every request must carry the token (compared in constant time) and a `Host` of `127.0.0.1` or
`localhost`, and must not carry an `Origin`; there are no CORS headers. A copy of that token
lives in your Worker as its `HOME_WX_TOKEN` secret. The data is public, so a leaked token
gives no account to anyone: it answers weather questions, can use your saved places by name,
and can make your computer download grids, one rainfall load at a time and each for at most
100 seconds.

**Tool results also cross Cloudflare on this path**, through your tunnel and your Worker, on
their way to Claude. Who holds what, and what a leaked token can do, is in the connector's
[threat model](https://github.com/piekstra/cli-common/blob/main/connector/README.md#threat-model).

`wx` has no `auth logout`, since it has no sign-in. To revoke the connector's access, run
`wx mcp --new-token --overwrite` and restart the server (`launchctl kickstart -k gui/$(id
-u)/com.piekstra.wx-mcp-http`); the old token works until the restart, and a token in
`$WX_MCP_TOKEN` wins until you change it. To stop serving, run `launchctl bootout gui/$(id
-u)/com.piekstra.wx-mcp-http` and stop the tunnel. To remove the token on macOS, run `security
delete-generic-password -s piekstra.wx -a mcp-http-token`.

## Supported versions

Before 1.0, only the latest release receives fixes.

# Changelog

All notable changes to `wx` are documented here. Newest first. The project follows
[SemVer](https://semver.org/); until 1.0, minors carry the breaking-change signal.

## v0.4.1 — 2026-11-04

- The config path and `self-update`'s archive name come from cli-common, which now covers `USERPROFILE` and `wx.exe`, in place of this crate's own copies.
- cli-common v0.29.0, up from v0.25.0.

## 0.4.0 — 2026-11-03

- `wx mcp`: four read-only tools for Claude: `wx_rain`, `wx_forecast`, `wx_conditions` and
  `wx_alerts`. They run over stdio for Claude Code and Claude Desktop, and with `--http` on
  127.0.0.1 behind a bearer token for Claude on the web and mobile through your own
  Cloudflare Worker and tunnel. No tool writes anything but the grid cache.
- A tool takes a saved place by name and no result or error repeats its coordinates.
- `wx conditions` exits 4 when stations answered and none has a recent observation, and 5 when
  none could answer. It exited 1 for both.
- A 404 on an NWS call that must succeed, such as a station list, exits 5. It exited 1.
- The config and cache follow `USERPROFILE` on Windows. With no home directory the config is
  an error, not a file in a temp directory that a cleanup can remove.
- Releases attach `wx-<version>.mcpb`, a Claude Desktop extension, and a Windows x86_64
  tarball, and publish only once every asset is there.
- `self-update` says to restart a running `wx mcp --http` and to install the new `.mcpb`, and
  runs outside the async runtime.
- cli-common v0.25.0, up from v0.20.0. Rust 1.89 or later.

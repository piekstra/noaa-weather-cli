# Contributing

## Getting set up

```sh
git clone https://github.com/piekstra/noaa-weather-cli
cd noaa-weather-cli
git config core.hooksPath .githooks     # pre-commit gitleaks and fmt
make verify
```

You need a recent stable Rust toolchain. There is no account to create and no key
to obtain. Tests run offline.

## Before you open a PR

`make verify` runs what CI runs, apart from `cargo audit`, `cargo deny` and
gitleaks. Those run in CI, and locally if you have them installed.

## House rules

- **No personal data in the repo.** Use a public city center or an invented place
  in tests, examples and docs. Never a real home.
- **Fixtures show payload shapes.** Invent the identifying values, and record what
  was done in `tests/fixtures/README.md`.
- **Don't fork shared behavior.** Errors, exit codes, output and self-update come
  from the `pk-cli-*` crates in
  [cli-common](https://github.com/piekstra/cli-common). A surface change, such as
  a new standard command or exit code, starts in that repo's `DESIGN.md`.
- **Read [docs/api.md](docs/api.md) before touching a client.** Most of what looks
  like a simplification there has already failed once.
- **Be polite upstream.** Keep downloads behind the concurrency limit, and cache
  what does not change.

## Reporting bad data

If `wx` and NOAA's own viewer disagree, open an issue with the place, the window
and both values. If NOAA's data itself looks wrong, that is a matter for NOAA.

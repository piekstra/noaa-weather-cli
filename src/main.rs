//! `wx` — rainfall, radar, alerts and forecasts from NOAA and NWS raw data.
//!
//! ```text
//! wx status                        what NOAA has published; what is cached
//! wx rain 24h --at 39.74,-104.99   total over a window, region and points
//! wx rain 45m --at cabin           the last 45 minutes, radar only
//! wx series today --at cabin       hour by hour; --step 15m for quarter hours
//! wx map 3d -o rain.png            heatmap PNG + world file; --style cells for raw cells
//! wx grid yesterday --format csv   the raw 1 km grid
//! wx gauges 7d                     rain gauges vs. MRMS at each gauge
//! wx radar --at cabin              the 2-minute rain rate, now and recently
//! wx alerts --at cabin             active NWS alerts
//! wx forecast --at cabin           the NWS forecast; --hourly for hour by hour
//! wx conditions --at cabin         current observations from the nearest station
//! wx places add cabin 39.74,-104.99
//! wx backfill --days 7             pre-fill the hourly cache
//! ```
//!
//! WINDOW is the grammar in `noaa_weather::window`: `45m`, `6h`, `3d`, `2w`,
//! `today`, `yesterday`, `2026-10-28..2026-10-30`, `2026-11-01T08:00..now`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use jiff::{tz::TimeZone, Timestamp};
use pk_cli_auth::{AuthMethod, AuthStatus};
use pk_cli_core::info::{AuthInfo, CliInfo};
use pk_cli_core::{output, CliError, CommonArgs};
use pk_cli_selfupdate::{SelfUpdateArgs, Updater};
use serde::Serialize;
use serde_json::{json, Value};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use noaa_weather::engine::{Accumulation, Availability, Progress, SeriesPoint};
use noaa_weather::grid::{mm_to_in, GridError, Region, MAX_AREA_CELLS, MISSING};
use noaa_weather::nws::{self, AlertQuery, NwsClient};
use noaa_weather::places::{default_config_path, Place, UserConfig};
use noaa_weather::render::{self, RenderOpts, Rows, Style};
use noaa_weather::window::{local_label, region_tz, Resolved, WindowError};
use noaa_weather::{ghcn, mrms, ramp, Config, Store, Weather, WindowSpec, MRMS_DOMAIN};

const BIN: &str = "wx";
const REPO: &str = "piekstra/noaa-weather-cli";
const CONFIG_KEYS: &str = "region, tz, db, alert_area, retain_days";

/// Rainfall, radar, alerts and forecasts from NOAA and NWS raw data. No API
/// keys. Unofficial; not affiliated with NOAA or the National Weather Service.
#[derive(Parser, Debug)]
#[command(name = BIN, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,

    /// Config file location.
    #[arg(long, env = "WX_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Cache database. Default: the user cache directory.
    #[arg(long, env = "WX_DB", global = true)]
    db: Option<PathBuf>,

    /// Area to cache, `west,south,east,north`. Default: a box around the `--at`
    /// points, or around your saved places.
    #[arg(long, env = "WX_REGION", allow_hyphen_values = true, global = true)]
    region: Option<String>,

    /// Time zone that "today" and local times refer to. Default: the system's.
    #[arg(long, env = "WX_TZ", global = true)]
    tz: Option<String>,

    /// Simultaneous downloads from the NOAA buckets.
    #[arg(long, env = "WX_CONCURRENCY", default_value_t = 6, global = true)]
    concurrency: usize,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Debug, Clone)]
struct WindowArg {
    /// Window: 45m, 6h, 3d, 2w, today, yesterday, 2026-10-28..2026-10-30, 2026-11-01T08:00..now
    #[arg(default_value = "24h", allow_hyphen_values = true)]
    window: String,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// What NOAA has published right now, and what is cached locally.
    Status,
    /// Total rainfall over a window: region summary, plus any --at points.
    Rain {
        #[command(flatten)]
        w: WindowArg,
        /// A point as `lat,lon` or a saved place name (repeatable).
        #[arg(long = "at", allow_hyphen_values = true)]
        at: Vec<String>,
    },
    /// Rainfall at one point, bar by bar.
    Series {
        #[command(flatten)]
        w: WindowArg,
        /// A point as `lat,lon` or a saved place name.
        #[arg(long = "at", allow_hyphen_values = true, required = true)]
        at: String,
        /// Bar length. 15m is radar only, limited to 48 hours.
        #[arg(long, value_enum, default_value_t = Step::Hour)]
        step: Step,
    },
    /// Render the window's total as a color-scaled PNG.
    Map {
        #[command(flatten)]
        w: WindowArg,
        #[arg(short, long, default_value = "rain.png")]
        out: PathBuf,
        /// Output pixels per 1 km cell.
        #[arg(long, default_value_t = 8)]
        scale: usize,
        /// banded: flat color bands with contoured edges. cells: each 1 km cell
        /// as a flat square. smooth: a continuous blend between the colors.
        #[arg(long, value_enum, default_value_t = StyleArg::Banded)]
        style: StyleArg,
        /// Stretch the colors across this map's own range (2nd to 98th percentile
        /// of the cells with rain) instead of the fixed scale. Shows the detail
        /// inside one storm, but colors no longer compare between maps.
        #[arg(long)]
        fit: bool,
        /// Space rows evenly in Web Mercator (for a web-map overlay) instead
        /// of latitude (for GIS, with a .pgw world file).
        #[arg(long)]
        mercator: bool,
    },
    /// Dump the raw grid: one row per 1 km cell.
    Grid {
        #[command(flatten)]
        w: WindowArg,
        #[arg(long, value_enum, default_value_t = GridFormat::Csv)]
        format: GridFormat,
        /// Write here instead of stdout.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Rain-gauge totals (CoCoRaHS, NWS COOP, USGS, ASOS) beside MRMS for the same hours.
    Gauges {
        #[command(flatten)]
        w: WindowArg,
        /// Read the gauge archive and same-day feeds again even if checked recently.
        #[arg(long)]
        refresh: bool,
    },
    /// The 2-minute radar rain rate, newest frames.
    Radar {
        /// A point as `lat,lon` or a saved place name.
        #[arg(long = "at", allow_hyphen_values = true)]
        at: Option<String>,
        /// Frames to read, newest last (1 to 45; each is 2 minutes).
        #[arg(long, default_value_t = 1)]
        frames: usize,
        /// Write the newest frame as a PNG here.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Active NWS alerts for a point, or for an area such as FL.
    Alerts {
        /// A point as `lat,lon` or a saved place name.
        #[arg(long = "at", allow_hyphen_values = true, conflicts_with = "area")]
        at: Option<String>,
        /// State or marine area code, such as FL. Default: the `alert_area` setting.
        #[arg(long)]
        area: Option<String>,
    },
    /// The NWS forecast for a point.
    Forecast {
        /// A point as `lat,lon` or a saved place name.
        #[arg(long = "at", allow_hyphen_values = true, required = true)]
        at: String,
        /// Hour by hour instead of the twice-daily periods.
        #[arg(long)]
        hourly: bool,
        /// Periods to show.
        #[arg(long, default_value_t = 6)]
        limit: usize,
    },
    /// Current observations from the nearest reporting station.
    Conditions {
        /// A point as `lat,lon` or a saved place name.
        #[arg(long = "at", allow_hyphen_values = true, required = true)]
        at: String,
    },
    /// Named places, usable anywhere `--at` is.
    #[command(subcommand)]
    Places(PlacesCmd),
    /// Pre-fill the hourly cache for the last N days.
    Backfill {
        #[arg(long, default_value_t = 7)]
        days: i64,
    },
    /// Raw GET against api.weather.gov, for fields this CLI does not model.
    Api {
        /// HTTP method. Only GET: this is a read-only tool.
        method: String,
        /// Path such as /alerts/types.
        path: String,
    },
    /// Machine-readable capability discovery (cli-info/v1).
    Info,
    /// Report credential state. This tool needs none.
    #[command(subcommand)]
    Auth(AuthCmd),
    /// Non-secret settings.
    #[command(subcommand)]
    #[command(name = "config")]
    ConfigCmd(ConfigCmd),
    /// Update wx to the latest release from GitHub.
    SelfUpdate(SelfUpdateArgs),
    /// Print a shell completion script.
    Completions { shell: Shell },
}

#[derive(Subcommand, Debug)]
enum AuthCmd {
    /// Report credential state.
    Status,
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Print the config file's path.
    Path,
    /// Print the current settings.
    Show,
    /// Create the file if it does not exist.
    Init,
    /// Set a value. Keys: region, tz, db, alert_area, retain_days.
    Set { key: String, value: String },
    /// Clear a value.
    Unset { key: String },
}

#[derive(Subcommand, Debug)]
enum PlacesCmd {
    /// List saved places.
    #[command(alias = "ls")]
    List,
    /// Add a place, or move it if the name exists.
    Add {
        name: String,
        /// `lat,lon`
        #[arg(allow_hyphen_values = true)]
        point: String,
    },
    /// Remove a place.
    #[command(alias = "rm")]
    Remove { name: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum GridFormat {
    Csv,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, ValueEnum)]
enum Step {
    #[value(name = "1h")]
    Hour,
    #[value(name = "15m")]
    Quarter,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StyleArg {
    Smooth,
    Cells,
    Banded,
}

/// What the commands share, resolved from flags, environment and config.
struct Ctx {
    json: bool,
    quiet: bool,
    config_path: PathBuf,
    user: UserConfig,
    db: PathBuf,
    region_flag: Option<Region>,
    tz: TimeZone,
    concurrency: usize,
}

impl Ctx {
    /// Bad flags and bad config fail here, before any network or cache use.
    fn new(cli: &Cli) -> Result<Self> {
        let config_path = cli.config.clone().unwrap_or_else(default_config_path);
        let user = UserConfig::load(&config_path)?;
        let region = cli.region.clone().or_else(|| user.region.clone());
        let region_flag = region.map(|r| Region::parse(&r)).transpose()?;
        let tz = match cli.tz.clone().or_else(|| user.tz.clone()) {
            Some(name) => region_tz(&name).map_err(|e| CliError::Usage(e.to_string()))?,
            None => TimeZone::system(),
        };
        let db = cli
            .db
            .clone()
            .or_else(|| user.db.clone())
            .unwrap_or_else(noaa_weather::store::default_db_path);
        Ok(Self {
            json: cli.common.json,
            quiet: cli.common.quiet,
            config_path,
            user,
            db,
            region_flag,
            tz,
            concurrency: cli.concurrency,
        })
    }

    /// [`Self::requested_region`], or not found when no MRMS grid reaches it.
    fn region(&self, points: &[(f64, f64)]) -> Result<Region> {
        let region = self.requested_region(points)?;
        if region.intersect(&MRMS_DOMAIN).is_none() {
            return Err(CliError::NotFound(format!(
                "that area is outside MRMS coverage: longitude {} to {}, latitude {} to {}",
                MRMS_DOMAIN.west, MRMS_DOMAIN.east, MRMS_DOMAIN.south, MRMS_DOMAIN.north
            ))
            .into());
        }
        Ok(region)
    }

    /// A configured region, else a box around the points, else a box around the
    /// saved places. A usage error when none of those say where to look.
    fn requested_region(&self, points: &[(f64, f64)]) -> Result<Region> {
        if let Some(r) = self.region_flag {
            return Ok(r);
        }
        let saved: Vec<(f64, f64)> = self.user.places.iter().map(|p| (p.lat, p.lon)).collect();
        let around = if points.is_empty() { &saved } else { points };
        let Some(region) = Region::around(around, 0.5) else {
            return Err(CliError::Usage(
                "nothing says where to look: pass --at or --region, or save one with `wx places add`"
                    .into(),
            )
            .into());
        };
        if region.cells() > MAX_AREA_CELLS {
            return Err(CliError::Usage(format!(
                "those points span about {} cells, more than the {MAX_AREA_CELLS} one cached grid allows: ask for them separately",
                region.cells()
            ))
            .into());
        }
        Ok(region)
    }

    fn store(&self) -> Result<Store> {
        if let Some(dir) = self.db.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Store::open(&self.db).with_context(|| format!("opening {}", self.db.display()))
    }

    fn weather(&self, points: &[(f64, f64)]) -> Result<Weather> {
        self.weather_for(self.region(points)?)
    }

    fn weather_for(&self, region: Region) -> Result<Weather> {
        let cfg = Config {
            tz: self.tz.clone(),
            concurrency: self.concurrency,
            retain_days: self.user.grid_retention(),
            ..Config::new(region)
        };
        Weather::new(cfg, self.store()?)
    }

    fn nws(&self) -> Result<NwsClient> {
        NwsClient::new(nws::DEFAULT_NWS_URL, self.store().ok())
    }

    /// `lat,lon` or a saved place's name.
    fn point(&self, s: &str) -> Result<(f64, f64)> {
        if let Some(p) = self.user.place(s) {
            return Ok((p.lat, p.lon));
        }
        parse_point(s)
    }

    /// A progress meter, unless output is for a machine.
    fn meter(&self, what: &'static str) -> Option<Progress> {
        if self.json || self.quiet {
            return None;
        }
        progress_meter(what)
    }
}

/// Exit-code mapping per the family's contract (SPEC v1 §1.5).
fn to_cli_error(e: &anyhow::Error) -> CliError {
    if let Some(c) = e.downcast_ref::<CliError>() {
        return c.clone();
    }
    if let Some(w) = e.downcast_ref::<WindowError>() {
        return match w {
            WindowError::Empty { .. } => CliError::NotFound(w.to_string()),
            _ => CliError::Usage(w.to_string()),
        };
    }
    if e.downcast_ref::<GridError>().is_some() {
        return CliError::Usage(e.to_string());
    }
    if e.downcast_ref::<nws::NoForecast>().is_some() {
        return CliError::NotFound(e.to_string());
    }
    let upstream = e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>().is_some()
            || c.downcast_ref::<noaa_weather::UpstreamError>().is_some()
    });
    if upstream {
        return CliError::Upstream(format!("{e:#}"));
    }
    CliError::Other(format!("{e:#}"))
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = if cli.common.verbose {
        "noaa_weather=debug"
    } else {
        "noaa_weather=warn"
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let json = cli.common.json;
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        // `wx grid | head` closing the pipe early is not an error.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            let code = output::fail(&to_cli_error(&e), json);
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    // These need no settings, so a broken config cannot lock anyone out of them.
    match &cli.cmd {
        Cmd::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(*shell, &mut cmd, BIN, &mut std::io::stdout());
            return Ok(());
        }
        Cmd::Info => return info(),
        Cmd::Auth(AuthCmd::Status) => return auth_status(cli.common.json),
        Cmd::SelfUpdate(args) => {
            return Ok(updater().run(args, cli.common.json, cli.common.quiet)?);
        }
        _ => {}
    }

    let ctx = Ctx::new(&cli)?;
    match cli.cmd {
        // What NOAA has published does not depend on any one area.
        Cmd::Status => status(&ctx.weather_for(MRMS_DOMAIN)?, ctx.json).await,
        Cmd::Rain { w, at } => {
            let points: Vec<(f64, f64)> = at.iter().map(|s| ctx.point(s)).collect::<Result<_>>()?;
            WindowSpec::parse(&w.window, &ctx.tz)?;
            rain(&ctx, &ctx.weather(&points)?, &w.window, &points).await
        }
        Cmd::Series { w, at, step } => {
            let point = ctx.point(&at)?;
            WindowSpec::parse(&w.window, &ctx.tz)?;
            series(&ctx, &ctx.weather(&[point])?, &w.window, point, step).await
        }
        Cmd::Map {
            w,
            out,
            scale,
            style,
            fit,
            mercator,
        } => {
            WindowSpec::parse(&w.window, &ctx.tz)?;
            let style = match style {
                StyleArg::Smooth => Style::Smooth,
                StyleArg::Cells => Style::Cells,
                StyleArg::Banded => Style::Banded,
            };
            map(
                &ctx,
                &ctx.weather(&[])?,
                &w.window,
                &out,
                MapOpts {
                    scale,
                    style,
                    fit,
                    mercator,
                },
            )
            .await
        }
        Cmd::Grid { w, format, out } => {
            WindowSpec::parse(&w.window, &ctx.tz)?;
            // `--json` and `--format json` ask for the same document.
            let format = if ctx.json { GridFormat::Json } else { format };
            grid(&ctx, &ctx.weather(&[])?, &w.window, format, out).await
        }
        Cmd::Gauges { w, refresh } => {
            WindowSpec::parse(&w.window, &ctx.tz)?;
            gauges(&ctx.weather(&[])?, &w.window, ctx.json, refresh).await
        }
        Cmd::Radar { at, frames, out } => {
            let point = at.as_deref().map(|s| ctx.point(s)).transpose()?;
            if !(1..=noaa_weather::engine::MAX_RATE_FRAMES).contains(&frames) {
                return Err(CliError::Usage(format!(
                    "--frames must be 1..={}",
                    noaa_weather::engine::MAX_RATE_FRAMES
                ))
                .into());
            }
            let points: Vec<_> = point.into_iter().collect();
            radar(&ctx, &ctx.weather(&points)?, point, frames, out).await
        }
        Cmd::Alerts { at, area } => alerts(&ctx, at, area).await,
        Cmd::Forecast { at, hourly, limit } => {
            let point = ctx.point(&at)?;
            forecast(&ctx, point, hourly, limit).await
        }
        Cmd::Conditions { at } => {
            let point = ctx.point(&at)?;
            conditions(&ctx, point).await
        }
        Cmd::Places(cmd) => places(&ctx, cmd),
        Cmd::ConfigCmd(cmd) => config_cmd(&ctx, cmd),
        Cmd::Backfill { days } => {
            if !(1..=92).contains(&days) {
                return Err(CliError::Usage("--days must be 1..=92".into()).into());
            }
            backfill(&ctx, &ctx.weather(&[])?, days).await
        }
        Cmd::Api { method, path } => {
            if !method.eq_ignore_ascii_case("GET") {
                return Err(CliError::Usage(format!(
                    "only GET is supported, not {method}: wx is read-only"
                ))
                .into());
            }
            match ctx.nws()?.raw(&path).await? {
                Some(v) => {
                    output::json(&v);
                    Ok(())
                }
                None => Err(CliError::NotFound(path).into()),
            }
        }
        Cmd::Completions { .. } | Cmd::Info | Cmd::Auth(_) | Cmd::SelfUpdate(_) => {
            unreachable!("handled above")
        }
    }
}

fn updater() -> Updater {
    Updater {
        repo: REPO.into(),
        binary: BIN.into(),
        target: env!("BUILD_TARGET").into(),
        current: env!("CARGO_PKG_VERSION").into(),
    }
}

fn info() -> Result<()> {
    let info = CliInfo::new(
        BIN,
        env!("CARGO_PKG_VERSION"),
        &format!("https://github.com/{REPO}"),
        AuthInfo {
            required: false,
            method: "none".into(),
            login_hint: None,
        },
        &[
            "rain",
            "series",
            "map",
            "grid",
            "gauges",
            "radar",
            "alerts",
            "forecast",
            "conditions",
            "places",
            "api",
        ],
    );
    output::json(&serde_json::to_value(&info)?);
    Ok(())
}

fn auth_status(json_mode: bool) -> Result<()> {
    let status = AuthStatus::new(false, true, AuthMethod::None);
    output::emit_one(json_mode, "auth-status", serde_json::to_value(&status)?);
    Ok(())
}

fn emit<T: Serialize>(schema: &str, v: &T) -> Result<()> {
    output::json(&output::tagged(schema, serde_json::to_value(v)?));
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn parse_point(s: &str) -> Result<(f64, f64)> {
    let usage = || CliError::Usage(format!("{s:?}: expected lat,lon or a saved place name"));
    let (a, b) = s.split_once(',').ok_or_else(usage)?;
    let lat: f64 = a.trim().parse().map_err(|_| usage())?;
    let lon: f64 = b.trim().parse().map_err(|_| usage())?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(CliError::Usage(format!(
            "{s:?}: out of range (expected lat,lon — latitude first)"
        ))
        .into());
    }
    Ok((lat, lon))
}

/// `0.42" (10.7 mm)`
fn amount(mm: f64) -> String {
    format!("{:.2}\" ({:.1} mm)", mm_to_in(mm), mm)
}

/// A one-line progress meter on stderr, only when it is a terminal and only
/// once something is actually being fetched.
fn progress_meter(what: &'static str) -> Option<Progress> {
    if !std::io::stderr().is_terminal() {
        return None;
    }
    Some(Arc::new(move |done, total| {
        if total > 1 {
            eprint!("\r  {what}: {done}/{total} files");
            if done == total {
                eprint!("\r\x1b[2K");
            }
            let _ = std::io::stderr().flush();
        }
    }))
}

async fn resolve(wx: &Weather, spec: &str) -> Result<(Resolved, Availability)> {
    let spec = WindowSpec::parse(spec, &wx.cfg.tz)?;
    wx.resolve(&spec).await
}

async fn accumulate(
    ctx: &Ctx,
    wx: &Weather,
    spec: &str,
) -> Result<(Resolved, Availability, Accumulation)> {
    let (r, a) = resolve(wx, spec).await?;
    let acc = wx.accumulate(r.window, ctx.meter("loading MRMS")).await?;
    Ok((r, a, acc))
}

fn window_line(r: &Resolved, tz: &TimeZone) -> String {
    let span = if r.window.is_hour_aligned() {
        format!("{} h", r.window.hours())
    } else {
        format!("{} min", r.window.minutes())
    };
    format!(
        "{} — {} → {} ({span}){}",
        r.label,
        local_label(r.window.start, tz),
        local_label(r.window.end, tz),
        if r.clipped {
            " · cut short at the newest data"
        } else {
            ""
        }
    )
}

fn provenance_line(acc: &Accumulation) -> String {
    let h = acc.hours;
    if h.quarters > 0 || h.missing_quarters > 0 {
        let mut s = format!(
            "{} 15-minute grids, radar only (no gauge correction at this resolution)",
            h.quarters
        );
        if h.missing_quarters > 0 {
            s.push_str(&format!(
                "; {} MISSING — total is low by those periods",
                h.missing_quarters
            ));
        }
        return s;
    }
    let mut parts = vec![format!("{} h final (Pass 2)", h.pass2)];
    if h.pass1 > 0 {
        parts.push(format!("{} h provisional (Pass 1)", h.pass1));
    }
    if h.radar > 0 {
        parts.push(format!("{} h radar-only", h.radar));
    }
    if h.missing > 0 {
        parts.push(format!(
            "{} h MISSING — total is low by those hours",
            h.missing
        ));
    }
    parts.join(", ")
}

// ----------------------------------------------------------------- commands

#[derive(Serialize)]
struct StatusOut {
    availability: Availability,
    latest_local: String,
    pass2_through_local: String,
    region: Region,
    cache: noaa_weather::store::CacheStats,
    /// Days a downloaded grid stays cached; null keeps everything.
    retain_days: Option<i64>,
}

async fn status(wx: &Weather, json: bool) -> Result<()> {
    let a = wx.availability().await?;
    let tz = &wx.cfg.tz;
    let cache = wx.store().stats()?;
    if json {
        let out = StatusOut {
            availability: a,
            latest_local: local_label(a.latest, tz),
            pass2_through_local: local_label(a.pass2_through, tz),
            region: *wx.region(),
            cache,
            retain_days: wx.cfg.retain_days,
        };
        return emit("wx-status", &out);
    }
    let ago = |t: Timestamp| (Timestamp::now().as_second() - t.as_second()) / 60;
    println!("NOAA MRMS (s3://noaa-mrms-pds)");
    println!(
        "  newest hour      {}  ({}, {} min ago)",
        local_label(a.latest, tz),
        a.latest_pass.describe(),
        ago(a.latest)
    );
    println!(
        "  final through    {}  ({} min ago)",
        local_label(a.pass2_through, tz),
        ago(a.pass2_through)
    );
    let r = wx.region();
    println!(
        "Region             {:.2},{:.2},{:.2},{:.2}  ({})",
        r.west,
        r.south,
        r.east,
        r.north,
        wx.cfg.tz.iana_name().unwrap_or("?")
    );
    println!("Cache");
    if cache.products.is_empty() {
        println!("  (empty — run `wx backfill` or any query)");
    }
    for p in &cache.products {
        println!(
            "  {:<5} {:>2} h  {:>5} grids  {:>7.1} MB  {} → {}",
            p.pass,
            p.hours,
            p.grids,
            p.bytes as f64 / 1e6,
            local_label(Timestamp::from_second(p.oldest)?, tz),
            local_label(Timestamp::from_second(p.newest)?, tz),
        );
    }
    println!(
        "  gauges: {} stations, {} daily reports, {} same-day readings",
        cache.gauge_stations, cache.gauge_reports, cache.feed_readings
    );
    match wx.cfg.retain_days {
        Some(days) => println!("  grids are dropped {days} days after download"),
        None => println!("  grids are kept until the file is deleted"),
    }
    Ok(())
}

#[derive(Serialize)]
struct PointOut {
    lat: f64,
    lon: f64,
    mm: Option<f64>,
    inches: Option<f64>,
}

#[derive(Serialize)]
struct RainOut<'a> {
    window: &'a Resolved,
    hours: noaa_weather::HourCounts,
    stats: noaa_weather::grid::GridStats,
    points: Vec<PointOut>,
    loads: noaa_weather::engine::LoadStats,
    attribution: &'static str,
}

async fn rain(ctx: &Ctx, wx: &Weather, spec: &str, points: &[(f64, f64)]) -> Result<()> {
    let json = ctx.json;
    let (r, _, acc) = accumulate(ctx, wx, spec).await?;
    let stats = acc.grid.stats();
    let pts: Vec<PointOut> = points
        .iter()
        .map(|&(lat, lon)| {
            let mm = acc.grid.mm_at(lat, lon);
            PointOut {
                lat,
                lon,
                mm,
                inches: mm.map(mm_to_in),
            }
        })
        .collect();

    if json {
        let out = RainOut {
            window: &r,
            hours: acc.hours,
            stats,
            points: pts,
            loads: acc.loads,
            attribution: mrms::ATTRIBUTION,
        };
        return emit("rain-total", &out);
    }

    let tz = &wx.cfg.tz;
    println!("{}", window_line(&r, tz));
    println!("  {}", provenance_line(&acc));
    println!();
    println!("  Wettest cell   {}", amount(stats.max_mm));
    if let Some([lon, lat]) = stats.max_at {
        println!("                 at {lat:.3}, {lon:.3}");
    }
    println!("  Region mean    {}", amount(stats.mean_mm));
    println!(
        "  Measurable     {:.0}% of the region (≥ 0.01\")",
        stats.wet_fraction * 100.0
    );
    for p in &pts {
        match p.mm {
            Some(mm) => println!("  {:.4}, {:.4}  {}", p.lat, p.lon, amount(mm)),
            None => println!("  {:.4}, {:.4}  outside the region / no data", p.lat, p.lon),
        }
    }
    println!();
    println!(
        "  {} files from cache, {} downloaded · {}",
        acc.loads.from_cache,
        acc.loads.downloaded,
        mrms::ATTRIBUTION
    );
    Ok(())
}

async fn series(
    ctx: &Ctx,
    wx: &Weather,
    spec: &str,
    (lat, lon): (f64, f64),
    step: Step,
) -> Result<()> {
    let json = ctx.json;
    let (mut r, _) = resolve(wx, spec).await?;
    let quarters = step == Step::Quarter || !r.window.is_hour_aligned();
    let pts: Vec<SeriesPoint> = if quarters {
        r.window = wx.to_latest_quarter(r.window).await?;
        wx.quarter_series(r.window, lat, lon, ctx.meter("loading 15-minute MRMS"))
            .await?
    } else {
        wx.series_at(r.window, lat, lon, ctx.meter("loading hourly MRMS"))
            .await?
    };
    let bar_secs = if quarters { 900 } else { 3600 };
    if json {
        return emit(
            "rain-series",
            &json!({
                "window": r, "lat": lat, "lon": lon,
                "step_minutes": bar_secs / 60,
                "points": pts,
                "attribution": mrms::ATTRIBUTION,
            }),
        );
    }

    let tz = &wx.cfg.tz;
    println!("{}", window_line(&r, tz));
    println!(
        "  {} at {lat:.4}, {lon:.4} (the 1 km cell containing it)",
        if quarters {
            "every 15 minutes"
        } else {
            "hourly"
        }
    );
    println!();
    let max = pts.iter().filter_map(|p| p.mm).fold(0.0f64, f64::max);
    let total: f64 = pts.iter().filter_map(|p| p.mm).sum();
    const WIDTH: f64 = 40.0;
    let mut last_day = None;
    for p in &pts {
        // Label the hour by its start, as a person reads it: "2 PM" = 2–3 PM,
        // and the day by that same start, so 11 PM–midnight stays on its day.
        let start = Timestamp::from_second(p.valid.as_second() - bar_secs)?.to_zoned(tz.clone());
        let day = start.strftime("%a %b %-d").to_string();
        let day_col = if last_day.as_ref() != Some(&day) {
            last_day = Some(day.clone());
            day
        } else {
            String::new()
        };
        let hour = start
            .strftime(if quarters { "%l:%M %p" } else { "%l %p" })
            .to_string();
        let (bar, val) = match p.mm {
            Some(mm) => {
                let n = if max > 0.0 { mm / max * WIDTH } else { 0.0 };
                let full = n.floor() as usize;
                let mut b = "█".repeat(full);
                if n - full as f64 >= 0.5 {
                    b.push('▌');
                }
                (b, format!("{:.2}\"", mm_to_in(mm)))
            }
            None => (String::new(), "  —  ".into()),
        };
        let tag = match p.pass {
            Some(mrms::Pass::Pass2) | None => "",
            Some(mrms::Pass::Pass1) => "  provisional",
            Some(mrms::Pass::Radar) => "  radar-only",
        };
        println!("  {day_col:<11}{hour}  {val:>6}  {bar}{tag}");
    }
    println!();
    println!("  Total {}", amount(total));
    Ok(())
}

struct MapOpts {
    scale: usize,
    style: Style,
    fit: bool,
    mercator: bool,
}

async fn map(ctx: &Ctx, wx: &Weather, spec: &str, out: &Path, opts: MapOpts) -> Result<()> {
    let (r, _, acc) = accumulate(ctx, wx, spec).await?;
    let rows = if opts.mercator {
        Rows::WebMercator
    } else {
        Rows::Geographic
    };
    let fit = opts
        .fit
        .then(|| acc.grid.wet_percentiles(0.02, 0.98))
        .flatten();
    let rendered = render::render_png(
        &acc.grid,
        RenderOpts {
            scale: opts.scale,
            rows,
            style: opts.style,
            fit,
            ..RenderOpts::default()
        },
    )?;
    std::fs::write(out, &rendered.png).with_context(|| format!("writing {}", out.display()))?;
    let [w, s, e, n] = rendered.bounds;
    let world_file = render::world_file(&rendered);
    if ctx.json {
        let pgw = world_file.as_ref().map(|_| out.with_extension("pgw"));
        if let (Some(p), Some(wf)) = (&pgw, &world_file) {
            std::fs::write(p, wf)?;
        }
        return emit(
            "rain-map",
            &json!({
                "window": r,
                "hours": acc.hours,
                "path": out,
                "world_file": pgw,
                "width": rendered.width,
                "height": rendered.height,
                "bounds": rendered.bounds,
                "fit_inches": fit,
                "attribution": mrms::ATTRIBUTION,
            }),
        );
    }
    println!("{}", window_line(&r, &wx.cfg.tz));
    println!("  {}", provenance_line(&acc));
    println!(
        "  wrote {} ({}×{}), bounds {w:.3},{s:.3},{e:.3},{n:.3}",
        out.display(),
        rendered.width,
        rendered.height
    );
    if let Some(wf) = world_file {
        let pgw = out.with_extension("pgw");
        std::fs::write(&pgw, wf)?;
        println!("  wrote {} (world file, EPSG:4326)", pgw.display());
    }
    println!("  scale (inches):");
    for bin in noaa_weather::ramp::legend() {
        println!("    {}  {}", bin.color, bin.label);
    }
    Ok(())
}

async fn grid(
    ctx: &Ctx,
    wx: &Weather,
    spec: &str,
    format: GridFormat,
    out: Option<PathBuf>,
) -> Result<()> {
    let (r, _, acc) = accumulate(ctx, wx, spec).await?;
    let g = &acc.grid;
    let mut buf: Vec<u8> = Vec::new();
    match format {
        GridFormat::Csv => {
            writeln!(buf, "lat,lon,mm,inches")?;
            for row in 0..g.geom.ny {
                for col in 0..g.geom.nx {
                    let v = g.tenths[row * g.geom.nx + col];
                    let (lat, lon) = (g.geom.lat(row), g.geom.lon(col));
                    if v == MISSING {
                        writeln!(buf, "{lat:.3},{lon:.3},,")?;
                    } else {
                        let mm = v as f64 / 10.0;
                        writeln!(buf, "{lat:.3},{lon:.3},{mm:.1},{:.3}", mm_to_in(mm))?;
                    }
                }
            }
        }
        GridFormat::Json => {
            let v = json!({
                "schema": "rain-grid/v1",
                "window": r,
                "hours": acc.hours,
                "geom": g.geom,
                "units": "tenths of a millimetre; -1 = no data; rows north→south",
                "tenths": g.tenths,
                "attribution": mrms::ATTRIBUTION,
            });
            serde_json::to_writer(&mut buf, &v)?;
            buf.push(b'\n');
        }
    }
    match out {
        Some(p) => {
            std::fs::write(&p, &buf)?;
            eprintln!("wrote {} ({} cells)", p.display(), g.tenths.len());
        }
        None => std::io::stdout().write_all(&buf)?,
    }
    Ok(())
}

async fn gauges(wx: &Weather, spec: &str, json: bool, refresh: bool) -> Result<()> {
    let (r, _) = resolve(wx, spec).await?;
    if refresh {
        let tz = &wx.cfg.tz;
        let mut years = vec![
            r.window.start.to_zoned(tz.clone()).year(),
            r.window.end.to_zoned(tz.clone()).year(),
        ];
        years.dedup();
        wx.refresh_gauges(&years, true).await?;
        wx.refresh_feeds(r.window, true).await;
    }
    let meter = !json && std::io::stderr().is_terminal();
    if meter {
        eprint!("  reading gauges…");
    }
    let rep = wx.gauges(r.window).await?;
    if meter {
        eprint!("\r\x1b[2K");
    }
    if json {
        return emit("gauges", &rep);
    }

    let tz = &wx.cfg.tz;
    println!("{}", window_line(&r, tz));
    println!(
        "  {} of {} gauges in the region reported inside the window · archive through {}",
        rep.gauges.len(),
        rep.stations_in_region,
        rep.archive_through.as_deref().unwrap_or("—")
    );
    for feed in &rep.feeds {
        if let Some(error) = &feed.error {
            eprintln!(
                "{} could not be read, so its gauges may be behind: {error}",
                feed.name
            );
        }
    }
    if rep.gauges.is_empty() {
        println!();
        println!("  No gauge has a whole report inside this window: a daily report needs its");
        println!("  whole 24 h inside, and an hourly gauge a whole hour. Try 2d or 7d.");
        return Ok(());
    }
    println!();
    let mut rows = rep.gauges.clone();
    rows.sort_by(|a, b| b.gauge_mm.total_cmp(&a.gauge_mm));
    let id_width = rows.iter().map(|g| g.id.len()).max().unwrap_or(0).max(7);
    println!(
        "  {:<id_width$} {:<9} {:<28} {:>7}  {:>7}  {:>7}  {:>7}",
        "station", "network", "name", "reports", "gauge", "MRMS", "diff"
    );
    for g in &rows {
        let (mrms, diff) = match g.mrms_mm {
            Some(m) => (
                format!("{:.2}\"", mm_to_in(m)),
                format!("{:+.2}\"", mm_to_in(m - g.gauge_mm)),
            ),
            None => ("—".into(), "".into()),
        };
        let unit = if g.step_hours == 1 { "h" } else { "d" };
        println!(
            "  {:<id_width$} {:<9} {:<28} {:>7}  {:>6.2}\"{} {:>7}  {:>7}",
            g.id,
            g.network_label,
            g.name.chars().take(28).collect::<String>(),
            format!("{}/{}{unit}", g.reports, g.possible),
            mm_to_in(g.gauge_mm),
            if g.provisional { "*" } else { " " },
            mrms,
            diff
        );
    }
    let paired: Vec<_> = rows
        .iter()
        .filter_map(|g| Some((g.gauge_mm, g.mrms_mm?)))
        .collect();
    if !paired.is_empty() {
        let n = paired.len() as f64;
        let bias = paired.iter().map(|(g, m)| m - g).sum::<f64>() / n;
        let mae = paired.iter().map(|(g, m)| (m - g).abs()).sum::<f64>() / n;
        println!();
        println!(
            "  MRMS − gauge across {} gauges: mean {:+.2}\", mean absolute {:.2}\"",
            paired.len(),
            mm_to_in(bias),
            mm_to_in(mae)
        );
    }
    println!();
    println!("  A daily gauge counts only reports whose whole 24 h is inside the window, and");
    println!("  an hourly gauge its longest unbroken stretch. MRMS is summed over exactly");
    println!("  those hours at the gauge's 1 km cell. In the reports column, d counts days");
    println!("  and h counts hours.");
    if rows.iter().any(|g| g.provisional) {
        println!("  * Provisional: read from the network directly, not yet through the archive.");
    }
    println!("  {} · {}", ghcn::ATTRIBUTION, mrms::ATTRIBUTION);
    Ok(())
}

async fn backfill(ctx: &Ctx, wx: &Weather, days: i64) -> Result<()> {
    if !ctx.quiet {
        eprintln!("backfilling {days} days of hourly MRMS into the cache…");
    }
    let n = wx
        .warm(days * 24, ctx.meter("fetching hourly MRMS"))
        .await?;
    if ctx.json {
        return emit("backfill", &json!({ "days": days, "downloaded": n }));
    }
    if !ctx.quiet {
        eprintln!("done: {n} files downloaded");
    }
    status(wx, false).await
}

async fn radar(
    ctx: &Ctx,
    wx: &Weather,
    point: Option<(f64, f64)>,
    count: usize,
    out: Option<PathBuf>,
) -> Result<()> {
    let frames = wx.rate_frames(count, ctx.meter("loading radar")).await?;
    let Some(newest) = frames.last() else {
        bail!("no radar frames were readable");
    };
    let rate_in =
        |f: &noaa_weather::RateFrame, (lat, lon): (f64, f64)| f.grid.mm_at(lat, lon).map(mm_to_in);
    let mut written = None;
    if let Some(path) = &out {
        let rendered = render::render_png(
            &newest.grid,
            RenderOpts {
                scale: 4,
                rows: Rows::Geographic,
                style: Style::Banded,
                ramp: ramp::RATE,
                fit: None,
            },
        )?;
        std::fs::write(path, &rendered.png)
            .with_context(|| format!("writing {}", path.display()))?;
        if let Some(wf) = render::world_file(&rendered) {
            std::fs::write(path.with_extension("pgw"), wf)?;
        }
        written = Some(path.clone());
    }
    let rows: Vec<Value> = frames
        .iter()
        .map(|f| {
            let mut row = json!({
                "valid": f.valid,
                "valid_local": local_label(f.valid, &wx.cfg.tz),
                "wet_fraction": f.grid.stats().wet_fraction,
                "max_in_per_h": mm_to_in(f.grid.stats().max_mm),
            });
            if let Some(p) = point {
                row["at_in_per_h"] = json!(rate_in(f, p));
            }
            row
        })
        .collect();
    if ctx.json {
        return emit(
            "radar",
            &json!({
                "lat": point.map(|p| p.0),
                "lon": point.map(|p| p.1),
                "frames": rows,
                "image": written,
                "attribution": mrms::ATTRIBUTION,
            }),
        );
    }
    println!("Radar rain rate, 2-minute frames (MRMS PrecipRate, radar only)");
    for row in &rows {
        let label = row["valid_local"].as_str().unwrap_or_default();
        let max = row["max_in_per_h"].as_f64().unwrap_or(0.0);
        match row.get("at_in_per_h") {
            Some(v) if v.is_number() => println!(
                "  {label}   here {:.2} in/h · region peak {max:.2} in/h",
                v.as_f64().unwrap_or(0.0)
            ),
            Some(_) => println!("  {label}   no data here · region peak {max:.2} in/h"),
            None => println!("  {label}   region peak {max:.2} in/h"),
        }
    }
    if let Some(p) = written {
        println!("  wrote {}", p.display());
    }
    Ok(())
}

fn alert_rows(alerts: &[nws::Alert]) -> Vec<Value> {
    alerts
        .iter()
        .map(|a| {
            json!({
                "event": a.event,
                "severity": a.severity,
                "area": a.area,
                "ends": a.ends.as_ref().or(a.expires.as_ref()),
            })
        })
        .collect()
}

async fn alerts(ctx: &Ctx, at: Option<String>, area: Option<String>) -> Result<()> {
    let query = match (at, area) {
        (Some(s), _) => {
            let (lat, lon) = ctx.point(&s)?;
            AlertQuery::Point(lat, lon)
        }
        (None, area) => match area.or_else(|| ctx.user.alert_area.clone()) {
            Some(a) => AlertQuery::Area(a),
            None => {
                return Err(CliError::Usage(
                    "nothing says where to look: pass --at or --area, or set one with `wx config set alert_area FL`"
                        .into(),
                )
                .into())
            }
        },
    };
    let list = ctx.nws()?.alerts(&query).await?;
    if ctx.json {
        return emit(
            "alerts",
            &json!({ "alerts": list, "attribution": nws::ATTRIBUTION }),
        );
    }
    if list.is_empty() {
        println!("No active alerts.");
        return Ok(());
    }
    output::table(&alert_rows(&list));
    println!();
    println!("{}", nws::ATTRIBUTION);
    Ok(())
}

async fn forecast(ctx: &Ctx, (lat, lon): (f64, f64), hourly: bool, limit: usize) -> Result<()> {
    let mut f = ctx.nws()?.forecast(lat, lon, hourly).await?;
    f.periods.truncate(limit.max(1));
    if ctx.json {
        return emit("forecast", &f);
    }
    if let Some(l) = &f.location {
        println!("Forecast near {l}");
    }
    for p in &f.periods {
        let name = if hourly {
            p.start
                .parse::<Timestamp>()
                .map(|t| t.to_zoned(ctx.tz.clone()).strftime("%a %-I %p").to_string())
                .unwrap_or_else(|_| p.start.clone())
        } else {
            p.name.clone()
        };
        let temp = p
            .temperature_f
            .map_or("—".to_string(), |t| format!("{t:.0}°F"));
        let rain = p
            .precip_chance_pct
            .map_or(String::new(), |c| format!("  {c:.0}% rain"));
        println!("  {name:<18} {temp:>5}  {}{rain}", p.short);
    }
    println!();
    println!("{}", nws::ATTRIBUTION);
    Ok(())
}

async fn conditions(ctx: &Ctx, (lat, lon): (f64, f64)) -> Result<()> {
    let c = ctx.nws()?.conditions(lat, lon).await?;
    if ctx.json {
        return emit("conditions", &c);
    }
    output::kv(&serde_json::to_value(&c)?, 0);
    Ok(())
}

fn places(ctx: &Ctx, cmd: PlacesCmd) -> Result<()> {
    let mut user = ctx.user.clone();
    match cmd {
        PlacesCmd::List => {
            let rows: Vec<Value> = user
                .places
                .iter()
                .map(|p| json!({ "name": p.name, "lat": p.lat, "lon": p.lon }))
                .collect();
            output::emit_list(ctx.json, "place", rows, &["name", "lat", "lon"]);
        }
        PlacesCmd::Add { name, point } => {
            let (lat, lon) = parse_point(&point)?;
            let place = Place { name, lat, lon };
            user.upsert_place(place.clone());
            user.save(&ctx.config_path)?;
            output::emit_one(ctx.json, "place", serde_json::to_value(&place)?);
        }
        PlacesCmd::Remove { name } => {
            if !user.remove_place(&name) {
                return Err(CliError::NotFound(format!("no saved place named {name:?}")).into());
            }
            user.save(&ctx.config_path)?;
            if ctx.json {
                emit("place-removed", &json!({ "name": name }))?;
            } else if !ctx.quiet {
                eprintln!("removed {name}");
            }
        }
    }
    Ok(())
}

fn config_cmd(ctx: &Ctx, cmd: ConfigCmd) -> Result<()> {
    let path = &ctx.config_path;
    let mut user = ctx.user.clone();
    match cmd {
        ConfigCmd::Path => {
            if ctx.json {
                emit("config-path", &json!({ "path": path }))?;
            } else {
                println!("{}", path.display());
            }
        }
        ConfigCmd::Show => {
            let v = serde_json::to_value(&user)?;
            if ctx.json {
                output::json(&output::tagged("config", v));
            } else {
                output::kv(&v, 0);
            }
        }
        ConfigCmd::Init => {
            if !path.exists() {
                user.save(path)?;
            }
            if ctx.json {
                emit("config-path", &json!({ "path": path }))?;
            } else if !ctx.quiet {
                eprintln!("config at {}", path.display());
            }
        }
        ConfigCmd::Set { key, value } => {
            match key.as_str() {
                "region" => {
                    Region::parse(&value)?;
                    user.region = Some(value);
                }
                "tz" => {
                    region_tz(&value).map_err(|e| CliError::Usage(e.to_string()))?;
                    user.tz = Some(value);
                }
                "db" => user.db = Some(PathBuf::from(value)),
                "alert_area" => user.alert_area = Some(value),
                "retain_days" => {
                    let days = value.parse().ok().filter(|d: &i64| *d >= 0);
                    user.retain_days = Some(days.ok_or_else(|| {
                        CliError::Usage(format!(
                            "retain_days must be a whole number of days, or 0 to keep \
                             everything; got {value:?}"
                        ))
                    })?);
                }
                other => {
                    return Err(CliError::Usage(format!(
                        "unknown key {other:?}; keys: {CONFIG_KEYS}"
                    ))
                    .into())
                }
            }
            user.save(path)?;
            if ctx.json {
                output::json(&output::tagged("config", serde_json::to_value(&user)?));
            }
        }
        ConfigCmd::Unset { key } => {
            match key.as_str() {
                "region" => user.region = None,
                "tz" => user.tz = None,
                "db" => user.db = None,
                "alert_area" => user.alert_area = None,
                "retain_days" => user.retain_days = None,
                other => {
                    return Err(CliError::Usage(format!(
                        "unknown key {other:?}; keys: {CONFIG_KEYS}"
                    ))
                    .into())
                }
            }
            user.save(path)?;
            if ctx.json {
                output::json(&output::tagged("config", serde_json::to_value(&user)?));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn failures_map_to_the_family_exit_codes() {
        let upstream = anyhow::Error::new(noaa_weather::UpstreamError("HTTP 500".into()))
            .context("reading the forecast");
        assert!(matches!(to_cli_error(&upstream), CliError::Upstream(_)));

        let uncovered = anyhow::Error::new(nws::NoForecast {
            lat: 51.0,
            lon: 0.0,
        });
        assert!(matches!(to_cli_error(&uncovered), CliError::NotFound(_)));
    }
}

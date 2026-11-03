//! `wx mcp`: rainfall, forecasts, conditions and alerts as MCP tools (cli-common
//! DESIGN.md §1.9, the MCP profile), for Claude Code, Claude Desktop and any MCP client.
//!
//! Four read-only tools over the library the commands use. Everything they read is
//! anonymous public data, so there is no credential here, and no tool can write
//! anything but the grid cache. The one secret this binary can touch is the bearer
//! token of `mcp --http`, held by `pk-cli-mcp`.
//!
//! # A saved place stays on the computer
//!
//! A saved place can be someone's home. A tool takes its name and sends its coordinates
//! only upstream, as a command would. No result or error carries them: a failure names
//! the place as the caller did, and never repeats the library's own message, which holds
//! coordinates, the request's URL or the cache's path. A forecast's nearest town and a
//! station's name and distance do say roughly where a place is, and `SECURITY.md` says so.
//!
//! # No Tokio on the calling thread
//!
//! The library is async on a reactor and `pk-cli-mcp` polls a call with none, so each
//! call blocks on the one runtime in [`runtime`]. That is also why `main` starts this
//! server before it starts a runtime of its own.
//!
//! The config is read on each call, so `wx places add` needs no restart, and listing
//! the tools reads nothing.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Result;
use jiff::{tz::TimeZone, Timestamp};
use pk_cli_core::CliError;
use pk_cli_mcp::{
    parse_args, ListArgs, Page, PageRequest, ResponseFormat, Server, ToolDef, ToolError,
    ToolOutput, Toolset,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::{HourCounts, SeriesPoint, MAX_QUARTER_HOURS, MAX_SERIES_HOURS};
use crate::grid::{mm_to_in, GridStats, Region, MRMS_DOMAIN};
use crate::nws::{self, Alert, AlertQuery, Conditions, Forecast, NwsClient, Period};
use crate::places::{parse_point, UserConfig};
use crate::window::{local_label, region_tz, Resolved, Window, WindowError};
use crate::{Config, Pass, Store, Weather, WindowSpec};

pub const RAIN_TOOL: &str = "wx_rain";
pub const FORECAST_TOOL: &str = "wx_forecast";
pub const CONDITIONS_TOOL: &str = "wx_conditions";
pub const ALERTS_TOOL: &str = "wx_alerts";

const DEFAULT_WINDOW: &str = "24h";
const ADD_A_PLACE: &str = "wx places add <name> <lat,lon>";

/// Where a tool's settings come from: `wx`'s global flags and environment, then the
/// config file on each call.
#[derive(Debug, Clone)]
pub struct Settings {
    pub config_path: PathBuf,
    pub db: Option<PathBuf>,
    pub region: Option<String>,
    pub tz: Option<String>,
    pub concurrency: usize,
    pub nws_url: String,
}

/// The settings one call runs under.
#[derive(Debug, Clone)]
pub struct Setup {
    pub user: UserConfig,
    /// The configured area to cache, when one is.
    pub region: Option<Region>,
    pub tz: TimeZone,
}

/// A rainfall total at one point.
#[derive(Debug, Clone)]
pub struct RainReading {
    pub resolved: Resolved,
    pub hours: HourCounts,
    pub mm: Option<f64>,
    /// The area around the point, when the whole grid was read.
    pub stats: Option<GridStats>,
}

/// Rainfall at one point, bar by bar.
#[derive(Debug, Clone)]
pub struct BarsReading {
    pub resolved: Resolved,
    pub quarters: bool,
    pub bars: Vec<SeriesPoint>,
}

/// What a tool needs from NOAA and from this computer. The library, in [`Live`];
/// canned answers in the tests.
pub trait Source {
    fn setup(&self) -> Result<Setup, ToolError>;
    fn rain(&self, setup: &Setup, spot: (f64, f64), spec: &WindowSpec) -> Result<RainReading>;
    fn bars(
        &self,
        setup: &Setup,
        spot: (f64, f64),
        spec: &WindowSpec,
        quarters: bool,
    ) -> Result<BarsReading>;
    fn forecast(&self, spot: (f64, f64), hourly: bool) -> Result<Forecast>;
    fn conditions(&self, spot: (f64, f64)) -> Result<Conditions>;
    fn alerts(&self, query: &AlertQuery) -> Result<Vec<Alert>>;
}

/// The one runtime the library's futures run on. Never dropped: dropping a runtime
/// where a server's shutdown may be in async code panics.
fn runtime() -> std::io::Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    Ok(RUNTIME.get_or_init(|| rt))
}

/// The real source.
pub struct Live {
    settings: Settings,
}

impl Live {
    pub fn new(settings: Settings) -> Self {
        Self { settings }
    }

    /// A configured region that holds the point, since its grids may be cached, or
    /// else a box around the point, as a command given only `--at` uses.
    fn region(&self, setup: &Setup, (lat, lon): (f64, f64)) -> Result<Region> {
        let region = setup
            .region
            .filter(|r| r.contains(lat, lon))
            .or_else(|| Region::around(&[(lat, lon)], 0.5))
            .expect("a box around one point exists");
        if region.intersect(&MRMS_DOMAIN).is_none() {
            return Err(CliError::NotFound(format!(
                "MRMS rainfall covers longitude {} to {} and latitude {} to {}",
                MRMS_DOMAIN.west, MRMS_DOMAIN.east, MRMS_DOMAIN.south, MRMS_DOMAIN.north
            ))
            .into());
        }
        Ok(region)
    }

    fn weather(&self, setup: &Setup, spot: (f64, f64)) -> Result<Weather> {
        let path = self
            .settings
            .db
            .clone()
            .or_else(|| setup.user.db.clone())
            .unwrap_or_else(crate::store::default_db_path);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let cfg = Config {
            tz: setup.tz.clone(),
            concurrency: self.settings.concurrency,
            retain_days: setup.user.grid_retention(),
            ..Config::new(self.region(setup, spot)?)
        };
        Weather::new(cfg, Store::open(&path)?)
    }

    fn nws(&self) -> Result<NwsClient> {
        NwsClient::new(self.settings.nws_url.as_str(), None)
    }
}

impl Source for Live {
    fn setup(&self) -> Result<Setup, ToolError> {
        let broken = |what: &str| {
            ToolError::other(format!("the wx {what} cannot be used")).with_fix(
                "Ask the user to run `wx config show` in a terminal and repair it, then call \
                 the tool again",
            )
        };
        let user = UserConfig::load(&self.settings.config_path).map_err(|_| broken("config"))?;
        let region = self
            .settings
            .region
            .clone()
            .or_else(|| user.region.clone())
            .map(|r| Region::parse(&r))
            .transpose()
            .map_err(|_| broken("region setting"))?;
        let tz = match self.settings.tz.clone().or_else(|| user.tz.clone()) {
            Some(name) => region_tz(&name).map_err(|_| broken("time zone setting"))?,
            None => TimeZone::system(),
        };
        Ok(Setup { user, region, tz })
    }

    fn rain(
        &self,
        setup: &Setup,
        (lat, lon): (f64, f64),
        spec: &WindowSpec,
    ) -> Result<RainReading> {
        let wx = self.weather(setup, (lat, lon))?;
        runtime()?.block_on(async {
            let (resolved, _) = wx.resolve(spec).await?;
            let acc = wx.accumulate(resolved.window, None).await?;
            Ok(RainReading {
                resolved,
                hours: acc.hours,
                mm: acc.grid.mm_at(lat, lon),
                stats: Some(acc.grid.stats()),
            })
        })
    }

    fn bars(
        &self,
        setup: &Setup,
        (lat, lon): (f64, f64),
        spec: &WindowSpec,
        quarters: bool,
    ) -> Result<BarsReading> {
        let wx = self.weather(setup, (lat, lon))?;
        runtime()?.block_on(async {
            let (mut resolved, _) = wx.resolve(spec).await?;
            let quarters = quarters || !resolved.window.is_hour_aligned();
            check_bars_window(resolved.window, quarters)?;
            let bars = if quarters {
                resolved.window = wx.to_latest_quarter(resolved.window).await?;
                wx.quarter_series(resolved.window, lat, lon, None).await?
            } else {
                wx.series_at(resolved.window, lat, lon, None).await?
            };
            Ok(BarsReading {
                resolved,
                quarters,
                bars,
            })
        })
    }

    fn forecast(&self, (lat, lon): (f64, f64), hourly: bool) -> Result<Forecast> {
        let client = self.nws()?;
        runtime()?.block_on(client.forecast(lat, lon, hourly))
    }

    fn conditions(&self, (lat, lon): (f64, f64)) -> Result<Conditions> {
        let client = self.nws()?;
        runtime()?.block_on(client.conditions(lat, lon))
    }

    fn alerts(&self, query: &AlertQuery) -> Result<Vec<Alert>> {
        let client = self.nws()?;
        runtime()?.block_on(client.alerts(query))
    }
}

/// The library's own limits on a series, as a usage error rather than a plain failure.
fn check_bars_window(window: Window, quarters: bool) -> Result<(), CliError> {
    if quarters && window.minutes() > MAX_QUARTER_HOURS * 60 {
        return Err(CliError::Usage(format!(
            "15-minute bars are limited to {MAX_QUARTER_HOURS} hours"
        )));
    }
    if !quarters && window.hours() > MAX_SERIES_HOURS {
        return Err(CliError::Usage(format!(
            "hourly bars are limited to {MAX_SERIES_HOURS} hours"
        )));
    }
    Ok(())
}

/// A failure as the model sees it. `at` is the place as the caller named it, and is the
/// only thing here that says where: the library's own messages hold coordinates, URLs
/// and the cache's path, so only a window's and a usage error's are repeated.
fn tool_error(e: anyhow::Error, at: &str) -> ToolError {
    if let Some(c) = e.downcast_ref::<CliError>() {
        return match c {
            CliError::NotFound(message) => {
                ToolError::not_found(format!("{at} is outside the rainfall grid. {message}"))
                    .with_fix(
                        "wx_forecast and wx_alerts also reach Alaska, Hawaii and nearby waters",
                    )
            }
            other => ToolError::from(other.clone()),
        };
    }
    if let Some(w) = e.downcast_ref::<WindowError>() {
        return match w {
            WindowError::Empty { .. } => ToolError::not_found(w.to_string())
                .with_fix("Ask for a window that ends earlier, such as yesterday"),
            _ => ToolError::usage(w.to_string()),
        };
    }
    if e.downcast_ref::<nws::NoForecast>().is_some() {
        return ToolError::not_found(format!(
            "NWS has no forecast for {at}: it covers US land and nearby waters only"
        ))
        .with_fix("Check the place with the user");
    }
    if e.downcast_ref::<nws::NoObservation>().is_some() {
        return ToolError::not_found(format!("no station near {at} has a recent observation"))
            .with_fix("wx_forecast still answers for this place");
    }
    let status = e
        .chain()
        .find_map(|c| c.downcast_ref::<reqwest::Error>())
        .and_then(reqwest::Error::status);
    if status.is_some_and(|s| s.is_client_error() && s.as_u16() != 429) {
        return ToolError::not_found(format!("NWS has nothing for {at}"))
            .with_fix("Check the place with the user: NWS covers US land and nearby waters");
    }
    let upstream = e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>().is_some()
            || c.downcast_ref::<crate::UpstreamError>().is_some()
    });
    if upstream {
        return ToolError::upstream("NOAA or the National Weather Service did not answer");
    }
    ToolError::other("wx could not read or write its grid cache, or could not start a client")
        .with_fix("If it persists, ask the user to run `wx status` in a terminal")
}

/// Which point `at` means: a saved place's name, or `lat,lon`.
fn spot(at: &str, user: &UserConfig) -> Result<(f64, f64), ToolError> {
    let at = at.trim();
    if at.is_empty() {
        return Err(ToolError::usage(
            "at must name a saved place or give lat,lon",
        ));
    }
    if let Some(p) = user.place(at) {
        return Ok((p.lat, p.lon));
    }
    if at.contains(',') {
        return parse_point(at).map_err(ToolError::usage);
    }
    let names: Vec<&str> = user.places.iter().map(|p| p.name.as_str()).collect();
    let fix = if names.is_empty() {
        format!(
            "No place is saved. Pass the point as lat,lon, or ask the user to run \
             `{ADD_A_PLACE}` in a terminal"
        )
    } else {
        format!(
            "Saved places: {}. Pass one of those, or the point as lat,lon",
            names.join(", ")
        )
    };
    Err(ToolError::not_found(format!("no saved place is named {at:?}")).with_fix(fix))
}

fn window_spec(window: Option<&str>, setup: &Setup) -> Result<WindowSpec, ToolError> {
    let window = window
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .unwrap_or(DEFAULT_WINDOW);
    WindowSpec::parse(window, &setup.tz).map_err(|e| {
        ToolError::usage(format!(
            "{e}. A window is 45m, 6h, 3d, 2w, today, yesterday, 2026-10-28..2026-10-30 or \
             2026-11-01T08:00..now"
        ))
    })
}

fn inches(mm: f64) -> f64 {
    (mm_to_in(mm) * 100.0).round() / 100.0
}

fn tenth(mm: f64) -> f64 {
    (mm * 10.0).round() / 10.0
}

// ---- wx_rain ----------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RainArgs {
    /// A saved place's name, or lat,lon.
    pub at: String,
    /// 45m, 6h, 3d, 2w, today, yesterday, 2026-10-28..2026-10-30 or 2026-11-01T08:00..now.
    /// Default 24h, at most 92 days, or 14 days at step 1h, or 48 hours at step 15m.
    #[serde(default)]
    pub window: Option<String>,
    /// 1h, or 15m (radar only), to fill items with a bar per step. Default: no bars.
    #[serde(default)]
    pub step: Option<String>,
    #[serde(flatten)]
    pub list: ListArgs,
}

#[derive(Serialize)]
struct RainFilter<'a> {
    at: &'a str,
    window: Option<&'a str>,
    step: Option<&'a str>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct BarItem {
    /// When this bar starts, local time.
    pub from: String,
    /// Null when no product covers this bar.
    pub inches: Option<f64>,
    /// final, provisional, radar-only or no data.
    pub quality: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mm: Option<f64>,
    /// When this bar ends, UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RainOut {
    pub from: String,
    pub to: String,
    /// The window was cut short at the newest data.
    pub cut_short: bool,
    /// The 1 km cell over the place, over the whole window. Null when it has no data.
    pub inches: Option<f64>,
    /// Hours from the final, gauge-corrected analysis.
    pub final_hours: u32,
    /// Hours from the provisional gauge-corrected analysis.
    pub provisional_hours: u32,
    /// Hours from radar alone.
    pub radar_hours: u32,
    /// Hours no product covers. The total leaves them out.
    pub missing_hours: u32,
    /// 15-minute radar grids in a sub-hour total.
    pub radar_quarters: u32,
    pub missing_quarters: u32,
    /// True when the total can still change.
    pub provisional: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mm: Option<f64>,
    /// The wettest cell in the area around the place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub area_wettest_inches: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub area_mean_inches: Option<f64>,
    #[serde(flatten)]
    pub page: Page<BarItem>,
}

fn rain_note(hours: HourCounts, mm: Option<f64>) -> Option<String> {
    let mut notes = Vec::new();
    if mm.is_none() {
        notes.push("This place's cell has no data for this window.");
    }
    if hours.quarters > 0 || hours.missing_quarters > 0 {
        notes.push("A sub-hour total is radar only, with no gauge correction.");
    }
    if hours.pass1 + hours.radar > 0 {
        notes.push("Some hours are not final yet, so this total can change.");
    }
    if hours.missing > 0 || hours.missing_quarters > 0 {
        notes.push("The total is low by the periods no product covers.");
    }
    (!notes.is_empty()).then(|| notes.join(" "))
}

fn rain_out(r: &RainReading, page: Page<BarItem>, tz: &TimeZone, detailed: bool) -> RainOut {
    let h = r.hours;
    let area = |v: Option<&GridStats>, read: fn(&GridStats) -> f64| {
        v.filter(|_| detailed).map(|s| inches(read(s)))
    };
    RainOut {
        from: local_label(r.resolved.window.start, tz),
        to: local_label(r.resolved.window.end, tz),
        cut_short: r.resolved.clipped,
        inches: r.mm.map(inches),
        final_hours: h.pass2,
        provisional_hours: h.pass1,
        radar_hours: h.radar,
        missing_hours: h.missing,
        radar_quarters: h.quarters,
        missing_quarters: h.missing_quarters,
        provisional: h.provisional() > 0 || h.quarters > 0,
        note: rain_note(h, r.mm),
        mm: r.mm.map(tenth).filter(|_| detailed),
        area_wettest_inches: area(r.stats.as_ref(), |s| s.max_mm),
        area_mean_inches: area(r.stats.as_ref(), |s| s.mean_mm),
        page,
    }
}

/// The total a run of bars adds up to, with the same counts a whole-window total has.
fn sum_bars(reading: &BarsReading) -> RainReading {
    let mut hours = HourCounts::default();
    for bar in &reading.bars {
        match (reading.quarters, bar.mm.is_some(), bar.pass) {
            (true, true, _) => hours.quarters += 1,
            (true, false, _) => hours.missing_quarters += 1,
            (false, true, Some(Pass::Pass2)) => hours.pass2 += 1,
            (false, true, Some(Pass::Pass1)) => hours.pass1 += 1,
            (false, true, Some(Pass::Radar)) => hours.radar += 1,
            (false, _, _) => hours.missing += 1,
        }
    }
    let mut known = reading.bars.iter().filter_map(|b| b.mm).peekable();
    let mm = known.peek().is_some().then(|| known.sum());
    RainReading {
        resolved: reading.resolved.clone(),
        hours,
        mm,
        stats: None,
    }
}

fn quality(pass: Option<Pass>) -> &'static str {
    match pass {
        Some(Pass::Pass2) => "final",
        Some(Pass::Pass1) => "provisional",
        Some(Pass::Radar) => "radar-only",
        None => "no data",
    }
}

fn bar_item(p: &SeriesPoint, bar_secs: i64, tz: &TimeZone, detailed: bool) -> BarItem {
    let start = Timestamp::from_second(p.valid.as_second() - bar_secs).unwrap_or(p.valid);
    BarItem {
        from: local_label(start, tz),
        inches: p.mm.map(inches),
        quality: quality(p.pass.filter(|_| p.mm.is_some())).to_string(),
        mm: p.mm.map(tenth).filter(|_| detailed),
        ends: detailed.then(|| p.valid.to_string()),
    }
}

// ---- wx_forecast ------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForecastArgs {
    /// A saved place's name, or lat,lon.
    pub at: String,
    /// Hour by hour instead of NWS's twice-daily periods. Default false.
    #[serde(default)]
    pub hourly: bool,
    #[serde(flatten)]
    pub list: ListArgs,
}

#[derive(Serialize)]
struct ForecastFilter<'a> {
    at: &'a str,
    hourly: bool,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct PeriodItem {
    /// Such as Tonight, or the local hour.
    pub name: String,
    pub temperature_f: Option<f64>,
    pub precip_chance_pct: Option<f64>,
    pub wind: Option<String>,
    pub short: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detailed: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ForecastOut {
    /// The nearest town NWS names.
    pub location: Option<String>,
    pub updated: Option<String>,
    #[serde(flatten)]
    pub page: Page<PeriodItem>,
}

fn period_item(p: Period, hourly: bool, tz: &TimeZone, detailed: bool) -> PeriodItem {
    let name = if hourly {
        p.start
            .parse::<Timestamp>()
            .map(|t| t.to_zoned(tz.clone()).strftime("%a %-I %p").to_string())
            .unwrap_or_else(|_| p.start.clone())
    } else {
        p.name
    };
    PeriodItem {
        name,
        temperature_f: p.temperature_f,
        precip_chance_pct: p.precip_chance_pct,
        wind: p.wind,
        short: p.short,
        start: detailed.then_some(p.start),
        end: detailed.then_some(p.end),
        detailed: p.detailed.filter(|_| detailed),
    }
}

// ---- wx_conditions ----------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConditionsArgs {
    /// A saved place's name, or lat,lon.
    pub at: String,
    /// concise (default), or detailed to add the station id, dewpoint, pressure and
    /// visibility.
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct ConditionsOut {
    pub station_name: Option<String>,
    pub distance_mi: Option<f64>,
    pub observed_at: String,
    pub summary: Option<String>,
    pub temperature_f: Option<f64>,
    pub humidity_pct: Option<f64>,
    pub wind_mph: Option<f64>,
    pub wind_gust_mph: Option<f64>,
    pub precip_last_hour_in: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dewpoint_f: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility_mi: Option<f64>,
}

fn conditions_out(c: Conditions, detailed: bool) -> ConditionsOut {
    ConditionsOut {
        station_name: c.station_name,
        distance_mi: c.distance_mi,
        observed_at: c.observed_at,
        summary: c.summary,
        temperature_f: c.temperature_f,
        humidity_pct: c.humidity_pct,
        wind_mph: c.wind_mph,
        wind_gust_mph: c.wind_gust_mph,
        precip_last_hour_in: c.precip_last_hour_in,
        station: detailed.then_some(c.station),
        dewpoint_f: c.dewpoint_f.filter(|_| detailed),
        pressure_mb: c.pressure_mb.filter(|_| detailed),
        visibility_mi: c.visibility_mi.filter(|_| detailed),
    }
}

// ---- wx_alerts --------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AlertsArgs {
    /// A saved place's name, or lat,lon. Leave out for area.
    #[serde(default)]
    pub at: Option<String>,
    /// A state or marine area code, such as FL. Default: the user's alert_area setting.
    #[serde(default)]
    pub area: Option<String>,
    #[serde(flatten)]
    pub list: ListArgs,
}

#[derive(Serialize)]
struct AlertsFilter<'a> {
    at: Option<&'a str>,
    area: Option<&'a str>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct AlertItem {
    pub event: String,
    pub severity: String,
    pub headline: Option<String>,
    pub area: Option<String>,
    /// When it ends, or else when this notice expires.
    pub ends: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub onset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AlertsOut {
    /// The place or area asked about.
    pub at: String,
    #[serde(flatten)]
    pub page: Page<AlertItem>,
}

fn alert_item(a: Alert, detailed: bool) -> AlertItem {
    AlertItem {
        ends: a.ends.or(a.expires),
        event: a.event,
        severity: a.severity,
        headline: a.headline,
        area: a.area,
        id: detailed.then_some(a.id),
        urgency: a.urgency.filter(|_| detailed),
        onset: a.onset.filter(|_| detailed),
        description: a.description.filter(|_| detailed),
        instruction: a.instruction.filter(|_| detailed),
    }
}

/// NWS's area codes are two letters. The code goes into a URL, so nothing else does.
fn area_code(area: &str) -> Result<String, ToolError> {
    let code = area.trim();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        Ok(code.to_ascii_uppercase())
    } else {
        Err(ToolError::usage(format!(
            "area must be a two-letter state or marine area code such as FL, got {code:?}"
        )))
    }
}

// ---- the toolset ------------------------------------------------------------------

/// The toolset `wx mcp` serves.
pub struct WxTools<S: Source> {
    source: S,
}

impl<S: Source> WxTools<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    fn rain(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: RainArgs = parse_args(args)?;
        let step = args
            .step
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let page = PageRequest::new(
            &args.list,
            &RainFilter {
                at: args.at.trim(),
                window: args.window.as_deref(),
                step,
            },
        )?;
        let quarters = match step {
            None | Some("1h") => false,
            Some("15m") => true,
            Some(other) => {
                return Err(ToolError::usage(format!(
                    "step must be 1h or 15m, got {other:?}"
                )))
            }
        };
        let setup = self.source.setup()?;
        let point = spot(&args.at, &setup.user)?;
        let spec = window_spec(args.window.as_deref(), &setup)?;
        let at = args.at.trim();
        let detailed = args.list.format().is_detailed();
        if step.is_none() {
            let reading = self
                .source
                .rain(&setup, point, &spec)
                .map_err(|e| tool_error(e, at))?;
            let none: Page<BarItem> = page.page(Vec::new(), "window")?;
            return ToolOutput::new(&rain_out(&reading, none, &setup.tz, detailed));
        }
        let bars = self
            .source
            .bars(&setup, point, &spec, quarters)
            .map_err(|e| tool_error(e, at))?;
        let bar_secs = if bars.quarters { 900 } else { 3600 };
        let items: Vec<BarItem> = bars
            .bars
            .iter()
            .map(|b| bar_item(b, bar_secs, &setup.tz, detailed))
            .collect();
        ToolOutput::new(&rain_out(
            &sum_bars(&bars),
            page.page(items, "window")?,
            &setup.tz,
            detailed,
        ))
    }

    fn forecast(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ForecastArgs = parse_args(args)?;
        let page = PageRequest::new(
            &args.list,
            &ForecastFilter {
                at: args.at.trim(),
                hourly: args.hourly,
            },
        )?;
        let setup = self.source.setup()?;
        let point = spot(&args.at, &setup.user)?;
        let forecast = self
            .source
            .forecast(point, args.hourly)
            .map_err(|e| tool_error(e, args.at.trim()))?;
        let detailed = args.list.format().is_detailed();
        let items: Vec<PeriodItem> = forecast
            .periods
            .into_iter()
            .map(|p| period_item(p, args.hourly, &setup.tz, detailed))
            .collect();
        ToolOutput::new(&ForecastOut {
            location: forecast.location,
            updated: forecast.updated,
            page: page.page(items, "hourly=false")?,
        })
    }

    fn conditions(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: ConditionsArgs = parse_args(args)?;
        let setup = self.source.setup()?;
        let point = spot(&args.at, &setup.user)?;
        let c = self
            .source
            .conditions(point)
            .map_err(|e| tool_error(e, args.at.trim()))?;
        let detailed = args.response_format.unwrap_or_default().is_detailed();
        ToolOutput::new(&conditions_out(c, detailed))
    }

    fn alerts(&self, args: Value) -> Result<ToolOutput, ToolError> {
        let args: AlertsArgs = parse_args(args)?;
        let at = args.at.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let area = args
            .area
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let page = PageRequest::new(&args.list, &AlertsFilter { at, area })?;
        if at.is_some() && area.is_some() {
            return Err(ToolError::usage("pass at or area, not both"));
        }
        let setup = self.source.setup()?;
        let (asked, query) = match (at, area) {
            (Some(at), _) => {
                let (lat, lon) = spot(at, &setup.user)?;
                (at.to_string(), AlertQuery::Point(lat, lon))
            }
            (None, Some(area)) => {
                let code = area_code(area)?;
                (code.clone(), AlertQuery::Area(code))
            }
            (None, None) => {
                let Some(saved) = setup.user.alert_area.as_deref() else {
                    return Err(ToolError::usage(
                        "nothing says where to look: pass at or area",
                    ));
                };
                let code = area_code(saved)?;
                (code.clone(), AlertQuery::Area(code))
            }
        };
        let alerts = self
            .source
            .alerts(&query)
            .map_err(|e| tool_error(e, &asked))?;
        let detailed = args.list.format().is_detailed();
        let items: Vec<AlertItem> = alerts
            .into_iter()
            .map(|a| alert_item(a, detailed))
            .collect();
        ToolOutput::new(&AlertsOut {
            at: asked,
            page: page.page(items, "at")?,
        })
    }
}

impl<S: Source> Toolset for WxTools<S> {
    fn server(&self) -> Server {
        Server::new(
            "wx",
            env!("CARGO_PKG_VERSION"),
            "NOAA and NWS weather",
            "wx",
        )
        // No credential is involved, so "ask the user to sign in" would send them to
        // a command that changes nothing here.
        .hosted(
            "through the open-source `wx` command-line tool, from public data with no \
                 credentials",
            "tell the user, since nothing here signs in",
        )
        .instructions(
            "`at` is a saved place's name or lat,lon; an unknown name's error lists the saved \
             names. Rainfall is the 1 km cell over the place, from NOAA MRMS; forecasts, \
             conditions and alerts are the National Weather Service's. A total with provisional \
             true can still change, so say so when reporting it. A first call for a long window \
             may download for a minute; calling again resumes from the cache. from and to are \
             local times. Unofficial; not affiliated with NOAA or NWS.",
        )
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![
            ToolDef::read(
                RAIN_TOOL,
                "Rainfall at a place",
                "How much rain fell at one place over a window; set step to see when, as a bar \
                 per hour or quarter hour, oldest first, in items. Use it for rainfall already \
                 fallen. Returns from, to, cut_short, inches, final_hours, provisional_hours, \
                 radar_hours, missing_hours, radar_quarters, missing_quarters, provisional, note \
                 and items of from, inches and quality; detailed adds mm, area_wettest_inches, \
                 area_mean_inches and ends.",
            )
            .input::<RainArgs>()
            .output::<RainOut>(),
            ToolDef::read(
                FORECAST_TOOL,
                "NWS forecast for a place",
                "The forecast for one place in the US, nearest period first. Use it for what the \
                 weather will do. Returns location, updated and items of name, temperature_f, \
                 precip_chance_pct, wind and short; detailed adds start, end and detailed.",
            )
            .input::<ForecastArgs>()
            .output::<ForecastOut>(),
            ToolDef::read(
                CONDITIONS_TOOL,
                "Current conditions near a place",
                "The latest observation from the nearest station with a fresh one. Use it for \
                 what the weather is doing now. Returns station_name, distance_mi, observed_at, \
                 summary, temperature_f, humidity_pct, wind_mph, wind_gust_mph and \
                 precip_last_hour_in; detailed adds station, dewpoint_f, pressure_mb and \
                 visibility_mi.",
            )
            .input::<ConditionsArgs>()
            .output::<ConditionsOut>(),
            ToolDef::read(
                ALERTS_TOOL,
                "Active NWS alerts",
                "Active watches, warnings and advisories for a place or area. Use it for whether \
                 anything is in effect; an empty list means nothing is. Returns at and items of \
                 event, severity, headline, area and ends; detailed adds id, urgency, onset, \
                 description and instruction.",
            )
            .input::<AlertsArgs>()
            .output::<AlertsOut>(),
        ]
    }

    async fn call(&self, name: &str, args: Value) -> Result<ToolOutput, ToolError> {
        match name {
            RAIN_TOOL => self.rain(args),
            FORECAST_TOOL => self.forecast(args),
            CONDITIONS_TOOL => self.conditions(args),
            ALERTS_TOOL => self.alerts(args),
            other => Err(ToolError::usage(format!("no tool named {other}"))),
        }
    }
}

#[cfg(test)]
mod tests;

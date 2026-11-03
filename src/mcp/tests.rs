//! The tools over canned answers, and [`Live`] over a local server that serves the
//! scrubbed fixtures, so the library's futures really run on the tools' runtime.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use pk_cli_mcp::conformance::{assert_conforms, assert_fits, call};
use pk_cli_mcp::{tools_list_json, CallToolResult};
use serde_json::{json, Value};

use super::*;
use crate::nws::{parse_alerts, parse_forecast, parse_observation, parse_stations};
use crate::places::Place;
use crate::window::Window;

/// Coordinates a test can search results for. Invented, and inside the MRMS domain.
const SAVED: (f64, f64) = (39.1234, -104.5678);
const SAVED_TEXTS: [&str; 4] = ["39.1234", "-104.5678", "39.12", "104.56"];

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn fixture_json(name: &str) -> Value {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn user() -> UserConfig {
    UserConfig {
        alert_area: Some("co".into()),
        places: vec![Place {
            name: "Example Beach".into(),
            lat: SAVED.0,
            lon: SAVED.1,
        }],
        ..UserConfig::default()
    }
}

fn resolved(start: &str, end: &str) -> Resolved {
    Resolved {
        window: Window::new(ts(start), ts(end)),
        clipped: false,
        label: "last 3 h".into(),
    }
}

fn stats() -> GridStats {
    GridStats {
        max_mm: 25.4,
        max_at: Some([-104.5, 39.5]),
        mean_mm: 2.54,
        wet_fraction: 0.4251,
        missing_cells: 0,
    }
}

/// Canned answers, and the points each call was asked about.
struct Canned {
    user: UserConfig,
    hours: HourCounts,
    mm: Option<f64>,
    bars: Vec<SeriesPoint>,
    asked: Arc<Mutex<Vec<(f64, f64)>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

impl Canned {
    fn new() -> Self {
        Self {
            user: user(),
            hours: HourCounts {
                pass2: 3,
                ..HourCounts::default()
            },
            mm: Some(12.7),
            bars: vec![
                SeriesPoint {
                    valid: ts("2026-01-15T13:00:00Z"),
                    mm: Some(2.54),
                    pass: Some(Pass::Pass2),
                },
                SeriesPoint {
                    valid: ts("2026-01-15T14:00:00Z"),
                    mm: Some(5.08),
                    pass: Some(Pass::Pass1),
                },
                SeriesPoint {
                    valid: ts("2026-01-15T15:00:00Z"),
                    mm: None,
                    pass: None,
                },
            ],
            asked: Arc::default(),
            queries: Arc::default(),
        }
    }
}

impl Source for Canned {
    fn setup(&self) -> Result<Setup, ToolError> {
        Ok(Setup {
            user: self.user.clone(),
            region: None,
            tz: TimeZone::UTC,
        })
    }

    fn rain(&self, _: &Setup, spot: (f64, f64), _: &WindowSpec) -> Result<RainReading> {
        self.asked.lock().unwrap().push(spot);
        Ok(RainReading {
            resolved: resolved("2026-01-15T12:00:00Z", "2026-01-15T15:00:00Z"),
            hours: self.hours,
            mm: self.mm,
            stats: Some(stats()),
        })
    }

    fn bars(&self, _: &Setup, spot: (f64, f64), _: &WindowSpec, q: bool) -> Result<BarsReading> {
        self.asked.lock().unwrap().push(spot);
        Ok(BarsReading {
            resolved: resolved("2026-01-15T12:00:00Z", "2026-01-15T15:00:00Z"),
            quarters: q,
            bars: self.bars.clone(),
        })
    }

    fn forecast(&self, spot: (f64, f64), _: bool) -> Result<Forecast> {
        self.asked.lock().unwrap().push(spot);
        Ok(parse_forecast(
            &fixture_json("nws_forecast.json"),
            Some("Example City, CO".into()),
        ))
    }

    fn conditions(&self, spot: (f64, f64)) -> Result<Conditions> {
        self.asked.lock().unwrap().push(spot);
        let stations = parse_stations(&fixture_json("nws_stations.json"));
        Ok(parse_observation(
            &fixture_json("nws_observation.json"),
            &stations[0],
            spot.0,
            spot.1,
        )
        .unwrap())
    }

    fn alerts(&self, query: &AlertQuery) -> Result<Vec<Alert>> {
        self.queries.lock().unwrap().push(match query {
            AlertQuery::Area(a) => format!("area {a}"),
            AlertQuery::Point(lat, lon) => format!("point {lat},{lon}"),
        });
        Ok(parse_alerts(&fixture_json("nws_alerts.json")))
    }
}

fn tools() -> WxTools<Canned> {
    WxTools::new(Canned::new())
}

fn fits(tools: &WxTools<impl Source>, name: &str, args: Value) -> Value {
    let result = call(tools, name, args);
    assert_fits(tools, name, &result).clone()
}

fn error_text(result: &CallToolResult) -> String {
    assert!(result.is_error(), "expected an error: {}", result.text());
    result.text().to_string()
}

#[test]
fn the_tools_conform_and_every_one_is_read_only() {
    let t = tools();
    assert_conforms(&t);
    let defs = t.tools();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [RAIN_TOOL, FORECAST_TOOL, CONDITIONS_TOOL, ALERTS_TOOL]
    );
    for d in &defs {
        assert!(d.annotations.read_only, "{}", d.name);
        assert!(!d.annotations.destructive, "{}", d.name);
    }
}

/// A source that fails any use: listing the tools must reach none of it.
struct Untouchable;

impl Source for Untouchable {
    fn setup(&self) -> Result<Setup, ToolError> {
        panic!("setup touched")
    }
    fn rain(&self, _: &Setup, _: (f64, f64), _: &WindowSpec) -> Result<RainReading> {
        panic!("rain touched")
    }
    fn bars(&self, _: &Setup, _: (f64, f64), _: &WindowSpec, _: bool) -> Result<BarsReading> {
        panic!("bars touched")
    }
    fn forecast(&self, _: (f64, f64), _: bool) -> Result<Forecast> {
        panic!("forecast touched")
    }
    fn conditions(&self, _: (f64, f64)) -> Result<Conditions> {
        panic!("conditions touched")
    }
    fn alerts(&self, _: &AlertQuery) -> Result<Vec<Alert>> {
        panic!("alerts touched")
    }
}

#[test]
fn listing_the_tools_reads_nothing() {
    let t = WxTools::new(Untouchable);
    assert_eq!(tools_list_json(&t)["tools"].as_array().unwrap().len(), 4);
    assert!(pk_cli_mcp::instructions(&t).contains("no credentials"));
}

#[test]
fn a_saved_place_is_asked_about_by_its_coordinates_and_never_shown_them() {
    let t = tools();
    for (name, args) in [
        (RAIN_TOOL, json!({"at": "example beach"})),
        (
            RAIN_TOOL,
            json!({"at": "Example Beach", "response_format": "detailed"}),
        ),
        (RAIN_TOOL, json!({"at": "Example Beach", "step": "1h"})),
        (FORECAST_TOOL, json!({"at": "Example Beach"})),
        (
            CONDITIONS_TOOL,
            json!({"at": "Example Beach", "response_format": "detailed"}),
        ),
    ] {
        let out = fits(&t, name, args).to_string();
        for text in SAVED_TEXTS {
            assert!(!out.contains(text), "{name}: {out}");
        }
        assert_eq!(t.source.asked.lock().unwrap().pop(), Some(SAVED), "{name}");
    }
}

#[test]
fn rain_keeps_its_passes_and_says_when_it_can_change() {
    let detailed = fits(
        &tools(),
        RAIN_TOOL,
        json!({"at": "39.74,-104.99", "response_format": "detailed"}),
    );
    assert_eq!(detailed["mm"], 12.7);
    let out = fits(&tools(), RAIN_TOOL, json!({"at": "39.74,-104.99"}));
    assert_eq!(out["inches"], 0.5);
    assert_eq!(out["final_hours"], 3);
    assert_eq!(out["provisional"], false);
    assert!(out.get("note").is_none(), "{out}");
    assert!(out.get("mm").is_none(), "concise: {out}");

    let mut canned = Canned::new();
    canned.hours = HourCounts {
        pass2: 1,
        pass1: 1,
        radar: 1,
        missing: 1,
        ..HourCounts::default()
    };
    canned.mm = None;
    let out = fits(
        &WxTools::new(canned),
        RAIN_TOOL,
        json!({"at": "39.74,-104.99", "response_format": "detailed"}),
    );
    assert_eq!(out["provisional"], true);
    assert_eq!(out["provisional_hours"], 1);
    assert_eq!(out["radar_hours"], 1);
    assert_eq!(out["missing_hours"], 1);
    assert_eq!(out["inches"], Value::Null);
    assert_eq!(out["returned"], 0, "no step, no bars");
    let note = out["note"].as_str().unwrap();
    assert!(note.contains("no data"), "{note}");
    assert!(note.contains("can change"), "{note}");
    assert!(note.contains("low by"), "{note}");
    assert_eq!(out["area_wettest_inches"], 1.0);
    assert_eq!(out["area_mean_inches"], 0.1);
}

#[test]
fn a_sub_hour_total_is_not_called_final() {
    let mut canned = Canned::new();
    canned.hours = HourCounts {
        quarters: 3,
        ..HourCounts::default()
    };
    let out = fits(
        &WxTools::new(canned),
        RAIN_TOOL,
        json!({"at": "39.74,-104.99"}),
    );
    assert_eq!(out["provisional"], true);
    assert_eq!(out["radar_quarters"], 3);
    assert!(out["note"].as_str().unwrap().contains("radar only"));
}

#[test]
fn bars_total_every_page_and_bind_their_cursor() {
    let t = tools();
    let args = json!({"at": "39.74,-104.99", "step": "1h", "limit": 1, "window": "3h"});
    let first = fits(&t, RAIN_TOOL, args.clone());
    assert_eq!(first["returned"], 1);
    assert_eq!(first["inches"], 0.3);
    assert_eq!(first["final_hours"], 1);
    assert_eq!(first["provisional_hours"], 1);
    assert_eq!(first["missing_hours"], 1);
    assert_eq!(first["provisional"], true);
    assert_eq!(first["items"][0]["quality"], "final");
    assert_eq!(first["items"][0]["from"], "Jan 15, 12:00 PM UTC");
    assert!(first["items"][0].get("ends").is_none(), "{first}");

    let cursor = first["next_cursor"].as_str().unwrap();
    let mut next = args.clone();
    next["cursor"] = json!(cursor);
    let second = fits(&t, RAIN_TOOL, next.clone());
    assert_eq!(second["items"][0]["quality"], "provisional");

    next["window"] = json!("6h");
    assert!(call(&t, RAIN_TOOL, next).is_error());

    let detailed = fits(
        &t,
        RAIN_TOOL,
        json!({"at": "39.74,-104.99", "step": "1h", "response_format": "detailed"}),
    );
    assert_eq!(detailed["mm"], 7.6);
    assert!(detailed.get("area_mean_inches").is_none(), "{detailed}");
    assert_eq!(detailed["items"][2]["quality"], "no data");
    assert_eq!(detailed["items"][0]["mm"], 2.5);
    assert_eq!(detailed["items"][0]["ends"], "2026-01-15T13:00:00Z");
}

#[test]
fn rain_takes_two_steps_only() {
    let t = tools();
    let q = fits(&t, RAIN_TOOL, json!({"at": "39.74,-104.99", "step": "15m"}));
    assert_eq!(q["radar_quarters"], 2);
    assert_eq!(q["missing_quarters"], 1);
    assert_eq!(q["final_hours"], 0);
    assert!(q["note"].as_str().unwrap().contains("radar only"), "{q}");
    let text = error_text(&call(
        &t,
        RAIN_TOOL,
        json!({"at": "39.74,-104.99", "step": "5m"}),
    ));
    assert!(text.starts_with("usage:"), "{text}");
}

#[test]
fn the_library_limits_on_a_series_are_usage_errors() {
    let start = ts("2026-01-01T00:00:00Z");
    let at = |hours: i64| {
        Window::new(
            start,
            Timestamp::from_second(start.as_second() + hours * 3600).unwrap(),
        )
    };
    assert!(check_bars_window(at(MAX_SERIES_HOURS), false).is_ok());
    assert!(matches!(
        check_bars_window(at(MAX_SERIES_HOURS + 1), false),
        Err(CliError::Usage(_))
    ));
    assert!(check_bars_window(at(MAX_QUARTER_HOURS), true).is_ok());
    assert!(matches!(
        check_bars_window(at(MAX_QUARTER_HOURS + 1), true),
        Err(CliError::Usage(_))
    ));
}

#[test]
fn a_bad_window_is_refused_before_any_read() {
    let mut canned = Canned::new();
    canned.user = user();
    let asked = canned.asked.clone();
    let t = WxTools::new(canned);
    let text = error_text(&call(
        &t,
        RAIN_TOOL,
        json!({"at": "39.74,-104.99", "window": "sometime"}),
    ));
    assert!(text.starts_with("usage:"), "{text}");
    assert!(text.contains("yesterday"), "{text}");
    assert!(asked.lock().unwrap().is_empty());
}

#[test]
fn forecast_is_concise_unless_asked_and_pages() {
    let t = tools();
    let out = fits(
        &t,
        FORECAST_TOOL,
        json!({"at": "39.74,-104.99", "limit": 1}),
    );
    assert_eq!(out["location"], "Example City, CO");
    assert_eq!(out["returned"], 1);
    assert!(out["next_cursor"].is_string(), "{out}");
    assert!(out["items"][0].get("detailed").is_none(), "{out}");
    let out = fits(
        &t,
        FORECAST_TOOL,
        json!({"at": "39.74,-104.99", "response_format": "detailed"}),
    );
    assert!(out["items"][0]["detailed"].is_string(), "{out}");
    assert!(out["items"][0]["start"].is_string(), "{out}");
}

#[test]
fn an_hourly_forecast_names_each_hour() {
    let out = fits(
        &tools(),
        FORECAST_TOOL,
        json!({"at": "39.74,-104.99", "hourly": true, "limit": 1}),
    );
    let name = out["items"][0]["name"].as_str().unwrap();
    assert!(name.ends_with("AM") || name.ends_with("PM"), "{name}");
}

#[test]
fn conditions_are_concise_unless_asked() {
    let t = tools();
    let out = fits(&t, CONDITIONS_TOOL, json!({"at": "39.74,-104.99"}));
    assert_eq!(out["station_name"], "Example Airport");
    assert_eq!(out["temperature_f"], 71.6);
    assert!(out.get("pressure_mb").is_none(), "{out}");
    let out = fits(
        &t,
        CONDITIONS_TOOL,
        json!({"at": "39.74,-104.99", "response_format": "detailed"}),
    );
    assert_eq!(out["station"], "KXYZ");
    assert_eq!(out["pressure_mb"], 1013.2);
}

#[test]
fn alerts_ask_about_a_place_or_an_area_and_fall_back_on_the_setting() {
    let canned = Canned::new();
    let queries = canned.queries.clone();
    let t = WxTools::new(canned);

    let out = fits(&t, ALERTS_TOOL, json!({"area": "fl"}));
    assert_eq!(out["at"], "FL");
    assert_eq!(out["items"][0]["event"], "Flood Warning");
    assert!(out["items"][0].get("description").is_none(), "{out}");
    assert_eq!(queries.lock().unwrap().pop().unwrap(), "area FL");

    fits(&t, ALERTS_TOOL, json!({"at": "Example Beach"}));
    assert_eq!(
        queries.lock().unwrap().pop().unwrap(),
        format!("point {},{}", SAVED.0, SAVED.1)
    );

    let out = fits(&t, ALERTS_TOOL, json!({"response_format": "detailed"}));
    assert_eq!(out["at"], "CO", "the alert_area setting");
    assert!(out["items"][0]["instruction"].is_string(), "{out}");
    assert_eq!(out["items"][0]["id"], "urn:oid:example.1");
    assert_eq!(queries.lock().unwrap().pop().unwrap(), "area CO");
}

#[test]
fn alerts_refuse_what_cannot_go_in_their_url() {
    let t = tools();
    for bad in ["FL&limit=1", "FLA", "F", "F1", "../x"] {
        let text = error_text(&call(&t, ALERTS_TOOL, json!({"area": bad})));
        assert!(text.starts_with("usage:"), "{bad}: {text}");
    }
    assert!(t.source.queries.lock().unwrap().is_empty());

    let text = error_text(&call(&t, ALERTS_TOOL, json!({"at": "1,2", "area": "FL"})));
    assert!(text.contains("not both"), "{text}");

    let mut canned = Canned::new();
    canned.user.alert_area = None;
    let text = error_text(&call(&WxTools::new(canned), ALERTS_TOOL, json!({})));
    assert!(text.contains("pass at or area"), "{text}");
}

#[test]
fn an_unknown_place_names_the_saved_ones() {
    let t = tools();
    let text = error_text(&call(&t, FORECAST_TOOL, json!({"at": "the lake"})));
    assert!(text.starts_with("not_found:"), "{text}");
    assert!(text.contains("the lake"), "{text}");
    assert!(text.contains("Saved places: Example Beach"), "{text}");
    for coordinate in SAVED_TEXTS {
        assert!(!text.contains(coordinate), "{text}");
    }

    let mut canned = Canned::new();
    canned.user.places.clear();
    let text = error_text(&call(
        &WxTools::new(canned),
        FORECAST_TOOL,
        json!({"at": "the lake"}),
    ));
    assert!(text.contains("wx places add"), "{text}");
}

#[test]
fn a_point_out_of_range_or_empty_is_a_usage_error() {
    let t = tools();
    for bad in ["91,0", "0,181", "abc,def", "  "] {
        let text = error_text(&call(&t, CONDITIONS_TOOL, json!({"at": bad})));
        assert!(text.starts_with("usage:"), "{bad}: {text}");
    }
}

#[test]
fn a_misspelled_argument_is_refused() {
    let text = error_text(&call(
        &tools(),
        RAIN_TOOL,
        json!({"at": "39.74,-104.99", "windw": "3h"}),
    ));
    assert!(text.starts_with("usage:"), "{text}");
}

// ---- Live, over a local server ----------------------------------------------------

/// A plain HTTP server: `route` maps a request path to a status and a body.
fn stub(
    route: impl Fn(&str) -> (u16, String) + Send + 'static,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let paths: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = paths.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 8192];
            let n = conn.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            seen.lock().unwrap().push(path.clone());
            let (status, body) = route(&path);
            let reply = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = conn.write_all(reply.as_bytes());
        }
    });
    (url, paths)
}

/// The `/points` and forecast routes over the fixtures, with their links pointing back.
fn nws_route(url: &str, path: &str) -> (u16, String) {
    let body = if path.starts_with("/points/") {
        fixture("nws_point.json")
    } else if path.contains("/stations?") {
        fixture("nws_stations.json")
    } else if path.ends_with("/forecast") || path.ends_with("/forecast/hourly") {
        fixture("nws_forecast.json")
    } else if path.ends_with("/observations/latest") {
        fixture("nws_observation.json")
    } else if path.starts_with("/alerts/active") {
        fixture("nws_alerts.json")
    } else {
        return (404, String::new());
    };
    (200, body.replace("https://api.weather.gov", url))
}

fn live(dir: &Path, nws_url: &str) -> WxTools<Live> {
    let config_path = dir.join("config.json");
    user().save(&config_path).unwrap();
    WxTools::new(Live::new(Settings {
        config_path,
        db: Some(dir.join("cache.sqlite")),
        region: None,
        tz: Some("UTC".into()),
        concurrency: 2,
        nws_url: nws_url.into(),
    }))
}

fn assert_no_coordinates(text: &str) {
    for coordinate in SAVED_TEXTS {
        assert!(!text.contains(coordinate), "{text}");
    }
    assert!(!text.contains("http"), "{text}");
    assert!(!text.contains("sqlite"), "{text}");
}

#[test]
fn live_reads_the_nws_on_its_own_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let cell: Arc<Mutex<String>> = Arc::default();
    let shared = cell.clone();
    let (url, paths) = stub(move |path| {
        let base = shared.lock().unwrap().clone();
        nws_route(&base, path)
    });
    *cell.lock().unwrap() = url.clone();
    let t = live(dir.path(), &url);

    let out = fits(&t, FORECAST_TOOL, json!({"at": "Example Beach"}));
    assert_eq!(out["location"], "Example City, CO");
    let asked = paths.lock().unwrap().clone();
    assert!(
        asked[0].starts_with("/points/39.1234,-104.5678"),
        "{asked:?}"
    );

    let out = fits(&t, ALERTS_TOOL, json!({"area": "FL"}));
    assert_eq!(out["returned"], 2);
    assert!(paths
        .lock()
        .unwrap()
        .contains(&"/alerts/active?area=FL".to_string()));

    // The fixture's observation is older than any "current" one.
    let text = error_text(&call(&t, CONDITIONS_TOOL, json!({"at": "Example Beach"})));
    assert!(text.starts_with("not_found:"), "{text}");
    assert!(text.contains("Example Beach"), "{text}");
    assert_no_coordinates(&text);
}

#[test]
fn a_failure_upstream_says_what_to_do_and_nothing_of_the_place() {
    let dir = tempfile::tempdir().unwrap();
    let (url, _) = stub(|_| (500, String::new()));
    let t = live(dir.path(), &url);
    let text = error_text(&call(&t, FORECAST_TOOL, json!({"at": "Example Beach"})));
    assert!(text.starts_with("upstream:"), "{text}");
    assert_no_coordinates(&text);

    let (url, _) = stub(|_| (404, String::new()));
    let t = live(dir.path(), &url);
    let text = error_text(&call(&t, FORECAST_TOOL, json!({"at": "Example Beach"})));
    assert!(text.starts_with("not_found:"), "{text}");
    assert!(text.contains("Example Beach"), "{text}");
    assert_no_coordinates(&text);

    // NWS answers a point it does not cover with a 400.
    let (url, _) = stub(|_| (400, String::new()));
    let t = live(dir.path(), &url);
    let text = error_text(&call(&t, ALERTS_TOOL, json!({"at": "Example Beach"})));
    assert!(text.starts_with("not_found:"), "{text}");
    assert_no_coordinates(&text);
}

#[test]
fn a_broken_config_is_one_calls_error_and_names_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let t = live(dir.path(), "http://127.0.0.1:9");
    std::fs::write(dir.path().join("config.json"), "{not json").unwrap();
    assert_eq!(tools_list_json(&t)["tools"].as_array().unwrap().len(), 4);
    let text = error_text(&call(&t, FORECAST_TOOL, json!({"at": "1,2"})));
    assert!(text.starts_with("other:"), "{text}");
    assert!(text.contains("wx config show"), "{text}");
    assert!(!text.contains(dir.path().to_str().unwrap()), "{text}");
}

#[test]
fn a_config_change_applies_on_the_next_call() {
    let dir = tempfile::tempdir().unwrap();
    let t = live(dir.path(), "http://127.0.0.1:9");
    let text = error_text(&call(&t, RAIN_TOOL, json!({"at": "New Place"})));
    assert!(text.contains("no saved place"), "{text}");

    let mut changed = user();
    changed.upsert_place(Place {
        name: "New Place".into(),
        lat: 0.0,
        lon: 0.0,
    });
    changed.save(&dir.path().join("config.json")).unwrap();
    let text = error_text(&call(&t, RAIN_TOOL, json!({"at": "New Place"})));
    assert!(text.contains("outside the rainfall grid"), "{text}");
    assert!(text.contains("New Place"), "{text}");
}

#[test]
fn a_configured_region_is_used_only_when_it_holds_the_point() {
    let dir = tempfile::tempdir().unwrap();
    let t = live(dir.path(), "http://127.0.0.1:9");
    let setup = Setup {
        user: user(),
        region: Some(Region::parse("-110,35,-100,45").unwrap()),
        tz: TimeZone::UTC,
    };
    assert_eq!(
        t.source.region(&setup, SAVED).unwrap(),
        setup.region.unwrap()
    );
    let elsewhere = t.source.region(&setup, (27.0, -80.0)).unwrap();
    assert!(elsewhere.contains(27.0, -80.0));
    assert!(!elsewhere.contains(SAVED.0, SAVED.1));
}

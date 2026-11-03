//! National Weather Service: active alerts, forecasts, current conditions.
//!
//! `api.weather.gov` needs no key, only a User-Agent naming the client.
//!
//! ## Traps
//!
//! * **Most alerts have no polygon.** A watch or advisory issued by zone comes
//!   with `geometry: null` and a list of `affectedZones` URLs; only
//!   storm-based warnings carry their own shape. Drawing an alert on a map means
//!   fetching those zones. Zone shapes barely change, so they are cached.
//! * **The nearest station may have nothing current.** A station's latest
//!   observation can be missing or hours old, so conditions walk the forecast
//!   grid's station list until one is fresh. Station ids also change: West Palm
//!   Beach's airport moved from `KPBI` to `KDJT`, so never hard-code one.
//! * **`/points` has three answers.** 404 outside NWS coverage, 500 over open
//!   ocean, and an ordinary grid over water near shore. The 500 is a real
//!   answer from NWS, so it surfaces as an upstream failure after the retries.
//! * **Observations are SI, forecasts are °F.** Values carry a `unitCode`, and
//!   any of them may be null.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{json, Value};

use crate::grid::Region;
use crate::store::Store;
use crate::UpstreamError;

pub const DEFAULT_NWS_URL: &str = "https://api.weather.gov";

pub const ATTRIBUTION: &str = "Alerts, forecasts and observations: National Weather Service";

/// NWS has no forecast grid at this point: it is outside the US and its waters.
#[derive(Debug, thiserror::Error)]
#[error("NWS has no forecast for {lat:.4}, {lon:.4}: US land and nearby waters only")]
pub struct NoForecast {
    pub lat: f64,
    pub lon: f64,
}

/// Every station near a point was tried and none has a fresh observation.
#[derive(Debug, thiserror::Error)]
#[error("none of the {stations} stations nearest {lat:.4}, {lon:.4} has a recent observation")]
pub struct NoObservation {
    pub stations: usize,
    pub lat: f64,
    pub lon: f64,
}

/// Zone shapes are re-fetched after this long.
const ZONE_REFRESH_SECS: i64 = 30 * 86_400;

/// An observation older than this is not "current".
const MAX_OBSERVATION_AGE_SECS: i64 = 3 * 3600;

/// Stations tried for current conditions, nearest first.
const STATIONS_TRIED: usize = 6;

const ATTEMPTS: u32 = 3;

/// What `/points/{lat},{lon}` says about a location.
#[derive(Debug, Clone, Serialize)]
pub struct PointInfo {
    pub lat: f64,
    pub lon: f64,
    pub city: Option<String>,
    pub state: Option<String>,
    pub time_zone: Option<String>,
    pub office: Option<String>,
    pub forecast_url: String,
    pub hourly_url: String,
    pub stations_url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Period {
    pub name: String,
    pub start: String,
    pub end: String,
    pub is_daytime: bool,
    pub temperature_f: Option<f64>,
    pub precip_chance_pct: Option<f64>,
    pub wind: Option<String>,
    pub wind_direction: Option<String>,
    pub short: String,
    pub detailed: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Forecast {
    pub location: Option<String>,
    pub updated: Option<String>,
    pub periods: Vec<Period>,
    pub attribution: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Conditions {
    pub station: String,
    pub station_name: Option<String>,
    pub distance_mi: Option<f64>,
    pub observed_at: String,
    pub summary: Option<String>,
    pub temperature_f: Option<f64>,
    pub dewpoint_f: Option<f64>,
    pub humidity_pct: Option<f64>,
    pub wind_mph: Option<f64>,
    pub wind_gust_mph: Option<f64>,
    pub wind_direction_deg: Option<f64>,
    pub pressure_mb: Option<f64>,
    pub visibility_mi: Option<f64>,
    pub precip_last_hour_in: Option<f64>,
    pub attribution: &'static str,
}

/// What NWS can say about a whole region.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RegionInfo {
    /// State codes, for an alert query.
    pub states: Vec<String>,
    pub time_zone: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub id: String,
    pub event: String,
    pub severity: String,
    pub urgency: Option<String>,
    pub certainty: Option<String>,
    pub headline: Option<String>,
    pub area: Option<String>,
    pub onset: Option<String>,
    pub ends: Option<String>,
    pub expires: Option<String>,
    pub sender: Option<String>,
    pub description: Option<String>,
    pub instruction: Option<String>,
    /// Zone URLs, used to find a shape when `geometry` is absent.
    #[serde(skip)]
    pub zones: Vec<String>,
    /// GeoJSON geometry, the alert's own or built from its zones.
    pub geometry: Option<Value>,
}

/// Which alerts to ask for.
#[derive(Debug, Clone, PartialEq)]
pub enum AlertQuery {
    /// A state or marine area code, e.g. `FL`.
    Area(String),
    Point(f64, f64),
}

pub struct NwsClient {
    http: reqwest::Client,
    base: String,
    store: Option<Store>,
    points: Mutex<HashMap<(i64, i64), PointInfo>>,
}

impl NwsClient {
    /// `store`, when given, keeps zone shapes between runs.
    pub fn new(base: impl Into<String>, store: Option<Store>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!(
                    "noaa-weather-cli/",
                    env!("CARGO_PKG_VERSION"),
                    " (+https://github.com/piekstra/noaa-weather-cli)"
                ))
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(45))
                .build()
                .context("building HTTP client")?,
            base: base.into().trim_end_matches('/').to_string(),
            store,
            points: Mutex::new(HashMap::new()),
        })
    }

    /// GET JSON. `Ok(None)` on 404. Retries the 5xx and network failures this
    /// API produces now and then.
    async fn get(&self, url: &str) -> Result<Option<Value>> {
        let mut last = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * u64::from(attempt))).await;
            }
            let resp = self
                .http
                .get(url)
                .header(reqwest::header::ACCEPT, "application/geo+json")
                .send()
                .await;
            match resp {
                Ok(r) if r.status().as_u16() == 404 => return Ok(None),
                Ok(r) if r.status().is_server_error() => {
                    last = Some(UpstreamError(format!("GET {url}: HTTP {}", r.status())).into());
                }
                Ok(r) => {
                    let r = r.error_for_status().with_context(|| format!("GET {url}"))?;
                    let v = r.json().await.with_context(|| format!("reading {url}"))?;
                    return Ok(Some(v));
                }
                Err(e) => last = Some(anyhow::Error::new(e).context(format!("GET {url}"))),
            }
        }
        Err(last.expect("at least one attempt"))
    }

    async fn get_required(&self, url: &str) -> Result<Value> {
        self.get(url)
            .await?
            .ok_or_else(|| UpstreamError(format!("GET {url}: not found")).into())
    }

    /// Raw GET against the API, for fields this crate does not model.
    pub async fn raw(&self, path: &str) -> Result<Option<Value>> {
        self.get(&format!("{}/{}", self.base, path.trim_start_matches('/')))
            .await
    }

    pub async fn point(&self, lat: f64, lon: f64) -> Result<PointInfo> {
        let key = ((lat * 1e4).round() as i64, (lon * 1e4).round() as i64);
        if let Some(p) = self.points.lock().expect("points lock").get(&key) {
            return Ok(p.clone());
        }
        let url = format!("{}/points/{lat:.4},{lon:.4}", self.base);
        let Some(v) = self.get(&url).await? else {
            return Err(NoForecast { lat, lon }.into());
        };
        let p = parse_point(&v, lat, lon)?;
        self.points
            .lock()
            .expect("points lock")
            .insert(key, p.clone());
        Ok(p)
    }

    /// The states and time zone of a region, from what NWS says about nine
    /// points spread across it. Points it does not cover, such as open water,
    /// are skipped, so both can come back empty.
    pub async fn region_info(&self, region: &Region) -> RegionInfo {
        let dx = (region.east - region.west) * 0.3;
        let dy = (region.north - region.south) * 0.3;
        let (cx, cy) = (
            (region.east + region.west) / 2.0,
            (region.north + region.south) / 2.0,
        );
        let mut asks = Vec::new();
        for i in [0.0, -1.0, 1.0] {
            for j in [0.0, -1.0, 1.0] {
                asks.push(self.point(cy + i * dy, cx + j * dx));
            }
        }
        let points: Vec<PointInfo> = futures::stream::iter(asks)
            .buffered(4)
            .filter_map(|r| async move { r.ok() })
            .collect()
            .await;
        let mut states: Vec<String> = points.iter().filter_map(|p| p.state.clone()).collect();
        states.sort();
        states.dedup();
        RegionInfo {
            states,
            time_zone: points.iter().find_map(|p| p.time_zone.clone()),
        }
    }

    pub async fn forecast(&self, lat: f64, lon: f64, hourly: bool) -> Result<Forecast> {
        let p = self.point(lat, lon).await?;
        let url = if hourly {
            &p.hourly_url
        } else {
            &p.forecast_url
        };
        let v = self.get_required(url).await?;
        Ok(parse_forecast(&v, location_of(&p)))
    }

    pub async fn conditions(&self, lat: f64, lon: f64) -> Result<Conditions> {
        let p = self.point(lat, lon).await?;
        let list = self
            .get_required(&format!("{}?limit={STATIONS_TRIED}", p.stations_url))
            .await?;
        let stations = parse_stations(&list);
        let now = Timestamp::now();
        let mut answered = false;
        let mut failure = None;
        for s in &stations {
            let url = format!("{}/stations/{}/observations/latest", self.base, s.id);
            let v = match self.get(&url).await {
                Ok(Some(v)) => v,
                Ok(None) => {
                    answered = true;
                    continue;
                }
                Err(e) => {
                    failure = Some(e);
                    continue;
                }
            };
            answered = true;
            let Some(c) = parse_observation(&v, s, lat, lon) else {
                continue;
            };
            let fresh = c
                .observed_at
                .parse::<Timestamp>()
                .is_ok_and(|t| now.as_second() - t.as_second() <= MAX_OBSERVATION_AGE_SECS);
            if fresh && c.temperature_f.is_some() {
                return Ok(c);
            }
        }
        // "Nothing recent" is a fact only if a station said so. If none could answer, the
        // service failed, and that says nothing about the place.
        if let (false, Some(e)) = (answered, failure) {
            return Err(e);
        }
        Err(NoObservation {
            stations: stations.len(),
            lat,
            lon,
        }
        .into())
    }

    pub async fn alerts(&self, q: &AlertQuery) -> Result<Vec<Alert>> {
        let url = match q {
            AlertQuery::Area(a) => format!("{}/alerts/active?area={a}", self.base),
            AlertQuery::Point(lat, lon) => {
                format!("{}/alerts/active?point={lat:.4},{lon:.4}", self.base)
            }
        };
        Ok(parse_alerts(&self.get_required(&url).await?))
    }

    /// Active alerts touching `region`, each with a shape to draw.
    ///
    /// An alert whose zones cannot be fetched stays in the list with no
    /// geometry: a warning that cannot be drawn must still be reported.
    pub async fn alerts_in_region(&self, area: &str, region: &Region) -> Result<Vec<Alert>> {
        let mut alerts = self.alerts(&AlertQuery::Area(area.to_string())).await?;
        let mut zones: HashMap<String, Option<Value>> = HashMap::new();
        for a in &mut alerts {
            if a.geometry.is_some() {
                continue;
            }
            let mut shapes = Vec::new();
            for z in &a.zones {
                if !zones.contains_key(z) {
                    let shape = self.zone_shape(z).await.unwrap_or_else(|e| {
                        tracing::warn!(zone = %z, error = %e, "zone shape unavailable");
                        None
                    });
                    zones.insert(z.clone(), shape);
                }
                if let Some(Some(g)) = zones.get(z) {
                    shapes.push(g.clone());
                }
            }
            a.geometry = merge_polygons(&shapes);
        }
        alerts.retain(|a| match a.geometry.as_ref().and_then(bounds_of) {
            Some(b) => intersects(b, region),
            None => true,
        });
        Ok(alerts)
    }

    async fn zone_shape(&self, url: &str) -> Result<Option<Value>> {
        let key = format!("nws_zone:{url}");
        if let Some(store) = &self.store {
            if let Some((text, at)) = store.meta(&key)? {
                let age = now_secs() - at;
                if age < ZONE_REFRESH_SECS {
                    return Ok(serde_json::from_str(&text).ok());
                }
            }
        }
        let Some(v) = self.get(url).await? else {
            return Ok(None);
        };
        let shape = v.get("geometry").filter(|g| !g.is_null()).map(simplify);
        if let (Some(store), Some(g)) = (&self.store, &shape) {
            store.set_meta(&key, &g.to_string())?;
        }
        Ok(shape)
    }
}

fn now_secs() -> i64 {
    Timestamp::now().as_second()
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn location_of(p: &PointInfo) -> Option<String> {
    match (&p.city, &p.state) {
        (Some(c), Some(s)) => Some(format!("{c}, {s}")),
        (Some(c), None) => Some(c.clone()),
        _ => None,
    }
}

pub fn parse_point(v: &Value, lat: f64, lon: f64) -> Result<PointInfo> {
    let p = v.get("properties").context("point has no properties")?;
    let url = |key: &str| str_at(p, key).with_context(|| format!("point has no {key}"));
    let rel = p.pointer("/relativeLocation/properties");
    Ok(PointInfo {
        lat,
        lon,
        city: rel.and_then(|r| str_at(r, "city")),
        state: rel.and_then(|r| str_at(r, "state")),
        time_zone: str_at(p, "timeZone"),
        office: str_at(p, "gridId"),
        forecast_url: url("forecast")?,
        hourly_url: url("forecastHourly")?,
        stations_url: url("observationStations")?,
    })
}

pub fn parse_forecast(v: &Value, location: Option<String>) -> Forecast {
    let props = v.get("properties");
    let periods = props
        .and_then(|p| p.get("periods"))
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .map(|p| Period {
                    name: str_at(p, "name").unwrap_or_default(),
                    start: str_at(p, "startTime").unwrap_or_default(),
                    end: str_at(p, "endTime").unwrap_or_default(),
                    is_daytime: p.get("isDaytime").and_then(Value::as_bool).unwrap_or(true),
                    temperature_f: p.get("temperature").and_then(Value::as_f64),
                    precip_chance_pct: p
                        .pointer("/probabilityOfPrecipitation/value")
                        .and_then(Value::as_f64),
                    wind: str_at(p, "windSpeed"),
                    wind_direction: str_at(p, "windDirection"),
                    short: str_at(p, "shortForecast").unwrap_or_default(),
                    detailed: str_at(p, "detailedForecast"),
                })
                .collect()
        })
        .unwrap_or_default();
    Forecast {
        location,
        updated: props.and_then(|p| str_at(p, "updateTime").or_else(|| str_at(p, "updated"))),
        periods,
        attribution: ATTRIBUTION,
    }
}

#[derive(Debug, Clone)]
pub struct StationRef {
    pub id: String,
    pub name: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

pub fn parse_stations(v: &Value) -> Vec<StationRef> {
    v.get("features")
        .and_then(Value::as_array)
        .map(|fs| {
            fs.iter()
                .filter_map(|f| {
                    let p = f.get("properties")?;
                    let c = f.pointer("/geometry/coordinates");
                    Some(StationRef {
                        id: str_at(p, "stationIdentifier")?,
                        name: str_at(p, "name"),
                        lon: c.and_then(|c| c.get(0)).and_then(Value::as_f64),
                        lat: c.and_then(|c| c.get(1)).and_then(Value::as_f64),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn distance_mi(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = (p2 - p1, (lon2 - lon1).to_radians());
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    3958.8 * 2.0 * a.sqrt().asin()
}

/// One decimal place: an observation is not more exact than that.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub fn parse_observation(v: &Value, s: &StationRef, lat: f64, lon: f64) -> Option<Conditions> {
    let p = v.get("properties")?;
    let value = |key: &str| {
        p.get(key)
            .and_then(|q| q.get("value"))
            .and_then(Value::as_f64)
    };
    let f = |c: f64| round1(c * 9.0 / 5.0 + 32.0);
    let mph = |kmh: f64| round1(kmh / 1.609_344);
    Some(Conditions {
        station: s.id.clone(),
        station_name: s.name.clone(),
        distance_mi: s
            .lat
            .zip(s.lon)
            .map(|(a, b)| round1(distance_mi(lat, lon, a, b))),
        observed_at: str_at(p, "timestamp")?,
        summary: str_at(p, "textDescription"),
        temperature_f: value("temperature").map(f),
        dewpoint_f: value("dewpoint").map(f),
        humidity_pct: value("relativeHumidity").map(f64::round),
        wind_mph: value("windSpeed").map(mph),
        wind_gust_mph: value("windGust").map(mph),
        wind_direction_deg: value("windDirection"),
        pressure_mb: value("barometricPressure").map(|pa| round1(pa / 100.0)),
        visibility_mi: value("visibility").map(|m| round1(m / 1609.344)),
        precip_last_hour_in: value("precipitationLastHour")
            .map(|mm| (mm / 25.4 * 100.0).round() / 100.0),
        attribution: ATTRIBUTION,
    })
}

pub fn parse_alerts(v: &Value) -> Vec<Alert> {
    v.get("features")
        .and_then(Value::as_array)
        .map(|fs| {
            fs.iter()
                .filter_map(|f| {
                    let p = f.get("properties")?;
                    Some(Alert {
                        id: str_at(p, "id").or_else(|| str_at(f, "id"))?,
                        event: str_at(p, "event")?,
                        severity: str_at(p, "severity").unwrap_or_else(|| "Unknown".into()),
                        urgency: str_at(p, "urgency"),
                        certainty: str_at(p, "certainty"),
                        headline: str_at(p, "headline"),
                        area: str_at(p, "areaDesc"),
                        onset: str_at(p, "onset"),
                        ends: str_at(p, "ends"),
                        expires: str_at(p, "expires"),
                        sender: str_at(p, "senderName"),
                        description: str_at(p, "description"),
                        instruction: str_at(p, "instruction"),
                        zones: p
                            .get("affectedZones")
                            .and_then(Value::as_array)
                            .map(|z| {
                                z.iter()
                                    .filter_map(|u| u.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        geometry: f.get("geometry").filter(|g| !g.is_null()).cloned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Alerts as a GeoJSON FeatureCollection. Alerts with no shape are left out of
/// the features and counted, so the list can still say they exist.
pub fn alerts_geojson(alerts: &[Alert]) -> Value {
    let features: Vec<Value> = alerts
        .iter()
        .filter(|a| a.geometry.is_some())
        .map(|a| {
            json!({
                "type": "Feature",
                "geometry": a.geometry,
                "properties": {
                    "id": a.id,
                    "event": a.event,
                    "severity": a.severity,
                    "headline": a.headline,
                    "area": a.area,
                    "ends": a.ends.as_ref().or(a.expires.as_ref()),
                },
            })
        })
        .collect();
    json!({ "type": "FeatureCollection", "features": features })
}

/// Round coordinates to ~100 m and drop the repeats that leaves. County-sized
/// zones ship thousands of coastline vertices no alert shading needs.
pub fn simplify(g: &Value) -> Value {
    fn walk(v: &Value) -> Value {
        match v {
            Value::Array(items) if is_position(items) => Value::Array(
                items
                    .iter()
                    .map(|n| {
                        n.as_f64()
                            .map_or(Value::Null, |f| json!((f * 1000.0).round() / 1000.0))
                    })
                    .collect(),
            ),
            Value::Array(items) => {
                let mut out: Vec<Value> = items.iter().map(walk).collect();
                if items
                    .first()
                    .is_some_and(|i| i.as_array().is_some_and(|a| is_position(a)))
                {
                    out.dedup();
                    // A ring must stay a ring.
                    if out.len() < 4 {
                        return Value::Array(items.iter().map(walk).collect());
                    }
                }
                Value::Array(out)
            }
            other => other.clone(),
        }
    }
    let mut out = g.clone();
    if let Some(c) = g.get("coordinates") {
        out["coordinates"] = walk(c);
    }
    out
}

fn is_position(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(Value::is_number)
}

/// Polygons and multipolygons, combined into one MultiPolygon.
pub fn merge_polygons(shapes: &[Value]) -> Option<Value> {
    let mut polys: Vec<Value> = Vec::new();
    for s in shapes {
        match s.get("type").and_then(Value::as_str) {
            Some("Polygon") => polys.extend(s.get("coordinates").cloned()),
            Some("MultiPolygon") => {
                if let Some(arr) = s.get("coordinates").and_then(Value::as_array) {
                    polys.extend(arr.iter().cloned());
                }
            }
            _ => {}
        }
    }
    (!polys.is_empty()).then(|| json!({ "type": "MultiPolygon", "coordinates": polys }))
}

/// West, south, east, north of a geometry.
pub fn bounds_of(g: &Value) -> Option<[f64; 4]> {
    fn walk(v: &Value, b: &mut Option<[f64; 4]>) {
        let Some(items) = v.as_array() else { return };
        if let (Some(lon), Some(lat)) = (
            items.first().and_then(Value::as_f64),
            items.get(1).and_then(Value::as_f64),
        ) {
            let e = b.get_or_insert([lon, lat, lon, lat]);
            e[0] = e[0].min(lon);
            e[1] = e[1].min(lat);
            e[2] = e[2].max(lon);
            e[3] = e[3].max(lat);
            return;
        }
        for i in items {
            walk(i, b);
        }
    }
    let mut b = None;
    walk(g.get("coordinates")?, &mut b);
    b
}

fn intersects(b: [f64; 4], r: &Region) -> bool {
    b[0] <= r.east && b[2] >= r.west && b[1] <= r.north && b[3] >= r.south
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn point_yields_the_urls_everything_else_needs() {
        let p = parse_point(&fixture("nws_point.json"), 39.74, -104.99).unwrap();
        assert!(p.forecast_url.ends_with("/forecast"));
        assert!(p.hourly_url.ends_with("/forecast/hourly"));
        assert!(p.stations_url.ends_with("/stations"));
        assert!(p.city.is_some());
        assert_eq!(p.time_zone.as_deref(), Some("America/Denver"));
    }

    #[test]
    fn forecast_periods_keep_temperature_and_rain_chance() {
        let f = parse_forecast(&fixture("nws_forecast.json"), Some("Somewhere".into()));
        assert!(!f.periods.is_empty());
        let p = &f.periods[0];
        assert!(p.temperature_f.is_some());
        assert!(!p.short.is_empty());
        assert!(!p.start.is_empty());
    }

    #[test]
    fn observation_converts_si_units() {
        let s = StationRef {
            id: "KXYZ".into(),
            name: Some("Example".into()),
            lat: Some(27.0),
            lon: Some(-80.0),
        };
        let c = parse_observation(&fixture("nws_observation.json"), &s, 27.5, -80.0).unwrap();
        assert_eq!(c.temperature_f, Some(71.6), "22 °C");
        assert_eq!(c.distance_mi, Some(34.5));
        assert_eq!(c.pressure_mb, Some(1013.2));
        assert_eq!(c.wind_mph, Some(9.2), "14.76 km/h");
        assert_eq!(c.precip_last_hour_in, None, "null stays absent, not zero");
    }

    #[test]
    fn alerts_keep_their_own_shape_or_their_zones() {
        let a = parse_alerts(&fixture("nws_alerts.json"));
        assert_eq!(a.len(), 2);
        let storm = a.iter().find(|x| x.geometry.is_some()).unwrap();
        assert_eq!(storm.severity, "Severe");
        let zoned = a.iter().find(|x| x.geometry.is_none()).unwrap();
        assert_eq!(zoned.zones.len(), 2);
    }

    #[test]
    fn geojson_counts_only_drawable_alerts() {
        let a = parse_alerts(&fixture("nws_alerts.json"));
        let g = alerts_geojson(&a);
        assert_eq!(g["features"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merges_polygons_and_multipolygons() {
        let tri = json!([[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]]]);
        let shapes = [
            json!({"type": "Polygon", "coordinates": tri.clone()}),
            json!({"type": "MultiPolygon", "coordinates": [tri.clone(), tri]}),
            json!({"type": "Point", "coordinates": [0.0, 0.0]}),
        ];
        let m = merge_polygons(&shapes).unwrap();
        assert_eq!(m["coordinates"].as_array().unwrap().len(), 3);
        assert_eq!(merge_polygons(&[]), None);
    }

    #[test]
    fn bounds_and_region_test() {
        let g = json!({"type": "Polygon", "coordinates":
            [[[-80.5, 26.0], [-80.0, 26.0], [-80.0, 26.5], [-80.5, 26.0]]]});
        let b = bounds_of(&g).unwrap();
        assert_eq!(b, [-80.5, 26.0, -80.0, 26.5]);
        let inside = crate::grid::Region {
            west: -81.0,
            south: 25.0,
            east: -79.0,
            north: 27.0,
        };
        let away = crate::grid::Region {
            west: -90.0,
            south: 30.0,
            east: -85.0,
            north: 35.0,
        };
        assert!(intersects(b, &inside));
        assert!(!intersects(b, &away));
    }

    #[test]
    fn simplify_rounds_and_dedups_but_keeps_rings_closed() {
        let g = json!({"type": "Polygon", "coordinates": [[
            [-80.12341, 26.00001], [-80.12342, 26.00002], [-80.0, 26.0],
            [-80.0, 26.5], [-80.12341, 26.00001]
        ]]});
        let s = simplify(&g);
        let ring = s["coordinates"][0].as_array().unwrap();
        assert_eq!(ring.len(), 4, "the near-duplicate vertex collapses");
        assert_eq!(ring.first(), ring.last());
        assert_eq!(ring[0][0], json!(-80.123));
    }
}

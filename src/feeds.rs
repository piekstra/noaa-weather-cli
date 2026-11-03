//! Same-day rain gauges, read from the networks that publish them.
//!
//! GHCN-Daily runs 2 to 3 days behind, so it cannot say how a gauge did today. These
//! feeds can, at the price of readings nobody has checked yet: every figure from them
//! is marked provisional.
//!
//! | Feed | Steps | Reach | Read by |
//! |---|---|---|---|
//! | CoCoRaHS export | one report a day, about 7 AM | the morning it is entered | state |
//! | USGS continuous values | 5 to 60 minutes, summed to hours | under an hour old | box |
//! | ASOS through the Iowa Environmental Mesonet | hourly | about an hour old | state |
//!
//! ## One row per gauge
//!
//! A CoCoRaHS station is in GHCN-Daily too, as `US1CODN0012` for `CO-DN-12`, so its
//! fresh reports join its archive reports under that id, and the archive wins for a
//! date both hold. ASOS airports are also in the archive, under an id with nothing in
//! common, so they are matched by distance: see [`same_gauge`].
//!
//! ## What is sent upstream
//!
//! A two-letter state, or for USGS a box of whole degrees, never a saved place.
//! `docs/api.md` records each endpoint and its traps.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use jiff::{civil, tz::TimeZone, Timestamp};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::engine::Weather;
use crate::ghcn::{Network, Report, Station};
use crate::grid::Region;
use crate::store::{now_secs, Store};
use crate::window::Window;

pub const DEFAULT_USGS_URL: &str = "https://api.waterdata.usgs.gov/ogcapi/v1";
pub const DEFAULT_COCORAHS_URL: &str = "https://data.cocorahs.org";
pub const DEFAULT_IEM_URL: &str = "https://mesonet.agron.iastate.edu";
/// Several times the slowest first read seen: 13 s, USGS over a 10 by 6 degree box and
/// 3 days of mostly 5 and 15 minute readings.
pub const DEFAULT_FEED_DEADLINE: Duration = Duration::from_secs(60);

/// `s_flag` on a report from a feed, so a total can say it holds unchecked figures.
pub const LIVE_SOURCE: &str = "live";

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// How a feed is read.
pub struct Pace {
    /// How far back it is read.
    pub reach: i64,
    /// How far behind the newest read to read again, for readings that arrive late.
    pub settle: i64,
    /// Least time between reads of the same recent hours.
    pub every: i64,
}

/// An 8 degree box of USGS sites can hold 60,000 readings a day, so the hourly feeds
/// reach no further than the archive's lag.
const HOURLY: Pace = Pace {
    reach: 3 * DAY,
    settle: 3 * HOUR,
    every: 15 * 60,
};
const DAILY: Pace = Pace {
    reach: 7 * DAY,
    settle: 2 * DAY,
    every: 30 * 60,
};
/// Readings older than this are dropped from the cache.
const KEEP_SECS: i64 = 14 * DAY;
/// After a failure, how long the cached rows answer without asking again.
const AFTER_FAILURE: i64 = 5 * 60;
const STATION_LIST_EVERY: i64 = 30 * DAY;
/// The US record for an hour, 12 inches at Holt, Missouri. USGS serves unqualified
/// readings far above it, so a larger hour is an instrument fault and is not counted.
const MAX_HOUR_MM: f64 = 305.0;
/// The US record for a day, 43 inches at Alvin, Texas.
const MAX_DAY_MM: f64 = 1100.0;
const USGS_PAGE: usize = 10_000;
/// A safety stop for a next link that never ends: far above an 8 degree box in 3 days.
const USGS_MAX_PAGES: usize = 100;
const USGS_IDS_PER_REQUEST: usize = 100;
/// An ASOS report counts for the nearest hour only if it is this close to it.
const ASOS_MAX_OFFSET_SECS: i64 = 15 * 60;
/// Airports whose ASOS and archive coordinates are this close are one gauge.
const SAME_GAUGE_KM: f64 = 3.0;

/// One feed's state after a refresh, for a reader deciding how far to trust a gap.
#[derive(Debug, Clone, Serialize)]
pub struct FeedStatus {
    pub name: &'static str,
    /// Why the feed could not be read. What it has cached still answers.
    pub error: Option<String>,
}

/// A complete hour at one gauge.
#[derive(Debug, Clone, PartialEq)]
pub struct HourReading {
    pub station: String,
    /// End of the hour, unix seconds.
    pub end: i64,
    pub mm: f64,
    pub trace: bool,
    /// The source gave a rain amount above zero, which shows its gauge is working.
    pub measured_rain: bool,
}

// ---------------------------------------------------------------------- HTTP

pub struct FeedClient {
    http: reqwest::Client,
    usgs: String,
    cocorahs: String,
    iem: String,
}

impl FeedClient {
    pub fn new(usgs: &str, cocorahs: &str, iem: &str) -> Result<Self> {
        let base = |s: &str| s.trim_end_matches('/').to_string();
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(concat!(
                    "noaa-weather-cli/",
                    env!("CARGO_PKG_VERSION"),
                    " (+https://github.com/piekstra/noaa-weather-cli)"
                ))
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(90))
                .build()?,
            usgs: base(usgs),
            cocorahs: base(cocorahs),
            iem: base(iem),
        })
    }

    /// One text body. A busy or failing upstream gets two more tries, spaced out.
    async fn get(&self, url: &str) -> Result<String> {
        let mut last = None;
        for wait in [0, 2, 6] {
            if wait > 0 {
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
            match self.http.get(url).send().await {
                Ok(r) if r.status().is_success() => {
                    return r.text().await.with_context(|| format!("GET {url}"));
                }
                Ok(r) if r.status().is_server_error() || r.status().as_u16() == 429 => {
                    last = Some(anyhow::anyhow!("GET {url}: {}", r.status()));
                }
                Ok(r) => anyhow::bail!("GET {url}: {}", r.status()),
                Err(e) => last = Some(anyhow::Error::new(e).context(format!("GET {url}"))),
            }
        }
        Err(last.expect("at least one try"))
    }

    async fn usgs_readings(&self, region: &Region, from: i64, to: i64) -> Result<Vec<UsgsReading>> {
        let [w, s, e, n] = whole_degrees(region);
        let (from, to) = (Timestamp::from_second(from)?, Timestamp::from_second(to)?);
        let mut url = format!(
            "{}/collections/continuous/items?f=json&skipGeometry=true&bbox={w},{s},{e},{n}\
             &parameter_code=00045&time={}/{}&limit={USGS_PAGE}\
             &properties=monitoring_location_id,time_series_id,time,value",
            self.usgs,
            from.strftime("%Y-%m-%dT%H:%M:%SZ"),
            to.strftime("%Y-%m-%dT%H:%M:%SZ"),
        );
        let mut out = Vec::new();
        for _ in 0..USGS_MAX_PAGES {
            let page = parse_usgs_page(&self.get(&url).await?)?;
            out.extend(page.readings);
            match page.next {
                // Only its query is taken, so a page cannot send this client elsewhere.
                Some(next) => {
                    let (_, query) = next
                        .split_once('?')
                        .context("USGS sent a next link with no query")?;
                    url = format!("{}/collections/continuous/items?{query}", self.usgs);
                }
                None => return Ok(out),
            }
        }
        anyhow::bail!("USGS still had readings after {USGS_MAX_PAGES} pages")
    }

    async fn usgs_stations(&self, ids: &[String]) -> Result<Vec<Station>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(USGS_IDS_PER_REQUEST) {
            let url = format!(
                "{}/collections/monitoring-locations/items?f=json&limit={USGS_IDS_PER_REQUEST}\
                 &properties=monitoring_location_name&id={}",
                self.usgs,
                chunk.join(",")
            );
            out.extend(parse_usgs_stations(&self.get(&url).await?)?);
        }
        Ok(out)
    }

    async fn asos_stations(&self, state: &str) -> Result<Vec<Station>> {
        let url = format!("{}/geojson/network/{state}_ASOS.geojson", self.iem);
        parse_iem_network(&self.get(&url).await?)
    }

    async fn asos_hours(
        &self,
        states: &[String],
        from: civil::Date,
        to: civil::Date,
    ) -> Result<Vec<HourReading>> {
        // The end date is exclusive.
        let to = to.tomorrow()?;
        let networks: String = states
            .iter()
            .map(|s| format!("&network={s}_ASOS"))
            .collect();
        let url = format!(
            "{}/cgi-bin/request/asos.py?data=p01i&data=metar{networks}\
             &year1={}&month1={}&day1={}&year2={}&month2={}&day2={}\
             &tz=Etc/UTC&format=onlycomma&latlon=yes&missing=M&trace=T&direct=no&report_type=3",
            self.iem,
            from.year(),
            from.month(),
            from.day(),
            to.year(),
            to.month(),
            to.day(),
        );
        parse_iem_hours(&self.get(&url).await?)
    }

    async fn cocorahs_reports(
        &self,
        state: &str,
        from: civil::Date,
        to: civil::Date,
    ) -> Result<CocorahsDay> {
        let url = format!(
            "{}/export/exportreports.aspx?ReportType=Daily&dtf=1&Format=CSV\
             &ReportDateType=reportdate&TimesInGMT=False&State={state}&StartDate={}&EndDate={}",
            self.cocorahs,
            from.strftime("%m/%d/%Y"),
            to.strftime("%m/%d/%Y"),
        );
        parse_cocorahs(&self.get(&url).await?)
    }
}

/// The region's box, widened to whole degrees so a request does not outline saved places.
fn whole_degrees(r: &Region) -> [i64; 4] {
    [
        r.west.floor() as i64,
        r.south.floor() as i64,
        r.east.ceil() as i64,
        r.north.ceil() as i64,
    ]
}

// ------------------------------------------------------------------- parsing

#[derive(Debug, PartialEq)]
pub struct UsgsReading {
    pub site: String,
    pub series: String,
    /// Unix seconds, the end of the interval the value covers.
    pub at: i64,
    pub inches: f64,
}

struct UsgsPage {
    readings: Vec<UsgsReading>,
    next: Option<String>,
}

fn parse_usgs_page(body: &str) -> Result<UsgsPage> {
    #[derive(Deserialize)]
    struct Doc {
        // Required: a body without it is a changed response, not an empty one.
        features: Vec<Feature>,
        #[serde(default)]
        links: Vec<Link>,
    }
    #[derive(Deserialize)]
    struct Feature {
        properties: Props,
    }
    #[derive(Deserialize)]
    struct Props {
        monitoring_location_id: String,
        time_series_id: String,
        time: String,
        // Null on the rows that stand for an outage, which carry a qualifier instead:
        // EQUIP, SEASONAL, UNAVAIL, DISCONTINUED.
        value: Option<String>,
    }
    #[derive(Deserialize)]
    struct Link {
        rel: String,
        href: String,
    }
    let doc: Doc = serde_json::from_str(body).context("USGS continuous values")?;
    let mut readings = Vec::with_capacity(doc.features.len());
    let (mut understood, mut not_understood) = (0, 0);
    for f in doc.features {
        let p = f.properties;
        let Ok(at) = p.time.parse::<Timestamp>() else {
            not_understood += 1;
            continue;
        };
        understood += 1;
        let Some(inches) = p.value.and_then(|v| v.parse::<f64>().ok()) else {
            continue;
        };
        readings.push(UsgsReading {
            site: p.monitoring_location_id,
            series: p.time_series_id,
            at: at.as_second(),
            inches,
        });
    }
    anyhow::ensure!(
        understood > 0 || not_understood == 0,
        "the USGS time format has changed"
    );
    let next = doc
        .links
        .into_iter()
        .find(|l| l.rel == "next")
        .map(|l| l.href);
    Ok(UsgsPage { readings, next })
}

fn parse_usgs_stations(body: &str) -> Result<Vec<Station>> {
    #[derive(Deserialize)]
    struct Doc {
        features: Vec<Feature>,
    }
    #[derive(Deserialize)]
    struct Feature {
        id: String,
        geometry: Option<Point>,
        properties: Props,
    }
    #[derive(Deserialize)]
    struct Point {
        coordinates: (f64, f64),
    }
    #[derive(Deserialize)]
    struct Props {
        monitoring_location_name: Option<String>,
    }
    let doc: Doc = serde_json::from_str(body).context("USGS monitoring locations")?;
    Ok(doc
        .features
        .into_iter()
        .filter_map(|f| {
            let (lon, lat) = f.geometry?.coordinates;
            Some(Station {
                name: f.properties.monitoring_location_name.unwrap_or_default(),
                id: f.id,
                lat,
                lon,
                elev_m: None,
            })
        })
        .collect())
}

/// Sum a site's readings into complete hours.
///
/// A value covers the interval ending at its time, so `:15`, `:30`, `:45` and `:00`
/// make the hour ending at `:00`. An hour counts only with every reading the site's
/// own step calls for: an hour with a gap would read as less rain than fell. A site
/// with two instruments has two series, and the one with the lowest id is used.
pub fn usgs_hours(mut readings: Vec<UsgsReading>) -> Vec<HourReading> {
    readings.sort_by(|a, b| (&a.site, &a.series, a.at).cmp(&(&b.site, &b.series, b.at)));
    let mut out = Vec::new();
    for site in readings.chunk_by(|a, b| a.site == b.site) {
        let series = site
            .chunk_by(|a, b| a.series == b.series)
            .next()
            .expect("non-empty");
        let Some(step) = modal_step(series) else {
            continue;
        };
        if step == 0 || step > HOUR || HOUR % step != 0 {
            continue;
        }
        let wanted = (HOUR / step) as usize;
        let mut hours: BTreeMap<i64, (usize, f64)> = BTreeMap::new();
        for r in series {
            let h = hours.entry(hour_ending_at_or_after(r.at)).or_default();
            h.0 += 1;
            h.1 += r.inches;
        }
        for (end, (count, inches)) in hours {
            let mm = inches * 25.4;
            if count == wanted && (0.0..=MAX_HOUR_MM).contains(&mm) {
                out.push(HourReading {
                    station: series[0].site.clone(),
                    end,
                    mm,
                    trace: false,
                    // A USGS zero is a reading, unlike a report that names no amount.
                    measured_rain: true,
                });
            }
        }
    }
    out
}

fn modal_step(series: &[UsgsReading]) -> Option<i64> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for pair in series.windows(2) {
        *counts.entry(pair[1].at - pair[0].at).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(step, n)| (n, step))
        .map(|(step, _)| step)
}

fn hour_ending_at_or_after(t: i64) -> i64 {
    (t + HOUR - 1).div_euclid(HOUR) * HOUR
}

fn parse_iem_network(body: &str) -> Result<Vec<Station>> {
    #[derive(Deserialize)]
    struct Doc {
        features: Vec<Feature>,
    }
    #[derive(Deserialize)]
    struct Feature {
        geometry: Point,
        properties: Props,
    }
    #[derive(Deserialize)]
    struct Point {
        coordinates: (f64, f64),
    }
    #[derive(Deserialize)]
    struct Props {
        sid: String,
        sname: String,
    }
    let doc: Doc = serde_json::from_str(body).context("IEM station list")?;
    Ok(doc
        .features
        .into_iter()
        .map(|f| Station {
            id: format!("ASOS-{}", f.properties.sid),
            name: f.properties.sname,
            lat: f.geometry.coordinates.1,
            lon: f.geometry.coordinates.0,
            elev_m: None,
        })
        .collect())
}

/// Whether a METAR weather group names precipitation falling at the station, as
/// `-RA`, `+TSRA` or `SHRA`. Vicinity groups do not count, and neither does a token
/// that only happens to contain the letters, as `KRAL` or `BKN020` do.
fn weather_group_has_precipitation(token: &str) -> bool {
    const DESCRIPTORS: [&str; 8] = ["MI", "PR", "BC", "DR", "BL", "SH", "TS", "FZ"];
    const PRECIPITATION: [&str; 9] = ["RA", "DZ", "SN", "SG", "IC", "PL", "GR", "GS", "UP"];
    let body = token.trim_start_matches(['-', '+']);
    if !body.is_ascii() || !body.len().is_multiple_of(2) {
        return false;
    }
    let mut falling = false;
    for code in body.as_bytes().chunks(2) {
        let code = std::str::from_utf8(code).unwrap_or("");
        if PRECIPITATION.contains(&code) {
            falling = true;
        } else if !DESCRIPTORS.contains(&code) {
            return false;
        }
    }
    falling
}

/// Whether a remark says precipitation began or ended this hour, as `RAB18` or `RAE24`.
fn remark_has_precipitation(token: &str) -> bool {
    [
        "RAB", "RAE", "DZB", "DZE", "SHRAB", "SHRAE", "UPB", "UPE", "SNB", "SNE",
    ]
    .iter()
    .any(|p| {
        token
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// What a routine report says about its hour: the rain amount and whether it was a
/// trace, or `None` if the report does not measure the hour.
///
/// `p01i` reads 0.00 whenever a report has no amount group, even while the same report
/// says it is raining, so that zero means nothing. Remarks mark a sensor that is out
/// with `PNO`. Otherwise no group means no rain, which is how a dry hour is reported.
fn asos_hour(p01i: &str, metar: &str) -> Option<(f64, bool, bool)> {
    let (body, remarks) = metar.split_once(" RMK ").unwrap_or((metar, ""));
    let remarks: Vec<&str> = remarks.split_whitespace().collect();
    if remarks.contains(&"PNO") {
        return None;
    }
    let has_amount = remarks
        .iter()
        .any(|t| t.len() == 5 && t.starts_with('P') && t[1..].bytes().all(|b| b.is_ascii_digit()));
    let reports_precipitation = body.split_whitespace().any(weather_group_has_precipitation)
        || remarks.iter().any(|t| remark_has_precipitation(t));
    if reports_precipitation && !has_amount {
        return None;
    }
    match p01i.trim() {
        "T" => Some((0.0, true, false)),
        "M" | "" => None,
        v => {
            let mm = v.parse::<f64>().ok()? * 25.4;
            (0.0..=MAX_HOUR_MM)
                .contains(&mm)
                .then_some((mm, false, has_amount && mm > 0.0))
        }
    }
}

/// Routine ASOS reports as hours. A report's `p01i` is the rain since the last hourly
/// reset, usually near `:53`, so it stands for the hour ending at the nearest `:00`.
fn parse_iem_hours(body: &str) -> Result<Vec<HourReading>> {
    #[derive(Deserialize)]
    struct Row {
        station: String,
        valid: String,
        p01i: String,
        metar: String,
    }
    // Later reports overwrite earlier ones for the same hour.
    let mut hours: BTreeMap<(String, i64), (f64, bool, bool)> = BTreeMap::new();
    let (mut understood, mut not_understood) = (0, 0);
    for row in csv::Reader::from_reader(body.as_bytes()).deserialize::<Row>() {
        let Ok(row) = row else {
            not_understood += 1;
            continue;
        };
        let Ok(valid) = civil::DateTime::strptime("%Y-%m-%d %H:%M", row.valid.trim()) else {
            not_understood += 1;
            continue;
        };
        understood += 1;
        let at = valid.to_zoned(TimeZone::UTC)?.timestamp().as_second();
        let end = (at + HOUR / 2).div_euclid(HOUR) * HOUR;
        if (at - end).abs() > ASOS_MAX_OFFSET_SECS {
            continue;
        }
        let key = (format!("ASOS-{}", row.station), end);
        match asos_hour(&row.p01i, &row.metar) {
            Some(hour) => {
                hours.insert(key, hour);
            }
            None => {
                hours.remove(&key);
            }
        }
    }
    anyhow::ensure!(
        understood > 0 || not_understood == 0,
        "the IEM ASOS format has changed"
    );
    Ok(hours
        .into_iter()
        .map(|((station, end), (mm, trace, measured_rain))| HourReading {
            station,
            end,
            mm,
            trace,
            measured_rain,
        })
        .collect())
}

/// `CO-DN-12` is `US1CODN0012` in GHCN-Daily. A county of three letters or an id of
/// another shape has no such twin, and keeps its CoCoRaHS number.
pub fn cocorahs_id(number: &str) -> String {
    let parts: Vec<&str> = number.split('-').collect();
    if let [state, county, n] = parts[..] {
        let letters = |s: &str, len| s.len() == len && s.bytes().all(|b| b.is_ascii_uppercase());
        if let (true, true, Ok(n)) = (letters(state, 2), letters(county, 2), n.parse::<u32>()) {
            if n <= 9999 {
                return format!("US1{state}{county}{n:04}");
            }
        }
    }
    format!("CoCoRaHS-{number}")
}

#[derive(Debug, Default)]
pub struct CocorahsDay {
    pub stations: Vec<Station>,
    pub reports: Vec<Report>,
}

fn parse_cocorahs(body: &str) -> Result<CocorahsDay> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(rename = "ObservationDate")]
        date: String,
        #[serde(rename = "ObservationTime")]
        time: String,
        #[serde(rename = "StationNumber")]
        number: String,
        #[serde(rename = "StationName")]
        name: String,
        #[serde(rename = "Latitude")]
        lat: f64,
        #[serde(rename = "Longitude")]
        lon: f64,
        #[serde(rename = "TotalPrecipAmt")]
        amount: String,
    }
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());
    let mut out = CocorahsDay::default();
    let mut seen = HashSet::new();
    let (mut understood, mut not_understood) = (0, 0);
    for row in reader.deserialize::<Row>() {
        let Ok(row) = row else {
            not_understood += 1;
            continue;
        };
        let Ok(date) = civil::Date::strptime("%Y-%m-%d", &row.date) else {
            not_understood += 1;
            continue;
        };
        understood += 1;
        let (prcp, m_flag) = match row.amount.as_str() {
            "T" => (0, Some("T".to_string())),
            "NA" | "" => continue,
            v => match v.parse::<f64>() {
                Ok(i) if (0.0..=MAX_DAY_MM / 25.4).contains(&i) => {
                    ((i * 254.0).round() as i64, None)
                }
                _ => continue,
            },
        };
        let id = cocorahs_id(&row.number);
        if seen.insert(id.clone()) {
            out.stations.push(Station {
                id: id.clone(),
                name: row.name,
                lat: row.lat,
                lon: row.lon,
                elev_m: None,
            });
        }
        out.reports.push(Report {
            station: id,
            date,
            prcp,
            obs_time: civil::Time::strptime("%I:%M %p", &row.time)
                .ok()
                .map(|t| t.strftime("%H%M").to_string()),
            m_flag,
            s_flag: Some(LIVE_SOURCE.to_string()),
        });
    }
    anyhow::ensure!(
        understood > 0 || not_understood == 0,
        "the CoCoRaHS export format has changed"
    );
    Ok(out)
}

// ------------------------------------------------------------------- planning

/// What to read so a span of unix seconds is covered, given what was read before, and
/// the span to remember afterwards. Hours never read are always read. Older hours are
/// read once, and recent ones again from `settle` behind the newest read, at most once
/// per `every`.
pub fn plan_reads(
    have: Option<(i64, i64)>,
    want: (i64, i64),
    age: i64,
    pace: &Pace,
) -> (Vec<(i64, i64)>, (i64, i64)) {
    let Some((from, through)) = have else {
        return (vec![want], want);
    };
    // Nothing joins what was read to what is wanted, and the gap may be long.
    if want.0 > through || want.1 < from {
        return (vec![want], want);
    }
    let mut reads = Vec::new();
    if want.0 < from {
        reads.push((want.0, from));
    }
    let recent = through - pace.settle;
    if want.1 > recent && age >= pace.every {
        reads.push((recent, want.1));
    } else if want.1 > through {
        reads.push((through, want.1));
    }
    (reads, (from.min(want.0), through.max(want.1)))
}

fn parse_span(value: &str) -> Option<(i64, i64)> {
    let (a, b) = value.split_once(',')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

// -------------------------------------------------------------------- storage

fn put_stations(store: &Store, stations: &[Station]) -> Result<()> {
    let mut conn = store.pool().get()?;
    let tx = conn.transaction()?;
    for s in stations {
        tx.execute(
            "INSERT OR REPLACE INTO feed_stations (id, name, lat, lon) VALUES (?1, ?2, ?3, ?4)",
            params![s.id, s.name, s.lat, s.lon],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn stations_in(store: &Store, region: &Region) -> Result<Vec<Station>> {
    let conn = store.pool().get()?;
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, lat, lon FROM feed_stations
         WHERE lat BETWEEN ?1 AND ?2 AND lon BETWEEN ?3 AND ?4 ORDER BY id",
    )?;
    let rows = stmt.query_map(
        params![region.south, region.north, region.west, region.east],
        |r| {
            Ok(Station {
                id: r.get(0)?,
                name: r.get(1)?,
                lat: r.get(2)?,
                lon: r.get(3)?,
                elev_m: None,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn put_hours(store: &Store, hours: &[HourReading]) -> Result<()> {
    let mut conn = store.pool().get()?;
    let tx = conn.transaction()?;
    for h in hours {
        tx.execute(
            "INSERT OR REPLACE INTO feed_hours (station, hour_end, mm, trace, measured_rain)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![h.station, h.end, h.mm, h.trace, h.measured_rain],
        )?;
    }
    tx.execute(
        "DELETE FROM feed_hours WHERE hour_end < ?1",
        params![now_secs() - KEEP_SECS],
    )?;
    tx.commit()?;
    Ok(())
}

/// Hours wholly inside `start..end`, per station, oldest first. A gauge that has never
/// given a rain amount is left out, since its zeros may only mean it reports none.
pub(crate) fn hours_between(
    store: &Store,
    start: i64,
    end: i64,
) -> Result<HashMap<String, Vec<HourReading>>> {
    let conn = store.pool().get()?;
    let mut stmt = conn.prepare_cached(
        "SELECT station, hour_end, mm, trace, measured_rain FROM feed_hours
         WHERE hour_end >= ?1 AND hour_end <= ?2
           AND station IN (SELECT station FROM feed_hours WHERE measured_rain = 1)
         ORDER BY hour_end",
    )?;
    let rows = stmt.query_map(params![start + HOUR, end], |r| {
        Ok(HourReading {
            station: r.get(0)?,
            end: r.get(1)?,
            mm: r.get(2)?,
            trace: r.get(3)?,
            measured_rain: r.get(4)?,
        })
    })?;
    let mut out: HashMap<String, Vec<HourReading>> = HashMap::new();
    for row in rows {
        let row = row?;
        out.entry(row.station.clone()).or_default().push(row);
    }
    Ok(out)
}

fn put_reports(store: &Store, reports: &[Report]) -> Result<()> {
    let mut conn = store.pool().get()?;
    let tx = conn.transaction()?;
    for r in reports {
        tx.execute(
            "INSERT OR REPLACE INTO feed_reports (station, date, prcp, obs_time, m_flag)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                r.station,
                r.date.strftime("%Y%m%d").to_string(),
                r.prcp,
                r.obs_time,
                r.m_flag
            ],
        )?;
    }
    let oldest = Timestamp::from_second(now_secs() - KEEP_SECS)?
        .to_zoned(TimeZone::UTC)
        .date()
        .strftime("%Y%m%d")
        .to_string();
    tx.execute("DELETE FROM feed_reports WHERE date < ?1", params![oldest])?;
    tx.commit()?;
    Ok(())
}

/// Reports from CoCoRaHS for dates the archive does not hold yet.
pub(crate) fn reports_between(
    store: &Store,
    from: civil::Date,
    to: civil::Date,
) -> Result<Vec<Report>> {
    let conn = store.pool().get()?;
    let mut stmt = conn.prepare_cached(
        "SELECT station, date, prcp, obs_time, m_flag FROM feed_reports f
         WHERE date >= ?1 AND date <= ?2 AND NOT EXISTS (
             SELECT 1 FROM gauge_obs o WHERE o.station = f.station AND o.date = f.date
         )",
    )?;
    let f = from.strftime("%Y%m%d").to_string();
    let t = to.strftime("%Y%m%d").to_string();
    let rows = stmt.query_map(params![f, t], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (station, date, prcp, obs_time, m_flag) = row?;
        if let Ok(date) = civil::Date::strptime("%Y%m%d", &date) {
            out.push(Report {
                station,
                date,
                prcp,
                obs_time,
                m_flag,
                s_flag: Some(LIVE_SOURCE.to_string()),
            });
        }
    }
    Ok(out)
}

// -------------------------------------------------------------- the comparison

/// The hours of a gauge to compare: the longest unbroken stretch, the newest of equal
/// ones. MRMS is summed over exactly these hours, and a stretch costs one summed grid,
/// so a gauge with scattered gaps does not multiply the work.
pub fn longest_stretch(hours: &[HourReading]) -> &[HourReading] {
    let mut best = &hours[..0];
    for run in hours.chunk_by(|a, b| b.end - a.end == HOUR) {
        if run.len() >= best.len() {
            best = run;
        }
    }
    best
}

/// Distance between two points close together, in kilometres.
fn km_between(a: (f64, f64), b: (f64, f64)) -> f64 {
    let dlat = (a.0 - b.0) * 111.2;
    let dlon = (a.1 - b.1) * 111.2 * a.0.to_radians().cos();
    dlat.hypot(dlon)
}

/// Whether two rows are one airport's gauge, seen through the feed and the archive.
pub fn same_gauge(a: &crate::ghcn::GaugeTotal, b: &crate::ghcn::GaugeTotal) -> bool {
    a.network == Network::Asos
        && b.network == Network::Asos
        && a.step_hours != b.step_hours
        && km_between((a.lat, a.lon), (b.lat, b.lon)) <= SAME_GAUGE_KM
}

/// Keep one row for an airport in both sources: the one covering more hours, and the
/// archive's if they tie, because it has been checked.
pub fn one_row_per_gauge(rows: Vec<crate::ghcn::GaugeTotal>) -> Vec<crate::ghcn::GaugeTotal> {
    let hours = |g: &crate::ghcn::GaugeTotal| g.reports * g.step_hours;
    let dropped: HashSet<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, g)| {
            rows.iter().any(|other| {
                same_gauge(g, other)
                    && (hours(other) > hours(g) || (hours(other) == hours(g) && g.step_hours == 1))
            })
        })
        .map(|(i, _)| i)
        .collect();
    rows.into_iter()
        .enumerate()
        .filter(|(i, _)| !dropped.contains(i))
        .map(|(_, g)| g)
        .collect()
}

// ------------------------------------------------------------------- refreshing

impl Weather {
    /// Bring the feeds up to date for a window. A feed that fails is named in the
    /// result and its cached rows still answer.
    pub async fn refresh_feeds(&self, window: Window, force: bool) -> Vec<FeedStatus> {
        let states = self.region_states();
        let by_state = |feed: &'static str| match &states {
            Ok(Some(states)) => Ok(states.as_slice()),
            Ok(None) => Err(anyhow::anyhow!(
                "{feed} is read by state, and the station list naming them has not been read"
            )),
            Err(e) => Err(anyhow::anyhow!("{feed}: {e:#}")),
        };
        let (cocorahs, usgs, asos) = tokio::join!(
            self.read_feed("CoCoRaHS", window, &DAILY, force, |span| async move {
                self.read_cocorahs(by_state("CoCoRaHS")?, span).await
            }),
            self.read_feed("USGS", window, &HOURLY, force, |span| self.read_usgs(span)),
            self.read_feed("ASOS", window, &HOURLY, force, |span| async move {
                self.read_asos(by_state("ASOS")?, span).await
            }),
        );
        vec![cocorahs, usgs, asos]
    }

    /// The US states the region touches, from the archive's station list, or `None`
    /// before that list has been read.
    fn region_states(&self) -> Result<Option<Vec<String>>> {
        let meta = self.store().meta(&crate::ghcn::states_key(self.region()))?;
        Ok(meta.map(|(value, _)| {
            value
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        }))
    }

    async fn read_feed<F, Fut>(
        &self,
        name: &'static str,
        window: Window,
        pace: &Pace,
        force: bool,
        read: F,
    ) -> FeedStatus
    where
        F: Fn((i64, i64)) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let outcome = async {
            let store = self.store();
            let key = format!("feed:{name}:{}", self.region().key());
            let failed_key = format!("{key}:failed");
            let now = now_secs();
            let want = (
                window.start.as_second().max(now - pace.reach),
                // Whole hours, so a window ending in the future does not count as new
                // hours on every call.
                window.end.as_second().min(now).div_euclid(HOUR) * HOUR,
            );
            if want.0 >= want.1 {
                return anyhow::Ok(None);
            }
            if !force {
                if let Some((message, at)) = store.meta(&failed_key)? {
                    if !message.is_empty() && now - at < AFTER_FAILURE {
                        return Ok(Some(message));
                    }
                }
            }
            let meta = store.meta(&key)?;
            let have = meta.as_ref().and_then(|(v, _)| parse_span(v));
            let age = match meta {
                Some((_, at)) if !force => now - at,
                _ => i64::MAX,
            };
            let (reads, span) = plan_reads(have, want, age, pace);
            let started = std::time::Instant::now();
            let reading = async {
                for r in &reads {
                    read(*r).await?;
                }
                anyhow::Ok(())
            };
            // The span is only recorded after a whole read, and writes replace rows, so
            // a read cut off here is safe to repeat.
            let outcome = tokio::time::timeout(self.cfg.feed_deadline, reading)
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "no complete answer in {} s",
                        self.cfg.feed_deadline.as_secs()
                    ))
                });
            tracing::debug!(
                feed = name,
                reads = reads.len(),
                secs = started.elapsed().as_secs_f32(),
                "gauge feed read"
            );
            if let Err(e) = outcome {
                tracing::warn!(feed = name, error = %e, "gauge feed failed");
                let message = format!("{e:#}");
                store.set_meta(&failed_key, &message)?;
                return Ok(Some(message));
            }
            if !reads.is_empty() {
                store.set_meta(&key, &format!("{},{}", span.0, span.1))?;
                store.set_meta(&failed_key, "")?;
            }
            Ok(None)
        };
        FeedStatus {
            name,
            error: outcome.await.unwrap_or_else(|e| Some(format!("{e:#}"))),
        }
    }

    async fn read_usgs(&self, span: (i64, i64)) -> Result<()> {
        let readings = self
            .feeds()
            .usgs_readings(self.region(), span.0, span.1)
            .await?;
        let hours = usgs_hours(readings);
        let known: HashSet<String> = {
            let conn = self.store().pool().get()?;
            let mut stmt = conn.prepare("SELECT id FROM feed_stations WHERE id LIKE 'USGS-%'")?;
            let ids = stmt.query_map([], |r| r.get(0))?;
            ids.collect::<rusqlite::Result<_>>()?
        };
        let mut unknown: Vec<String> = hours
            .iter()
            .map(|h| h.station.clone())
            .filter(|id| !known.contains(id))
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        if !unknown.is_empty() {
            let stations = self.feeds().usgs_stations(&unknown).await?;
            put_stations(self.store(), &stations)?;
        }
        put_hours(self.store(), &hours)
    }

    async fn read_asos(&self, states: &[String], span: (i64, i64)) -> Result<()> {
        if states.is_empty() {
            return Ok(());
        }
        for state in states {
            let key = format!("feed:ASOS:stations:{state}");
            let stale = self
                .store()
                .meta(&key)?
                .is_none_or(|(_, at)| now_secs() - at > STATION_LIST_EVERY);
            if stale {
                let stations = self.feeds().asos_stations(state).await?;
                put_stations(self.store(), &stations)?;
                self.store().set_meta(&key, "")?;
            }
        }
        let (from, to) = utc_dates(span)?;
        let hours = self.feeds().asos_hours(states, from, to).await?;
        put_hours(self.store(), &hours)
    }

    async fn read_cocorahs(&self, states: &[String], span: (i64, i64)) -> Result<()> {
        // A report is dated by its observer's clock, so pad a day past the UTC span.
        let (from, to) = utc_dates(span)?;
        let (from, to) = (from.yesterday()?, to.tomorrow()?);
        for state in states {
            let day = self.feeds().cocorahs_reports(state, from, to).await?;
            put_stations(self.store(), &day.stations)?;
            put_reports(self.store(), &day.reports)?;
        }
        Ok(())
    }
}

fn utc_dates(span: (i64, i64)) -> Result<(civil::Date, civil::Date)> {
    let date = |t: i64| -> Result<civil::Date> {
        Ok(Timestamp::from_second(t)?.to_zoned(TimeZone::UTC).date())
    };
    Ok((date(span.0)?, date(span.1)?))
}

#[cfg(test)]
#[path = "feeds_tests.rs"]
mod tests;

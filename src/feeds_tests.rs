use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::engine::Config;
use crate::ghcn::GaugeTotal;

const COCORAHS: &str = include_str!("../tests/fixtures/cocorahs_daily.csv");
const USGS_VALUES: &str = include_str!("../tests/fixtures/usgs_continuous.json");
const USGS_LOCATIONS: &str = include_str!("../tests/fixtures/usgs_locations.json");
const IEM_ASOS: &str = include_str!("../tests/fixtures/iem_asos.csv");
const IEM_NETWORK: &str = include_str!("../tests/fixtures/iem_network.geojson");

fn t(s: &str) -> i64 {
    s.parse::<Timestamp>().unwrap().as_second()
}

#[test]
fn a_cocorahs_number_maps_to_its_archive_id_only_where_it_has_one() {
    assert_eq!(cocorahs_id("CO-DN-12"), "US1CODN0012");
    assert_eq!(cocorahs_id("CO-DN-138"), "US1CODN0138");
    assert_eq!(cocorahs_id("MO-FSA-021"), "CoCoRaHS-MO-FSA-021");
    assert_eq!(cocorahs_id("samp001"), "CoCoRaHS-samp001");
    assert_eq!(cocorahs_id("CO-DN-12345"), "CoCoRaHS-CO-DN-12345");
}

#[test]
fn reads_the_cocorahs_export() {
    let day = parse_cocorahs(COCORAHS).unwrap();
    let ids: Vec<&str> = day.reports.iter().map(|r| r.station.as_str()).collect();
    // NA is no report, and 99 inches is no reading.
    assert_eq!(
        ids,
        [
            "US1CODN0012",
            "US1CODN0003",
            "CoCoRaHS-MO-FSA-021",
            "CoCoRaHS-samp001"
        ]
    );
    let first = &day.reports[0];
    assert_eq!(first.prcp, 64); // 0.25 in, in tenths of a mm
    assert_eq!(first.obs_time.as_deref(), Some("0700"));
    assert_eq!(first.s_flag.as_deref(), Some(LIVE_SOURCE));
    let trace = &day.reports[1];
    assert_eq!((trace.prcp, trace.m_flag.as_deref()), (0, Some("T")));
    assert_eq!(trace.obs_time.as_deref(), Some("1230"));
    assert_eq!(day.stations[1].name, "Sampleton, East 2.0 E");
}

#[test]
fn changed_columns_fail_instead_of_reading_as_no_rain() {
    assert!(parse_cocorahs("Date,Rain\n2026-01-15,0.25\n").is_err());
    assert!(parse_iem_hours("Station,Time\nSMP,2026-01-15 13:53\n").is_err());
    assert!(parse_cocorahs(COCORAHS.lines().next().unwrap()).is_ok());
}

#[test]
fn a_changed_date_format_fails_instead_of_reading_as_no_rain() {
    let bad_date = COCORAHS.replacen("2026-01-15", "01/15/2026", usize::MAX);
    assert_ne!(bad_date, COCORAHS);
    assert!(parse_cocorahs(&bad_date).is_err());
    let bad_valid = IEM_ASOS.replace("2026-01-15 ", "15 Jan 2026 ");
    assert_ne!(bad_valid, IEM_ASOS);
    assert!(parse_iem_hours(&bad_valid).is_err());
}

#[test]
fn a_changed_usgs_response_fails_and_one_bad_row_does_not() {
    assert!(parse_usgs_page(r#"{"items":[]}"#).is_err());
    assert!(parse_usgs_stations(r#"{"items":[]}"#).is_err());
    assert!(parse_usgs_page(r#"{"features":[]}"#)
        .unwrap()
        .readings
        .is_empty());
    let row = |time: &str| {
        format!(
            r#"{{"properties":{{"monitoring_location_id":"USGS-09999001","time_series_id":"a","time":"{time}","value":"0.10"}}}}"#
        )
    };
    let page = |rows: &[String]| format!(r#"{{"features":[{}]}}"#, rows.join(","));
    let one_bad = page(&[row("2026-01-15T14:15:00Z"), row("soon")]);
    assert_eq!(parse_usgs_page(&one_bad).unwrap().readings.len(), 1);
    let all_bad = page(&[row("soon"), row("later")]);
    assert!(parse_usgs_page(&all_bad).is_err());
}

#[test]
fn reads_a_usgs_page_and_skips_outages() {
    let page = parse_usgs_page(USGS_VALUES).unwrap();
    assert_eq!(page.readings.len(), 1);
    assert_eq!(page.readings[0].site, "USGS-09999001");
    assert_eq!(page.readings[0].at, t("2026-01-15T14:15:00Z"));
    assert!(page.next.unwrap().contains("cursor=abc"));
}

#[test]
fn reads_usgs_locations_with_a_position() {
    let stations = parse_usgs_stations(USGS_LOCATIONS).unwrap();
    assert_eq!(stations.len(), 1);
    assert_eq!(stations[0].name, "SAMPLE CREEK NEAR SAMPLETON");
    assert_eq!((stations[0].lat, stations[0].lon), (39.7, -105.0));
}

fn reading(site: &str, series: &str, at: &str, inches: f64) -> UsgsReading {
    UsgsReading {
        site: site.into(),
        series: series.into(),
        at: t(at),
        inches,
    }
}

fn quarter_hours(site: &str, series: &str, hour: u32, inches: f64) -> Vec<UsgsReading> {
    let at = |h: u32, m: u32| format!("2026-01-15T{h:02}:{m:02}:00Z");
    vec![
        reading(site, series, &at(hour, 15), inches),
        reading(site, series, &at(hour, 30), inches),
        reading(site, series, &at(hour, 45), inches),
        reading(site, series, &at(hour + 1, 0), inches),
    ]
}

#[test]
fn a_usgs_hour_ends_on_its_last_reading_and_needs_all_of_them() {
    let mut readings = quarter_hours("S", "a", 10, 0.01);
    readings.extend(quarter_hours("S", "a", 11, 0.01));
    readings.remove(5); // 11:30
    let hours = usgs_hours(readings);
    assert_eq!(hours.len(), 1);
    assert_eq!(hours[0].end, t("2026-01-15T11:00:00Z"));
    assert!((hours[0].mm - 1.016).abs() < 1e-9);
}

#[test]
fn a_five_minute_site_needs_twelve_readings() {
    let at = |m: u32| format!("2026-01-15T10:{m:02}:00Z");
    let mut readings: Vec<_> = (1..=11)
        .map(|i| reading("S", "a", &at(i * 5), 0.01))
        .collect();
    assert!(usgs_hours(readings.iter().map(clone_reading).collect()).is_empty());
    readings.push(reading("S", "a", "2026-01-15T11:00:00Z", 0.01));
    assert_eq!(usgs_hours(readings).len(), 1);
}

fn clone_reading(r: &UsgsReading) -> UsgsReading {
    UsgsReading {
        site: r.site.clone(),
        series: r.series.clone(),
        at: r.at,
        inches: r.inches,
    }
}

#[test]
fn a_site_with_two_instruments_counts_once() {
    let mut readings = quarter_hours("S", "bbb", 10, 0.05);
    readings.extend(quarter_hours("S", "aaa", 10, 0.01));
    let hours = usgs_hours(readings);
    assert_eq!(hours.len(), 1);
    assert!((hours[0].mm - 1.016).abs() < 1e-9);
}

#[test]
fn an_impossible_hour_is_not_counted() {
    let mut readings = quarter_hours("S", "a", 10, 1087.0);
    readings.extend(quarter_hours("S", "a", 11, 0.01));
    let hours = usgs_hours(readings);
    assert_eq!(hours.len(), 1);
    assert_eq!(hours[0].end, t("2026-01-15T12:00:00Z"));
}

#[test]
fn reads_asos_reports_as_hours() {
    let hours = parse_iem_hours(IEM_ASOS).unwrap();
    let summary: Vec<(i64, f64, bool)> = hours
        .iter()
        .map(|h| (h.end, (h.mm * 100.0).round() / 100.0, h.trace))
        .collect();
    // M is no reading, 16:21 is far from any hour, and 17:58 overwrites 17:53.
    assert_eq!(
        summary,
        [
            (t("2026-01-15T14:00:00Z"), 3.05, false),
            (t("2026-01-15T15:00:00Z"), 0.0, true),
            (t("2026-01-15T18:00:00Z"), 0.51, false),
        ]
    );
    assert_eq!(hours[0].station, "ASOS-SMP");
    assert!(hours[0].measured_rain);
    assert!(!hours[1].measured_rain);
}

#[test]
fn an_asos_zero_counts_only_if_the_report_could_have_measured_rain() {
    let measured = |metar| asos_hour("0.00", metar).map(|(mm, _, _)| mm);
    // Dry, no group: a dry hour.
    assert_eq!(
        measured("KSMP 151353Z 07005KT 10SM FEW015 23/22 A3005 RMK AO2"),
        Some(0.0)
    );
    // Raining, no group: unmeasured, however the report spells it.
    for raining in [
        "KSMP 151353Z 16005KT 4SM -RA BR OVC250 23/22 A3006 RMK AO2",
        "KSMP 151353Z 16005KT 4SM +TSRA OVC250 23/22 A3006",
        "KSMP 151353Z 16005KT 10SM OVC250 23/22 A3006 RMK RAB18",
        "KSMP 151353Z 16005KT 10SM OVC250 23/22 A3006 RMK AO2 RAE24",
    ] {
        assert_eq!(measured(raining), None, "{raining}");
    }
    // A sensor marked out.
    assert_eq!(
        measured("KSMP 151353Z 07005KT 10SM OVC250 23/22 A3005 RMK AO2 T02390228 PNO $"),
        None
    );
    // Letters inside other groups are not weather, and vicinity rain is not here.
    assert_eq!(
        measured("KRAL 151353Z 07005KT 10SM VCSH BKN020 23/22 A3005 RMK AO2 SLPNO"),
        Some(0.0)
    );
    // Rain with its amount group is a reading.
    let (mm, trace, measured_rain) = asos_hour(
        "0.12",
        "KSMP 151353Z 16005KT 4SM -RA OVC250 23/22 A3006 RMK AO2 P0012",
    )
    .unwrap();
    assert!((mm - 3.048).abs() < 1e-9);
    assert!(!trace && measured_rain);
    // A trace shows a live report but no measured rain.
    let (_, trace, measured_rain) = asos_hour(
        "T",
        "KSMP 151353Z 16005KT 4SM -RA OVC250 23/22 A3006 RMK AO2 P0000",
    )
    .unwrap();
    assert!(trace && !measured_rain);
}

#[test]
fn a_gauge_that_never_gave_an_amount_is_left_out_of_the_comparison() {
    let store = Store::open_in_memory().unwrap();
    let now = now_secs().div_euclid(HOUR) * HOUR;
    let row = |station: &str, end, mm, measured_rain| HourReading {
        station: station.into(),
        end,
        mm,
        trace: false,
        measured_rain,
    };
    put_hours(
        &store,
        &[
            row("ASOS-WET", now - 2 * HOUR, 3.0, true),
            row("ASOS-WET", now - HOUR, 0.0, false),
            row("ASOS-MUTE", now - HOUR, 0.0, false),
        ],
    )
    .unwrap();
    let got = hours_between(&store, now - 3 * HOUR, now).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got["ASOS-WET"].len(), 2);
}

#[test]
fn reads_the_asos_station_list() {
    let stations = parse_iem_network(IEM_NETWORK).unwrap();
    assert_eq!(stations[0].id, "ASOS-SMP");
    assert_eq!(stations[0].name, "Sampleton Airport");
    assert_eq!((stations[0].lat, stations[0].lon), (39.7, -105.0));
}

const PACE: Pace = Pace {
    reach: 1000,
    settle: 30,
    every: 15,
};

#[test]
fn plans_what_to_read() {
    // First read.
    assert_eq!(
        plan_reads(None, (100, 200), i64::MAX, &PACE),
        (vec![(100, 200)], (100, 200))
    );
    // Asked again too soon: nothing, and what was read stays remembered.
    assert_eq!(
        plan_reads(Some((100, 200)), (100, 200), 5, &PACE),
        (vec![], (100, 200))
    );
    // Later: reread the unsettled end.
    assert_eq!(
        plan_reads(Some((100, 200)), (100, 260), 20, &PACE),
        (vec![(170, 260)], (100, 260))
    );
    // Older hours are read at once, however recently the newest were.
    assert_eq!(
        plan_reads(Some((100, 200)), (40, 200), 5, &PACE),
        (vec![(40, 100)], (40, 200))
    );
    // A window wholly older than what was read needs no recent read.
    assert_eq!(
        plan_reads(Some((100, 200)), (110, 150), 20, &PACE),
        (vec![], (100, 200))
    );
    // A gap is not assumed read.
    assert_eq!(
        plan_reads(Some((100, 200)), (300, 400), 20, &PACE),
        (vec![(300, 400)], (300, 400))
    );
    // Hours never read wait for nothing, whether they touch what was read or overlap it,
    // and the hours already read still wait.
    assert_eq!(
        plan_reads(Some((100, 200)), (200, 260), 5, &PACE),
        (vec![(200, 260)], (100, 260))
    );
    assert_eq!(
        plan_reads(Some((100, 200)), (150, 260), 5, &PACE),
        (vec![(200, 260)], (100, 260))
    );
}

fn hour(end: i64) -> HourReading {
    HourReading {
        station: "S".into(),
        end,
        mm: 1.0,
        trace: false,
        measured_rain: true,
    }
}

#[test]
fn compares_the_longest_stretch_and_the_newest_of_equals() {
    let hours: Vec<_> = [1, 2, 4, 5, 6, 8].map(|h| hour(h * HOUR)).to_vec();
    let ends: Vec<i64> = longest_stretch(&hours).iter().map(|h| h.end).collect();
    assert_eq!(ends, [4 * HOUR, 5 * HOUR, 6 * HOUR]);
    let hours: Vec<_> = [1, 2, 4, 5].map(|h| hour(h * HOUR)).to_vec();
    let ends: Vec<i64> = longest_stretch(&hours).iter().map(|h| h.end).collect();
    assert_eq!(ends, [4 * HOUR, 5 * HOUR]);
    assert!(longest_stretch(&[]).is_empty());
}

fn airport(id: &str, lat: f64, reports: u32, step_hours: u32) -> GaugeTotal {
    GaugeTotal {
        id: id.into(),
        name: String::new(),
        network: Network::of(id),
        network_label: Network::of(id).label(),
        lat,
        lon: -105.0,
        reports,
        possible: reports,
        step_hours,
        provisional: step_hours == 1,
        gauge_mm: 0.0,
        mrms_mm: None,
        trace_reports: 0,
        assumed_time_reports: 0,
        covers: [Timestamp::UNIX_EPOCH, Timestamp::UNIX_EPOCH],
    }
}

fn kept(rows: Vec<GaugeTotal>) -> Vec<String> {
    one_row_per_gauge(rows).into_iter().map(|g| g.id).collect()
}

#[test]
fn an_airport_in_both_sources_keeps_the_row_covering_more_hours() {
    let archive = |days| airport("USW00099999", 39.70, days, 24);
    let feed = |hours| airport("ASOS-SMP", 39.71, hours, 1);
    assert_eq!(kept(vec![archive(1), feed(48)]), ["ASOS-SMP"]);
    assert_eq!(kept(vec![archive(3), feed(48)]), ["USW00099999"]);
    // The archive's row has been checked, so it takes a tie.
    assert_eq!(kept(vec![archive(2), feed(48)]), ["USW00099999"]);
}

#[test]
fn airports_apart_and_other_networks_are_left_alone() {
    let far = vec![
        airport("USW00099999", 39.70, 1, 24),
        airport("ASOS-SMP", 39.80, 48, 1),
    ];
    assert_eq!(kept(far).len(), 2);
    let usgs = vec![
        airport("USW00099999", 39.70, 1, 24),
        airport("USGS-09999001", 39.70, 48, 1),
    ];
    assert_eq!(kept(usgs).len(), 2);
}

#[test]
fn the_archive_answers_for_a_date_it_holds() {
    let store = Store::open_in_memory().unwrap();
    let date = |s: &str| s.parse::<civil::Date>().unwrap();
    let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
    let yesterday = today.yesterday().unwrap();
    let report = |d: civil::Date, prcp| Report {
        station: "US1COXX0001".into(),
        date: d,
        prcp,
        obs_time: Some("0700".into()),
        m_flag: None,
        s_flag: Some(LIVE_SOURCE.into()),
    };
    put_reports(&store, &[report(yesterday, 11), report(today, 22)]).unwrap();
    store
        .pool()
        .get()
        .unwrap()
        .execute(
            "INSERT INTO gauge_obs (station, date, prcp) VALUES ('US1COXX0001', ?1, 99)",
            [yesterday.strftime("%Y%m%d").to_string()],
        )
        .unwrap();
    let got = reports_between(&store, date("2000-01-01"), today).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].date, got[0].prcp), (today, 22));
}

/// A plain HTTP server over canned bodies, counting the requests each path receives.
struct Stub {
    url: String,
    requests: Arc<AtomicUsize>,
}

fn stub(route: impl Fn(&str) -> Option<String> + Send + 'static) -> Stub {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut buf = [0u8; 8192];
            let n = conn.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = match route(&path) {
                Some(body) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = conn.write_all(reply.as_bytes());
        }
    });
    Stub { url, requests }
}

fn test_region() -> Region {
    Region {
        west: -106.0,
        south: 39.0,
        east: -104.0,
        north: 40.0,
    }
}

fn weather_against(stub: &Stub) -> Weather {
    weather_with(stub, DEFAULT_FEED_STALL, DEFAULT_FEED_DEADLINE)
}

fn weather_with(stub: &Stub, feed_stall: Duration, feed_deadline: Duration) -> Weather {
    let cfg = Config {
        tz: TimeZone::UTC,
        bucket_url: stub.url.clone(),
        ghcn_url: stub.url.clone(),
        usgs_url: stub.url.clone(),
        cocorahs_url: stub.url.clone(),
        iem_url: stub.url.clone(),
        feed_stall,
        feed_deadline,
        ..Config::new(test_region())
    };
    let wx = Weather::new(cfg, Store::open_in_memory().unwrap()).unwrap();
    wx.store()
        .set_meta(&crate::ghcn::states_key(&test_region()), "CO")
        .unwrap();
    wx
}

/// USGS and ASOS bodies for the hour before `end`, and a CoCoRaHS report on its date.
fn feed_route(end: Timestamp) -> impl Fn(&str) -> Option<String> {
    let usgs: Vec<String> = (0..4)
        .map(|i| {
            let at = Timestamp::from_second(end.as_second() - i * 15 * 60).unwrap();
            format!(
                r#"{{"properties":{{"monitoring_location_id":"USGS-09999001","time_series_id":"a","time":"{}","value":"0.10"}}}}"#,
                at.strftime("%Y-%m-%dT%H:%M:%SZ")
            )
        })
        .collect();
    let usgs = format!(r#"{{"features":[{}],"links":[]}}"#, usgs.join(","));
    let asos_time = Timestamp::from_second(end.as_second() - 7 * 60).unwrap();
    let asos = format!(
        "station,valid,lon,lat,p01i,metar\nSMP,{},-105.0,39.7,0.20,KSMP 151353Z -RA RMK AO2 P0020\n",
        asos_time.strftime("%Y-%m-%d %H:%M")
    );
    let report_date = end.to_zoned(TimeZone::UTC).date();
    let report_end = report_date.strftime("%Y-%m-%d");
    let cocorahs = format!(
        "ObservationDate,ObservationTime,StationNumber,StationName,Latitude,Longitude,TotalPrecipAmt\n\
         {report_end},12:00 AM,CO-DN-12,Sampleton 1.1 N,39.7,-105.0,0.25\n"
    );
    move |path: &str| {
        if path.contains("/collections/continuous/") {
            Some(usgs.clone())
        } else if path.contains("/collections/monitoring-locations/") {
            Some(USGS_LOCATIONS.to_string())
        } else if path.contains("/geojson/network/CO_ASOS") {
            Some(IEM_NETWORK.to_string())
        } else if path.contains("/cgi-bin/request/asos.py") {
            Some(asos.clone())
        } else if path.contains("/export/exportreports.aspx") {
            Some(cocorahs.clone())
        } else {
            None
        }
    }
}

fn recent_end() -> Timestamp {
    Timestamp::from_second(now_secs().div_euclid(HOUR) * HOUR).unwrap()
}

#[tokio::test]
async fn a_repeated_question_asks_nobody_and_the_gauges_come_back_provisional() {
    let end = recent_end();
    let server = stub(feed_route(end));
    let wx = weather_against(&server);
    let start = Timestamp::from_second(end.as_second() - 26 * HOUR).unwrap();
    let window = Window::new(start, end);

    let statuses = wx.refresh_feeds(window, false).await;
    assert!(statuses.iter().all(|s| s.error.is_none()), "{statuses:?}");
    let asked = server.requests.load(Ordering::SeqCst);
    assert!(asked >= 5, "{asked} requests");

    wx.refresh_feeds(window, false).await;
    assert_eq!(server.requests.load(Ordering::SeqCst), asked);

    let report = wx.gauges(window).await.unwrap();
    let row = |id: &str| report.gauges.iter().find(|g| g.id == id).unwrap().clone();
    let usgs = row("USGS-09999001");
    assert_eq!((usgs.step_hours, usgs.reports), (1, 1));
    assert!(usgs.provisional);
    assert!((usgs.gauge_mm - 10.2).abs() < 0.01, "{}", usgs.gauge_mm);
    assert_eq!(usgs.network, Network::Usgs);
    let asos = row("ASOS-SMP");
    assert_eq!((asos.step_hours, asos.network), (1, Network::Asos));
    assert!((asos.gauge_mm - 5.1).abs() < 0.01, "{}", asos.gauge_mm);
    assert!(report.feeds.iter().all(|f| f.error.is_none()));
}

fn silent_stub() -> Stub {
    stub(|_| {
        std::thread::sleep(Duration::from_secs(5));
        None
    })
}

async fn errors_within_two_seconds(wx: &Weather) -> Vec<String> {
    let end = recent_end();
    let start = Timestamp::from_second(end.as_second() - 26 * HOUR).unwrap();
    let began = std::time::Instant::now();
    let statuses = wx.refresh_feeds(Window::new(start, end), false).await;
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    statuses
        .into_iter()
        .map(|s| s.error.unwrap_or_default())
        .collect()
}

#[tokio::test]
async fn a_silent_feed_fails_once_and_is_named() {
    let server = silent_stub();
    let wx = weather_with(&server, Duration::from_millis(100), DEFAULT_FEED_DEADLINE);
    // Two more tries would take at least the 8 s between them.
    for error in errors_within_two_seconds(&wx).await {
        assert!(error.contains("GET "), "{error}");
    }
}

#[tokio::test]
async fn the_deadline_bounds_a_feed_that_never_trips_the_stall_limit() {
    let server = silent_stub();
    let wx = weather_with(&server, DEFAULT_FEED_STALL, Duration::from_millis(200));
    for error in errors_within_two_seconds(&wx).await {
        assert!(error.contains("no complete answer"), "{error}");
    }
}

#[tokio::test]
async fn a_gauge_outside_the_region_costs_no_mrms_sum() {
    let end = recent_end();
    let start = Timestamp::from_second(end.as_second() - 26 * HOUR).unwrap();
    let window = Window::new(start, end);
    let requests_for_gauges = |foreign: bool| {
        let route = feed_route(end);
        async move {
            let server = stub(route);
            let wx = weather_against(&server);
            wx.refresh_feeds(window, false).await;
            if foreign {
                let far = Station {
                    id: "USGS-09999999".into(),
                    name: "Far away".into(),
                    lat: 25.0,
                    lon: -81.0,
                    elev_m: None,
                };
                put_stations(wx.store(), &[far]).unwrap();
                let hour_end = end.as_second() - 3 * HOUR;
                let far_hour = HourReading {
                    station: "USGS-09999999".into(),
                    end: hour_end,
                    mm: 1.0,
                    trace: false,
                    measured_rain: true,
                };
                put_hours(wx.store(), &[far_hour]).unwrap();
            }
            let asked = server.requests.load(Ordering::SeqCst);
            let report = wx.gauges(window).await.unwrap();
            assert!(report.gauges.iter().all(|g| g.id != "USGS-09999999"));
            server.requests.load(Ordering::SeqCst) - asked
        }
    };
    assert_eq!(
        requests_for_gauges(true).await,
        requests_for_gauges(false).await
    );
}

#[tokio::test]
async fn usgs_pages_are_followed_on_the_configured_host() {
    let end = recent_end();
    let working = feed_route(end);
    // The next link names a host and an API version this client was not given.
    let first = r#"{"features":[],"links":[{"rel":"next","href":"https://elsewhere.test/ogcapi/v9/collections/continuous/items?cursor=abc&limit=2"}]}"#;
    let server = stub(move |path| {
        if path.contains("/collections/continuous/") && !path.contains("cursor=abc") {
            Some(first.to_string())
        } else {
            working(path)
        }
    });
    let wx = weather_against(&server);
    let start = Timestamp::from_second(end.as_second() - 26 * HOUR).unwrap();
    let window = Window::new(start, end);
    let statuses = wx.refresh_feeds(window, false).await;
    assert!(statuses.iter().all(|s| s.error.is_none()), "{statuses:?}");
    let got = hours_between(wx.store(), start.as_second(), end.as_second()).unwrap();
    assert_eq!(got["USGS-09999001"].len(), 1);
}

#[tokio::test]
async fn a_failing_feed_is_named_and_the_others_still_answer() {
    let end = recent_end();
    let working = feed_route(end);
    let server = stub(move |path| {
        if path.contains("/collections/continuous/") {
            None
        } else {
            working(path)
        }
    });
    let wx = weather_against(&server);
    let start = Timestamp::from_second(end.as_second() - 26 * HOUR).unwrap();
    let statuses = wx.refresh_feeds(Window::new(start, end), false).await;
    let failed: Vec<&str> = statuses
        .iter()
        .filter(|s| s.error.is_some())
        .map(|s| s.name)
        .collect();
    assert_eq!(failed, ["USGS"]);

    // It is not asked again straight away, and says why.
    let asked = server.requests.load(Ordering::SeqCst);
    let statuses = wx.refresh_feeds(Window::new(start, end), false).await;
    assert_eq!(server.requests.load(Ordering::SeqCst), asked);
    let usgs = statuses.iter().find(|s| s.name == "USGS").unwrap();
    assert!(usgs.error.as_deref().unwrap().contains("404"));
}

#[tokio::test]
async fn feeds_read_by_state_wait_for_the_station_list() {
    let server = stub(|_| None);
    let cfg = Config {
        usgs_url: server.url.clone(),
        cocorahs_url: server.url.clone(),
        iem_url: server.url.clone(),
        ..Config::new(test_region())
    };
    let wx = Weather::new(cfg, Store::open_in_memory().unwrap()).unwrap();
    let end = recent_end();
    let start = Timestamp::from_second(end.as_second() - 26 * HOUR).unwrap();
    let statuses = wx.refresh_feeds(Window::new(start, end), false).await;
    let by_state = statuses.iter().find(|s| s.name == "ASOS").unwrap();
    assert!(by_state.error.as_deref().unwrap().contains("station list"));
}

#[test]
fn a_request_box_covers_whole_degrees() {
    let tight = Region {
        west: -105.04,
        south: 39.71,
        east: -104.96,
        north: 39.79,
    };
    assert_eq!(whole_degrees(&tight), [-106, 39, -104, 40]);
}

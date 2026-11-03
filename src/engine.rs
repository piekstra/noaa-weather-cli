//! Rainfall over any window: plan which MRMS files cover it, load each from
//! cache or the bucket, and sum.
//!
//! ## Planning
//!
//! A window is a run of hours. Every hour old enough to have a final (Pass 2)
//! analysis is covered by Pass 2 products; the newest hour or two, which only
//! have Pass 1 or radar-only so far, are covered hour by hour.
//!
//! Pass 2 hours are covered by **aligned blocks**: a 72 h product whose end is
//! a multiple of 72 h since the epoch, a 24 h product ending at 00Z, and so on
//! down to single hours at the window's ragged edges. Alignment is what makes
//! blocks reusable — "past 7 days" asked an hour later lands on the same 72 h
//! and 24 h blocks and only fetches the new edge hours. Because the multi-hour
//! products are the exact sum of the hourly ones (verified; see `mrms`), the
//! choice changes how many bytes are fetched, never the answer.
//!
//! A cold "past 30 days" measured 17 files in 2.8 s — mostly 72 h blocks —
//! instead of 720 hourly ones (~600 MB).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use jiff::{tz::TimeZone, Timestamp};
use serde::Serialize;

use crate::feeds::{FeedClient, DEFAULT_COCORAHS_URL, DEFAULT_IEM_URL, DEFAULT_USGS_URL};
use crate::ghcn::{GhcnClient, DEFAULT_GHCN_URL};
use crate::grid::{Grid, Region};
use crate::mrms::{MrmsClient, Pass, Product, DEFAULT_BUCKET_URL, PASS2_BLOCKS};
use crate::store::Store;
use crate::window::{floor_quarter, Resolved, Window, WindowSpec, HOUR_SECS, QUARTER_SECS};

/// Longest window served by default: a quarter year.
pub const DEFAULT_MAX_HOURS: i64 = 24 * 92;

/// Days a downloaded grid stays cached: the longest window a request may ask for.
pub const DEFAULT_RETAIN_DAYS: i64 = 92;

/// Longest window for an hour-by-hour series (one bar per hour).
pub const MAX_SERIES_HOURS: i64 = 24 * 14;

/// Longest window served from 15-minute grids: ~190 files at this limit.
pub const MAX_QUARTER_HOURS: i64 = 48;

/// Most 2-minute rate frames a request may ask for: an hour and a half.
pub const MAX_RATE_FRAMES: usize = 45;

#[derive(Clone)]
pub struct Config {
    pub region: Region,
    pub tz: TimeZone,
    pub bucket_url: String,
    /// GHCN-Daily bucket, for gauge reports.
    pub ghcn_url: String,
    /// USGS Water Data API, for same-day gauges.
    pub usgs_url: String,
    /// CoCoRaHS data site, for same-day gauges.
    pub cocorahs_url: String,
    /// Iowa Environmental Mesonet, for same-day gauges.
    pub iem_url: String,
    /// Simultaneous downloads.
    pub concurrency: usize,
    pub max_hours: i64,
    /// Days to keep a downloaded grid. `None` keeps everything, and so does a value
    /// of 0 or less. [`UserConfig::grid_retention`](crate::places::UserConfig::grid_retention)
    /// reads the setting.
    pub retain_days: Option<i64>,
}

impl Config {
    /// Settings for one area, in the system's time zone.
    pub fn new(region: Region) -> Self {
        Self {
            region,
            tz: TimeZone::system(),
            bucket_url: DEFAULT_BUCKET_URL.into(),
            ghcn_url: DEFAULT_GHCN_URL.into(),
            usgs_url: DEFAULT_USGS_URL.into(),
            cocorahs_url: DEFAULT_COCORAHS_URL.into(),
            iem_url: DEFAULT_IEM_URL.into(),
            concurrency: 6,
            max_hours: DEFAULT_MAX_HOURS,
            retain_days: Some(DEFAULT_RETAIN_DAYS),
        }
    }
}

/// What the bucket has published right now.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Availability {
    /// End of the newest hour with any product.
    pub latest: Timestamp,
    /// Which pass that newest hour comes from.
    pub latest_pass: Pass,
    /// End of the newest hour with a final (Pass 2) analysis.
    pub pass2_through: Timestamp,
    pub checked_at: Timestamp,
}

/// How many hours of a total came from each pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HourCounts {
    pub pass2: u32,
    pub pass1: u32,
    pub radar: u32,
    /// Hours no product covers; the total excludes them.
    pub missing: u32,
    /// 15-minute grids in a sub-hourly total. Always radar only.
    pub quarters: u32,
    /// 15-minute grids a sub-hourly total should have had but could not find.
    pub missing_quarters: u32,
}

impl HourCounts {
    fn add(&mut self, pass: Pass, hours: u32) {
        match pass {
            Pass::Pass2 => self.pass2 += hours,
            Pass::Pass1 => self.pass1 += hours,
            Pass::Radar => self.radar += hours,
        }
    }

    pub fn provisional(&self) -> u32 {
        self.pass1 + self.radar
    }
}

/// Files read to answer a request.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct LoadStats {
    pub from_cache: u32,
    pub downloaded: u32,
}

/// Hours of `window` newer than the final (Pass 2) analysis — the most that
/// may legitimately come from a provisional pass.
pub fn expected_provisional(window: Window, a: &Availability) -> u32 {
    let from = window.start.max(a.pass2_through).as_second();
    ((window.end.as_second() - from).max(0) / HOUR_SECS) as u32
}

/// A rainfall total over a window.
#[derive(Debug, Clone)]
pub struct Accumulation {
    pub window: Window,
    pub grid: Grid,
    pub hours: HourCounts,
    pub missing_hours: Vec<Timestamp>,
    pub loads: LoadStats,
}

/// One hour of a point series.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SeriesPoint {
    /// End of the hour.
    pub valid: Timestamp,
    pub mm: Option<f64>,
    pub pass: Option<Pass>,
}

/// One 2-minute rain-rate field.
#[derive(Debug, Clone)]
pub struct RateFrame {
    pub valid: Timestamp,
    /// Rate in tenths of mm/h, on the same lattice as a rainfall grid.
    pub grid: Grid,
}

/// One unit of work in a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Item {
    product: Product,
    /// Unix hour (seconds / 3600) at the end of the accumulation.
    end_hour: i64,
}

/// Progress callback: (files done, files planned).
pub type Progress = Arc<dyn Fn(usize, usize) + Send + Sync>;

pub struct Weather {
    pub cfg: Config,
    store: Store,
    client: MrmsClient,
    ghcn: GhcnClient,
    feeds: FeedClient,
    availability: Mutex<Option<(Instant, Availability)>>,
    latest_quarter: Mutex<Option<(Instant, Timestamp)>>,
}

/// How long a cached availability probe is trusted.
const AVAILABILITY_TTL: Duration = Duration::from_secs(60);
/// An older answer still beats an error when the bucket can't be reached: MRMS
/// publishes hourly, so this is at worst an hour or so behind.
const AVAILABILITY_STALE_OK: Duration = Duration::from_secs(60 * 60);

fn ts_hour(h: i64) -> Timestamp {
    Timestamp::from_second(h * HOUR_SECS).expect("timestamp in range")
}

fn unix_hour(t: Timestamp) -> i64 {
    t.as_second().div_euclid(HOUR_SECS)
}

impl Weather {
    pub fn new(cfg: Config, store: Store) -> Result<Self> {
        let client = MrmsClient::new(&cfg.bucket_url, cfg.concurrency)?;
        let ghcn = GhcnClient::new(&cfg.ghcn_url)?;
        let feeds = FeedClient::new(&cfg.usgs_url, &cfg.cocorahs_url, &cfg.iem_url)?;
        Ok(Self {
            cfg,
            store,
            client,
            ghcn,
            feeds,
            availability: Mutex::new(None),
            latest_quarter: Mutex::new(None),
        })
    }

    pub(crate) fn ghcn(&self) -> &GhcnClient {
        &self.ghcn
    }

    pub(crate) fn feeds(&self) -> &FeedClient {
        &self.feeds
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn region(&self) -> &Region {
        &self.cfg.region
    }

    /// Find the newest published hour for each pass.
    ///
    /// Pass 2 lags ~1 h and Pass 1 ~15 min, so probing a handful of hours back
    /// from now with HEAD requests is enough, and much cheaper than listing a
    /// day's prefix (radar-only alone has ~700 objects a day).
    pub async fn availability(&self) -> Result<Availability> {
        let cached = *self.availability.lock().expect("availability lock");
        if let Some((at, a)) = cached {
            if at.elapsed() < AVAILABILITY_TTL {
                return Ok(a);
            }
        }
        match self.probe_availability().await {
            Err(e) => match cached {
                Some((at, a)) if at.elapsed() < AVAILABILITY_STALE_OK => {
                    tracing::warn!(error = %format!("{e:#}"), "using the last known MRMS availability");
                    Ok(a)
                }
                _ => Err(e),
            },
            ok => ok,
        }
    }

    async fn probe_availability(&self) -> Result<Availability> {
        let now = Timestamp::now();
        let now_h = unix_hour(now);
        let p2 = Product::hourly(Pass::Pass2);

        let mut pass2_through = None;
        for h in (now_h - 12..=now_h).rev() {
            if self.is_published(&p2, h).await? {
                pass2_through = Some(h);
                break;
            }
        }
        let pass2_through = pass2_through
            .context("no Pass 2 MRMS data in the last 12 hours — is the bucket reachable?")?;

        let mut latest = (pass2_through, Pass::Pass2);
        'probe: for h in (pass2_through + 1..=now_h).rev() {
            for pass in [Pass::Pass1, Pass::Radar] {
                if self.is_published(&Product::hourly(pass), h).await? {
                    latest = (h, pass);
                    break 'probe;
                }
            }
        }

        let a = Availability {
            latest: ts_hour(latest.0),
            latest_pass: latest.1,
            pass2_through: ts_hour(pass2_through),
            checked_at: now,
        };
        *self.availability.lock().expect("availability lock") = Some((Instant::now(), a));
        Ok(a)
    }

    async fn is_published(&self, p: &Product, h: i64) -> Result<bool> {
        self.is_published_at(p, ts_hour(h)).await
    }

    async fn is_published_at(&self, p: &Product, valid: Timestamp) -> Result<bool> {
        if self.store.has_grid(p, valid, &self.cfg.region)? {
            return Ok(true);
        }
        if self.store.recently_absent(p, valid, 120)? {
            return Ok(false);
        }
        let found = self.client.exists(p, valid).await?;
        if found {
            // An earlier probe may have marked it missing minutes ago. Left in
            // place, that marker would make the loader skip a file this probe
            // just declared published, and quietly fall back to Pass 1.
            self.store.clear_absent(p, valid)?;
        } else {
            self.store.mark_absent(p, valid)?;
        }
        Ok(found)
    }

    /// Resolve a spec against what is published now.
    pub async fn resolve(&self, spec: &WindowSpec) -> Result<(Resolved, Availability)> {
        let a = self.availability().await?;
        if let WindowSpec::LastMinutes { minutes } = *spec {
            let latest = self.latest_quarter().await?;
            let r = WindowSpec::resolve_minutes(minutes, latest, MAX_QUARTER_HOURS)?;
            return Ok((r, a));
        }
        let r = spec.resolve(Timestamp::now(), a.latest, &self.cfg.tz, self.cfg.max_hours)?;
        Ok((r, a))
    }

    /// End of the newest published 15-minute accumulation.
    pub async fn latest_quarter(&self) -> Result<Timestamp> {
        if let Some((at, t)) = *self.latest_quarter.lock().expect("quarter lock") {
            if at.elapsed() < AVAILABILITY_TTL {
                return Ok(t);
            }
        }
        let newest = floor_quarter(Timestamp::now()).as_second();
        for k in 0..=8 {
            let t = Timestamp::from_second(newest - k * QUARTER_SECS)?;
            if self.is_published_at(&Product::RADAR_15M, t).await? {
                *self.latest_quarter.lock().expect("quarter lock") = Some((Instant::now(), t));
                return Ok(t);
            }
        }
        anyhow::bail!("no 15-minute MRMS data in the last 2 hours — is the bucket reachable?")
    }

    /// A window ending at the newest hour, extended to the newest quarter hour
    /// when a 15-minute view can show more than an hourly one.
    pub async fn to_latest_quarter(&self, window: Window) -> Result<Window> {
        let a = self.availability().await?;
        if window.end < a.latest {
            return Ok(window);
        }
        Ok(Window::new(
            window.start,
            self.latest_quarter().await?.max(window.end),
        ))
    }

    /// Total rainfall over `window` from 15-minute grids: radar only, but exact
    /// to the quarter hour.
    pub async fn accumulate_quarters(
        &self,
        window: Window,
        progress: Option<Progress>,
    ) -> Result<Accumulation> {
        anyhow::ensure!(
            window.minutes() <= MAX_QUARTER_HOURS * 60,
            "a 15-minute total is limited to {MAX_QUARTER_HOURS} hours"
        );
        let ends: Vec<Timestamp> = window.quarter_ends().collect();
        let results = self.load_many(Product::RADAR_15M, ends, progress).await;

        let mut grid: Option<Grid> = None;
        let mut hours = HourCounts::default();
        let mut missing_hours = Vec::new();
        let mut loads = LoadStats::default();
        for r in results {
            let (valid, loaded) = r?;
            match loaded {
                Some((g, downloaded)) => {
                    hours.quarters += 1;
                    if downloaded {
                        loads.downloaded += 1;
                    } else {
                        loads.from_cache += 1;
                    }
                    match &mut grid {
                        Some(acc) => acc.add(&g)?,
                        None => grid = Some(g),
                    }
                }
                None => {
                    hours.missing_quarters += 1;
                    missing_hours.push(valid);
                }
            }
        }
        missing_hours.sort();
        let Some(grid) = grid else {
            anyhow::bail!(
                "no 15-minute MRMS data in this window ({} grids missing)",
                hours.missing_quarters
            );
        };
        Ok(Accumulation {
            window,
            grid,
            hours,
            missing_hours,
            loads,
        })
    }

    /// Rain at one point, one bar per 15 minutes.
    pub async fn quarter_series(
        &self,
        window: Window,
        lat: f64,
        lon: f64,
        progress: Option<Progress>,
    ) -> Result<Vec<SeriesPoint>> {
        anyhow::ensure!(
            window.minutes() <= MAX_QUARTER_HOURS * 60,
            "a 15-minute series is limited to {MAX_QUARTER_HOURS} hours"
        );
        anyhow::ensure!(
            self.cfg.region.contains(lat, lon),
            "({lat}, {lon}) is outside the weather region"
        );
        let ends: Vec<Timestamp> = window.quarter_ends().collect();
        let mut out = Vec::with_capacity(ends.len());
        for r in self.load_many(Product::RADAR_15M, ends, progress).await {
            let (valid, loaded) = r?;
            out.push(SeriesPoint {
                valid,
                mm: loaded.as_ref().and_then(|(g, _)| g.mm_at(lat, lon)),
                pass: loaded.map(|_| Pass::Radar),
            });
        }
        out.sort_by_key(|p| p.valid);
        Ok(out)
    }

    /// The newest `count` 2-minute rain-rate fields, oldest first.
    pub async fn rate_frames(
        &self,
        count: usize,
        progress: Option<Progress>,
    ) -> Result<Vec<RateFrame>> {
        anyhow::ensure!(
            (1..=MAX_RATE_FRAMES).contains(&count),
            "ask for 1 to {MAX_RATE_FRAMES} radar frames"
        );
        let times = self.rate_times(count).await?;
        let mut frames = Vec::with_capacity(times.len());
        for r in self.load_many(Product::PRECIP_RATE, times, progress).await {
            let (valid, loaded) = r?;
            if let Some((grid, _)) = loaded {
                frames.push(RateFrame { valid, grid });
            }
        }
        frames.sort_by_key(|f| f.valid);
        Ok(frames)
    }

    /// Valid times of the newest `count` published rate fields, from a listing.
    /// A quarter-hourly product can be probed; this one lands every 2 minutes
    /// on a schedule that drifts, so it has to be listed.
    pub async fn rate_times(&self, count: usize) -> Result<Vec<Timestamp>> {
        let now = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
        let today = now.strftime("%Y%m%d").to_string();
        let mut times = self.client.list_day(&Product::PRECIP_RATE, &today).await?;
        if times.len() < count {
            let yesterday = Timestamp::from_second(now.timestamp().as_second() - 86_400)?
                .to_zoned(jiff::tz::TimeZone::UTC)
                .strftime("%Y%m%d")
                .to_string();
            let mut older = self
                .client
                .list_day(&Product::PRECIP_RATE, &yesterday)
                .await?;
            older.append(&mut times);
            times = older;
        }
        let skip = times.len().saturating_sub(count);
        Ok(times.split_off(skip))
    }

    /// One rate field, from cache or the bucket.
    pub async fn rate_frame(&self, valid: Timestamp) -> Result<Option<RateFrame>> {
        Ok(self
            .load_at(&Product::PRECIP_RATE, valid)
            .await?
            .map(|(grid, _)| RateFrame { valid, grid }))
    }

    /// Load grids for many times concurrently, in completion order.
    async fn load_many(
        &self,
        product: Product,
        times: Vec<Timestamp>,
        progress: Option<Progress>,
    ) -> Vec<Result<(Timestamp, Option<(Grid, bool)>)>> {
        let total = times.len();
        let done = std::sync::atomic::AtomicUsize::new(0);
        let (done, progress, product) = (&done, &progress, &product);
        stream::iter(times)
            .map(move |t| async move {
                let r = self.load_at(product, t).await.map(|g| (t, g));
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Some(cb) = progress {
                    cb(n, total);
                }
                r
            })
            .buffer_unordered(self.cfg.concurrency.max(1))
            .collect()
            .await
    }

    /// Total rainfall over `window`.
    pub async fn accumulate(
        &self,
        window: Window,
        progress: Option<Progress>,
    ) -> Result<Accumulation> {
        if !window.is_hour_aligned() {
            return self.accumulate_quarters(window, progress).await;
        }
        let a = self.availability().await?;
        let items = self.plan(window, unix_hour(a.pass2_through))?;
        self.run(window, items, progress).await
    }

    /// Hour-by-hour rainfall at one point (the cell containing it).
    pub async fn series_at(
        &self,
        window: Window,
        lat: f64,
        lon: f64,
        progress: Option<Progress>,
    ) -> Result<Vec<SeriesPoint>> {
        anyhow::ensure!(
            window.hours() <= MAX_SERIES_HOURS,
            "an hourly series is limited to {} hours",
            MAX_SERIES_HOURS
        );
        anyhow::ensure!(
            self.cfg.region.contains(lat, lon),
            "({lat}, {lon}) is outside the weather region"
        );
        let a = self.availability().await?;
        let p2 = unix_hour(a.pass2_through);
        let items: Vec<Item> = window
            .hour_ends()
            .map(|t| Item {
                product: Product::hourly(if unix_hour(t) <= p2 {
                    Pass::Pass2
                } else {
                    Pass::Pass1
                }),
                end_hour: unix_hour(t),
            })
            .collect();
        let total = items.len();
        let done = std::sync::atomic::AtomicUsize::new(0);
        let (done, progress) = (&done, &progress);

        let mut out: Vec<SeriesPoint> = stream::iter(items)
            .map(move |it| async move {
                let r = self.load_hour(it).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Some(cb) = &progress {
                    cb(n, total);
                }
                r.map(|(loaded, _)| SeriesPoint {
                    valid: ts_hour(it.end_hour),
                    mm: loaded.as_ref().and_then(|(_, g)| g.mm_at(lat, lon)),
                    pass: loaded.map(|(p, _)| p),
                })
            })
            .buffer_unordered(self.cfg.concurrency.max(1))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<_>>()?;
        out.sort_by_key(|p| p.valid);
        Ok(out)
    }

    /// Make sure the newest `hours` hourly grids are cached, and drop
    /// provisional grids that have been superseded. Returns files downloaded.
    pub async fn warm(&self, hours: i64, progress: Option<Progress>) -> Result<u32> {
        let a = self.availability().await?;
        let end = a.latest;
        let start = Timestamp::from_second(end.as_second() - hours * HOUR_SECS)?;
        let w = Window::new(start, end);
        // A plan of hourly items only, so the series and short windows are warm.
        let p2 = unix_hour(a.pass2_through);
        let items: Vec<Item> = w
            .hour_ends()
            .map(|t| Item {
                product: Product::hourly(if unix_hour(t) <= p2 {
                    Pass::Pass2
                } else {
                    Pass::Pass1
                }),
                end_hour: unix_hour(t),
            })
            .collect();
        let acc = self.run(w, items, progress).await?;
        let cutoff = Timestamp::from_second(a.pass2_through.as_second() - 36 * HOUR_SECS)?;
        self.store.prune_provisional(cutoff)?;
        Ok(acc.loads.downloaded)
    }

    /// Cover `window` with products. Pure apart from cache lookups.
    fn plan(&self, window: Window, pass2_through_hour: i64) -> Result<Vec<Item>> {
        let s = unix_hour(window.start);
        let e = unix_hour(window.end);
        let region = &self.cfg.region;

        // What is already cached, per Pass 2 block size, over the window.
        let mut cached: HashMap<u32, HashSet<i64>> = HashMap::new();
        for b in PASS2_BLOCKS {
            let p = Product::block(b);
            let set = self
                .store
                .cached_valids(&p, window.start, window.end, region)?
                .into_iter()
                .map(|secs| secs.div_euclid(HOUR_SECS))
                .collect();
            cached.insert(b, set);
        }
        Ok(plan_items(s, e, pass2_through_hour, &cached))
    }

    async fn run(
        &self,
        window: Window,
        items: Vec<Item>,
        progress: Option<Progress>,
    ) -> Result<Accumulation> {
        let total = items.len();
        let done = std::sync::atomic::AtomicUsize::new(0);
        let (done, progress) = (&done, &progress);

        let results: Vec<Result<Vec<Loaded>>> = stream::iter(items)
            .map(move |it| async move {
                let r = self.load_item(it).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if let Some(cb) = &progress {
                    cb(n, total);
                }
                r
            })
            .buffer_unordered(self.cfg.concurrency.max(1))
            .collect()
            .await;

        let mut grid: Option<Grid> = None;
        let mut hours = HourCounts::default();
        let mut missing_hours = Vec::new();
        let mut loads = LoadStats::default();
        for r in results {
            for l in r? {
                match l.grid {
                    Some((pass, g)) => {
                        hours.add(pass, l.hours);
                        match &mut grid {
                            Some(acc) => acc.add(&g)?,
                            None => grid = Some(g),
                        }
                    }
                    None => {
                        hours.missing += l.hours;
                        missing_hours.push(ts_hour(l.end_hour));
                    }
                }
                if l.downloaded {
                    loads.downloaded += 1;
                } else {
                    loads.from_cache += 1;
                }
            }
        }
        missing_hours.sort();

        let grid = match grid {
            Some(g) => g,
            None => anyhow::bail!(
                "no MRMS data for any hour in this window ({} hours missing)",
                hours.missing
            ),
        };
        Ok(Accumulation {
            window,
            grid,
            hours,
            missing_hours,
            loads,
        })
    }

    /// Load one planned item; a multi-hour block that is not on the bucket
    /// falls back to its hours one by one.
    async fn load_item(&self, it: Item) -> Result<Vec<Loaded>> {
        if it.product.hours == 1 {
            let (grid, downloaded) = self.load_hour(it).await?;
            return Ok(vec![Loaded {
                end_hour: it.end_hour,
                hours: 1,
                grid,
                downloaded,
            }]);
        }
        if let Some((g, downloaded)) = self.load_one(&it.product, it.end_hour).await? {
            return Ok(vec![Loaded {
                end_hour: it.end_hour,
                hours: it.product.hours,
                grid: Some((Pass::Pass2, g)),
                downloaded,
            }]);
        }
        tracing::debug!(product = %it.product.name(), "block missing; falling back to hourly");
        let mut out = Vec::new();
        for h in (it.end_hour - it.product.hours as i64 + 1)..=it.end_hour {
            let item = Item {
                product: Product::hourly(Pass::Pass2),
                end_hour: h,
            };
            let (grid, downloaded) = Box::pin(self.load_hour(item)).await?;
            out.push(Loaded {
                end_hour: h,
                hours: 1,
                grid,
                downloaded,
            });
        }
        Ok(out)
    }

    /// One hour, best available pass first, starting from the planned one.
    async fn load_hour(&self, it: Item) -> Result<(Option<(Pass, Grid)>, bool)> {
        let order: &[Pass] = match it.product.pass {
            Pass::Pass2 => &[Pass::Pass2, Pass::Pass1, Pass::Radar],
            Pass::Pass1 => &[Pass::Pass1, Pass::Radar],
            Pass::Radar => &[Pass::Radar],
        };
        let mut any_download = false;
        for &pass in order {
            let p = Product::hourly(pass);
            if let Some((g, downloaded)) = self.load_one(&p, it.end_hour).await? {
                return Ok((Some((pass, g)), any_download || downloaded));
            }
            any_download = true;
        }
        Ok((None, any_download))
    }

    /// Apply the retention limit. The cache grows only on a download, so no other path
    /// needs it. A limit of 0 or less means nothing rather than everything, because the
    /// mistake it guards against would empty the cache. A failure costs only disk, so
    /// the download that prompted it goes on.
    fn forget_old_grids(&self) {
        let Some(days) = self.cfg.retain_days.filter(|days| *days > 0) else {
            return;
        };
        match self.store.retain_downloaded(days) {
            Ok(0) => {}
            Ok(n) => tracing::info!(grids = n, days, "dropped cached grids past retention"),
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "cache retention failed"),
        }
    }

    /// Cache, then bucket. `None` when the file does not exist.
    async fn load_one(&self, p: &Product, end_hour: i64) -> Result<Option<(Grid, bool)>> {
        self.load_at(p, ts_hour(end_hour)).await
    }

    /// Cache, then bucket. `None` when the file does not exist.
    async fn load_at(&self, p: &Product, valid: Timestamp) -> Result<Option<(Grid, bool)>> {
        let region = &self.cfg.region;
        if let Some(g) = self.store.get_grid(p, valid, region)? {
            return Ok(Some((g, false)));
        }
        // A recent hour may appear any minute; an old one that is missing is
        // most likely an upstream outage that will not be backfilled.
        let age = Timestamp::now().as_second() - valid.as_second();
        let absent_ttl = if age < 6 * HOUR_SECS {
            300
        } else {
            6 * HOUR_SECS
        };
        if self.store.recently_absent(p, valid, absent_ttl)? {
            return Ok(None);
        }
        match self.client.fetch(p, valid, region).await? {
            Some(g) => {
                self.store.put_grid(p, valid, region, &g)?;
                self.forget_old_grids();
                Ok(Some((g, true)))
            }
            None => {
                self.store.mark_absent(p, valid)?;
                Ok(None)
            }
        }
    }
}

struct Loaded {
    end_hour: i64,
    hours: u32,
    grid: Option<(Pass, Grid)>,
    downloaded: bool,
}

/// The planner itself, separated from I/O so it can be tested exhaustively.
///
/// `s`, `e`: unix hours bounding the window `(s, e]`. `p2`: newest unix hour
/// with Pass 2. `cached[b]`: end hours already cached for the `b`-hour Pass 2
/// product.
fn plan_items(s: i64, e: i64, p2: i64, cached: &HashMap<u32, HashSet<i64>>) -> Vec<Item> {
    let mut items = Vec::new();
    let mut t = e;

    // Hours with no Pass 2 yet: one at a time, provisional.
    while t > s && t > p2 {
        items.push(Item {
            product: Product::hourly(Pass::Pass1),
            end_hour: t,
        });
        t -= 1;
    }

    let has = |b: u32, h: i64| cached.get(&b).is_some_and(|set| set.contains(&h));

    while t > s {
        let remaining = t - s;
        let mut take = 1u32;
        for &b in PASS2_BLOCKS.iter().filter(|&&b| b > 1) {
            let bi = b as i64;
            if bi > remaining || t.rem_euclid(bi) != 0 {
                continue;
            }
            if has(b, t) {
                take = b;
                break;
            }
            // Fetch the block only when it saves downloads: a b-hour file is
            // roughly the size of b/7 hourly ones (72 h ≈ 8.7 MB vs ~0.9 MB),
            // so it pays once more than b/6 of its hours are not cached.
            let uncached = (t - bi + 1..=t).filter(|&h| !has(1, h)).count() as i64;
            if uncached * 6 > bi {
                take = b;
                break;
            }
        }
        items.push(Item {
            product: Product::block(take),
            end_hour: t,
        });
        t -= take as i64;
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_a_long_downloaded_grid(store: &Store, p: &Product, valid: Timestamp, r: &Region) {
        let geom = crate::grid::GridGeom {
            nx: 1,
            ny: 1,
            lat0: 27.0,
            lon0: -80.1,
            dlat: 0.01,
            dlon: 0.01,
        };
        let grid = Grid {
            geom,
            tenths: vec![5],
        };
        store.put_grid(p, valid, r, &grid).unwrap();
        store
            .pool()
            .get()
            .unwrap()
            .execute("UPDATE mrms_grids SET fetched_at = 1000", [])
            .unwrap();
    }

    #[test]
    fn retention_runs_once_an_hour_across_programs_and_can_be_turned_off() {
        let region = Region::parse("-80.2,26.8,-80.0,27.0").unwrap();
        let p = Product::hourly(Pass::Pass2);
        let valid = Timestamp::from_second(1_800_000_000).unwrap();
        let weather = |store: &Store, retain_days| {
            let cfg = Config {
                retain_days,
                ..Config::new(region)
            };
            Weather::new(cfg, store.clone()).unwrap()
        };
        let cached = |store: &Store| store.has_grid(&p, valid, &region).unwrap();

        for keeps_all in [None, Some(0), Some(-1)] {
            let store = Store::open_in_memory().unwrap();
            store_a_long_downloaded_grid(&store, &p, valid, &region);
            weather(&store, keeps_all).forget_old_grids();
            assert!(cached(&store), "{keeps_all:?} should keep everything");
        }

        let store = Store::open_in_memory().unwrap();
        store_a_long_downloaded_grid(&store, &p, valid, &region);
        weather(&store, Some(1)).forget_old_grids();
        assert!(!cached(&store));

        store_a_long_downloaded_grid(&store, &p, valid, &region);
        weather(&store, Some(1)).forget_old_grids();
        assert!(cached(&store), "a second pass within the hour should wait");
    }

    /// Hours covered by a plan, each exactly once, as a sorted list.
    fn covered(items: &[Item]) -> Vec<i64> {
        let mut v: Vec<i64> = items
            .iter()
            .flat_map(|it| (it.end_hour - it.product.hours as i64 + 1)..=it.end_hour)
            .collect();
        v.sort();
        v
    }

    fn sizes(items: &[Item]) -> Vec<u32> {
        items.iter().map(|i| i.product.hours).collect()
    }

    // 2026-11-03T00:00Z is unix hour 498_240 — a multiple of 72 and of 24.
    const MIDNIGHT: i64 = 498_240;

    #[test]
    fn midnight_constant_is_aligned() {
        assert_eq!(MIDNIGHT % 72, 0);
        assert_eq!(
            ts_hour(MIDNIGHT),
            "2026-11-03T00:00:00Z".parse::<Timestamp>().unwrap()
        );
    }

    #[test]
    fn every_hour_covered_exactly_once() {
        let empty = HashMap::new();
        for (s, e) in [
            (MIDNIGHT - 1, MIDNIGHT),
            (MIDNIGHT - 168, MIDNIGHT + 17),
            (MIDNIGHT - 721, MIDNIGHT + 5),
            (MIDNIGHT + 3, MIDNIGHT + 4),
        ] {
            let items = plan_items(s, e, e, &empty);
            assert_eq!(
                covered(&items),
                ((s + 1)..=e).collect::<Vec<_>>(),
                "{s}..{e}"
            );
        }
    }

    #[test]
    fn cold_week_uses_big_aligned_blocks() {
        // Past 7 days ending 17Z: 17 edge hours, then 72+72+24 aligned blocks.
        let e = MIDNIGHT + 17;
        let items = plan_items(e - 168, e, e, &HashMap::new());
        assert_eq!(covered(&items).len(), 168);
        let n72 = items.iter().filter(|i| i.product.hours == 72).count();
        assert_eq!(n72, 2, "{:?}", sizes(&items));
        // Far fewer files than 168 hourly ones.
        assert!(
            items.len() < 20,
            "{} items: {:?}",
            items.len(),
            sizes(&items)
        );
    }

    #[test]
    fn warm_hourlies_are_preferred_over_new_blocks() {
        let e = MIDNIGHT;
        let mut cached = HashMap::new();
        cached.insert(1u32, ((e - 23)..=e).collect::<HashSet<_>>());
        let items = plan_items(e - 24, e, e, &cached);
        assert_eq!(sizes(&items), vec![1; 24]);
    }

    #[test]
    fn cached_block_is_reused() {
        let e = MIDNIGHT;
        let mut cached = HashMap::new();
        cached.insert(24u32, HashSet::from([e]));
        let items = plan_items(e - 24, e, e, &cached);
        assert_eq!(sizes(&items), vec![24]);
    }

    #[test]
    fn hours_after_pass2_are_hourly_provisional() {
        let e = MIDNIGHT + 2;
        let items = plan_items(e - 26, e, MIDNIGHT, &HashMap::new());
        assert_eq!(items[0].product, Product::hourly(Pass::Pass1));
        assert_eq!(items[1].product, Product::hourly(Pass::Pass1));
        assert!(items[2..].iter().all(|i| i.product.pass == Pass::Pass2));
        assert_eq!(covered(&items).len(), 26);
    }

    #[test]
    fn expected_provisional_counts_hours_past_pass2() {
        let t = |s: &str| s.parse::<Timestamp>().unwrap();
        let a = Availability {
            latest: t("2026-11-02T23:00:00Z"),
            latest_pass: Pass::Pass1,
            pass2_through: t("2026-11-02T22:00:00Z"),
            checked_at: t("2026-11-02T23:20:00Z"),
        };
        let w = Window::new(t("2026-11-01T23:00:00Z"), t("2026-11-02T23:00:00Z"));
        assert_eq!(expected_provisional(w, &a), 1);
        // A window ending before Pass 2's frontier is entirely final.
        let w = Window::new(t("2026-11-01T00:00:00Z"), t("2026-11-02T00:00:00Z"));
        assert_eq!(expected_provisional(w, &a), 0);
    }

    #[test]
    fn blocks_never_cross_the_window_start() {
        // Window starts 5 h before an aligned 72 h boundary.
        let e = MIDNIGHT;
        let items = plan_items(e - 5, e, e, &HashMap::new());
        assert!(
            items.iter().all(|i| i.product.hours <= 3),
            "{:?}",
            sizes(&items)
        );
        assert_eq!(covered(&items).len(), 5);
    }
}

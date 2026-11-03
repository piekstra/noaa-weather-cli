//! NOAA MRMS (Multi-Radar Multi-Sensor) precipitation products on AWS.
//!
//! MRMS is the gridded analysis behind the NWS's own rainfall maps: every
//! NEXRAD radar in the country merged onto a 1 km grid, then bias-corrected
//! against rain gauges. NOAA publishes it through the AWS Open Data program at
//! `s3://noaa-mrms-pds` — no key, no account.
//!
//! ## Which product
//!
//! The *MultiSensor QPE* family, in two passes:
//!
//! | Pass | Gauges folded in | Published (measured) |
//! |---|---|---|
//! | Pass 2 | the full gauge network | ~56 min after the hour |
//! | Pass 1 | gauges reported so far | ~15 min after the hour |
//!
//! Pass 2 is the record. Pass 1 stands in only for the most recent hour or so,
//! and *RadarOnly* (no gauge correction at all, ~3 min after the hour) only for
//! an hour neither pass covers yet. Every total reports how many of its hours
//! came from each, so a provisional figure is never presented as final.
//!
//! ## Accumulation periods compose exactly
//!
//! The multi-hour Pass 2 products (3, 6, 12, 24, 72 h) are the exact sum of the
//! hourly ones: 24 hourly Pass 2 grids ending 2026-11-01 12Z summed over the
//! 10,656 cells in one region match the 24 h product with a maximum difference
//! of 7e-15 mm. So a window can be assembled from whichever products are
//! cheapest to fetch — one 72 h file instead of 72 hourly ones — and the result
//! is identical. See `docs/api.md`.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::grib2;
use crate::grid::{Grid, GridGeom, Region, MISSING};
use crate::UpstreamError;

pub const DEFAULT_BUCKET_URL: &str = "https://noaa-mrms-pds.s3.amazonaws.com";

/// Tries per request before a download or probe counts as failed.
const ATTEMPTS: u32 = 4;

pub const ATTRIBUTION: &str =
    "Rainfall: NOAA/NSSL MRMS Multi-Sensor QPE (via NOAA Open Data on AWS)";

/// How a grid's values were produced, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Pass {
    /// Gauge-corrected with the full gauge network. Final.
    Pass2,
    /// Gauge-corrected with the gauges reported so far. Provisional.
    Pass1,
    /// Radar estimate with no gauge correction. Preliminary.
    Radar,
}

impl Pass {
    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Pass2 => "pass2",
            Pass::Pass1 => "pass1",
            Pass::Radar => "radar",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pass2" => Some(Pass::Pass2),
            "pass1" => Some(Pass::Pass1),
            "radar" => Some(Pass::Radar),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Pass::Pass2 => "final (gauge-corrected, Pass 2)",
            Pass::Pass1 => "provisional (Pass 1)",
            Pass::Radar => "preliminary (radar only)",
        }
    }
}

/// What a product measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Rain accumulated over `hours`.
    Accumulation,
    /// Rain accumulated over 15 minutes, radar only.
    Quarter,
    /// Instantaneous rain rate in mm/h, every 2 minutes, radar only.
    Rate,
}

/// One MRMS product.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct Product {
    pub pass: Pass,
    /// Accumulation length in hours; 0 for anything shorter than an hour.
    pub hours: u32,
    pub kind: Kind,
}

/// Multi-hour Pass 2 accumulations, largest first. 48 h exists upstream but is
/// left out: it does not tile with 72 h on a fixed grid, and the planner only
/// uses blocks aligned to multiples of their own length (see `engine`).
pub const PASS2_BLOCKS: [u32; 6] = [72, 24, 12, 6, 3, 1];

impl Product {
    /// 15-minute accumulations, published about 4 minutes after each quarter.
    /// No gauge correction exists at this resolution.
    pub const RADAR_15M: Self = Self {
        pass: Pass::Radar,
        hours: 0,
        kind: Kind::Quarter,
    };

    /// The 2-minute rain-rate field behind radar loops.
    pub const PRECIP_RATE: Self = Self {
        pass: Pass::Radar,
        hours: 0,
        kind: Kind::Rate,
    };

    pub const fn hourly(pass: Pass) -> Self {
        Self {
            pass,
            hours: 1,
            kind: Kind::Accumulation,
        }
    }

    /// A multi-hour Pass 2 accumulation.
    pub const fn block(hours: u32) -> Self {
        Self {
            pass: Pass::Pass2,
            hours,
            kind: Kind::Accumulation,
        }
    }

    /// The directory name on the bucket, e.g. `MultiSensor_QPE_24H_Pass2_00.00`.
    pub fn name(&self) -> String {
        match self.kind {
            Kind::Quarter => return "RadarOnly_QPE_15M_00.00".into(),
            Kind::Rate => return "PrecipRate_00.00".into(),
            Kind::Accumulation => {}
        }
        match self.pass {
            Pass::Pass2 => format!("MultiSensor_QPE_{:02}H_Pass2_00.00", self.hours),
            Pass::Pass1 => format!("MultiSensor_QPE_{:02}H_Pass1_00.00", self.hours),
            Pass::Radar => format!("RadarOnly_QPE_{:02}H_00.00", self.hours),
        }
    }

    /// Object key for the accumulation ending at `valid`.
    pub fn key(&self, valid: Timestamp) -> String {
        let dt = valid.to_zoned(jiff::tz::TimeZone::UTC);
        let date = dt.strftime("%Y%m%d").to_string();
        let stamp = dt.strftime("%Y%m%d-%H%M%S").to_string();
        let name = self.name();
        format!("CONUS/{name}/{date}/MRMS_{name}_{stamp}.grib2.gz")
    }
}

/// The full 7000 × 3500 raster at 16 bits is 49 MB before PNG compression, so
/// nothing real comes close to this.
const MAX_RAW_BYTES: u64 = 128 * 1024 * 1024;

/// Decode one gzipped MRMS file and crop it to `region`.
///
/// Checks the file's own reference time against `valid`: a key that resolved
/// to the wrong hour must fail loudly, not be summed in as the right one.
pub fn decode_cropped(gz: &[u8], valid: Timestamp, region: &Region) -> Result<Grid> {
    let mut raw = Vec::with_capacity(gz.len() * 2);
    flate2::read::GzDecoder::new(gz)
        .take(MAX_RAW_BYTES + 1)
        .read_to_end(&mut raw)
        .context("gunzip MRMS file")?;
    if raw.len() as u64 > MAX_RAW_BYTES {
        bail!("MRMS file inflates past {MAX_RAW_BYTES} bytes");
    }
    let msg = grib2::parse(&raw).context("parse GRIB2")?;

    let t = msg.ref_time;
    let file_time = jiff::civil::date(t.year as i16, t.month as i8, t.day as i8)
        .at(t.hour as i8, t.minute as i8, t.second as i8, 0)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .context("reference time")?
        .timestamp();
    if file_time != valid {
        bail!("MRMS file is for {file_time}, expected {valid}");
    }

    let win = msg
        .grid
        .window(region.west, region.south, region.east, region.north)
        .context("region is outside the MRMS grid")?;

    // MRMS encodes "no radar coverage" as -1 and "missing" as -3; any negative
    // is no value. Round to tenths: values are exact tenths by construction.
    let tenths = msg.decode_window(win, |v| {
        if v < 0.0 {
            MISSING
        } else {
            (v * 10.0).round() as i32
        }
    })?;

    let north_row = if msg.grid.rows_northward {
        win.j1
    } else {
        win.j0
    };
    let geom = GridGeom {
        nx: win.width(),
        ny: win.height(),
        lat0: msg.grid.lat(north_row),
        lon0: msg.grid.lon(win.i0),
        dlat: msg.grid.dj,
        dlon: msg.grid.di,
    };
    Ok(Grid { geom, tenths })
}

/// HTTP access to the bucket.
pub struct MrmsClient {
    http: reqwest::Client,
    base: String,
    /// Bounds simultaneous downloads. S3 would take far more; this is about
    /// not saturating the operator's own connection during a cold backfill.
    permits: Arc<Semaphore>,
}

impl MrmsClient {
    pub fn new(base: impl Into<String>, concurrency: usize) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "noaa-weather-cli/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/piekstra/noaa-weather-cli)"
            ))
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(120))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
        })
    }

    pub fn url(&self, product: &Product, valid: Timestamp) -> String {
        format!("{}/{}", self.base, product.key(valid))
    }

    /// Download the raw gzipped file, retrying transient failures. `Ok(None)`
    /// when it does not exist (yet).
    pub async fn download(&self, product: &Product, valid: Timestamp) -> Result<Option<Vec<u8>>> {
        let _permit = self.permits.acquire().await.expect("semaphore closed");
        let url = self.url(product, valid);
        let mut attempt = 1;
        loop {
            match self.download_once(&url).await {
                // A dropped connection or a busy S3 usually clears on the next try.
                Err(e) if attempt < ATTEMPTS => {
                    tracing::debug!(%url, attempt, error = %format!("{e:#}"), "retrying download");
                    tokio::time::sleep(Duration::from_millis(500 << (attempt - 1))).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    async fn download_once(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        // S3 answers 403 rather than 404 for a missing key on a bucket that
        // does not grant ListBucket to the caller; this one does grant it, but
        // treat both as "not there" so a policy change upstream degrades to
        // missing hours instead of a hard error.
        if matches!(resp.status().as_u16(), 403 | 404) {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading {url}"))?;
        Ok(Some(bytes.to_vec()))
    }

    /// Download, decode, and crop. `Ok(None)` when the file does not exist.
    pub async fn fetch(
        &self,
        product: &Product,
        valid: Timestamp,
        region: &Region,
    ) -> Result<Option<Grid>> {
        let Some(gz) = self.download(product, valid).await? else {
            return Ok(None);
        };
        let region = *region;
        // Inflating ~49 MB of PNG is CPU work; keep it off the async workers.
        let grid = tokio::task::spawn_blocking(move || decode_cropped(&gz, valid, &region))
            .await
            .context("decode task")??;
        Ok(Some(grid))
    }

    /// Valid times of a product's files on one UTC day (`YYYYMMDD`), ascending.
    pub async fn list_day(&self, product: &Product, day: &str) -> Result<Vec<Timestamp>> {
        let _permit = self.permits.acquire().await.expect("semaphore closed");
        let prefix = format!("CONUS/{}/{day}/", product.name());
        let url = format!("{}/?list-type=2&prefix={prefix}", self.base);
        let text = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?
            .text()
            .await
            .with_context(|| format!("reading {url}"))?;
        Ok(valid_times_in_listing(&text))
    }

    /// Whether a file exists, without downloading it.
    ///
    /// Takes no download permit: a probe is a few bytes, and asking what is
    /// published must not wait behind a backfill.
    pub async fn exists(&self, product: &Product, valid: Timestamp) -> Result<bool> {
        let url = self.url(product, valid);
        let mut attempt = 1;
        loop {
            match self.exists_once(&url).await {
                Err(e) if attempt < ATTEMPTS => {
                    tracing::debug!(%url, attempt, error = %format!("{e:#}"), "retrying probe");
                    tokio::time::sleep(Duration::from_millis(500 << (attempt - 1))).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    async fn exists_once(&self, url: &str) -> Result<bool> {
        let resp = self
            .http
            .head(url)
            .send()
            .await
            .with_context(|| format!("HEAD {url}"))?;
        match resp.status().as_u16() {
            200 => Ok(true),
            403 | 404 => Ok(false),
            s => Err(UpstreamError(format!("HEAD {url}: HTTP {s}")).into()),
        }
    }
}

/// Pull the `YYYYMMDD-HHMMSS` stamps out of an S3 listing, ascending.
fn valid_times_in_listing(xml: &str) -> Vec<Timestamp> {
    let mut out: Vec<Timestamp> = xml
        .split("<Key>")
        .skip(1)
        .filter_map(|part| part.split("</Key>").next())
        .filter_map(|key| {
            let stamp = key.rsplit('_').next()?.strip_suffix(".grib2.gz")?;
            jiff::civil::DateTime::strptime("%Y%m%d-%H%M%S", stamp)
                .ok()?
                .to_zoned(jiff::tz::TimeZone::UTC)
                .ok()
                .map(|z| z.timestamp())
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grib2::tests::{synth_grib, MRMS_PACKING, T0};
    use std::io::Write;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// The box the sample file's expected values were read from.
    const REAL_FILE_REGION: Region = Region {
        west: -80.90,
        south: 26.30,
        east: -79.90,
        north: 27.40,
    };

    #[test]
    fn listing_yields_sorted_valid_times() {
        let xml = "<ListBucketResult>\
            <Contents><Key>CONUS/PrecipRate_00.00/20261103/MRMS_PrecipRate_00.00_20261103-035400.grib2.gz</Key></Contents>\
            <Contents><Key>CONUS/PrecipRate_00.00/20261103/MRMS_PrecipRate_00.00_20261103-035200.grib2.gz</Key></Contents>\
            <Contents><Key>CONUS/PrecipRate_00.00/20261103/not-a-grib.txt</Key></Contents>\
            </ListBucketResult>";
        assert_eq!(
            valid_times_in_listing(xml),
            vec![ts("2026-11-03T03:52:00Z"), ts("2026-11-03T03:54:00Z")]
        );
    }

    #[test]
    fn sub_hourly_names_and_keys_match_the_bucket() {
        assert_eq!(Product::RADAR_15M.name(), "RadarOnly_QPE_15M_00.00");
        assert_eq!(
            Product::PRECIP_RATE.key(ts("2026-11-03T03:54:00Z")),
            "CONUS/PrecipRate_00.00/20261103/MRMS_PrecipRate_00.00_20261103-035400.grib2.gz"
        );
    }

    #[test]
    fn product_names_match_the_bucket() {
        assert_eq!(
            Product::hourly(Pass::Pass2).name(),
            "MultiSensor_QPE_01H_Pass2_00.00"
        );
        assert_eq!(Product::block(72).name(), "MultiSensor_QPE_72H_Pass2_00.00");
        assert_eq!(
            Product::hourly(Pass::Radar).name(),
            "RadarOnly_QPE_01H_00.00"
        );
    }

    #[test]
    fn key_matches_a_real_object() {
        // Verified to exist on s3://noaa-mrms-pds (see docs/data-sources.md).
        assert_eq!(
            Product::hourly(Pass::Pass2).key(ts("2026-11-01T12:00:00Z")),
            "CONUS/MultiSensor_QPE_01H_Pass2_00.00/20261101/\
             MRMS_MultiSensor_QPE_01H_Pass2_00.00_20261101-120000.grib2.gz"
        );
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    /// A 6×5 synthetic MRMS file: NW corner 27.00N 80.10W, 0.01°, values
    /// row·10 + col tenths, with one no-coverage (-1) cell.
    fn synthetic() -> Vec<u8> {
        let (ni, nj) = (6u32, 5u32);
        let mut raw: Vec<u16> = (0..nj)
            .flat_map(|j| (0..ni).map(move |i| (30 + 10 * j + i) as u16))
            .collect();
        raw[7] = 20; // -1.0 → no coverage
        gz(&synth_grib(
            ni,
            nj,
            27_000_000,
            (360.0e6 - 80.10e6) as u32,
            10_000,
            0,
            MRMS_PACKING,
            &raw,
            T0,
        ))
    }

    #[test]
    fn decodes_crops_and_masks() {
        let region = Region {
            west: -80.09,
            south: 26.97,
            east: -80.07,
            north: 26.99,
        };
        let g = decode_cropped(&synthetic(), ts("2026-11-01T12:00:00Z"), &region).unwrap();
        assert_eq!((g.geom.nx, g.geom.ny), (3, 3));
        assert!((g.geom.lat0 - 26.99).abs() < 1e-9);
        assert!((g.geom.lon0 - -80.09).abs() < 1e-9);
        // Row j=1 → cols 1..3: raw[7] (j=1,i=1) is the no-coverage cell.
        assert_eq!(&g.tenths[0..3], &[MISSING, 12, 13]);
        assert_eq!(&g.tenths[6..9], &[31, 32, 33]);
        assert_eq!(g.mm_at(26.97, -80.07), Some(3.3));
    }

    #[test]
    fn wrong_reference_time_is_an_error() {
        let err = decode_cropped(
            &synthetic(),
            ts("2026-11-01T13:00:00Z"),
            &crate::grid::TEST_REGION,
        )
        .unwrap_err();
        assert!(err.to_string().contains("expected"), "{err}");
    }

    /// Decode a real MRMS file and compare against values read independently
    /// with pygrib/ecCodes. Run with:
    ///
    /// ```text
    /// WX_SAMPLE_72H=/path/MRMS_MultiSensor_QPE_72H_Pass2_00.00_20261101-120000.grib2.gz \
    ///   cargo test -p noaa-weather-cli -- --ignored real_file
    /// ```
    #[test]
    #[ignore = "needs a downloaded MRMS sample"]
    fn real_file_matches_ecc_codes() {
        let path = std::env::var("WX_SAMPLE_72H").expect("set WX_SAMPLE_72H");
        let gz = std::fs::read(path).unwrap();
        let g = decode_cropped(&gz, ts("2026-11-01T12:00:00Z"), &REAL_FILE_REGION).unwrap();
        // Grid points sit at x.xx5°, so the region's x.xx0 edges enclose 100 × 110.
        assert_eq!((g.geom.nx, g.geom.ny), (100, 110));
        // Reference values from pygrib 2.1.8 at the nearest grid point.
        assert_eq!(g.mm_at(27.197, -80.253), Some(77.1));
        assert_eq!(g.mm_at(26.715, -80.054), Some(5.2));
        let s = g.stats();
        assert_eq!(s.max_mm, 94.3);
        assert_eq!(s.missing_cells, 0);
    }
}

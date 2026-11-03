//! Local cache: cropped MRMS grids and gauge reports, in SQLite.
//!
//! A single file that holds only what can be fetched again, so it is safe to
//! delete. Programs using the library choose where it lives; the `wx` binary
//! keeps it in the user's cache directory.
//!
//! ## What is cached, and for how long
//!
//! * **Pass 2 grids — until the retention limit.** A final MRMS analysis never
//!   changes, so it is not fetched twice while it is kept. A cropped hour is ~22 KB.
//!   [`Store::prune_downloaded_before`] drops grids by when they were downloaded, and
//!   SQLite reuses the freed pages, so the file stops growing rather than shrinking.
//! * **Pass 1 / radar-only grids — until superseded.** Kept so the newest hour
//!   is not re-downloaded on every request, then pruned once a day old (by
//!   then the Pass 2 grid for the same hour exists).
//! * **"Not on the bucket (yet)" — briefly.** Remembered so the newest hour,
//!   which is usually not published yet, is not re-requested on every call.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use std::io::{Read, Write};

use anyhow::{Context, Result};
use jiff::Timestamp;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::grid::{Grid, GridGeom, Region};
use crate::mrms::{Pass, Product};

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

/// `${XDG_CACHE_HOME:-~/.cache}/wx/weather.sqlite`.
pub fn default_db_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("wx").join("weather.sqlite")
}

/// Rain falls in patches and most cells hold zero, so this shrinks a grid a lot.
fn deflate(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(
        Vec::with_capacity(bytes.len() / 4),
        flate2::Compression::fast(),
    );
    enc.write_all(bytes)?;
    Ok(enc.finish()?)
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Clone)]
pub struct Store {
    pool: Pool,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let manager = SqliteConnectionManager::file(path).with_init(|c| {
            c.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA busy_timeout = 5000;",
            )
        });
        let pool = r2d2::Pool::builder()
            .max_size(8)
            .build(manager)
            .context("building sqlite pool")?;
        let s = Self { pool };
        s.migrate()?;
        Ok(s)
    }

    /// One shared in-memory connection, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let pool = r2d2::Pool::builder()
            .max_size(1)
            .build(SqliteConnectionManager::memory())?;
        let s = Self { pool };
        s.migrate()?;
        Ok(s)
    }

    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    fn migrate(&self) -> Result<()> {
        self.pool.get()?.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS mrms_grids (
                product    TEXT    NOT NULL,   -- bucket directory, e.g. MultiSensor_QPE_01H_Pass2_00.00
                pass       TEXT    NOT NULL,   -- pass2 | pass1 | radar
                hours      INTEGER NOT NULL,   -- accumulation length
                valid      INTEGER NOT NULL,   -- unix seconds, end of the accumulation
                region     TEXT    NOT NULL,   -- Region::key()
                nx INTEGER NOT NULL, ny INTEGER NOT NULL,
                lat0 REAL NOT NULL, lon0 REAL NOT NULL,
                dlat REAL NOT NULL, dlon REAL NOT NULL,
                data       BLOB    NOT NULL,   -- zlib of u16 LE tenths of mm, 0xFFFF = missing
                fetched_at INTEGER NOT NULL,
                PRIMARY KEY (product, valid, region)
            );
            CREATE INDEX IF NOT EXISTS mrms_grids_pass ON mrms_grids (pass, valid);
            CREATE INDEX IF NOT EXISTS mrms_grids_fetched ON mrms_grids (fetched_at);

            CREATE TABLE IF NOT EXISTS mrms_absent (
                product    TEXT    NOT NULL,
                valid      INTEGER NOT NULL,
                checked_at INTEGER NOT NULL,
                PRIMARY KEY (product, valid)
            );

            CREATE TABLE IF NOT EXISTS feed_stations (
                id   TEXT PRIMARY KEY,   -- US1FLPB0012, USGS-02300500, ASOS-PBI
                name TEXT NOT NULL,
                lat  REAL NOT NULL,
                lon  REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS feed_stations_place ON feed_stations (lat, lon);

            CREATE TABLE IF NOT EXISTS feed_hours (
                station  TEXT    NOT NULL,
                hour_end INTEGER NOT NULL,     -- unix seconds
                mm       REAL    NOT NULL,
                trace    INTEGER NOT NULL,
                measured_rain INTEGER NOT NULL,
                PRIMARY KEY (station, hour_end)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS feed_hours_end ON feed_hours (hour_end);

            CREATE TABLE IF NOT EXISTS feed_reports (
                station  TEXT    NOT NULL,
                date     TEXT    NOT NULL,     -- YYYYMMDD, the report date
                prcp     INTEGER NOT NULL,     -- tenths of mm, as gauge_obs
                obs_time TEXT,                 -- HHMM local, as reported
                m_flag   TEXT,
                PRIMARY KEY (station, date)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS gauge_stations (
                id      TEXT PRIMARY KEY,      -- GHCN-Daily station id
                name    TEXT NOT NULL,
                lat     REAL NOT NULL,
                lon     REAL NOT NULL,
                elev_m  REAL,
                region  TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS gauge_obs (
                station  TEXT    NOT NULL,
                date     TEXT    NOT NULL,     -- YYYYMMDD, the report date
                prcp     INTEGER NOT NULL,     -- tenths of mm
                obs_time TEXT,                 -- HHMM local, as reported
                m_flag   TEXT,
                s_flag   TEXT,
                PRIMARY KEY (station, date)
            );

            CREATE TABLE IF NOT EXISTS meta (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            ",
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------- grids

    pub fn get_grid(&self, p: &Product, valid: Timestamp, region: &Region) -> Result<Option<Grid>> {
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                "SELECT nx, ny, lat0, lon0, dlat, dlon, data FROM mrms_grids
                 WHERE product = ?1 AND valid = ?2 AND region = ?3",
                params![p.name(), valid.as_second(), region.key()],
                |r| {
                    Ok((
                        GridGeom {
                            nx: r.get::<_, i64>(0)? as usize,
                            ny: r.get::<_, i64>(1)? as usize,
                            lat0: r.get(2)?,
                            lon0: r.get(3)?,
                            dlat: r.get(4)?,
                            dlon: r.get(5)?,
                        },
                        r.get::<_, Vec<u8>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(geom, blob)| {
            // Stored deflated, except rows older than that, which are exact size.
            if blob.len() == geom.len() * 2 {
                return Grid::from_blob(geom, &blob).map_err(Into::into);
            }
            let mut raw = Vec::with_capacity(geom.len() * 2);
            flate2::read::ZlibDecoder::new(&blob[..])
                .read_to_end(&mut raw)
                .context("inflating a cached grid")?;
            Grid::from_blob(geom, &raw).map_err(Into::into)
        })
        .transpose()
    }

    /// Whether a grid is cached, without reading or inflating it.
    pub fn has_grid(&self, p: &Product, valid: Timestamp, region: &Region) -> Result<bool> {
        let conn = self.pool.get()?;
        Ok(conn
            .query_row(
                "SELECT 1 FROM mrms_grids WHERE product = ?1 AND valid = ?2 AND region = ?3",
                params![p.name(), valid.as_second(), region.key()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Which of `valids` are cached for `p`, as unix seconds.
    pub fn cached_valids(
        &self,
        p: &Product,
        from: Timestamp,
        to: Timestamp,
        region: &Region,
    ) -> Result<HashSet<i64>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare_cached(
            "SELECT valid FROM mrms_grids
             WHERE product = ?1 AND region = ?2 AND valid >= ?3 AND valid <= ?4",
        )?;
        let rows = stmt.query_map(
            params![p.name(), region.key(), from.as_second(), to.as_second()],
            |r| r.get::<_, i64>(0),
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn put_grid(&self, p: &Product, valid: Timestamp, region: &Region, g: &Grid) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO mrms_grids
               (product, pass, hours, valid, region, nx, ny, lat0, lon0, dlat, dlon, data, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                p.name(),
                p.pass.as_str(),
                p.hours,
                valid.as_second(),
                region.key(),
                g.geom.nx as i64,
                g.geom.ny as i64,
                g.geom.lat0,
                g.geom.lon0,
                g.geom.dlat,
                g.geom.dlon,
                deflate(&g.to_blob())?,
                now_secs(),
            ],
        )?;
        conn.execute(
            "DELETE FROM mrms_absent WHERE product = ?1 AND valid = ?2",
            params![p.name(), valid.as_second()],
        )?;
        Ok(())
    }

    pub fn mark_absent(&self, p: &Product, valid: Timestamp) -> Result<()> {
        self.pool.get()?.execute(
            "INSERT OR REPLACE INTO mrms_absent (product, valid, checked_at) VALUES (?1, ?2, ?3)",
            params![p.name(), valid.as_second(), now_secs()],
        )?;
        Ok(())
    }

    /// Forget a "missing" marker — the file has since been seen upstream.
    pub fn clear_absent(&self, p: &Product, valid: Timestamp) -> Result<()> {
        self.pool.get()?.execute(
            "DELETE FROM mrms_absent WHERE product = ?1 AND valid = ?2",
            params![p.name(), valid.as_second()],
        )?;
        Ok(())
    }

    /// True when the file was confirmed missing within `ttl_secs`.
    pub fn recently_absent(&self, p: &Product, valid: Timestamp, ttl_secs: i64) -> Result<bool> {
        let at: Option<i64> = self
            .pool
            .get()?
            .query_row(
                "SELECT checked_at FROM mrms_absent WHERE product = ?1 AND valid = ?2",
                params![p.name(), valid.as_second()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(at.is_some_and(|t| now_secs() - t < ttl_secs))
    }

    /// Drop provisional grids for hours old enough to have a final one.
    pub fn prune_provisional(&self, older_than: Timestamp) -> Result<usize> {
        let conn = self.pool.get()?;
        let n = conn.execute(
            "DELETE FROM mrms_grids WHERE pass IN (?1, ?2) AND valid < ?3",
            params![
                Pass::Pass1.as_str(),
                Pass::Radar.as_str(),
                older_than.as_second()
            ],
        )?;
        conn.execute(
            "DELETE FROM mrms_absent WHERE checked_at < ?1",
            params![now_secs() - 7 * 86_400],
        )?;
        Ok(n)
    }

    /// Drop grids downloaded before `before`, final ones included. By download time
    /// rather than the hour a grid covers, so a look back at an old storm stays cached
    /// as long as a look at yesterday.
    pub fn prune_downloaded_before(&self, before: Timestamp) -> Result<usize> {
        Ok(self.pool.get()?.execute(
            "DELETE FROM mrms_grids WHERE fetched_at < ?1",
            params![before.as_second()],
        )?)
    }

    /// Drop grids downloaded more than `days` ago, at most once an hour however many
    /// programs share this file. The cutoff saturates at the epoch, so a huge `days`
    /// drops nothing.
    pub fn retain_downloaded(&self, days: i64) -> Result<usize> {
        const LAST_PASS: &str = "retention_pass";
        let now = now_secs();
        if self
            .meta(LAST_PASS)?
            .is_some_and(|(_, at)| now - at < 60 * 60)
        {
            return Ok(0);
        }
        self.set_meta(LAST_PASS, "")?;
        let cutoff = now.saturating_sub(days.saturating_mul(24 * 60 * 60)).max(0);
        self.prune_downloaded_before(Timestamp::from_second(cutoff)?)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT pass, hours, COUNT(*), COALESCE(SUM(LENGTH(data)), 0), MIN(valid), MAX(valid)
             FROM mrms_grids GROUP BY pass, hours ORDER BY pass, hours",
        )?;
        let products = stmt
            .query_map([], |r| {
                Ok(CachedProduct {
                    pass: r.get(0)?,
                    hours: r.get(1)?,
                    grids: r.get(2)?,
                    bytes: r.get(3)?,
                    oldest: r.get(4)?,
                    newest: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let gauge_stations: i64 =
            conn.query_row("SELECT COUNT(*) FROM gauge_stations", [], |r| r.get(0))?;
        let gauge_reports: i64 =
            conn.query_row("SELECT COUNT(*) FROM gauge_obs", [], |r| r.get(0))?;
        let feed_readings: i64 = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM feed_hours) + (SELECT COUNT(*) FROM feed_reports)",
            [],
            |r| r.get(0),
        )?;
        Ok(CacheStats {
            products,
            gauge_stations,
            gauge_reports,
            feed_readings,
        })
    }

    // ----------------------------------------------------------------- meta

    pub fn meta(&self, key: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .pool
            .get()?
            .query_row(
                "SELECT value, updated_at FROM meta WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.pool.get()?.execute(
            "INSERT OR REPLACE INTO meta (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![key, value, now_secs()],
        )?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct CachedProduct {
    pub pass: String,
    pub hours: i64,
    pub grids: i64,
    pub bytes: i64,
    pub oldest: i64,
    pub newest: i64,
}

#[derive(Debug, Serialize)]
pub struct CacheStats {
    pub products: Vec<CachedProduct>,
    pub gauge_stations: i64,
    pub gauge_reports: i64,
    pub feed_readings: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{MISSING, TEST_REGION as DEFAULT_REGION};

    fn t(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn grid() -> Grid {
        Grid {
            geom: GridGeom {
                nx: 2,
                ny: 2,
                lat0: 27.0,
                lon0: -80.1,
                dlat: 0.01,
                dlon: 0.01,
            },
            tenths: vec![0, 5, MISSING, 1234],
        }
    }

    #[test]
    fn grid_round_trip_and_lookup() {
        let s = Store::open_in_memory().unwrap();
        let p = Product::hourly(Pass::Pass2);
        let v = t("2026-11-01T12:00:00Z");
        assert!(s.get_grid(&p, v, &DEFAULT_REGION).unwrap().is_none());
        s.put_grid(&p, v, &DEFAULT_REGION, &grid()).unwrap();
        assert_eq!(s.get_grid(&p, v, &DEFAULT_REGION).unwrap(), Some(grid()));

        let got = s
            .cached_valids(
                &p,
                t("2026-11-01T00:00:00Z"),
                t("2026-11-02T00:00:00Z"),
                &DEFAULT_REGION,
            )
            .unwrap();
        assert_eq!(got, HashSet::from([v.as_second()]));

        // A different pass or region is a different entry.
        assert!(s
            .get_grid(&Product::hourly(Pass::Pass1), v, &DEFAULT_REGION)
            .unwrap()
            .is_none());
    }

    #[test]
    fn absent_markers_expire_and_clear_on_put() {
        let s = Store::open_in_memory().unwrap();
        let p = Product::hourly(Pass::Pass2);
        let v = t("2026-11-01T12:00:00Z");
        s.mark_absent(&p, v).unwrap();
        assert!(s.recently_absent(&p, v, 600).unwrap());
        assert!(!s.recently_absent(&p, v, 0).unwrap());
        s.put_grid(&p, v, &DEFAULT_REGION, &grid()).unwrap();
        assert!(!s.recently_absent(&p, v, 600).unwrap());

        s.mark_absent(&p, v).unwrap();
        s.clear_absent(&p, v).unwrap();
        assert!(!s.recently_absent(&p, v, 600).unwrap());
    }

    #[test]
    fn pruning_by_download_time_ignores_the_hour_a_grid_covers() {
        let s = Store::open_in_memory().unwrap();
        let (r, p2) = (DEFAULT_REGION, Product::hourly(Pass::Pass2));
        let (old, recent) = (t("2020-01-01T00:00:00Z"), t("2020-01-01T01:00:00Z"));
        s.put_grid(&p2, old, &r, &grid()).unwrap();
        s.put_grid(&p2, recent, &r, &grid()).unwrap();
        s.pool()
            .get()
            .unwrap()
            .execute(
                "UPDATE mrms_grids SET fetched_at = ?1 WHERE valid = ?2",
                params![1_000, recent.as_second()],
            )
            .unwrap();

        // `recent` was downloaded long ago, and `old` was downloaded just now.
        let cutoff = Timestamp::from_second(2_000).unwrap();
        assert_eq!(s.prune_downloaded_before(cutoff).unwrap(), 1);
        assert!(s.has_grid(&p2, old, &r).unwrap());
        assert!(!s.has_grid(&p2, recent, &r).unwrap());
    }

    #[test]
    fn prune_keeps_final_grids() {
        let s = Store::open_in_memory().unwrap();
        let v = t("2026-11-01T12:00:00Z");
        for pass in [Pass::Pass2, Pass::Pass1, Pass::Radar] {
            s.put_grid(&Product::hourly(pass), v, &DEFAULT_REGION, &grid())
                .unwrap();
        }
        assert_eq!(s.prune_provisional(t("2026-11-02T00:00:00Z")).unwrap(), 2);
        let st = s.stats().unwrap();
        assert_eq!(st.products.len(), 1);
        assert_eq!(st.products[0].pass, "pass2");
    }
}

//! Cropped rainfall grids and the region they cover.
//!
//! Values are held as **integer tenths of a millimetre**. That is MRMS's own
//! precision (decimal scale 1), so storage is lossless, and summing hundreds of
//! hourly grids is exact — no float drift between a 30-day total computed from
//! hourly grids and the same total computed from 72-hour ones.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Sentinel for "no value" (MRMS no-coverage or missing).
pub const MISSING: i32 = -1;

/// Millimetres per inch.
pub const MM_PER_INCH: f64 = 25.4;

#[derive(Debug, Error)]
pub enum GridError {
    #[error("bad region {0:?}: expected west,south,east,north")]
    BadRegion(String),
    #[error("grids are on different lattices and cannot be combined")]
    Lattice,
    #[error("stored grid blob has {got} bytes, expected {want}")]
    Blob { got: usize, want: usize },
}

/// A geographic box, west/south/east/north in degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Region {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

/// What MRMS covers: the contiguous US and the edges of its neighbors. Alaska,
/// Hawaii and everywhere else have no data here.
pub const MRMS_DOMAIN: Region = Region {
    west: -130.0,
    south: 20.0,
    east: -60.0,
    north: 55.0,
};

/// Area edges sit on multiples of this many degrees, so nearby views share one
/// cache key instead of each downloading its own crop.
pub const AREA_STEP_DEG: f64 = 4.0;

/// An area is at least this many degrees on a side. Wide enough that a whole
/// metro region, and panning around it, stays inside one set of cached grids.
pub const AREA_MIN_SPAN_DEG: f64 = 8.0;

/// Largest area a caller should ask for at once: 16° × 8°.
pub const MAX_AREA_CELLS: usize = 1_300_000;

impl Region {
    pub fn parse(s: &str) -> Result<Self, GridError> {
        let parts: Vec<f64> = s
            .split(',')
            .map(|p| p.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|_| GridError::BadRegion(s.into()))?;
        let [west, south, east, north] = parts[..] else {
            return Err(GridError::BadRegion(s.into()));
        };
        let ok = west < east
            && south < north
            && (-180.0..=180.0).contains(&west)
            && (-180.0..=180.0).contains(&east)
            && (-90.0..=90.0).contains(&south)
            && (-90.0..=90.0).contains(&north);
        if !ok {
            return Err(GridError::BadRegion(s.into()));
        }
        Ok(Self {
            west,
            south,
            east,
            north,
        })
    }

    /// Stable cache key. Three decimals is ~100 m, finer than the 1 km grid.
    pub fn key(&self) -> String {
        format!(
            "{:.3},{:.3},{:.3},{:.3}",
            self.west, self.south, self.east, self.north
        )
    }

    /// The box around some points, `margin` degrees beyond them on each side,
    /// with edges on whole tenths so nearby requests share one cache key.
    pub fn around(points: &[(f64, f64)], margin: f64) -> Option<Self> {
        let (first, rest) = points.split_first()?;
        let (mut south, mut north, mut west, mut east) = (first.0, first.0, first.1, first.1);
        for &(lat, lon) in rest {
            south = south.min(lat);
            north = north.max(lat);
            west = west.min(lon);
            east = east.max(lon);
        }
        Some(Self {
            west: ((west - margin) * 10.0).floor() / 10.0,
            south: ((south - margin) * 10.0).floor() / 10.0,
            east: ((east + margin) * 10.0).ceil() / 10.0,
            north: ((north + margin) * 10.0).ceil() / 10.0,
        })
    }

    pub fn contains(&self, lat: f64, lon: f64) -> bool {
        lat >= self.south && lat <= self.north && lon >= self.west && lon <= self.east
    }

    /// True when all of `other` is inside this region.
    pub fn covers(&self, other: &Region) -> bool {
        other.west >= self.west
            && other.east <= self.east
            && other.south >= self.south
            && other.north <= self.north
    }

    /// The part of this region inside `other`, if any.
    pub fn intersect(&self, other: &Region) -> Option<Region> {
        let r = Region {
            west: self.west.max(other.west),
            south: self.south.max(other.south),
            east: self.east.min(other.east),
            north: self.north.min(other.north),
        };
        (r.west < r.east && r.south < r.north).then_some(r)
    }

    /// Approximate MRMS cells inside: the grid is 0.01° square.
    pub fn cells(&self) -> usize {
        ((self.east - self.west) * 100.0 * (self.north - self.south) * 100.0).round() as usize
    }

    /// The area to load for a view of the map: the view plus room to pan, with
    /// edges on [`AREA_STEP_DEG`], at least [`AREA_MIN_SPAN_DEG`] across, cut to
    /// [`MRMS_DOMAIN`]. `None` when the view is outside MRMS coverage or wider
    /// than [`MAX_AREA_CELLS`].
    pub fn area_for_view(view: &Region) -> Option<Region> {
        let margin = |span: f64| (span * 0.25).max(0.6);
        let axis = |lo: f64, hi: f64| {
            let start = (lo / AREA_STEP_DEG).floor() * AREA_STEP_DEG;
            let end = ((hi / AREA_STEP_DEG).ceil() * AREA_STEP_DEG).max(start + AREA_MIN_SPAN_DEG);
            (start, end)
        };
        let mx = margin(view.east - view.west);
        let my = margin(view.north - view.south);
        let (west, east) = axis(view.west - mx, view.east + mx);
        let (south, north) = axis(view.south - my, view.north + my);
        let area = Region {
            west,
            south,
            east,
            north,
        }
        .intersect(&MRMS_DOMAIN)?;
        (area.cells() <= MAX_AREA_CELLS).then_some(area)
    }

    /// The area to load for one point: what a town-level view of it would load,
    /// so a point lookup reuses the map's cached grids.
    pub fn area_for_point(lat: f64, lon: f64) -> Option<Region> {
        Self::area_for_view(&Region {
            west: lon - 0.15,
            south: lat - 0.15,
            east: lon + 0.15,
            north: lat + 0.15,
        })
    }

    /// True when this is a region [`Region::area_for_view`] could have produced.
    pub fn is_valid_area(&self) -> bool {
        let snapped = |v: f64, edge: f64| v == edge || (v / AREA_STEP_DEG).fract() == 0.0;
        MRMS_DOMAIN.covers(self)
            && snapped(self.west, MRMS_DOMAIN.west)
            && snapped(self.south, MRMS_DOMAIN.south)
            && snapped(self.east, MRMS_DOMAIN.east)
            && snapped(self.north, MRMS_DOMAIN.north)
            && self.cells() <= MAX_AREA_CELLS
    }
}

/// Where a grid's cells sit. Cell centers, row 0 = northernmost.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GridGeom {
    pub nx: usize,
    pub ny: usize,
    /// Latitude of row 0's cell centers.
    pub lat0: f64,
    /// Longitude of column 0's cell centers.
    pub lon0: f64,
    /// Row spacing, degrees (positive; rows run south).
    pub dlat: f64,
    /// Column spacing, degrees.
    pub dlon: f64,
}

impl GridGeom {
    pub fn len(&self) -> usize {
        self.nx * self.ny
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn lat(&self, row: usize) -> f64 {
        self.lat0 - row as f64 * self.dlat
    }

    pub fn lon(&self, col: usize) -> f64 {
        self.lon0 + col as f64 * self.dlon
    }

    /// The cell whose ~1 km square contains `(lat, lon)`.
    pub fn cell_at(&self, lat: f64, lon: f64) -> Option<(usize, usize)> {
        let col = ((lon - self.lon0) / self.dlon).round();
        let row = ((self.lat0 - lat) / self.dlat).round();
        if col < 0.0 || row < 0.0 || col >= self.nx as f64 || row >= self.ny as f64 {
            return None;
        }
        Some((col as usize, row as usize))
    }

    /// Outer edges of the grid (half a cell beyond the outermost centers):
    /// west, south, east, north. This is what an image overlay is pinned to.
    pub fn edges(&self) -> [f64; 4] {
        [
            self.lon0 - self.dlon / 2.0,
            self.lat(self.ny - 1) - self.dlat / 2.0,
            self.lon(self.nx - 1) + self.dlon / 2.0,
            self.lat0 + self.dlat / 2.0,
        ]
    }

    /// Same cells in the same places, to within float noise.
    pub fn same_lattice(&self, other: &GridGeom) -> bool {
        const EPS: f64 = 1e-7;
        self.nx == other.nx
            && self.ny == other.ny
            && (self.lat0 - other.lat0).abs() < EPS
            && (self.lon0 - other.lon0).abs() < EPS
            && (self.dlat - other.dlat).abs() < EPS
            && (self.dlon - other.dlon).abs() < EPS
    }
}

/// A rainfall field: one total per cell, in tenths of a millimetre.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid {
    pub geom: GridGeom,
    /// Row-major, north row first. [`MISSING`] where there is no value.
    pub tenths: Vec<i32>,
}

/// Summary over the cells of a grid.
#[derive(Debug, Clone, Serialize)]
pub struct GridStats {
    pub max_mm: f64,
    /// Center of the wettest cell, `[lon, lat]`.
    pub max_at: Option<[f64; 2]>,
    pub mean_mm: f64,
    /// Share of cells (with data) at or above 0.01 in — the threshold the NWS
    /// uses to call a day "measurable".
    pub wet_fraction: f64,
    pub missing_cells: usize,
}

impl Grid {
    pub fn zeros(geom: GridGeom) -> Self {
        Self {
            geom,
            tenths: vec![0; geom.len()],
        }
    }

    /// Add `other` cell-by-cell. A missing cell in either stays missing: a
    /// total with an hour silently left out would read as less rain than fell.
    pub fn add(&mut self, other: &Grid) -> Result<(), GridError> {
        if !self.geom.same_lattice(&other.geom) {
            return Err(GridError::Lattice);
        }
        for (a, &b) in self.tenths.iter_mut().zip(&other.tenths) {
            if *a == MISSING || b == MISSING {
                *a = MISSING;
            } else {
                *a += b;
            }
        }
        Ok(())
    }

    /// Value of the cell containing a point, in millimetres.
    pub fn mm_at(&self, lat: f64, lon: f64) -> Option<f64> {
        let (c, r) = self.geom.cell_at(lat, lon)?;
        let v = self.tenths[r * self.geom.nx + c];
        (v != MISSING).then(|| v as f64 / 10.0)
    }

    /// A grid `k` times coarser, for showing a wide area. Each new cell is the
    /// mean of the cells it covers, ignoring missing ones; a partial block at
    /// the south or east edge is dropped.
    pub fn coarsened(&self, k: usize) -> Grid {
        let g = &self.geom;
        if k <= 1 || g.nx < k || g.ny < k {
            return self.clone();
        }
        let half = (k - 1) as f64 / 2.0;
        let geom = GridGeom {
            nx: g.nx / k,
            ny: g.ny / k,
            lat0: g.lat0 - half * g.dlat,
            lon0: g.lon0 + half * g.dlon,
            dlat: g.dlat * k as f64,
            dlon: g.dlon * k as f64,
        };
        let mut tenths = Vec::with_capacity(geom.nx * geom.ny);
        for r in 0..geom.ny {
            for c in 0..geom.nx {
                let (mut sum, mut n) = (0i64, 0i64);
                for rr in r * k..(r + 1) * k {
                    for cc in c * k..(c + 1) * k {
                        let v = self.tenths[rr * g.nx + cc];
                        if v != MISSING {
                            sum += i64::from(v);
                            n += 1;
                        }
                    }
                }
                tenths.push(if n == 0 { MISSING } else { (sum / n) as i32 });
            }
        }
        Grid { geom, tenths }
    }

    /// Inches at two percentiles of the measurable cells, for fitting a color
    /// scale to this grid. `None` when nothing measurable fell.
    pub fn wet_percentiles(&self, low: f64, high: f64) -> Option<(f64, f64)> {
        let mut wet: Vec<i32> = self
            .tenths
            .iter()
            .copied()
            .filter(|&v| v != MISSING && mm_to_in(f64::from(v) / 10.0) >= 0.01)
            .collect();
        if wet.is_empty() {
            return None;
        }
        wet.sort_unstable();
        let at = |p: f64| {
            let i = ((wet.len() - 1) as f64 * p).round() as usize;
            mm_to_in(f64::from(wet[i]) / 10.0)
        };
        Some((at(low), at(high)))
    }

    pub fn stats(&self) -> GridStats {
        let mut max = i32::MIN;
        let mut max_idx = None;
        let mut sum = 0i64;
        let mut n = 0usize;
        let mut wet = 0usize;
        let mut missing = 0usize;
        // 0.01 in = 0.254 mm → 2.54 tenths; anything ≥ 3 tenths is measurable.
        const WET_TENTHS: i32 = 3;
        for (i, &v) in self.tenths.iter().enumerate() {
            if v == MISSING {
                missing += 1;
                continue;
            }
            n += 1;
            sum += v as i64;
            if v >= WET_TENTHS {
                wet += 1;
            }
            if v > max {
                max = v;
                max_idx = Some(i);
            }
        }
        let max_at = max_idx.map(|i| {
            let (r, c) = (i / self.geom.nx, i % self.geom.nx);
            [self.geom.lon(c), self.geom.lat(r)]
        });
        GridStats {
            max_mm: if n == 0 { 0.0 } else { max as f64 / 10.0 },
            max_at,
            mean_mm: if n == 0 {
                0.0
            } else {
                sum as f64 / n as f64 / 10.0
            },
            wet_fraction: if n == 0 { 0.0 } else { wet as f64 / n as f64 },
            missing_cells: missing,
        }
    }

    /// Storage form: little-endian `u16` tenths, `0xFFFF` = missing.
    ///
    /// Only used for single products (≤ 72 h), whose cells never approach
    /// 6.5 m; sums are never stored.
    pub fn to_blob(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.tenths.len() * 2);
        for &v in &self.tenths {
            let w: u16 = if v == MISSING {
                u16::MAX
            } else {
                v.clamp(0, u16::MAX as i32 - 1) as u16
            };
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }

    pub fn from_blob(geom: GridGeom, blob: &[u8]) -> Result<Self, GridError> {
        if blob.len() != geom.len() * 2 {
            return Err(GridError::Blob {
                got: blob.len(),
                want: geom.len() * 2,
            });
        }
        let tenths = blob
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| {
                let w = u16::from_le_bytes(*c);
                if w == u16::MAX {
                    MISSING
                } else {
                    w as i32
                }
            })
            .collect();
        Ok(Self { geom, tenths })
    }
}

pub fn mm_to_in(mm: f64) -> f64 {
    mm / MM_PER_INCH
}

/// A real-sized region for tests.
#[cfg(test)]
pub(crate) const TEST_REGION: Region = Region {
    west: -81.50,
    south: 25.00,
    east: -79.90,
    north: 27.70,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn geom() -> GridGeom {
        GridGeom {
            nx: 3,
            ny: 2,
            lat0: 27.0,
            lon0: -80.1,
            dlat: 0.01,
            dlon: 0.01,
        }
    }

    #[test]
    fn areas_snap_outward_and_are_shared_by_nearby_views() {
        let view = |lat: f64, lon: f64| Region::area_for_point(lat, lon).unwrap();
        assert_eq!(
            view(39.74, -104.99).key(),
            "-108.000,36.000,-100.000,44.000"
        );
        assert_eq!(view(40.01, -105.27), view(39.74, -104.99));
        assert_eq!(view(38.83, -104.82), view(39.74, -104.99));
        assert_eq!(
            view(47.61, -122.33).key(),
            "-124.000,44.000,-116.000,52.000"
        );
        assert!(view(47.61, -122.33).is_valid_area());
    }

    #[test]
    fn areas_stop_at_the_edge_of_mrms_coverage() {
        assert_eq!(Region::area_for_point(21.3, -157.8), None, "Hawaii");
        assert_eq!(Region::area_for_point(51.5, -0.1), None, "London");
        let edge = Region::area_for_point(54.9, -100.0).unwrap();
        assert_eq!(edge.north, 55.0);
        assert!(edge.is_valid_area());
    }

    #[test]
    fn areas_refuse_a_view_too_wide_to_load() {
        let conus = Region {
            west: -125.0,
            south: 25.0,
            east: -67.0,
            north: 49.0,
        };
        assert_eq!(Region::area_for_view(&conus), None);
        assert!(!conus.is_valid_area());
    }

    #[test]
    fn a_hand_made_area_must_be_snapped() {
        let r = Region::parse("-80.3,26.0,-78.0,28.0").unwrap();
        assert!(!r.is_valid_area());
    }

    #[test]
    fn coarsening_averages_blocks_and_keeps_their_centers() {
        let g = Grid {
            geom: GridGeom {
                nx: 4,
                ny: 2,
                lat0: 27.0,
                lon0: -80.0,
                dlat: 0.01,
                dlon: 0.01,
            },
            tenths: vec![10, 30, 100, MISSING, 30, 10, 100, MISSING],
        };
        let c = g.coarsened(2);
        assert_eq!((c.geom.nx, c.geom.ny), (2, 1));
        assert_eq!(c.tenths, vec![20, 100]);
        assert!((c.geom.lat0 - 26.995).abs() < 1e-9);
        assert!((c.geom.lon0 + 79.995).abs() < 1e-9);
        assert!((g.geom.edges()[0] - c.geom.edges()[0]).abs() < 1e-9);
        assert_eq!(g.coarsened(1), g);
    }

    #[test]
    fn wet_percentiles_ignore_dry_and_missing_cells() {
        let g = Grid {
            geom: geom(),
            tenths: vec![MISSING, 0, 100, 200, 300, 400],
        };
        let (lo, hi) = g.wet_percentiles(0.0, 1.0).unwrap();
        assert!((lo - 10.0 / 25.4).abs() < 1e-6);
        assert!((hi - 40.0 / 25.4).abs() < 1e-6);
        let dry = Grid {
            geom: geom(),
            tenths: vec![0; 6],
        };
        assert_eq!(dry.wet_percentiles(0.0, 1.0), None);
    }

    #[test]
    fn around_boxes_points_with_a_margin_on_tenths() {
        let r = Region::around(&[(30.33, -81.66), (30.1, -81.9)], 0.5).unwrap();
        assert_eq!(r.key(), "-82.400,29.600,-81.100,30.900");
        assert!(r.contains(30.33, -81.66));
        assert_eq!(Region::around(&[], 0.5), None);
    }

    #[test]
    fn region_parse_and_key() {
        let r = Region::parse("-81.5, 25.0, -79.9, 27.7").unwrap();
        assert_eq!(r, TEST_REGION);
        assert_eq!(r.key(), "-81.500,25.000,-79.900,27.700");
        assert!(Region::parse("1,2,3").is_err());
        assert!(
            Region::parse("-79,26,-80,27").is_err(),
            "west must be < east"
        );
    }

    #[test]
    fn add_propagates_missing() {
        let mut a = Grid {
            geom: geom(),
            tenths: vec![1, 2, 3, 4, MISSING, 6],
        };
        let b = Grid {
            geom: geom(),
            tenths: vec![10, 10, MISSING, 10, 10, 10],
        };
        a.add(&b).unwrap();
        assert_eq!(a.tenths, vec![11, 12, MISSING, 14, MISSING, 16]);
    }

    #[test]
    fn add_rejects_other_lattice() {
        let mut a = Grid::zeros(geom());
        let mut g = geom();
        g.lon0 += 0.005;
        assert!(a.add(&Grid::zeros(g)).is_err());
    }

    #[test]
    fn cell_lookup_uses_nearest_center() {
        let g = Grid {
            geom: geom(),
            tenths: vec![1, 2, 3, 4, 5, 6],
        };
        // Row 1 (26.99), column 2 (-80.08).
        assert_eq!(g.mm_at(26.992, -80.0805), Some(0.6));
        assert_eq!(g.mm_at(27.004, -80.104), Some(0.1));
        assert_eq!(g.mm_at(30.0, -80.1), None);
    }

    #[test]
    fn edges_are_half_a_cell_out() {
        let e = geom().edges();
        assert!((e[0] - -80.105).abs() < 1e-9);
        assert!((e[1] - 26.985).abs() < 1e-9);
        assert!((e[2] - -80.075).abs() < 1e-9);
        assert!((e[3] - 27.005).abs() < 1e-9);
    }

    #[test]
    fn blob_round_trip() {
        let g = Grid {
            geom: geom(),
            tenths: vec![0, 1, 6553, MISSING, 250, 0],
        };
        let back = Grid::from_blob(geom(), &g.to_blob()).unwrap();
        assert_eq!(back, g);
        assert!(Grid::from_blob(geom(), &[0; 3]).is_err());
    }

    #[test]
    fn stats_ignore_missing() {
        let g = Grid {
            geom: geom(),
            tenths: vec![0, 2, 30, MISSING, 100, 0],
        };
        let s = g.stats();
        assert_eq!(s.max_mm, 10.0);
        assert_eq!(s.missing_cells, 1);
        assert!((s.mean_mm - 2.64).abs() < 1e-9);
        assert!((s.wet_fraction - 0.4).abs() < 1e-9);
        let at = s.max_at.unwrap();
        assert!((at[0] - -80.09).abs() < 1e-9 && (at[1] - 26.99).abs() < 1e-9);
    }
}

//! The color scales, shared by the CLI's PNGs and the web map.
//!
//! **Stepped, in distinct colors.** Each band of rainfall is one flat color, and
//! neighboring bands differ in hue as well as lightness, so a boundary between
//! them is visible at a glance. A smooth one-hue gradient shows order well but
//! blurs neighboring amounts together. Checked with the dataviz validator:
//! worst adjacent ΔE 15.7 for normal vision and 8.4 under color-blindness
//! simulation. Lightness still falls at every step, so "darker means more" holds
//! for every reader.
//!
//! **Two ways to use it.** [`Scale::band`] is the fixed form: a color always
//! means the same amount, so two maps can be compared, but a storm that dropped
//! 8 to 12 inches everywhere renders as one band. [`Scale::band_fitted`] spreads
//! the colors across a range the caller picks, usually what is in view, so the
//! differences within that view show. [`Scale::color`] and
//! [`Scale::color_fitted`] interpolate between the same colors, for a map that
//! wants a continuous look.
//!
//! Below 0.01 in, the NWS threshold for "measurable", nothing is drawn.

use serde::Serialize;

use crate::grid::MM_PER_INCH;

/// Lower edge of each bin, inches. The last bin is open-ended.
pub const BIN_EDGES_IN: [f64; 8] = [0.01, 0.10, 0.25, 0.50, 1.0, 2.0, 4.0, 8.0];

/// One color per bin, light to dark.
pub const BIN_COLORS: [[u8; 3]; 8] = [
    [0xf6, 0xff, 0x84],
    [0x95, 0xeb, 0x05],
    [0x09, 0xd6, 0xa0],
    [0x00, 0xb2, 0xe1],
    [0x00, 0x79, 0xbf],
    [0x4a, 0x2f, 0xec],
    [0x71, 0x02, 0x83],
    [0x66, 0x00, 0x27],
];

/// Bin index for a value in millimetres, or `None` below 0.01 in.
pub fn bin_for_mm(mm: f64) -> Option<usize> {
    let inches = mm / MM_PER_INCH;
    // Compare with a hair of tolerance: 0.254 mm is exactly 0.01 in.
    BIN_EDGES_IN.iter().rposition(|&edge| inches + 1e-9 >= edge)
}

pub fn color_for_mm(mm: f64) -> Option<[u8; 3]> {
    bin_for_mm(mm).map(|i| BIN_COLORS[i])
}

pub fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// A sequential color scale: a color at each stop, interpolated between them.
#[derive(Debug, Clone, Copy)]
pub struct Scale {
    /// Stop positions in the scale's unit, ascending.
    pub stops: &'static [f64],
    pub colors: &'static [[u8; 3]],
    /// Unit of `stops`, for a legend.
    pub unit: &'static str,
}

/// Rainfall totals in inches: one stop per bin.
pub const RAIN: Scale = Scale {
    stops: &BIN_EDGES_IN,
    colors: &BIN_COLORS,
    unit: "in",
};

/// Rain rate in inches per hour, in the green-yellow-red order every radar map
/// uses, so it reads at once and is not mistaken for a total. Validator: worst
/// adjacent ΔE 15.4 for normal vision and 9.3 under color-blindness simulation.
pub const RATE: Scale = Scale {
    stops: &[0.01, 0.05, 0.10, 0.25, 0.50, 1.0, 2.0, 4.0],
    colors: &[
        [0xa8, 0xe6, 0xa1],
        [0x45, 0xc1, 0x4a],
        [0x0b, 0x7d, 0x2c],
        [0xf7, 0xe0, 0x3c],
        [0xf5, 0x9e, 0x0b],
        [0xe5, 0x38, 0x1f],
        [0xc2, 0x18, 0x7a],
        [0x6a, 0x1b, 0x9a],
    ],
    unit: "in/h",
};

impl Scale {
    /// The flat color of the band a value falls in; `None` below the first stop.
    pub fn band(&self, v: f64) -> Option<[u8; 3]> {
        let v = v + 1e-9;
        self.stops
            .iter()
            .rposition(|&s| v >= s)
            .map(|i| self.colors[i])
    }

    /// The flat color for a value, with `lo..=hi` divided evenly among all the
    /// colors. `None` below the first stop, however the scale is stretched: an
    /// unmeasurable amount stays clear.
    pub fn band_fitted(&self, v: f64, lo: f64, hi: f64) -> Option<[u8; 3]> {
        if v + 1e-9 < self.stops[0] {
            return None;
        }
        let n = self.colors.len();
        let t = if hi > lo {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.5
        };
        Some(self.colors[((t * n as f64) as usize).min(n - 1)])
    }

    /// Color for a value, interpolated between stops; `None` below the first.
    pub fn color(&self, v: f64) -> Option<[u8; 3]> {
        // A hair of tolerance: 0.254 mm is exactly 0.01 in.
        let v = v + 1e-9;
        if v < self.stops[0] {
            return None;
        }
        let i = self.stops.iter().rposition(|&s| v >= s).expect("v ≥ first");
        let Some(&next) = self.stops.get(i + 1) else {
            return Some(self.colors[i]);
        };
        let f = (v - self.stops[i]) / (next - self.stops[i]);
        let (a, b) = (self.colors[i], self.colors[i + 1]);
        let mix = |k: usize| (a[k] as f64 + f * (b[k] as f64 - a[k] as f64)).round() as u8;
        Some([mix(0), mix(1), mix(2)])
    }

    /// Color for a value on these colors stretched evenly across `lo..=hi`, in
    /// the scale's unit. `None` below the first stop, however the scale is
    /// stretched: an unmeasurable amount stays clear.
    pub fn color_fitted(&self, v: f64, lo: f64, hi: f64) -> Option<[u8; 3]> {
        if v + 1e-9 < self.stops[0] {
            return None;
        }
        let last = self.colors.len() - 1;
        let t = if hi > lo {
            ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.5
        } * last as f64;
        let i = (t.floor() as usize).min(last);
        let f = t - i as f64;
        let (a, b) = (self.colors[i], self.colors[(i + 1).min(last)]);
        let mix = |k: usize| (a[k] as f64 + f * (b[k] as f64 - a[k] as f64)).round() as u8;
        Some([mix(0), mix(1), mix(2)])
    }

    /// Stops as `[position, "#rrggbb"]`, for a legend gradient.
    pub fn stops_json(&self) -> Vec<(f64, String)> {
        self.stops
            .iter()
            .zip(self.colors)
            .map(|(&s, &c)| (s, hex(c)))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LegendBin {
    pub min_in: f64,
    /// `None` for the open-ended top bin.
    pub max_in: Option<f64>,
    pub color: String,
    pub label: String,
}

pub fn legend() -> Vec<LegendBin> {
    let fmt = |v: f64| {
        if v < 1.0 {
            format!("{v:.2}").trim_start_matches('0').to_string()
        } else {
            format!("{v:.0}")
        }
    };
    (0..BIN_EDGES_IN.len())
        .map(|i| {
            let min = BIN_EDGES_IN[i];
            let max = BIN_EDGES_IN.get(i + 1).copied();
            let label = match max {
                Some(m) => format!("{}–{}\"", fmt(min), fmt(m)),
                None => format!("{}\"+", fmt(min)),
            };
            LegendBin {
                min_in: min,
                max_in: max,
                color: hex(BIN_COLORS[i]),
                label,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins() {
        assert_eq!(bin_for_mm(0.0), None);
        assert_eq!(bin_for_mm(0.25), None, "just under 0.01 in");
        assert_eq!(bin_for_mm(0.254), Some(0), "0.01 in exactly");
        assert_eq!(bin_for_mm(25.4), Some(4), "1 in");
        assert_eq!(bin_for_mm(100.0), Some(5), "3.9 in, in the 2 to 4 in bin");
        assert_eq!(bin_for_mm(500.0), Some(7), "open-ended top");
    }

    #[test]
    fn continuous_scale_hits_its_stops_and_separates_between_them() {
        assert_eq!(RAIN.color(0.005), None, "below measurable stays clear");
        assert_eq!(RAIN.color(0.01), Some(BIN_COLORS[0]));
        assert_eq!(RAIN.color(4.0), Some(BIN_COLORS[6]));
        assert_eq!(RAIN.color(99.0), Some(BIN_COLORS[7]));
        // The bins draw 4.2" and 5.8" in one color; the scale must not.
        assert_ne!(RAIN.color(4.2), RAIN.color(5.8));
        assert_ne!(RAIN.color(4.2), RAIN.color(4.3));
    }

    #[test]
    fn fitted_scale_spreads_a_narrow_range_across_every_color() {
        let (lo, hi) = (8.0, 12.0);
        assert_eq!(RAIN.color_fitted(8.0, lo, hi), Some(BIN_COLORS[0]));
        assert_eq!(RAIN.color_fitted(12.0, lo, hi), Some(BIN_COLORS[7]));
        assert_ne!(
            RAIN.color_fitted(9.0, lo, hi),
            RAIN.color_fitted(11.0, lo, hi)
        );
        // The fixed scale draws that same range nearly one color.
        assert_eq!(
            RAIN.color_fitted(0.001, lo, hi),
            None,
            "below measurable stays clear"
        );
        assert_eq!(RAIN.color_fitted(99.0, lo, hi), Some(BIN_COLORS[7]));
    }

    #[test]
    fn neighboring_rate_bands_are_visibly_different() {
        for pair in RATE.colors.windows(2) {
            let d: i32 = (0..3)
                .map(|k| (i32::from(pair[0][k]) - i32::from(pair[1][k])).abs())
                .sum();
            assert!(d > 60, "{pair:?} are too close");
        }
    }

    #[test]
    fn a_band_is_flat_and_a_fitted_band_covers_every_color() {
        assert_eq!(RAIN.band(1.1), RAIN.band(1.9));
        assert_ne!(RAIN.band(1.9), RAIN.band(2.1));
        assert_eq!(RAIN.band(0.001), None);
        assert_eq!(RAIN.band_fitted(8.0, 8.0, 12.0), Some(BIN_COLORS[0]));
        assert_eq!(RAIN.band_fitted(12.0, 8.0, 12.0), Some(BIN_COLORS[7]));
        assert_eq!(RAIN.band_fitted(0.001, 8.0, 12.0), None);
    }

    #[test]
    fn legend_labels() {
        let l = legend();
        assert_eq!(l.len(), 8);
        assert_eq!(l[0].label, ".01–.10\"");
        assert_eq!(l[4].label, "1–2\"");
        assert_eq!(l[7].label, "8\"+");
        assert_eq!(l[0].color, "#f6ff84");
    }

    #[test]
    fn lightness_falls_monotonically() {
        // Relative luminance as a cheap proxy.
        let lum = |c: [u8; 3]| {
            let f = |v: u8| {
                let s = v as f64 / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2])
        };
        for w in BIN_COLORS.windows(2) {
            assert!(lum(w[0]) > lum(w[1]), "{:?} → {:?}", w[0], w[1]);
        }
    }
}

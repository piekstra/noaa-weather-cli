//! Rainfall grid → colored PNG.
//!
//! Three looks, chosen by [`Style`]:
//!
//! * **Smooth** — the ~1 km MRMS cells are interpolated *in value space*
//!   (bilinear between cell centers) and colored on the continuous scale, the
//!   way broadcast graphics do it, with a pixel colored by the interpolated
//!   amount and never by blending two colors.
//! * **Cells** — each MRMS cell is one flat square, its own value, nothing
//!   invented between them. This is what the data actually resolves.
//! * **Banded** — smooth, but on the flat NWS bins.
//!
//! Two row layouts:
//!
//! * [`Rows::Geographic`] — rows evenly spaced in latitude. Right for a
//!   standalone image with a world file.
//! * [`Rows::WebMercator`] — rows evenly spaced in Web Mercator *y*. A map
//!   library pins an image overlay by its corners and stretches it linearly in
//!   Mercator space; feeding it equal-latitude rows would misplace the middle
//!   of a 1.1°-tall region by ~150 m. This layout makes the stretch exact.

use anyhow::Result;

use crate::grid::{Grid, MISSING, MM_PER_INCH};
use crate::ramp;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rows {
    Geographic,
    WebMercator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Smooth,
    Cells,
    Banded,
}

#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    /// Output pixels per grid cell along each axis.
    pub scale: usize,
    pub rows: Rows,
    pub style: Style,
    /// Colors, and the unit grid values are converted to before lookup.
    pub ramp: ramp::Scale,
    /// Stretch the colors across this range, in the ramp's unit, instead of
    /// using its fixed stops.
    pub fit: Option<(f64, f64)>,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            scale: 4,
            rows: Rows::WebMercator,
            style: Style::Banded,
            ramp: ramp::RAIN,
            fit: None,
        }
    }
}

pub struct Rendered {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Outer edges the image is pinned to: west, south, east, north.
    pub bounds: [f64; 4],
    pub rows: Rows,
}

fn merc_y(lat: f64) -> f64 {
    let r = lat.to_radians();
    (std::f64::consts::FRAC_PI_4 + r / 2.0).tan().ln()
}

fn inv_merc_y(y: f64) -> f64 {
    (2.0 * y.exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees()
}

/// Bilinear value (mm) at fractional grid coordinates, skipping missing
/// neighbors. `None` when all four are missing.
fn sample(grid: &Grid, col: f64, row: f64) -> Option<f64> {
    let g = &grid.geom;
    let c = col.clamp(0.0, (g.nx - 1) as f64);
    let r = row.clamp(0.0, (g.ny - 1) as f64);
    let (c0, r0) = (c.floor() as usize, r.floor() as usize);
    let (c1, r1) = ((c0 + 1).min(g.nx - 1), (r0 + 1).min(g.ny - 1));
    let (fx, fy) = (c - c0 as f64, r - r0 as f64);
    let mut acc = 0.0;
    let mut wsum = 0.0;
    for (cc, rr, w) in [
        (c0, r0, (1.0 - fx) * (1.0 - fy)),
        (c1, r0, fx * (1.0 - fy)),
        (c0, r1, (1.0 - fx) * fy),
        (c1, r1, fx * fy),
    ] {
        let v = grid.tenths[rr * g.nx + cc];
        if v != MISSING && w > 0.0 {
            acc += w * v as f64 / 10.0;
            wsum += w;
        }
    }
    (wsum > 0.0).then(|| acc / wsum)
}

/// Value (mm) of the cell containing fractional grid coordinates.
fn nearest(grid: &Grid, col: f64, row: f64) -> Option<f64> {
    let g = &grid.geom;
    let (c, r) = (col.round(), row.round());
    if c < 0.0 || r < 0.0 || c >= g.nx as f64 || r >= g.ny as f64 {
        return None;
    }
    let v = grid.tenths[r as usize * g.nx + c as usize];
    (v != MISSING).then_some(v as f64 / 10.0)
}

pub fn render_png(grid: &Grid, opts: RenderOpts) -> Result<Rendered> {
    let g = &grid.geom;
    anyhow::ensure!(!g.is_empty(), "empty grid");
    let scale = opts.scale.clamp(1, 32);
    let (w, h) = (g.nx * scale, g.ny * scale);
    let [west, south, east, north] = g.edges();

    let (y_n, y_s) = (merc_y(north), merc_y(south));
    let mut rgba = vec![0u8; w * h * 4];
    for py in 0..h {
        let f = (py as f64 + 0.5) / h as f64;
        let lat = match opts.rows {
            Rows::Geographic => north - f * (north - south),
            Rows::WebMercator => inv_merc_y(y_n - f * (y_n - y_s)),
        };
        let row = (g.lat0 - lat) / g.dlat;
        for px in 0..w {
            let lon = west + (px as f64 + 0.5) / w as f64 * (east - west);
            let col = (lon - g.lon0) / g.dlon;
            let value = match opts.style {
                Style::Cells => nearest(grid, col, row),
                Style::Smooth | Style::Banded => sample(grid, col, row),
            };
            let color = value.and_then(|mm| match opts.style {
                Style::Banded | Style::Cells => match opts.fit {
                    Some((lo, hi)) => opts.ramp.band_fitted(mm / MM_PER_INCH, lo, hi),
                    None => opts.ramp.band(mm / MM_PER_INCH),
                },
                Style::Smooth => match opts.fit {
                    Some((lo, hi)) => opts.ramp.color_fitted(mm / MM_PER_INCH, lo, hi),
                    None => opts.ramp.color(mm / MM_PER_INCH),
                },
            });
            if let Some(c) = color {
                let o = (py * w + px) * 4;
                rgba[o..o + 3].copy_from_slice(&c);
                rgba[o + 3] = 255;
            }
        }
    }

    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Default);
        let mut wr = enc.write_header()?;
        wr.write_image_data(&rgba)?;
    }
    Ok(Rendered {
        png: png_bytes,
        width: w as u32,
        height: h as u32,
        bounds: [west, south, east, north],
        rows: opts.rows,
    })
}

/// An ESRI world file (`.pgw`) for a [`Rows::Geographic`] render, so GIS tools
/// place the PNG without a sidecar explanation.
pub fn world_file(r: &Rendered) -> Option<String> {
    if r.rows != Rows::Geographic {
        return None;
    }
    let [w, s, e, n] = r.bounds;
    let dx = (e - w) / r.width as f64;
    let dy = (n - s) / r.height as f64;
    Some(format!(
        "{dx:.10}\n0.0\n0.0\n{:.10}\n{:.10}\n{:.10}\n",
        -dy,
        w + dx / 2.0,
        n - dy / 2.0
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::GridGeom;

    fn grid(tenths: Vec<i32>) -> Grid {
        Grid {
            geom: GridGeom {
                nx: 2,
                ny: 2,
                lat0: 27.0,
                lon0: -80.1,
                dlat: 0.01,
                dlon: 0.01,
            },
            tenths,
        }
    }

    #[test]
    fn cells_are_flat_and_smooth_is_not() {
        let g = Grid {
            geom: GridGeom {
                nx: 2,
                ny: 1,
                lat0: 27.0,
                lon0: -80.0,
                dlat: 0.01,
                dlon: 0.01,
            },
            tenths: vec![254, 1524],
        };
        let distinct = |style| {
            let r = render_png(
                &g,
                RenderOpts {
                    scale: 16,
                    rows: Rows::Geographic,
                    style,
                    ..RenderOpts::default()
                },
            )
            .unwrap();
            let decoder = png::Decoder::new(&r.png[..]);
            let mut reader = decoder.read_info().unwrap();
            let mut buf = vec![0; reader.output_buffer_size()];
            reader.next_frame(&mut buf).unwrap();
            let mut colors: Vec<&[u8]> = buf.chunks(4).collect();
            colors.sort();
            colors.dedup();
            colors.len()
        };
        assert_eq!(distinct(Style::Cells), 2, "one color per cell");
        assert!(distinct(Style::Smooth) > 6, "a gradient between the cells");
    }

    #[test]
    fn mercator_round_trip() {
        for lat in [26.3, 26.93, 27.4] {
            assert!((inv_merc_y(merc_y(lat)) - lat).abs() < 1e-9);
        }
    }

    #[test]
    fn bilinear_interpolates_amounts_and_skips_missing() {
        let g = grid(vec![0, 100, 0, 100]);
        assert_eq!(sample(&g, 0.5, 0.5), Some(5.0));
        let g = grid(vec![MISSING, 100, MISSING, 100]);
        assert_eq!(sample(&g, 0.5, 0.5), Some(10.0));
        let g = grid(vec![MISSING; 4]);
        assert_eq!(sample(&g, 0.5, 0.5), None);
    }

    #[test]
    fn renders_a_decodable_png() {
        let g = grid(vec![0, 300, 30, 1000]);
        let r = render_png(
            &g,
            RenderOpts {
                scale: 4,
                rows: Rows::Geographic,
                style: Style::Banded,
                ..RenderOpts::default()
            },
        )
        .unwrap();
        assert_eq!((r.width, r.height), (8, 8));
        let dec = png::Decoder::new(std::io::Cursor::new(&r.png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!(info.color_type, png::ColorType::Rgba);
        // Top-left pixel is near the 0 mm cell: transparent.
        assert_eq!(buf[3], 0);
        // Bottom-right is near 100 mm (3.9 in): opaque, in the 3–4 in bin.
        let o = (7 * 8 + 7) * 4;
        let c = ramp::BIN_COLORS[5];
        assert_eq!(&buf[o..o + 4], &[c[0], c[1], c[2], 255]);
        assert!(world_file(&r).unwrap().starts_with("0.0025000000\n"));
    }
}

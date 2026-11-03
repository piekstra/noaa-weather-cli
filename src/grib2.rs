//! A deliberately small GRIB2 reader: exactly what NOAA's MRMS products use.
//!
//! MRMS publishes every product as a single-message GRIB2 file on a regular
//! 0.01° latitude/longitude grid (grid template 3.0) with values packed as a
//! PNG image (data representation template 5.41). That is the whole surface
//! this module supports. Anything else — other grid projections, other packing
//! schemes, a bitmap section — is rejected with an error rather than decoded
//! approximately, because a silently mis-read grid would draw rain in the wrong
//! place.
//!
//! Decoding streams the PNG row by row and keeps only the rows and columns
//! inside the requested window, so cropping a small area out of the
//! 7000 × 3500 CONUS grid never holds the full ~49 MB raster in memory.
//!
//! Layout references: WMO Manual on Codes, FM 92 GRIB edition 2 — sections 0–8,
//! grid template 3.0, product template 4.0, data representation template 5.41.

use std::io::Cursor;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GribError {
    #[error("not a GRIB file")]
    NotGrib,
    #[error("unsupported GRIB edition {0} (only edition 2)")]
    Edition(u8),
    #[error("truncated GRIB message")]
    Truncated,
    #[error("section {0} missing")]
    MissingSection(u8),
    #[error("unsupported grid template 3.{0} (only 3.0, regular lat/lon)")]
    GridTemplate(u16),
    #[error("unsupported data representation template 5.{0} (only 5.41, PNG)")]
    DataTemplate(u16),
    #[error("bitmap sections are not supported (indicator {0})")]
    Bitmap(u8),
    #[error("unsupported scanning mode {0:#010b}")]
    ScanMode(u8),
    #[error("PNG payload: {0}")]
    Png(String),
    #[error("PNG is {png_w}×{png_h} but the grid is {ni}×{nj}")]
    Shape {
        png_w: u32,
        png_h: u32,
        ni: u32,
        nj: u32,
    },
}

/// Grid template 3.0: a regular latitude/longitude grid.
#[derive(Debug, Clone, PartialEq)]
pub struct LatLonGrid {
    /// Points along a parallel (columns).
    pub ni: u32,
    /// Points along a meridian (rows).
    pub nj: u32,
    /// Latitude of the first grid point, degrees.
    pub la1: f64,
    /// Longitude of the first grid point, degrees, normalized to −180..180.
    pub lo1: f64,
    /// Column spacing, degrees.
    pub di: f64,
    /// Row spacing, degrees.
    pub dj: f64,
    /// True when rows run south → north (scan-mode bit 0x40); MRMS runs north → south.
    pub rows_northward: bool,
}

impl LatLonGrid {
    /// Latitude of grid row `j`, in file order.
    pub fn lat(&self, j: u32) -> f64 {
        if self.rows_northward {
            self.la1 + j as f64 * self.dj
        } else {
            self.la1 - j as f64 * self.dj
        }
    }

    /// Longitude of grid column `i`.
    pub fn lon(&self, i: u32) -> f64 {
        self.lo1 + i as f64 * self.di
    }

    /// The inclusive index window of grid points inside a box.
    ///
    /// Returns `(i0, i1, j0, j1)` in file order, or `None` when the box misses
    /// the grid entirely. A point exactly on the box edge is inside.
    pub fn window(&self, west: f64, south: f64, east: f64, north: f64) -> Option<IndexWindow> {
        // A small tolerance so a box edge that lands on a grid point (to within
        // the 1e-6° the file can express) keeps that point.
        const EPS: f64 = 1e-6;
        let i0 = ((west - self.lo1) / self.di - EPS).ceil().max(0.0);
        let i1 = ((east - self.lo1) / self.di + EPS)
            .floor()
            .min(self.ni as f64 - 1.0);
        let (j0, j1) = if self.rows_northward {
            (
                ((south - self.la1) / self.dj - EPS).ceil(),
                ((north - self.la1) / self.dj + EPS).floor(),
            )
        } else {
            (
                ((self.la1 - north) / self.dj - EPS).ceil(),
                ((self.la1 - south) / self.dj + EPS).floor(),
            )
        };
        let j0 = j0.max(0.0);
        let j1 = j1.min(self.nj as f64 - 1.0);
        if i0 > i1 || j0 > j1 {
            return None;
        }
        Some(IndexWindow {
            i0: i0 as u32,
            i1: i1 as u32,
            j0: j0 as u32,
            j1: j1 as u32,
        })
    }
}

/// An inclusive range of columns and rows, in file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexWindow {
    pub i0: u32,
    pub i1: u32,
    pub j0: u32,
    pub j1: u32,
}

impl IndexWindow {
    pub fn width(&self) -> usize {
        (self.i1 - self.i0 + 1) as usize
    }
    pub fn height(&self) -> usize {
        (self.j1 - self.j0 + 1) as usize
    }
}

/// Section 5 scaling: `value = (R + X·2^E) / 10^D`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Packing {
    pub reference: f32,
    pub binary_scale: i16,
    pub decimal_scale: i16,
    pub bits: u8,
}

impl Packing {
    pub fn unpack(&self, x: u32) -> f64 {
        let r = self.reference as f64;
        let e = 2f64.powi(self.binary_scale as i32);
        let d = 10f64.powi(self.decimal_scale as i32);
        (r + x as f64 * e) / d
    }
}

/// UTC reference time from section 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// One decoded GRIB2 message header plus a borrowed view of its packed data.
#[derive(Debug, Clone)]
pub struct Message<'a> {
    pub discipline: u8,
    pub center: u16,
    pub ref_time: RefTime,
    pub category: u8,
    pub parameter: u8,
    pub grid: LatLonGrid,
    pub packing: Packing,
    data: &'a [u8],
}

fn be_u16(b: &[u8], at: usize) -> Result<u16, GribError> {
    b.get(at..at + 2)
        .map(|s| u16::from_be_bytes([s[0], s[1]]))
        .ok_or(GribError::Truncated)
}

fn be_u32(b: &[u8], at: usize) -> Result<u32, GribError> {
    b.get(at..at + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or(GribError::Truncated)
}

fn byte(b: &[u8], at: usize) -> Result<u8, GribError> {
    b.get(at).copied().ok_or(GribError::Truncated)
}

/// GRIB2 signed integers are sign-magnitude (high bit = sign), not two's
/// complement. Reading them as `i32::from_be_bytes` gets every negative wrong.
fn sm_i32(b: &[u8], at: usize) -> Result<i32, GribError> {
    let v = be_u32(b, at)?;
    let mag = (v & 0x7fff_ffff) as i32;
    Ok(if v & 0x8000_0000 != 0 { -mag } else { mag })
}

fn sm_i16(b: &[u8], at: usize) -> Result<i16, GribError> {
    let v = be_u16(b, at)?;
    let mag = (v & 0x7fff) as i16;
    Ok(if v & 0x8000 != 0 { -mag } else { mag })
}

/// Parse the first message in `bytes`. MRMS files hold exactly one.
pub fn parse(bytes: &[u8]) -> Result<Message<'_>, GribError> {
    if bytes.len() < 16 || &bytes[0..4] != b"GRIB" {
        return Err(GribError::NotGrib);
    }
    let discipline = bytes[6];
    if bytes[7] != 2 {
        return Err(GribError::Edition(bytes[7]));
    }
    let total = u64::from_be_bytes(bytes[8..16].try_into().expect("8 bytes")) as usize;
    let msg = bytes.get(..total).ok_or(GribError::Truncated)?;

    let mut s1 = None;
    let mut s3 = None;
    let mut s4 = None;
    let mut s5 = None;
    let mut s6 = None;
    let mut s7 = None;

    let mut pos = 16;
    while pos + 4 <= msg.len() {
        if &msg[pos..pos + 4] == b"7777" {
            break;
        }
        let len = be_u32(msg, pos)? as usize;
        let num = byte(msg, pos + 4)?;
        if len < 5 {
            return Err(GribError::Truncated);
        }
        let sec = msg.get(pos..pos + len).ok_or(GribError::Truncated)?;
        match num {
            1 => s1 = Some(sec),
            3 => s3 = Some(sec),
            4 => s4 = Some(sec),
            5 => s5 = Some(sec),
            6 => s6 = Some(sec),
            7 => s7 = Some(sec),
            _ => {}
        }
        pos += len;
    }

    let s1 = s1.ok_or(GribError::MissingSection(1))?;
    let s3 = s3.ok_or(GribError::MissingSection(3))?;
    let s4 = s4.ok_or(GribError::MissingSection(4))?;
    let s5 = s5.ok_or(GribError::MissingSection(5))?;
    let s7 = s7.ok_or(GribError::MissingSection(7))?;

    // Section 1 — identification.
    let center = be_u16(s1, 5)?;
    let ref_time = RefTime {
        year: be_u16(s1, 12)?,
        month: byte(s1, 14)?,
        day: byte(s1, 15)?,
        hour: byte(s1, 16)?,
        minute: byte(s1, 17)?,
        second: byte(s1, 18)?,
    };

    // Section 3 — grid definition.
    let template3 = be_u16(s3, 12)?;
    if template3 != 0 {
        return Err(GribError::GridTemplate(template3));
    }
    let ni = be_u32(s3, 30)?;
    let nj = be_u32(s3, 34)?;
    let basic = be_u32(s3, 38)?;
    let subdiv = be_u32(s3, 42)?;
    // Angles are in units of basic/subdiv degrees; 0 or all-ones means the
    // default of 1e-6°.
    let unit = if basic == 0 || basic == u32::MAX || subdiv == 0 || subdiv == u32::MAX {
        1e-6
    } else {
        basic as f64 / subdiv as f64
    };
    let la1 = sm_i32(s3, 46)? as f64 * unit;
    let lo1_raw = be_u32(s3, 50)? as f64 * unit;
    let di = be_u32(s3, 63)? as f64 * unit;
    let dj = be_u32(s3, 67)? as f64 * unit;
    let scan = byte(s3, 71)?;
    // Supported: bit 0x80 clear (i runs east), 0x20 clear (rows are
    // consecutive), 0x10 clear (no boustrophedon). Bit 0x40 picks row direction.
    if scan & !0x40 != 0 {
        return Err(GribError::ScanMode(scan));
    }
    let lo1 = if lo1_raw > 180.0 {
        lo1_raw - 360.0
    } else {
        lo1_raw
    };
    let grid = LatLonGrid {
        ni,
        nj,
        la1,
        lo1,
        di,
        dj,
        rows_northward: scan & 0x40 != 0,
    };

    // Section 4 — product definition. Only category/parameter are read; they
    // are logged for provenance, not interpreted.
    let category = byte(s4, 9)?;
    let parameter = byte(s4, 10)?;

    // Section 5 — data representation.
    let template5 = be_u16(s5, 9)?;
    if template5 != 41 {
        return Err(GribError::DataTemplate(template5));
    }
    let packing = Packing {
        reference: f32::from_bits(be_u32(s5, 11)?),
        binary_scale: sm_i16(s5, 15)?,
        decimal_scale: sm_i16(s5, 17)?,
        bits: byte(s5, 19)?,
    };

    // Section 6 — 255 means "no bitmap": every grid point has a value.
    if let Some(s6) = s6 {
        let ind = byte(s6, 5)?;
        if ind != 255 {
            return Err(GribError::Bitmap(ind));
        }
    }

    Ok(Message {
        discipline,
        center,
        ref_time,
        category,
        parameter,
        grid,
        packing,
        data: &s7[5..],
    })
}

impl Message<'_> {
    /// Decode the values inside `win`, returned **north → south** row-major
    /// regardless of the file's scan direction.
    ///
    /// Each value is passed through `map` as it is unpacked, so callers can
    /// apply product semantics (MRMS marks "no coverage" with negatives) without
    /// a second pass.
    pub fn decode_window<T: Copy>(
        &self,
        win: IndexWindow,
        mut map: impl FnMut(f64) -> T,
    ) -> Result<Vec<T>, GribError> {
        let png_err = |e: png::DecodingError| GribError::Png(e.to_string());

        let mut decoder = png::Decoder::new_with_limits(
            Cursor::new(self.data),
            // One row at a time is all we hold, but the decoder still checks
            // the declared image size against this.
            png::Limits {
                bytes: 512 * 1024 * 1024,
            },
        );
        decoder.set_transformations(png::Transformations::IDENTITY);
        let mut reader = decoder.read_info().map_err(png_err)?;

        let info = reader.info();
        if info.width != self.grid.ni || info.height != self.grid.nj {
            return Err(GribError::Shape {
                png_w: info.width,
                png_h: info.height,
                ni: self.grid.ni,
                nj: self.grid.nj,
            });
        }
        if info.interlaced {
            return Err(GribError::Png("interlaced PNG payload".into()));
        }
        let samples = info.color_type.samples();
        let depth_bits = match info.bit_depth {
            png::BitDepth::Eight => 8,
            png::BitDepth::Sixteen => 16,
            other => return Err(GribError::Png(format!("unsupported bit depth {other:?}"))),
        };
        let bytes_per_px = samples * depth_bits / 8;
        if bytes_per_px * 8 != self.packing.bits as usize || bytes_per_px > 4 {
            return Err(GribError::Png(format!(
                "{} bits per pixel in PNG but section 5 declares {}",
                bytes_per_px * 8,
                self.packing.bits
            )));
        }

        let (w, h) = (win.width(), win.height());
        let mut out: Vec<Option<T>> = vec![None; w * h];

        // Rows past the window are never decoded — for MRMS (north → south)
        // and a small window that skips the bottom fifth of the file.
        let mut row_idx: u32 = 0;
        while row_idx <= win.j1 {
            let Some(row) = reader.next_row().map_err(png_err)? else {
                return Err(GribError::Truncated);
            };
            if row_idx >= win.j0 {
                let data = row.data();
                let out_row = if self.grid.rows_northward {
                    // File row j0 is the window's southernmost row.
                    (win.j1 - row_idx) as usize
                } else {
                    (row_idx - win.j0) as usize
                };
                for (k, i) in (win.i0..=win.i1).enumerate() {
                    let at = i as usize * bytes_per_px;
                    let px = data
                        .get(at..at + bytes_per_px)
                        .ok_or(GribError::Truncated)?;
                    let x = px.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32);
                    out[out_row * w + k] = Some(map(self.packing.unpack(x)));
                }
            }
            row_idx += 1;
        }

        out.into_iter()
            .map(|v| v.ok_or(GribError::Truncated))
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a minimal single-message GRIB2 file with a PNG payload.
    ///
    /// Used to exercise the parser without shipping a multi-megabyte MRMS
    /// sample in the repository.
    // A fixture builder mirroring the GRIB2 fields one-to-one reads better
    // than a parameter struct used exactly here.
    #[allow(clippy::too_many_arguments)]
    pub fn synth_grib(
        ni: u32,
        nj: u32,
        la1_micro: i32,
        lo1_micro: u32,
        step_micro: u32,
        scan: u8,
        packing: Packing,
        raw: &[u16],
        ref_time: RefTime,
    ) -> Vec<u8> {
        assert_eq!(raw.len(), (ni * nj) as usize);
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, ni, nj);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Sixteen);
            let mut w = enc.write_header().unwrap();
            let data: Vec<u8> = raw.iter().flat_map(|v| v.to_be_bytes()).collect();
            w.write_image_data(&data).unwrap();
        }

        let sm32 = |v: i32| -> [u8; 4] {
            let mag = v.unsigned_abs() & 0x7fff_ffff;
            (if v < 0 { mag | 0x8000_0000 } else { mag }).to_be_bytes()
        };
        let sm16 = |v: i16| -> [u8; 2] {
            let mag = v.unsigned_abs() & 0x7fff;
            (if v < 0 { mag | 0x8000 } else { mag }).to_be_bytes()
        };

        let mut s1 = [0u8; 21];
        s1[0..4].copy_from_slice(&21u32.to_be_bytes());
        s1[4] = 1;
        s1[5..7].copy_from_slice(&161u16.to_be_bytes());
        s1[12..14].copy_from_slice(&ref_time.year.to_be_bytes());
        s1[14] = ref_time.month;
        s1[15] = ref_time.day;
        s1[16] = ref_time.hour;
        s1[17] = ref_time.minute;
        s1[18] = ref_time.second;

        let mut s3 = [0u8; 72];
        s3[0..4].copy_from_slice(&72u32.to_be_bytes());
        s3[4] = 3;
        s3[6..10].copy_from_slice(&(ni * nj).to_be_bytes());
        s3[12..14].copy_from_slice(&0u16.to_be_bytes());
        s3[30..34].copy_from_slice(&ni.to_be_bytes());
        s3[34..38].copy_from_slice(&nj.to_be_bytes());
        s3[38..42].copy_from_slice(&1u32.to_be_bytes());
        s3[42..46].copy_from_slice(&1_000_000u32.to_be_bytes());
        s3[46..50].copy_from_slice(&sm32(la1_micro));
        s3[50..54].copy_from_slice(&lo1_micro.to_be_bytes());
        s3[63..67].copy_from_slice(&step_micro.to_be_bytes());
        s3[67..71].copy_from_slice(&step_micro.to_be_bytes());
        s3[71] = scan;

        let mut s4 = [0u8; 34];
        s4[0..4].copy_from_slice(&34u32.to_be_bytes());
        s4[4] = 4;
        s4[9] = 6;
        s4[10] = 37;

        let mut s5 = [0u8; 21];
        s5[0..4].copy_from_slice(&21u32.to_be_bytes());
        s5[4] = 5;
        s5[5..9].copy_from_slice(&(ni * nj).to_be_bytes());
        s5[9..11].copy_from_slice(&41u16.to_be_bytes());
        s5[11..15].copy_from_slice(&packing.reference.to_bits().to_be_bytes());
        s5[15..17].copy_from_slice(&sm16(packing.binary_scale));
        s5[17..19].copy_from_slice(&sm16(packing.decimal_scale));
        s5[19] = packing.bits;

        let s6 = [0, 0, 0, 6, 6, 255];

        let mut s7 = Vec::new();
        s7.extend_from_slice(&((5 + png_bytes.len()) as u32).to_be_bytes());
        s7.push(7);
        s7.extend_from_slice(&png_bytes);

        let body_len = 16 + s1.len() + s3.len() + s4.len() + s5.len() + s6.len() + s7.len() + 4;
        let mut out = Vec::with_capacity(body_len);
        out.extend_from_slice(b"GRIB");
        out.extend_from_slice(&[0, 0, 209, 2]);
        out.extend_from_slice(&(body_len as u64).to_be_bytes());
        for s in [&s1[..], &s3, &s4, &s5, &s6, &s7] {
            out.extend_from_slice(s);
        }
        out.extend_from_slice(b"7777");
        out
    }

    pub const MRMS_PACKING: Packing = Packing {
        reference: -30.0,
        binary_scale: 0,
        decimal_scale: 1,
        bits: 16,
    };

    pub const T0: RefTime = RefTime {
        year: 2026,
        month: 11,
        day: 1,
        hour: 12,
        minute: 0,
        second: 0,
    };

    #[test]
    fn sign_magnitude_is_not_twos_complement() {
        // -5 in sign-magnitude is 0x8000_0005; as two's complement that is a
        // huge negative number.
        let b = [0x80, 0, 0, 5];
        assert_eq!(sm_i32(&b, 0).unwrap(), -5);
        let b = [0x80, 0x02];
        assert_eq!(sm_i16(&b, 0).unwrap(), -2);
    }

    #[test]
    fn unpack_follows_section5_formula() {
        // MRMS: X=0 → -3.0 (missing), X=20 → -1.0 (no coverage), X=127 → 9.7 mm.
        assert!((MRMS_PACKING.unpack(0) - -3.0).abs() < 1e-9);
        assert!((MRMS_PACKING.unpack(20) - -1.0).abs() < 1e-9);
        assert!((MRMS_PACKING.unpack(127) - 9.7).abs() < 1e-9);
        let p = Packing {
            reference: 1.0,
            binary_scale: -1,
            decimal_scale: -1,
            bits: 16,
        };
        // (1 + 4·0.5) / 0.1 = 30
        assert!((p.unpack(4) - 30.0).abs() < 1e-9);
    }

    /// A 5×4 grid at 0.01°, NW corner at 27.00N, 80.10W, values = 10·row + col
    /// tenths of a millimetre above the -3.0 floor.
    fn sample(scan: u8) -> Vec<u8> {
        let (ni, nj) = (5u32, 4u32);
        let raw: Vec<u16> = (0..nj)
            .flat_map(|j| (0..ni).map(move |i| (30 + 10 * j + i) as u16))
            .collect();
        let la1 = if scan & 0x40 != 0 {
            26_970_000
        } else {
            27_000_000
        };
        synth_grib(
            ni,
            nj,
            la1,
            (360.0e6 - 80.10e6) as u32,
            10_000,
            scan,
            MRMS_PACKING,
            &raw,
            T0,
        )
    }

    #[test]
    fn parses_header() {
        let bytes = sample(0);
        let m = parse(&bytes).unwrap();
        assert_eq!(m.discipline, 209);
        assert_eq!(m.center, 161);
        assert_eq!(m.ref_time, T0);
        assert_eq!((m.category, m.parameter), (6, 37));
        assert_eq!((m.grid.ni, m.grid.nj), (5, 4));
        assert!((m.grid.la1 - 27.0).abs() < 1e-9);
        assert!((m.grid.lo1 - -80.10).abs() < 1e-9);
        assert!((m.grid.di - 0.01).abs() < 1e-12);
        assert!(!m.grid.rows_northward);
        assert_eq!(m.packing, MRMS_PACKING);
    }

    #[test]
    fn crops_a_window_north_to_south() {
        let bytes = sample(0);
        let m = parse(&bytes).unwrap();
        // Columns -80.09..-80.07 (i=1..3), rows 26.99..26.98 (j=1..2).
        let win = m.grid.window(-80.09, 26.98, -80.07, 26.99).unwrap();
        assert_eq!(
            win,
            IndexWindow {
                i0: 1,
                i1: 3,
                j0: 1,
                j1: 2
            }
        );
        let v = m.decode_window(win, |x| (x * 10.0).round() as i32).unwrap();
        // value = (30 + 10j + i - 30)/10 mm → tenths = 10j + i
        assert_eq!(v, vec![11, 12, 13, 21, 22, 23]);
    }

    #[test]
    fn northward_rows_come_back_north_first() {
        let bytes = sample(0x40);
        let m = parse(&bytes).unwrap();
        assert!(m.grid.rows_northward);
        // la1 = 26.97 is the southern row; the full grid spans 26.97..27.00.
        let win = m.grid.window(-80.10, 26.97, -80.06, 27.00).unwrap();
        let v = m.decode_window(win, |x| (x * 10.0).round() as i32).unwrap();
        // First output row is the northernmost file row (j=3).
        assert_eq!(&v[0..5], &[30, 31, 32, 33, 34]);
        assert_eq!(&v[15..20], &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn window_outside_grid_is_none() {
        let bytes = sample(0);
        let m = parse(&bytes).unwrap();
        assert!(m.grid.window(-70.0, 40.0, -69.0, 41.0).is_none());
    }

    #[test]
    fn rejects_other_formats() {
        assert!(matches!(parse(b"nope"), Err(GribError::NotGrib)));
        let mut bytes = sample(0);
        // Corrupt the edition byte.
        bytes[7] = 1;
        assert!(matches!(parse(&bytes), Err(GribError::Edition(1))));
    }
}

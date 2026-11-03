//! Weather from the federal government's own raw data, with no API key.
//!
//! * **MRMS** ([`mrms`]) — NOAA's 1 km merged radar grid: gauge-corrected
//!   hourly rainfall, 15-minute radar-only totals, and the 2-minute rain rate
//!   behind a radar loop. From NOAA's Open Data bucket on AWS.
//! * **GHCN-Daily** ([`ghcn`]) — daily rain-gauge reports, including the
//!   CoCoRaHS volunteer network and NWS cooperative observers, as ground truth.
//! * **NWS** ([`nws`]) — active alerts, forecasts and current conditions.
//!
//! [`engine::Weather`] answers "how much rain fell here over this window?" for
//! any window in the [`window`] grammar (`45m`, `24h`, `3d`, `today`, a date
//! range), caching what it fetches in [`store`].
//!
//! The `wx` binary is a thin layer over this library. Other programs use the
//! library with default features off, which drops the CLI's dependencies.

/// A NOAA or NWS service answered with a failure, rather than with data or "not
/// found". Network errors reach callers as `reqwest::Error` instead.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct UpstreamError(pub String);

pub mod engine;
pub mod ghcn;
pub mod grib2;
pub mod grid;
pub mod mrms;
pub mod nws;
pub mod places;
pub mod ramp;
pub mod render;
pub mod store;
pub mod window;

/// Re-exported so callers use the same `Timestamp` type the API returns.
pub use jiff;

pub use engine::{Accumulation, Availability, Config, HourCounts, RateFrame, Weather};
pub use grid::{Grid, GridGeom, Region, MAX_AREA_CELLS, MRMS_DOMAIN};
pub use mrms::Pass;
pub use places::{Place, UserConfig};
pub use store::Store;
pub use window::{Window, WindowSpec};

# Upstream data: endpoints and traps

All anonymous and free. Each section gives what `wx` calls, what can go wrong, and
what the code does about it.

## MRMS on AWS

```text
https://noaa-mrms-pds.s3.amazonaws.com/CONUS/<product>/<YYYYMMDD>/MRMS_<product>_<YYYYMMDD>-<HHMMSS>.grib2.gz
```

The domain is longitude -130 to -60, latitude 20 to 55, on a 0.01° grid.

| Product directory | Holds | Step |
|---|---|---|
| `MultiSensor_QPE_{01,03,06,12,24,72}H_Pass2_00.00` | Gauge-corrected rainfall with the full gauge network | hourly valid times |
| `MultiSensor_QPE_{...}H_Pass1_00.00` | Gauge-corrected with the gauges reported so far | hourly valid times |
| `RadarOnly_QPE_{...}H_00.00` | Radar only | hourly valid times |
| `RadarOnly_QPE_15M_00.00` | Radar only, 15-minute accumulations | 15 minutes |
| `PrecipRate_00.00` | Instantaneous rate | 2 minutes |

Traps:

- **Nothing gauge-corrected is finer than one hour.** A sub-hour total can only be
  radar-only, so `wx` labels it that way rather than blending the two.
- **Pass 2 multi-hour products are the exact sum of the hourly ones**, verified to
  7e-15 mm over a region. The planner therefore covers a window with aligned 72 h
  and 24 h blocks instead of hundreds of hourly files. This holds for Pass 2. Pass
  1 and radar-only hours at the window's newest edge are loaded hour by hour.
- **Availability must be probed.** `wx` sends HEAD requests back from the current
  hour. S3 answers 403 or 404 for a key that does not exist, and both mean "not
  published". A probe takes no download permit, so it never waits behind a backfill.
- **A failed probe is not "no data".** The last known availability is used for up
  to an hour when the bucket cannot be reached.
- **Grid points sit at x.xx5°.** A region's x.xx0 edges enclose whole cells, and
  latitude runs north to south in the file.
- **Files are large for what a crop needs.** Decoding streams the PNG row by row
  and keeps only the requested window, so the full 7000 × 3500 raster is never held
  in memory.
- **Most cells are zero.** Cached grids are stored deflated, which shrinks them
  about tenfold.

## GRIB2

MRMS publishes one message per file, grid template 3.0, data representation
template 5.41 (PNG). `src/grib2.rs` supports exactly that and rejects anything
else, because a silently misread grid draws rain in the wrong place.

## GHCN-Daily on AWS

```text
https://noaa-ghcn-pds.s3.amazonaws.com/ghcnd-stations.txt
https://noaa-ghcn-pds.s3.amazonaws.com/parquet/by_year/YEAR=<year>/ELEMENT=PRCP/
```

Traps:

- **The archive runs 2 to 3 days behind.** Gauges answer "how did the radar do last
  week", not "how much fell an hour ago".
- **A report covers the 24 hours ending at its observation time**, not a calendar
  day. Only reports whose whole 24 hours falls inside the window count, and the MRMS
  figure beside each gauge is summed over exactly those hours.
- **`csv/by_station/<id>.csv` looks cheaper and is not reliably current.** A station's
  file has been seen ending 20 months before the per-year Parquet for the same
  station.
- **`ghcnd-stations.txt` is fixed width**, and -999.9 means no elevation.

## api.weather.gov

| Path | Used for |
|---|---|
| `/points/{lat},{lon}` | Forecast and station URLs, time zone, state, nearest city |
| `/gridpoints/{office}/{x},{y}/forecast`, `.../forecast/hourly` | Forecasts |
| `/gridpoints/{office}/{x},{y}/stations` | Stations near a point |
| `/stations/{id}/observations/latest` | Current conditions |
| `/alerts/active?point={lat},{lon}`, `?area={code,code}` | Active alerts |
| `/zones/...` | Shapes for alerts that have none of their own |

A User-Agent naming the client is required. `wx` sends the repository's URL and
nothing about the person running it.

Traps:

- **`/points` has three answers away from land.** 404 outside NWS coverage, 500 over
  open ocean, and an ordinary grid over water near shore. `wx` exits 4 for the 404
  and 5 for a 500 that survives its retries.
- **Most alerts have no polygon.** A watch or advisory issued by zone has `geometry:
  null` and a list of `affectedZones`. Only storm-based warnings carry a shape.
  Drawing the others means fetching the zones, which are cached.
- **The nearest station may have nothing current.** Conditions walk the forecast
  grid's station list until one is fresh.
- **Station ids change.** West Palm Beach's airport moved from `KPBI` to `KDJT`.
  Never hard-code one.
- **Observations are SI and forecasts are °F.** Values carry a `unitCode`, and any
  of them may be null.

## Coverage gaps

- Same-day gauges are not read. CoCoRaHS, USGS and ASOS current feeds exist but
  have no client here.
- Not yet verified: how MRMS behaves at the edges of the domain outside the US.

## Update trigger

Add to this file whenever an upstream surprise costs more than a few minutes.

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

- **The archive runs 2 to 3 days behind.** It answers "how did the radar do last week",
  and the feeds below answer "how much fell this morning".
- **A report covers the 24 hours ending at its observation time**, not a calendar
  day. Only reports whose whole 24 hours falls inside the window count, and the MRMS
  figure beside each gauge is summed over exactly those hours.
- **`csv/by_station/<id>.csv` looks cheaper and is not reliably current.** A station's
  file has been seen ending 20 months before the per-year Parquet for the same
  station.
- **`ghcnd-stations.txt` is fixed width**, and -999.9 means no elevation.

## CoCoRaHS export

```text
https://data.cocorahs.org/export/exportreports.aspx?ReportType=Daily&dtf=1&Format=CSV&ReportDateType=reportdate&TimesInGMT=False&State=<XX>&StartDate=<MM/DD/YYYY>&EndDate=<MM/DD/YYYY>
```

Traps:

- **A report covers the 24 hours ending at its observation time**, as it does in
  GHCN-Daily, and is held to the same rule: its whole 24 hours must be inside the
  window. So `24h` rarely holds one, and `2d` does.
- **`StationNumber` can start with a space.**
- **`CO-DN-12` is `US1CODN0012` in GHCN-Daily.** Coordinates agree for numbers with a
  two-letter county wherever the archive holds them, so those reports join the
  archive's station. A county of three letters or a number of another shape has no such
  twin.
- **The archive wins for a date both hold**, because it has been checked.
- **`TotalPrecipAmt` is inches, `T` or `NA`.** It is typed by hand, so an amount above
  the US 24-hour record is treated as a typo.
- **Reports arrive through the morning and can be revised**, so the last two days are
  read again.

## USGS Water Data API

```text
https://api.waterdata.usgs.gov/ogcapi/v1/collections/continuous/items?f=json&skipGeometry=true&bbox=<w,s,e,n>&parameter_code=00045&time=<start>/<end>&limit=10000&properties=monitoring_location_id,time_series_id,time,value
https://api.waterdata.usgs.gov/ogcapi/v1/collections/monitoring-locations/items?f=json&limit=100&properties=monitoring_location_name&id=<id,id,...>
```

Traps:

- **`waterservices.usgs.gov/nwis/iv` looks simpler and is being retired.** USGS has
  scheduled its shutdown for the first quarter of 2027, and it refuses a box over 25
  square degrees.
- **The next link can name another API version** than the one called, so only its
  query is taken.
- **A null `value` is an outage**, with a qualifier such as `EQUIP`, `SEASONAL`,
  `UNAVAIL` or `DISCONTINUED`. Present values are strings.
- **`00045` is the amount in each interval**, not a running total. A site's step can be
  5, 10, 15, 30 or 60 minutes, so an hour counts only with every reading its step calls
  for: an hour with a gap would read as less rain than fell.
- **A site can have two instruments**, as two `time_series_id` values reporting the
  same times. Only the lowest id is used, or the hour counts double.
- **Unqualified values can be impossible**, far above the US record for an hour. Such
  an hour is not counted.
- **A box can hold tens of thousands of readings a day**, which is why hourly feeds are
  read back only 3 days and why requests accept gzip.

## Iowa Environmental Mesonet ASOS

```text
https://mesonet.agron.iastate.edu/geojson/network/<XX>_ASOS.geojson
https://mesonet.agron.iastate.edu/cgi-bin/request/asos.py?network=<XX>_ASOS&data=p01i&data=metar&year1=<y>&month1=<m>&day1=<d>&year2=<y>&month2=<m>&day2=<d>&tz=Etc/UTC&format=onlycomma&latlon=yes&missing=M&trace=T&direct=no&report_type=3
```

Traps:

- **`p01i` reads 0.00 whenever a METAR has no amount group**, including a report that
  says it is raining. Reading that column alone shows working-looking gauges at zero
  under a storm. So `metar` is requested too, and an hour whose report names
  precipitation with no `Pnnnn` group is not counted.
- **`PNO` in the remarks means the gauge is out.** `SLPNO` is a different remark.
- **Some stations never report an amount.** No report of theirs can be told from a dry
  hour, so an airport appears only once it has reported an amount above zero. In a long
  dry spell, airports drop out of the comparison.
- **`p01i` is the rain since the station's last hourly reset**, usually near `:53`. A
  report stands for the hour ending at the nearest `:00`, offset from MRMS's hour by
  several minutes.
- **`day2` is exclusive, and `station=` alone returns nothing.** `network=` can be
  repeated.
- **Airports are matched to GHCN-Daily by distance, not id.** `ncei91` names a nearby
  normals station, a `GHCNH_ID` can point at a station far from the airport, and an
  airport's own id can change. Within 3 km is the same gauge, and the row covering more
  hours is kept.

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

- South Florida Water Management District gauges are not read: the service could not
  be reached when tried.
- An airport whose gauge stops working after it has reported an amount still shows,
  reading low. The figure beside MRMS is what exposes it.
- Not yet verified: how MRMS behaves at the edges of the domain outside the US.
- Not yet verified: the USGS API's rate limits, since its responses name none.
- Not yet verified: whether CoCoRaHS attaches terms to exported reports beyond the
  Creative Commons Attribution license its site carries. The attribution line credits
  it either way.

## Update trigger

Add to this file whenever an upstream surprise costs more than a few minutes.

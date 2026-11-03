# Test fixtures

Payload shapes for `src/nws.rs` and `src/feeds.rs` tests.

| File | Source | Handling |
|---|---|---|
| `nws_point.json` | Shape of `api.weather.gov/points/{lat},{lon}` | Trimmed to the fields read. City, grid and zones are invented. |
| `nws_forecast.json` | Shape of `api.weather.gov/gridpoints/{office}/{x},{y}/forecast` | Trimmed to two periods. Grid polygon is invented. |
| `nws_observation.json` | Shape of `stations/{id}/observations/latest` | Invented values, chosen to make unit conversions checkable. |
| `nws_alerts.json` | Shape of `alerts/active` | Invented alerts: one with its own polygon, one zone-only. |
| `cocorahs_daily.csv` | Shape of the CoCoRaHS daily export | Invented stations and amounts: a normal report, a trace, `NA`, a county of three letters, a number of another shape, and an impossible amount. |
| `usgs_continuous.json` | Shape of `collections/continuous/items` | Invented site and values: one reading, one outage, and a next link. |
| `usgs_locations.json` | Shape of `collections/monitoring-locations/items` | Invented sites: one placed, one with no position. |
| `iem_asos.csv` | Shape of `cgi-bin/request/asos.py` | Invented station, values and METARs: normal, trace, missing, off the hour, and two reports for one hour. |
| `iem_network.geojson` | Shape of `geojson/network/{state}_ASOS.geojson` | Invented station, trimmed to the fields read and a few around them. |

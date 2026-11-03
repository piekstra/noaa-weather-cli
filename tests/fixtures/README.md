# Test fixtures

Payload shapes for `src/nws.rs` tests.

| File | Source | Handling |
|---|---|---|
| `nws_point.json` | Shape of `api.weather.gov/points/{lat},{lon}` | Trimmed to the fields read. City, grid and zones are invented. |
| `nws_forecast.json` | Shape of `api.weather.gov/gridpoints/{office}/{x},{y}/forecast` | Trimmed to two periods. Grid polygon is invented. |
| `nws_observation.json` | Shape of `stations/{id}/observations/latest` | Invented values, chosen to make unit conversions checkable. |
| `nws_alerts.json` | Shape of `alerts/active` | Invented alerts: one with its own polygon, one zone-only. |

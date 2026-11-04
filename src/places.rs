//! Named places and non-secret settings, in one small JSON file.
//!
//! `${XDG_CONFIG_HOME:-~/.config}/wx/config.json`, written by `wx places` and
//! `wx config`. It lives in the library so another program — a map, say — reads
//! the same list the CLI writes, rather than keeping a second one.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserConfig {
    /// Area to cache, `west,south,east,north`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Time zone that "today" and local times refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    /// Cache database location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<PathBuf>,
    /// NWS alert area: a state or marine code such as `FL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_area: Option<String>,
    /// Days to keep a downloaded grid; 0 keeps everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_days: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub places: Vec<Place>,
}

/// `lat,lon`, latitude first. The error says what was wrong with it, for a usage error.
pub fn parse_point(s: &str) -> std::result::Result<(f64, f64), String> {
    let usage = || format!("{s:?}: expected lat,lon or a saved place name");
    let (a, b) = s.split_once(',').ok_or_else(usage)?;
    let lat: f64 = a.trim().parse().map_err(|_| usage())?;
    let lon: f64 = b.trim().parse().map_err(|_| usage())?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return Err(format!(
            "{s:?}: out of range (expected lat,lon, latitude first)"
        ));
    }
    Ok((lat, lon))
}

/// `${XDG_CONFIG_HOME:-~/.config}/wx/config.json`.
///
/// With no home directory this is an error and not a temp directory: saved places cannot
/// be fetched again.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(pk_cli_config::ConfigStore::new("wx").path()?)
}

impl UserConfig {
    /// Days to keep a downloaded grid under this setting, for
    /// [`Config::retain_days`](crate::engine::Config::retain_days): the default when
    /// unset, and `None` for 0, which keeps everything.
    pub fn grid_retention(&self) -> Option<i64> {
        match self.retain_days {
            None => Some(crate::engine::DEFAULT_RETAIN_DAYS),
            Some(days) => (days > 0).then_some(days),
        }
    }

    /// Read the file; a file that does not exist is an empty config.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text + "\n").with_context(|| format!("writing {}", path.display()))
    }

    /// A place by name, ignoring case.
    pub fn place(&self, name: &str) -> Option<&Place> {
        self.places
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
    }

    /// Add a place, or move it when the name already exists.
    pub fn upsert_place(&mut self, place: Place) {
        match self
            .places
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&place.name))
        {
            Some(existing) => *existing = place,
            None => self.places.push(place),
        }
    }

    /// Remove a place by name; whether it was there.
    pub fn remove_place(&mut self, name: &str) -> bool {
        let before = self.places.len();
        self.places
            .retain(|p| !p.name.eq_ignore_ascii_case(name.trim()));
        self.places.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_is_the_default_when_unset_and_off_at_zero() {
        let with = |retain_days| UserConfig {
            retain_days,
            ..UserConfig::default()
        };
        assert_eq!(
            with(None).grid_retention(),
            Some(crate::engine::DEFAULT_RETAIN_DAYS)
        );
        assert_eq!(with(Some(0)).grid_retention(), None);
        assert_eq!(with(Some(30)).grid_retention(), Some(30));
    }

    fn place(name: &str) -> Place {
        Place {
            name: name.into(),
            lat: 26.0,
            lon: -80.0,
        }
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = UserConfig::load(&dir.path().join("nope.json")).unwrap();
        assert!(cfg.places.is_empty());
    }

    #[test]
    fn round_trips_and_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/config.json");
        let mut cfg = UserConfig::default();
        cfg.upsert_place(place("Example Beach"));
        cfg.save(&path).unwrap();
        assert_eq!(UserConfig::load(&path).unwrap().places.len(), 1);
    }

    #[test]
    fn names_ignore_case_and_upsert_replaces() {
        let mut cfg = UserConfig::default();
        cfg.upsert_place(place("Example Beach"));
        cfg.upsert_place(Place {
            lat: 27.0,
            ..place("example beach")
        });
        assert_eq!(cfg.places.len(), 1);
        assert_eq!(cfg.place("EXAMPLE BEACH").unwrap().lat, 27.0);
        assert!(cfg.remove_place("example BEACH"));
        assert!(!cfg.remove_place("example beach"));
    }
}

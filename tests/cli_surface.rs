//! Black-box checks of the command surface. Offline: every case here fails or
//! finishes before any network request.

use std::sync::atomic::{AtomicUsize, Ordering};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// `wx` with its own config and cache directories, so tests cannot read each
/// other's files or the user's.
fn wx() -> (Command, tempfile::TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(&format!("wx-{}-", COUNTER.fetch_add(1, Ordering::Relaxed)))
        .tempdir()
        .unwrap();
    let mut cmd = Command::cargo_bin("wx").unwrap();
    cmd.env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("XDG_CACHE_HOME", dir.path().join("cache"))
        .env_remove("WX_REGION")
        .env_remove("WX_TZ")
        .env_remove("WX_DB")
        .env_remove("WX_CONFIG");
    (cmd, dir)
}

fn json_of(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("one JSON document on stdout")
}

#[test]
fn an_area_outside_mrms_is_not_found() {
    let (mut c, _d) = wx();
    c.args(["rain", "6h", "--at", "0,0"]).assert().code(4);
    let (mut c, _d) = wx();
    c.args(["map", "6h", "--region", "0,0,1,1"])
        .assert()
        .code(4)
        .stderr(predicate::str::contains("outside MRMS coverage"));
}

#[test]
fn every_command_has_help() {
    for args in [
        vec!["--help"],
        vec!["status", "--help"],
        vec!["rain", "--help"],
        vec!["series", "--help"],
        vec!["map", "--help"],
        vec!["grid", "--help"],
        vec!["gauges", "--help"],
        vec!["radar", "--help"],
        vec!["alerts", "--help"],
        vec!["forecast", "--help"],
        vec!["conditions", "--help"],
        vec!["places", "--help"],
        vec!["places", "add", "--help"],
        vec!["backfill", "--help"],
        vec!["api", "--help"],
        vec!["info", "--help"],
        vec!["auth", "status", "--help"],
        vec!["config", "set", "--help"],
        vec!["self-update", "--help"],
        vec!["completions", "--help"],
    ] {
        let (mut c, _d) = wx();
        c.args(&args).assert().success();
    }
}

#[test]
fn info_reports_the_family_spec_and_no_auth() {
    let (mut c, _d) = wx();
    let out = c.arg("info").assert().success().get_output().stdout.clone();
    let v = json_of(&out);
    assert_eq!(v["schema"], "cli-info/v1");
    assert_eq!(v["name"], "wx");
    assert_eq!(v["spec"], "piekstra-cli/1");
    assert_eq!(v["auth"]["required"], false);
}

#[test]
fn auth_status_says_none_is_needed() {
    let (mut c, _d) = wx();
    let out = c
        .args(["auth", "status", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v = json_of(&out);
    assert_eq!(v["schema"], "auth-status/v1");
    assert_eq!(v["required"], false);
}

#[test]
fn bad_window_is_a_usage_error_before_any_network() {
    let (mut c, _d) = wx();
    let out = c
        .args(["rain", "someday", "--json"])
        .assert()
        .code(2)
        .get_output()
        .stdout
        .clone();
    assert_eq!(json_of(&out)["error"]["code"], "usage");
}

#[test]
fn bad_point_is_a_usage_error() {
    let (mut c, _d) = wx();
    c.args(["rain", "24h", "--at", "not-a-place"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("expected lat,lon"));
    let (mut c, _d) = wx();
    c.args(["forecast", "--at", "95,-80"]).assert().code(2);
}

#[test]
fn bad_region_is_a_usage_error() {
    let (mut c, _d) = wx();
    c.args(["status", "--region", "1,2,3"]).assert().code(2);
}

#[test]
fn a_map_with_nothing_to_say_where_is_a_usage_error() {
    let (mut c, _d) = wx();
    c.args(["map", "24h"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("where to look"));
    let (mut c, _d) = wx();
    c.args(["alerts"]).assert().code(2);
    let (mut c, _d) = wx();
    c.args(["backfill"]).assert().code(2);
}

#[test]
fn points_too_far_apart_for_one_grid_are_a_usage_error() {
    let (mut c, _d) = wx();
    c.args(["rain", "24h", "--at", "47.6,-122.3", "--at", "25.8,-80.2"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("separately"));
}

#[test]
fn radar_frame_limit_is_checked_first() {
    let (mut c, _d) = wx();
    c.args(["radar", "--frames", "99"]).assert().code(2);
}

#[test]
fn api_refuses_anything_but_get() {
    let (mut c, _d) = wx();
    c.args(["api", "POST", "/alerts"]).assert().code(2);
}

#[test]
fn places_round_trip_through_the_config_file() {
    let (mut add, d) = wx();
    add.args(["places", "add", "Example Beach", "26.5,-80.05"])
        .assert()
        .success();

    let mut list = Command::cargo_bin("wx").unwrap();
    let out = list
        .env("XDG_CONFIG_HOME", d.path().join("config"))
        .args(["places", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("Example Beach"));

    let mut rm = Command::cargo_bin("wx").unwrap();
    rm.env("XDG_CONFIG_HOME", d.path().join("config"))
        .args(["places", "remove", "example beach"])
        .assert()
        .success();

    let mut again = Command::cargo_bin("wx").unwrap();
    again
        .env("XDG_CONFIG_HOME", d.path().join("config"))
        .args(["places", "remove", "example beach"])
        .assert()
        .code(4);
}

#[test]
fn config_set_validates_and_rejects_unknown_keys() {
    let (mut c, _d) = wx();
    c.args(["config", "set", "region", "not,a,box"])
        .assert()
        .code(2);
    let (mut c, _d) = wx();
    c.args(["config", "set", "color", "blue"]).assert().code(2);
    let (mut c, _d) = wx();
    c.args(["config", "set", "tz", "Mars/Olympus"])
        .assert()
        .code(2);
}

#[test]
fn no_color_env_is_accepted() {
    let (mut c, _d) = wx();
    c.env("NO_COLOR", "1").arg("info").assert().success();
}

#[test]
fn completions_render() {
    let (mut c, _d) = wx();
    c.args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("wx"));
}

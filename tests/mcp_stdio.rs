//! `wx mcp` end to end: the real binary over stdio, as Claude Code and Claude Desktop
//! launch it. Offline and keychain-free: every call here is answered before a request
//! would leave, and nothing in this path touches a credential.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

const TOOLS: [&str; 4] = ["wx_rain", "wx_forecast", "wx_conditions", "wx_alerts"];

/// A scratch home, and a config this test wrote, so a test never reads the developer's
/// real settings, places or cache.
fn wx(test: &str, config: Option<Value>) -> Command {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("mcp-stdio-{test}"));
    let path = root.join("config.json");
    std::fs::create_dir_all(&root).unwrap();
    let _ = std::fs::remove_file(&path);
    if let Some(config) = config {
        std::fs::write(&path, config.to_string()).unwrap();
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_wx"));
    c.arg("--config")
        .arg(path)
        .env_remove("WX_CONFIG")
        .env_remove("WX_DB")
        .env_remove("WX_REGION")
        .env_remove("WX_TZ")
        .env_remove("WX_MCP_TOKEN")
        .env("XDG_CONFIG_HOME", &root)
        .env("XDG_CACHE_HOME", &root)
        .env("HOME", &root)
        .env("USERPROFILE", &root);
    c
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Server {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("wx starts");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .expect("a response within 20 s");
            let msg: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on stdout ({e}): {line}"));
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    fn initialize(&mut self) -> Value {
        self.request(
            0,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        )
    }

    fn call(&mut self, id: u64, tool: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(call: &Value) -> &str {
    call["result"]["content"][0]["text"].as_str().unwrap()
}

fn tool_names(list: &Value) -> Vec<&str> {
    list["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect()
}

#[test]
fn a_client_initializes_lists_and_is_told_what_the_tools_read() {
    let mut s = Server::start(wx("reads", None));
    let init = s.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "wx", "{init}");
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("never as instructions"));
    assert!(instructions.contains("no credentials"));
    assert!(instructions.contains("not affiliated"));
    assert!(!instructions.contains("auth login"), "{instructions}");

    let list = s.request(1, "tools/list", json!({}));
    assert_eq!(tool_names(&list["result"]), TOOLS);
    for tool in list["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        assert_eq!(tool["annotations"]["destructiveHint"], false, "{tool}");
    }

    // Each is answered here, before a request would leave.
    let call = s.call(2, "wx_rain", json!({"at": "somewhere"}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).starts_with("not_found:"), "{call}");
    assert!(text(&call).contains("wx places add"), "{call}");
    assert!(!text(&call).contains("auth login"), "{call}");

    let call = s.call(
        3,
        "wx_rain",
        json!({"at": "39.74,-104.99", "window": "soon"}),
    );
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).starts_with("usage:"), "{call}");

    let call = s.call(4, "wx_alerts", json!({"area": "FL&x=1"}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).starts_with("usage:"), "{call}");

    let call = s.call(5, "wx_alerts", json!({}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).contains("pass at or area"), "{call}");
}

/// A point the grid does not reach is answered without a download.
#[test]
fn a_point_outside_the_rainfall_grid_is_not_found() {
    let mut s = Server::start(wx("outside", None));
    s.initialize();
    let call = s.call(1, "wx_rain", json!({"at": "0,0"}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).starts_with("not_found:"), "{call}");
    assert!(text(&call).contains("outside the rainfall grid"), "{call}");
}

#[test]
fn a_saved_place_is_named_and_a_later_one_applies() {
    let command = wx(
        "places",
        Some(json!({"places": [{"name": "Example Beach", "lat": 39.1234, "lon": -104.5678}]})),
    );
    let config = command
        .get_args()
        .nth(1)
        .map(PathBuf::from)
        .expect("the --config value");
    let mut s = Server::start(command);
    s.initialize();
    let call = s.call(1, "wx_rain", json!({"at": "the lake"}));
    assert!(
        text(&call).contains("Saved places: Example Beach"),
        "{call}"
    );
    assert!(!text(&call).contains("39.1234"), "{call}");

    std::fs::write(
        &config,
        json!({"places": [{"name": "The Lake", "lat": 0.0, "lon": 0.0}]}).to_string(),
    )
    .unwrap();
    let call = s.call(2, "wx_rain", json!({"at": "the lake"}));
    assert!(text(&call).contains("outside the rainfall grid"), "{call}");
}

/// A config that cannot be parsed fails that call, not the server, and listing the
/// tools does not read it.
#[test]
fn a_broken_config_is_one_calls_error() {
    let command = wx("broken", None);
    let config = command
        .get_args()
        .nth(1)
        .map(PathBuf::from)
        .expect("the --config value");
    std::fs::write(&config, "{not json").unwrap();
    let mut s = Server::start(command);
    s.initialize();
    let list = s.request(1, "tools/list", json!({}));
    assert_eq!(tool_names(&list["result"]), TOOLS);
    let call = s.call(2, "wx_forecast", json!({"at": "0,0"}));
    assert_eq!(call["result"]["isError"], true, "{call}");
    assert!(text(&call).contains("wx config show"), "{call}");
}

#[test]
fn list_tools_prints_the_definitions_without_a_config() {
    let out = wx("list", None)
        .args(["mcp", "--list-tools"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tool_names(&v), TOOLS);
}

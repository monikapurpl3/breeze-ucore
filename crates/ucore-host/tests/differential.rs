//! μCore against the real thing: a Breeze Core binary and μCore on the same
//! configuration and the same emulated units (breeze-mock), the same requests,
//! and the same answers. Byte for byte where Breeze Core's answer is fixed;
//! key for key and value for value where a reading moves between two calls.
//!
//!   BREEZE_CORE_BIN=/path/to/breeze-core BREEZE_MOCK_BIN=/path/to/breeze-mock \
//!     cargo test -p ucore-host --test differential
//!
//! Skipped without both. This is what holds μCore to Breeze Core.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const KEY: &str = "ucore-diff-key-not-a-secret";
const TOKEN: &str = "ucore-diff-device-token-not-a-secret-0123456789";

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// What a client could act on: everything but framing, dates and the
    /// server's name for itself.
    fn comparable_headers(&self) -> Vec<(String, String)> {
        let mut h: Vec<_> = self
            .headers
            .iter()
            .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
            .filter(|(n, _)| !matches!(n.as_str(), "connection" | "date" | "content-length" | "transfer-encoding" | "server"))
            .collect();
        h.sort();
        h
    }
}

fn call(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Answer {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let (n, v) = l.split_once(':').unwrap();
        headers.push((n.trim().to_string(), v.trim().to_string()));
    }
    let len: Option<usize> = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.parse().unwrap());
    let mut body = Vec::new();
    match len {
        Some(n) => {
            body.resize(n, 0);
            r.read_exact(&mut body).unwrap();
        }
        None => {
            r.read_to_end(&mut body).unwrap();
        }
    }
    Answer { status, headers, body }
}

/// A request: method, path, headers, body.
type Case<'a> = (&'a str, String, Vec<(&'a str, &'a str)>, &'a str);

const AUTH: [(&str, &str); 2] = [("X-API-Key", KEY), ("Authorization", "Bearer ucore-diff-device-token-not-a-secret-0123456789")];

fn wait_for(port: u16, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "{what} did not start on {port}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Keys and value types, recursively: for answers whose readings move.
fn shape(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(shape).collect()),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        Value::Bool(_) => json!("bool"),
        Value::Null => json!("null"),
    }
}

/// Without the readings that move between two calls.
fn steady(v: &Value) -> Value {
    let mut v = v.clone();
    for list in ["states", "errors"] {
        if let Some(items) = v.get_mut(list).and_then(Value::as_array_mut) {
            for item in items {
                item.as_object_mut().map(|o| o.remove("indoor_temperature"));
            }
        }
    }
    v.as_object_mut().map(|o| o.remove("indoor_temperature"));
    v
}

fn compare_exact(failures: &mut Vec<String>, real: u16, ucore: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) {
    let r = call(real, method, path, headers, body);
    let u = call(ucore, method, path, headers, body);
    if (r.status, r.comparable_headers(), &r.body) != (u.status, u.comparable_headers(), &u.body) {
        failures.push(format!(
            "{method} {path} {body}\n   real  {} {}\n   ucore {} {}",
            r.status,
            String::from_utf8_lossy(&r.body),
            u.status,
            String::from_utf8_lossy(&u.body)
        ));
    }
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Start, approve from here (the LAN), poll: the three answers.
fn pair(port: u16, start: &str) -> Vec<Value> {
    let key = [("X-API-Key", KEY)];
    let s = call(port, "POST", "/api/auth/enroll/start", &key, start).json();
    let code = s["user_code"].as_str().unwrap_or_else(|| panic!("start on {port}: {s}"));
    let a = call(port, "POST", "/api/auth/enroll/approve", &key, &format!(r#"{{"code": "{}"}}"#, code.to_lowercase())).json();
    let p = call(port, "POST", "/api/auth/enroll/poll", &key, &format!(r#"{{"session_id": "{}"}}"#, s["session_id"].as_str().unwrap())).json();
    assert_eq!(p["status"], "approved", "poll on {port}: {p}");
    vec![s, a, p]
}

/// A v2 request: Ed25519 over Breeze Core's canonical string.
#[allow(clippy::too_many_arguments)]
fn signed(port: u16, key: &ed25519_dalek::SigningKey, key_id: &str, method: &str, path: &str, body: &str, ts: &str, nonce: &str) -> Answer {
    use ed25519_dalek::Signer;
    let canonical = ucore::auth::canonical(method, path, ts, nonce, body.as_bytes());
    let sig = b64(&key.sign(&canonical).to_bytes());
    let headers = [
        ("X-API-Key", KEY),
        ("X-Breeze-Auth-Version", "2"),
        ("X-Breeze-Key-Id", key_id),
        ("X-Breeze-Timestamp", ts),
        ("X-Breeze-Nonce", nonce),
        ("X-Breeze-Signature", sig.as_str()),
    ];
    call(port, method, path, &headers, body)
}

/// The μCore under test: this build's, or UCORE_BIN (a cross-built one run
/// where this test binary was copied to).
fn ucore_bin() -> PathBuf {
    std::env::var_os("UCORE_BIN").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_ucore")))
}

fn bins() -> Option<(PathBuf, PathBuf)> {
    let core = std::env::var_os("BREEZE_CORE_BIN").map(PathBuf::from)?;
    let mock = std::env::var_os("BREEZE_MOCK_BIN").map(PathBuf::from)?;
    Some((core, mock))
}

fn devices_json(dir: &Path) {
    // The same paired device for both servers: a v1 token, stored hashed.
    let hash = ucore::auth::hash_secret(TOKEN);
    let doc = json!({"devices": [{
        "token_id": "diff-device", "label": "diff", "auth_version": 1,
        "token_hash": hash, "public_key": null, "created_at": 1790000000.0,
        "expires_at": null, "last_used": null
    }]});
    std::fs::write(dir.join("devices.json"), serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

/// A timer's answer with what moves masked: ids, clock readings, and the
/// minutes and seconds that depend on when it was asked. Key order is kept,
/// so the masked text still has to match Breeze Core's byte for byte.
fn mask(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let is_time = |w: &[u8]| {
        w.len() == 19
            && w.iter().enumerate().all(|(k, c)| match k {
                4 | 7 => *c == b'-',
                10 => *c == b'T',
                13 | 16 => *c == b':',
                _ => c.is_ascii_digit(),
            })
    };
    while i < b.len() {
        if i + 19 <= b.len() && is_time(&b[i..i + 19]) {
            out.push_str("TIME");
            i += 19;
            continue;
        }
        let mut done = false;
        for (key, hex) in [("\"id\":", true), ("\"seconds_remaining\":", false), ("\"minutes\":", false), ("\"runs\":", false)] {
            if b[i..].starts_with(key.as_bytes()) {
                out.push_str(key);
                i += key.len();
                while i < b.len() && b[i] == b' ' {
                    out.push(' ');
                    i += 1;
                }
                if hex && b.get(i) == Some(&b'"') && b.len() >= i + 14 && b[i + 1..i + 13].iter().all(u8::is_ascii_hexdigit) && b[i + 13] == b'"' {
                    out.push_str("\"ID\"");
                    i += 14;
                } else if !hex {
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    out.push('N');
                }
                done = true;
                break;
            }
        }
        if !done {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// What a sleep timer promises, checked on its own answer: the moment is
/// the minutes after its creation, and the countdown starts there.
fn check_sleep(t: &Value, minutes: i64) {
    let at = |k: &str| chrono::NaiveDateTime::parse_from_str(t[k].as_str().unwrap(), "%Y-%m-%dT%H:%M:%S").unwrap();
    assert_eq!((at("fires_at") - at("created_at")).num_minutes(), minutes, "{t}");
    let left = t["seconds_remaining"].as_i64().unwrap();
    assert!((minutes * 60 - 5..=minutes * 60).contains(&left), "{t}");
}

/// An open event stream: its status, header lines in order, and a reader for
/// its chunks.
struct EventStream {
    status: u16,
    head: Vec<String>,
    reader: BufReader<TcpStream>,
}

fn open_stream(port: u16) -> EventStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth: String = AUTH.iter().map(|(n, v)| format!("{n}: {v}\r\n")).collect();
    s.write_all(format!("GET /api/units/stream HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\n{auth}\r\n").as_bytes()).unwrap();
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split(' ').nth(1).unwrap().parse().unwrap();
    let mut head = Vec::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        head.push(l.to_string());
    }
    EventStream { status, head, reader }
}

impl EventStream {
    /// The next chunk's text; `None` when the stream ends or stalls.
    fn chunk(&mut self) -> Option<String> {
        let mut size = String::new();
        self.reader.read_line(&mut size).ok()?;
        let n = usize::from_str_radix(size.trim(), 16).ok()?;
        let mut payload = vec![0u8; n + 2];
        self.reader.read_exact(&mut payload).ok()?;
        payload.truncate(n);
        String::from_utf8(payload).ok()
    }

    /// The next state event's data, if one comes before `deadline`
    /// (keepalives keep a stalled stream's reads from ever timing out).
    fn state(&mut self, deadline: Instant) -> Option<Value> {
        loop {
            if Instant::now() > deadline {
                return None;
            }
            let c = self.chunk()?;
            if let Some(rest) = c.strip_prefix("event: state\ndata: ") {
                return serde_json::from_str(rest.trim_end()).ok();
            }
        }
    }
}

/// A list of unit states, each without the room temperature.
fn steady_list(v: &Value) -> Value {
    match v.as_array() {
        Some(list) => Value::Array(list.iter().map(steady).collect()),
        None => v.clone(),
    }
}

fn local_now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

#[test]
fn ucore_answers_as_breeze_core_does() {
    let Some((core_bin, mock_bin)) = bins() else {
        eprintln!("skipped: set BREEZE_CORE_BIN and BREEZE_MOCK_BIN to compare against Breeze Core");
        return;
    };
    let work = std::env::temp_dir().join(format!("ucore-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let blueprint = work.join("units.xml");
    std::fs::write(
        &blueprint,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<blueprint xmlns="urn:breeze-mock:blueprint:1" version="1" name="ucore-diff" seed="7">
  <environment><constant outdoor="10"/></environment>
  <network><segment name="lo" cidr="127.93.1.0/24" mode="loopback" discovery="false"/></network>
  <unit key="a" segment="lo" ip="127.93.1.2">
    <epithets name="Alpha"/><v3 token="auto" key="auto"/>
    <state power="on" mode="cool" target="24"/><behaviour reply-delay="0ms"/>
  </unit>
  <unit key="b" segment="lo" ip="127.93.1.3">
    <epithets name="Bravo"/><v3 token="auto" key="auto"/>
    <capabilities modes="auto cool heat fan-only" swing="vertical" fan="low high auto" eco="false" turbo="cool"/>
    <state power="on" mode="heat" target="22"/><behaviour reply-delay="0ms"/>
  </unit>
</blueprint>
"#,
    )
    .unwrap();
    let mock_data = work.join("mock-data");
    let _mock = Kill(
        Command::new(&mock_bin)
            .args(["run", blueprint.to_str().unwrap(), "-q"])
            .env("BREEZE_MOCK_DATA", &mock_data)
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect("127.93.1.3:6444").is_err() {
        assert!(Instant::now() < deadline, "breeze-mock did not start its units");
        std::thread::sleep(Duration::from_millis(50));
    }

    // One configuration for both: the exported units, plus a unit nothing
    // answers for and a unit without V3 credentials.
    let real_dir = work.join("real");
    let ucore_dir = work.join("ucore");
    for d in [&real_dir, &ucore_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    let out = Command::new(&mock_bin)
        .args(["export", "breeze-core-config", blueprint.to_str().unwrap()])
        .env("BREEZE_MOCK_DATA", &mock_data)
        .output()
        .unwrap();
    let mut config: Value = serde_json::from_slice(&out.stdout).expect("export prints the config");
    config["api_key"] = json!(KEY);
    let units = config["units"].as_array_mut().unwrap();
    let mut dead = units[0].clone();
    dead["name"] = json!("Dead");
    dead["ip"] = json!("127.93.1.9");
    dead["id"] = json!(111111111111i64);
    units.push(dead);
    units.push(json!({"name": "Bare", "ip": "127.93.1.10", "port": 6444, "id": 222222222222i64, "token": null, "key": null}));
    for d in [&real_dir, &ucore_dir] {
        std::fs::write(d.join("config.json"), serde_json::to_string_pretty(&config).unwrap()).unwrap();
        devices_json(d);
    }
    let alpha = config["units"][0]["id"].to_string();
    let bravo = config["units"][1]["id"].to_string();

    let (real, ucore) = (29301u16, 29302u16);
    let _real = Kill(
        Command::new(&core_bin)
            .arg("serve")
            .env("AC_CONFIG_DIR", &real_dir)
            .env("BREEZE_HOST", "127.0.0.1")
            .env("BREEZE_PORT", real.to_string())
            .env("BREEZE_LOG", "0")
            .env("AC_TIMER_TICK", "3600")
            .env("AC_SCHED_TICK", "3600")
            .env("AC_STREAM_TICK", "1")
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let _ucore = Kill(
        Command::new(ucore_bin())
            .env("AC_CONFIG_DIR", &ucore_dir)
            .env("BREEZE_HOST", "127.0.0.1")
            .env("BREEZE_PORT", ucore.to_string())
            .env("AC_TIMER_TICK", "3600")
            .env("AC_SCHED_TICK", "3600")
            .env("AC_STREAM_TICK", "1")
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for(real, "breeze-core");
    wait_for(ucore, "ucore");

    let ids = [alpha.as_str(), bravo.as_str(), "111111111111", "222222222222"];
    let mut exact: Vec<Case> = vec![
        ("GET", "/api/health".into(), vec![], ""),
        ("GET", "/api/units".into(), AUTH.to_vec(), ""),
        ("GET", "/api/units".into(), vec![], ""),
        ("GET", "/api/units".into(), vec![("X-API-Key", "wrong-key")], ""),
        ("GET", "/api/units".into(), vec![("X-API-Key", KEY), ("Authorization", "Bearer nobody-knows-this-token-0123456789")], ""),
        ("GET", "/api/units".into(), vec![("X-API-Key", KEY)], ""),
        ("PUT", "/api/units".into(), AUTH.to_vec(), ""),
        ("GET", "/api/nope".into(), AUTH.to_vec(), ""),
        ("GET", "/api/units/999/state".into(), AUTH.to_vec(), ""),
        ("GET", "/api/units/abc/state".into(), AUTH.to_vec(), ""),
        ("GET", format!("/api/units/{alpha}"), AUTH.to_vec(), ""),
        ("GET", format!("/api/units/{alpha}/control"), AUTH.to_vec(), ""),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), "nope"),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), ""),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), r#"{"power_state": "yes"}"#),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), r#"{"target_temperature": 99}"#),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), r#"{"target_temperature": 22.3}"#),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), r#"{"fan_speed": 7}"#),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), r#"{"operational_mode": "WARP"}"#),
        ("POST", format!("/api/units/{alpha}/control"), AUTH.to_vec(), r#"{"swing_mode": "sideways"}"#),
        ("POST", "/api/units/222222222222/control".into(), AUTH.to_vec(), r#"{"power_state": true}"#),
        ("GET", "/api/units/222222222222/state".into(), AUTH.to_vec(), ""),
        ("GET", "/api/units/111111111111/state".into(), AUTH.to_vec(), ""),
    ];
    for id in &ids[..2] {
        exact.push(("GET", format!("/api/units/{id}/capabilities"), AUTH.to_vec(), ""));
    }
    let mut failures = Vec::new();
    for (method, path, headers, body) in &exact {
        let r = call(real, method, path, headers, body);
        let u = call(ucore, method, path, headers, body);
        let what = format!("{method} {path} {body}");
        // Not vacuous: Breeze Core sends its hardening headers on everything.
        assert!(r.comparable_headers().iter().any(|(n, _)| n == "x-content-type-options"), "{what}: no headers to compare");
        if (u.status, u.comparable_headers()) != (r.status, r.comparable_headers()) {
            failures.push(format!("{what}\n   real  {} {:?}\n   ucore {} {:?}", r.status, r.comparable_headers(), u.status, u.comparable_headers()));
        }
        if u.body != r.body {
            failures.push(format!(
                "{what}: body\n   real  {}\n   ucore {}",
                String::from_utf8_lossy(&r.body),
                String::from_utf8_lossy(&u.body)
            ));
        }
    }

    // Readings that move: the same keys and types, and the same values apart
    // from the room temperature.
    for path in [format!("/api/units/{alpha}/state"), format!("/api/units/{bravo}/state"), "/api/units/state".into()] {
        let (r, u) = (call(real, "GET", &path, &AUTH, ""), call(ucore, "GET", &path, &AUTH, ""));
        if r.status != u.status || shape(&r.json()) != shape(&u.json()) || steady(&r.json()) != steady(&u.json()) {
            failures.push(format!("GET {path}\n   real  {} {}\n   ucore {} {}", r.status, r.json(), u.status, u.json()));
        }
    }

    // And not because both failed alike: what μCore said is what it should.
    let one = call(ucore, "GET", &format!("/api/units/{alpha}/state"), &AUTH, "").json();
    assert_eq!((one["online"].clone(), one["name"].clone()), (json!(true), json!("Alpha")), "{one}");
    let all = call(ucore, "GET", "/api/units/state", &AUTH, "").json();
    let names = |k: &str| all[k].as_array().unwrap().iter().map(|s| s["name"].clone()).collect::<Vec<_>>();
    assert_eq!((names("states"), names("errors")), (vec![json!("Alpha"), json!("Bravo")], vec![json!("Dead"), json!("Bare")]), "{all}");

    // Controls, through each in turn, on the same unit: the same answer, and
    // Bravo refuses turbo while heating in both.
    let mut answers = Vec::new();
    for body in [r#"{"turbo": true}"#, r#"{"power_state": true, "operational_mode": "HEAT", "target_temperature": 23.5, "fan_speed": 80}"#] {
        let path = format!("/api/units/{bravo}/control");
        let r = call(real, "POST", &path, &AUTH, body);
        let u = call(ucore, "POST", &path, &AUTH, body);
        if r.status != u.status || steady(&r.json()) != steady(&u.json()) {
            failures.push(format!("POST {path} {body}\n   real  {} {}\n   ucore {} {}", r.status, r.json(), u.status, u.json()));
        }
        answers.push(u.json());
    }
    assert_eq!(answers[0]["not_applied"], json!(["turbo"]), "{}", answers[0]);
    assert_eq!(
        (&answers[1]["operational_mode"], &answers[1]["target_temperature"], &answers[1]["fan_speed"], &answers[1]["not_applied"]),
        (&json!("HEAT"), &json!(23.5), &json!(80), &json!([])),
        "{}",
        answers[1]
    );

    // /api/version: Breeze Core's shape, μCore's identity.
    let (r, u) = (call(real, "GET", "/api/version", &AUTH, "").json(), call(ucore, "GET", "/api/version", &AUTH, "").json());
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&r), keys(&u), "/api/version keys");
    assert_eq!(u["name"], "Breeze μCore");
    assert_eq!(u["version"], format!("{}+u{}", ucore::NATIVE, ucore::VERSION));
    assert_eq!((r["units"].clone(), r["min_auth_version"].clone()), (u["units"].clone(), u["min_auth_version"].clone()));
    for f in u["features"].as_array().unwrap() {
        assert!(r["features"].as_array().unwrap().contains(f), "μCore advertises {f}, which Breeze Core does not");
    }

    // ------------------------------------------------------ pairing and auth
    let key = [("X-API-Key", KEY)];
    for (method, path, body) in [
        ("POST", "/api/auth/enroll/start", "nope"),
        ("POST", "/api/auth/enroll/start", r#"{"auth_version": 2, "public_key": "bad"}"#),
        ("POST", "/api/auth/enroll/approve", "nope"),
        ("POST", "/api/auth/enroll/approve", r#"{"code": "ZZZZ-ZZZZ"}"#),
        ("POST", "/api/auth/enroll/poll", "nope"),
        ("POST", "/api/auth/enroll/poll", r#"{"session_id": "nope"}"#),
        ("DELETE", "/api/auth/devices/nope", ""),
        ("GET", "/api/auth/devices/nope", ""),
        ("GET", "/api/auth/enroll/start", ""),
    ] {
        compare_exact(&mut failures, real, ucore, method, path, &key, body);
    }
    for body in ["{}", r#"{"public_key": "bad"}"#] {
        compare_exact(&mut failures, real, ucore, "POST", "/api/auth/upgrade", &AUTH, body);
    }
    let who = |port| {
        let mut v = call(port, "GET", "/api/auth/whoami", &AUTH, "").json();
        v.as_object_mut().unwrap().remove("last_used");
        v
    };
    if who(real) != who(ucore) {
        failures.push(format!("whoami\n   real  {}\n   ucore {}", who(real), who(ucore)));
    }

    // The whole of pairing, on each: v1 (a token handed over once), v2 (only
    // the public key kept, then signed requests).
    let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    let public = b64(signing.verifying_key().as_bytes());
    let mut paired = Vec::new();
    for port in [real, ucore] {
        let v1 = pair(port, r#"{"label": "phone"}"#);
        let v2 = pair(port, &format!(r#"{{"label": "signed", "auth_version": 2, "public_key": "{public}"}}"#));
        let up = pair(port, r#"{"label": "to-upgrade"}"#);
        paired.push((v1, v2, up));
    }
    let shapes = |i: usize| paired.iter().map(|p| [&p.0, &p.1, &p.2][i].iter().map(shape).collect::<Vec<_>>()).collect::<Vec<_>>();
    for i in 0..3 {
        let s = shapes(i);
        if s[0] != s[1] {
            failures.push(format!("pairing {i}: shapes differ\n   real  {:?}\n   ucore {:?}", s[0], s[1]));
        }
    }
    for (port, (v1, v2, up)) in [real, ucore].into_iter().zip(&paired) {
        let token = v1[2]["device_token"].as_str().unwrap();
        let bearer = format!("Bearer {token}");
        assert_eq!(call(port, "GET", "/api/units", &[("X-API-Key", KEY), ("Authorization", &bearer)], "").status, 200, "v1 on {port}");
        assert!(v2[2].get("device_token").is_none(), "v2 hands over no secret: {}", v2[2]);
        let key_id = v2[2]["token_id"].as_str().unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs().to_string();
        let ok = signed(port, &signing, key_id, "GET", "/api/units?x=1", "", &now, &format!("n-{port}-1"));
        assert_eq!(ok.status, 200, "v2 on {port}: {}", String::from_utf8_lossy(&ok.body));
        // The upgrade: from then on the old bearer token is nobody's.
        let up_token = format!("Bearer {}", up[2]["device_token"].as_str().unwrap());
        let up_auth = [("X-API-Key", KEY), ("Authorization", up_token.as_str())];
        let r = call(port, "POST", "/api/auth/upgrade", &up_auth, &format!(r#"{{"public_key": "{public}"}}"#));
        assert_eq!((r.status, r.json()["auth_version"].clone()), (200, json!(2)), "upgrade on {port}");
        assert_eq!(call(port, "GET", "/api/units", &up_auth, "").status, 401, "upgraded token on {port}");
    }
    // Rejections of signed requests, byte for byte (each server with its own
    // device id, so the ids are put back the same before comparing).
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs().to_string();
    let reject = |port: u16, i: usize, case: &str| {
        let key_id = paired[i].1[2]["token_id"].as_str().unwrap();
        let a = match case {
            "replay" => signed(port, &signing, key_id, "GET", "/api/units?x=1", "", &now, &format!("n-{port}-1")),
            "bad-signature" => signed(port, &ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]), key_id, "GET", "/api/units", "", &now, "n2"),
            "incomplete" => call(port, "GET", "/api/units", &[("X-API-Key", KEY), ("X-Breeze-Auth-Version", "2"), ("X-Breeze-Key-Id", key_id)], ""),
            "unknown-key" => signed(port, &signing, "nobody", "GET", "/api/units", "", &now, "n3"),
            _ => {
                let mut a = signed(port, &signing, key_id, "GET", "/api/units", "", "1000", "n4");
                let mut v = a.json();
                v["detail"].as_object_mut().unwrap().remove("server_time");
                a.body = serde_json::to_vec(&v).unwrap();
                a
            }
        };
        (a.status, a.comparable_headers(), String::from_utf8_lossy(&a.body).into_owned())
    };
    // A replayed nonce, a key that is not this device's, missing headers, an
    // unknown key id, a clock 50 years off.
    for case in ["replay", "bad-signature", "incomplete", "unknown-key", "clock-skew"] {
        let (r, u) = (reject(real, 0, case), reject(ucore, 1, case));
        if r != u {
            failures.push(format!("signed {case}\n   real  {r:?}\n   ucore {u:?}"));
        }
    }
    let list = |port| call(port, "GET", "/api/auth/devices", &key, "").json();
    if shape(&list(real)) != shape(&list(ucore)) {
        failures.push(format!("devices\n   real  {}\n   ucore {}", list(real), list(ucore)));
    }
    for (port, i) in [(real, 0), (ucore, 1)] {
        let id = paired[i].0[2]["token_id"].as_str().unwrap();
        let r = call(port, "DELETE", &format!("/api/auth/devices/{id}"), &key, "");
        assert_eq!((r.status, r.body.len()), (204, 0), "revoke on {port}");
    }
    // devices.json as Breeze Core writes it: the same lines with the values
    // taken out, and no trailing newline.
    let skeleton = |dir: &Path| {
        let text = std::fs::read_to_string(dir.join("devices.json")).unwrap();
        assert!(!text.ends_with('\n'), "{}: trailing newline", dir.display());
        text.lines().map(|l| l.split_once(':').map_or(l, |(k, _)| k).to_string()).collect::<Vec<_>>()
    };
    if skeleton(&real_dir) != skeleton(&ucore_dir) {
        failures.push(format!("devices.json\n   real  {:?}\n   ucore {:?}", skeleton(&real_dir), skeleton(&ucore_dir)));
    }

    // ------------------------------------------------------------- timers
    // Refusals byte for byte; then real timers, compared with what moves
    // masked, and checked for what they promise.
    let many = format!(r#"{{"minutes": 30, "unit_ids": [{}]}}"#, vec![format!("\"{alpha}\""); 65].join(", "));
    let refusals: Vec<(&str, String, String)> = vec![
        ("GET", "/api/timers".into(), String::new()),
        ("GET", "/api/timers?kind=all".into(), String::new()),
        ("GET", "/api/timers?kind=both".into(), String::new()),
        ("GET", "/api/timers?kind=st%61rt".into(), String::new()),
        ("GET", "/api/timers/status".into(), String::new()),
        ("PUT", "/api/timers".into(), String::new()),
        ("GET", "/api/timers/abc123".into(), String::new()),
        ("DELETE", "/api/timers/abc123".into(), String::new()),
        ("DELETE", "/api/timers/".into(), String::new()),
        ("DELETE", "/api/timers/a/b".into(), String::new()),
        ("POST", "/api/timers".into(), "nope".into()),
        ("POST", "/api/timers".into(), String::new()),
        ("POST", "/api/timers".into(), "{}".into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 30, "at": "07:00"}"#.into()),
        ("POST", "/api/timers".into(), r#"{"days": 2}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 30, "days": 1}"#.into()),
        ("POST", "/api/timers".into(), r#"{"at": "07:00", "settings": {"eco": true}}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 0}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 1441}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": -5}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 30, "settings": {"target_temperature": 99}}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 30, "settings": {"operational_mode": "WARP"}}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 30, "settings": {"swing_mode": "up"}}"#.into()),
        ("POST", "/api/timers".into(), r#"{"minutes": 30, "unit_ids": ["999", "abc"]}"#.into()),
        ("POST", "/api/timers".into(), many),
        ("POST", "/api/timers".into(), r#"{"days": 31, "at": "07:00"}"#.into()),
        ("POST", "/api/timers".into(), r#"{"days": 0, "at": "7.30"}"#.into()),
        ("POST", "/api/timers".into(), r#"{"days": 0, "at": "00:00"}"#.into()),
    ];
    for (method, path, body) in &refusals {
        compare_exact(&mut failures, real, ucore, method, path, &AUTH, body);
    }
    let mut created: Vec<(String, String, String)> = Vec::new();
    for (body, minutes) in [
        (format!(r#"{{"unit_ids": ["{alpha}"], "minutes": 45, "label": "bedtime"}}"#), Some(45)),
        // Replaces the one above: one sleep timer per unit.
        (format!(r#"{{"unit_ids": ["{alpha}"], "minutes": 30, "settings": {{"eco": true, "operational_mode": "cool", "target_temperature": 24}}}}"#), Some(30)),
        (format!(r#"{{"unit_ids": ["{bravo}"], "days": 1, "at": "07:30", "label": "morning"}}"#), None),
        (r#"{"minutes": 10}"#.to_string(), Some(10)),
    ] {
        let (r, u) = (call(real, "POST", "/api/timers", &AUTH, &body), call(ucore, "POST", "/api/timers", &AUTH, &body));
        let (rt, ut) = (String::from_utf8_lossy(&r.body).into_owned(), String::from_utf8_lossy(&u.body).into_owned());
        if (r.status, r.comparable_headers(), mask(&rt)) != (u.status, u.comparable_headers(), mask(&ut)) {
            failures.push(format!("POST /api/timers {body}\n   real  {} {rt}\n   ucore {} {ut}", r.status, u.status));
        }
        if let Some(m) = minutes {
            check_sleep(&u.json(), m);
        }
        created.push((r.json()["id"].as_str().unwrap_or("").into(), u.json()["id"].as_str().unwrap_or("").into(), body));
    }
    for path in ["/api/timers", "/api/timers?kind=start", "/api/timers?kind=all", "/api/timers/status"] {
        let (r, u) = (call(real, "GET", path, &AUTH, ""), call(ucore, "GET", path, &AUTH, ""));
        let (rt, ut) = (String::from_utf8_lossy(&r.body).into_owned(), String::from_utf8_lossy(&u.body).into_owned());
        if (r.status, mask(&rt)) != (u.status, mask(&ut)) {
            failures.push(format!("GET {path}\n   real  {} {rt}\n   ucore {} {ut}", r.status, u.status));
        }
    }
    let listed = call(ucore, "GET", "/api/timers?kind=all", &AUTH, "").json();
    let kinds: Vec<_> = listed.as_array().unwrap().iter().map(|t| (t["kind"].clone(), t["label"].clone())).collect();
    assert_eq!(kinds, [(json!("sleep"), json!("")), (json!("start"), json!("morning")), (json!("sleep"), json!(""))], "{listed}");
    let (rj, uj) = (std::fs::read_to_string(real_dir.join("timers.json")).unwrap(), std::fs::read_to_string(ucore_dir.join("timers.json")).unwrap());
    if mask(&rj) != mask(&uj) {
        failures.push(format!("timers.json\n   real  {rj}\n   ucore {uj}"));
    }
    // Cancelled, then gone: 204, then 404, and an empty store.
    for (r_id, u_id, _) in &created[1..] {
        for _ in 0..2 {
            let (r, u) = (call(real, "DELETE", &format!("/api/timers/{r_id}"), &AUTH, ""), call(ucore, "DELETE", &format!("/api/timers/{u_id}"), &AUTH, ""));
            if (r.status, r.comparable_headers(), &r.body) != (u.status, u.comparable_headers(), &u.body) {
                failures.push(format!("DELETE /api/timers/{{id}}\n   real  {} {}\n   ucore {} {}", r.status, String::from_utf8_lossy(&r.body), u.status, String::from_utf8_lossy(&u.body)));
            }
        }
    }
    let (rj, uj) = (std::fs::read(real_dir.join("timers.json")).unwrap(), std::fs::read(ucore_dir.join("timers.json")).unwrap());
    if rj != uj {
        failures.push(format!("timers.json, emptied\n   real  {}\n   ucore {}", String::from_utf8_lossy(&rj), String::from_utf8_lossy(&uj)));
    }

    // ------------------------------------------------------------- programs
    let long_name = "x".repeat(65);
    let program_refusals: Vec<(&str, String, String)> = vec![
        ("GET", "/api/programs".into(), String::new()),
        ("GET", "/api/programs/status".into(), String::new()),
        ("PATCH", "/api/programs".into(), String::new()),
        ("GET", "/api/programs/nope".into(), String::new()),
        ("PATCH", "/api/programs/nope".into(), String::new()),
        ("DELETE", "/api/programs/nope".into(), String::new()),
        ("PUT", "/api/programs/nope".into(), r#"{"name": "Valid"}"#.into()),
        ("PUT", "/api/programs/nope".into(), r#"{"name": ""}"#.into()),
        ("POST", "/api/programs/nope/apply".into(), String::new()),
        ("GET", "/api/programs/nope/apply".into(), String::new()),
        ("POST", "/api/programs//apply".into(), String::new()),
        ("GET", "/api/programs/a/b".into(), String::new()),
        ("POST", "/api/programs".into(), "nope".into()),
        ("POST", "/api/programs".into(), String::new()),
        ("POST", "/api/programs".into(), "{}".into()),
        ("POST", "/api/programs".into(), r#"{"name": ""}"#.into()),
        ("POST", "/api/programs".into(), format!(r#"{{"name": "{long_name}"}}"#)),
        ("POST", "/api/programs".into(), r#"{"name": "x", "enabled": "yes"}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "macro"}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "schedule", "schedule": [{"days": [7], "time": "07:30", "settings": {}}]}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "schedule", "schedule": [{"days": [300], "time": "07:30", "settings": {}}]}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "schedule", "schedule": [{"time": "7:30", "settings": {}}]}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "schedule", "schedule": [{"time": "24:00", "settings": {}}]}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "schedule", "schedule": [{"time": "07:30"}]}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "schedule", "schedule": [{"time": "07:30", "settings": {"swing_mode": "up"}}]}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "curve", "curve": {"operational_mode": "warp"}}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "curve", "curve": {"fan_speed": 7}}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "curve", "curve": {"points": [{"time": "06:00", "temperature": 31.5}]}}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "kind": "curve", "curve": {"points": [{"time": "6:00", "temperature": 20}]}}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "favourite": {"fan_speed": 7}}"#.into()),
        ("POST", "/api/programs".into(), r#"{"name": "x", "favourite": {"target_temperature": 15}}"#.into()),
    ];
    for (method, path, body) in &program_refusals {
        compare_exact(&mut failures, real, ucore, method, path, &AUTH, body);
    }
    // Stored programs: compared with their ids masked, per server's own ids.
    let specs = [
        format!(r#"{{"name": "Cosy", "unit_ids": ["{alpha}"], "favourite": {{"power_state": true, "operational_mode": "heat", "target_temperature": 23}}}}"#),
        format!(r#"{{"name": "Week", "kind": "schedule", "unit_ids": ["{bravo}"], "schedule": [{{"days": [4, 0, 4], "time": "07:30", "settings": {{"power_state": true}}}}]}}"#),
        format!(r#"{{"name": "Warm", "kind": "curve", "unit_ids": ["{alpha}"], "curve": {{"operational_mode": "heat", "fan_speed": 60, "points": [{{"time": "06:00", "temperature": 21}}, {{"time": "22:00", "temperature": 18.5}}]}}}}"#),
        r#"{"name": "Empty"}"#.to_string(),
        format!(r#"{{"name": "Flat", "kind": "curve", "unit_ids": ["{alpha}"], "curve": {{}}}}"#),
        r#"{"name": "Bare curve", "kind": "curve"}"#.to_string(),
        r#"{"name": "Everyone", "enabled": false, "favourite": {"power_state": true}}"#.to_string(),
        r#"{"name": "Nobody", "unit_ids": ["999"], "favourite": {"power_state": true}}"#.to_string(),
    ];
    let mut ids_of: Vec<(String, String)> = Vec::new();
    for body in &specs {
        let (r, u) = (call(real, "POST", "/api/programs", &AUTH, body), call(ucore, "POST", "/api/programs", &AUTH, body));
        let (rt, ut) = (String::from_utf8_lossy(&r.body).into_owned(), String::from_utf8_lossy(&u.body).into_owned());
        if (r.status, r.comparable_headers(), mask(&rt)) != (u.status, u.comparable_headers(), mask(&ut)) {
            failures.push(format!("POST /api/programs {body}\n   real  {} {rt}\n   ucore {} {ut}", r.status, u.status));
        }
        ids_of.push((r.json()["id"].as_str().unwrap_or("").into(), u.json()["id"].as_str().unwrap_or("").into()));
    }
    let pair = |n: usize| ids_of[n].clone();
    for n in 0..specs.len() {
        let (rid, uid) = pair(n);
        let (r, u) = (call(real, "GET", &format!("/api/programs/{rid}"), &AUTH, ""), call(ucore, "GET", &format!("/api/programs/{uid}"), &AUTH, ""));
        if (r.status, mask(&String::from_utf8_lossy(&r.body))) != (u.status, mask(&String::from_utf8_lossy(&u.body))) {
            failures.push(format!("GET /api/programs/{{id}} #{n}\n   real  {}\n   ucore {}", String::from_utf8_lossy(&r.body), String::from_utf8_lossy(&u.body)));
        }
    }
    let update = format!(r#"{{"name": "Cosier", "unit_ids": ["{alpha}"], "favourite": {{"power_state": true, "operational_mode": "HEAT", "target_temperature": 23.5, "fan_speed": 40}}}}"#);
    for (method, n, suffix, body) in [
        ("PUT", 0usize, "", update.as_str()),
        ("PUT", 0, "", r#"{"name": "x", "kind": "macro"}"#),
        // Apply: a schedule and an empty favourite refuse; the rest apply,
        // unit by unit, and one that cannot be reached fails the request.
        ("POST", 1, "/apply", ""),
        ("POST", 3, "/apply", ""),
        ("POST", 5, "/apply", ""),
        ("POST", 7, "/apply", ""),
    ] {
        let (rid, uid) = pair(n);
        let (r, u) = (call(real, method, &format!("/api/programs/{rid}{suffix}"), &AUTH, body), call(ucore, method, &format!("/api/programs/{uid}{suffix}"), &AUTH, body));
        let (rt, ut) = (String::from_utf8_lossy(&r.body).into_owned(), String::from_utf8_lossy(&u.body).into_owned());
        if (r.status, r.comparable_headers(), mask(&rt)) != (u.status, u.comparable_headers(), mask(&ut)) {
            failures.push(format!("{method} /api/programs/{{id}}{suffix} (#{n}) {body}\n   real  {} {rt}\n   ucore {} {ut}", r.status, u.status));
        }
    }
    // Applied for real: the readings move, so compared steady.
    for n in [0usize, 2, 4, 6] {
        let (rid, uid) = pair(n);
        let (r, u) = (call(real, "POST", &format!("/api/programs/{rid}/apply"), &AUTH, ""), call(ucore, "POST", &format!("/api/programs/{uid}/apply"), &AUTH, ""));
        if r.status != u.status || steady_list(&r.json()) != steady_list(&u.json()) {
            failures.push(format!("POST /api/programs/{{id}}/apply (#{n})\n   real  {} {}\n   ucore {} {}", r.status, r.json(), u.status, u.json()));
        }
        if n == 0 {
            let a = &u.json()[0];
            assert_eq!((&a["operational_mode"], &a["target_temperature"], &a["fan_speed"]), (&json!("HEAT"), &json!(23.5), &json!(40)), "{a}");
        }
    }
    let (rp, up) = (std::fs::read_to_string(real_dir.join("programs.json")).unwrap(), std::fs::read_to_string(ucore_dir.join("programs.json")).unwrap());
    if mask(&rp) != mask(&up) {
        failures.push(format!("programs.json\n   real  {rp}\n   ucore {up}"));
    }
    for (rid, uid) in &ids_of {
        for _ in 0..2 {
            let (r, u) = (call(real, "DELETE", &format!("/api/programs/{rid}"), &AUTH, ""), call(ucore, "DELETE", &format!("/api/programs/{uid}"), &AUTH, ""));
            // The 404 names the id asked about, and each server made its own.
            let ub = String::from_utf8_lossy(&u.body).replace(uid.as_str(), rid);
            if (r.status, String::from_utf8_lossy(&r.body).into_owned()) != (u.status, ub) {
                failures.push(format!("DELETE /api/programs/{{id}}\n   real  {} {}\n   ucore {} {}", r.status, String::from_utf8_lossy(&r.body), u.status, String::from_utf8_lossy(&u.body)));
            }
        }
    }
    let (rp, up) = (std::fs::read(real_dir.join("programs.json")).unwrap(), std::fs::read(ucore_dir.join("programs.json")).unwrap());
    if rp != up {
        failures.push(format!("programs.json, emptied\n   real  {}\n   ucore {}", String::from_utf8_lossy(&rp), String::from_utf8_lossy(&up)));
    }

    // ------------------------------------------------------------- the stream
    compare_exact(&mut failures, real, ucore, "GET", "/api/units/stream", &[], "");
    compare_exact(&mut failures, real, ucore, "GET", "/api/units/stream", &[("X-API-Key", KEY)], "");
    compare_exact(&mut failures, real, ucore, "POST", "/api/units/stream", &AUTH, "");
    let (mut rs, mut us) = (open_stream(real), open_stream(ucore));
    if (rs.status, &rs.head) != (us.status, &us.head) {
        failures.push(format!("the stream's head\n   real  {} {:?}\n   ucore {} {:?}", rs.status, rs.head, us.status, us.head));
    }
    assert_eq!(us.chunk().as_deref(), Some(": connected\n\n"));
    assert_eq!(rs.chunk().as_deref(), Some(": connected\n\n"));
    // Every unit once, as each server first reads it.
    let first = |es: &mut EventStream| {
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut seen = std::collections::BTreeMap::new();
        while seen.len() < ids.len() {
            let Some(v) = es.state(deadline) else { break };
            seen.entry(v["id"].as_str().unwrap_or("").to_string()).or_insert(v);
        }
        seen
    };
    let (rf, uf) = (first(&mut rs), first(&mut us));
    for id in ids {
        let (r, u) = (rf.get(id).map(steady), uf.get(id).map(steady));
        if r != u || r.is_none() {
            failures.push(format!("the stream's first state of {id}\n   real  {r:?}\n   ucore {u:?}"));
        }
    }
    // A change made through one server reaches both streams.
    call(real, "POST", &format!("/api/units/{alpha}/control"), &AUTH, r#"{"power_state": true, "operational_mode": "COOL", "target_temperature": 26.5}"#);
    let until = Instant::now() + Duration::from_secs(40);
    let changed = |es: &mut EventStream| loop {
        match es.state(until) {
            Some(v) if v["id"].as_str() == Some(alpha.trim_matches('"')) && v["target_temperature"] == json!(26.5) => break Some(v),
            Some(_) => continue,
            None => break None,
        }
    };
    let (rc, uc) = (changed(&mut rs), changed(&mut us));
    if rc.as_ref().map(steady) != uc.as_ref().map(steady) || uc.is_none() {
        failures.push(format!("the stream after a change\n   real  {rc:?}\n   ucore {uc:?}"));
    }
    // μCore's own ceiling: two streams, then Breeze Core's refusal for one too many.
    let second = open_stream(ucore);
    assert_eq!(second.status, 200);
    // Its head only: a stream accepted here would never end.
    let mut third = open_stream(ucore);
    let mut body = String::new();
    if third.status == 503 {
        third.reader.read_to_string(&mut body).unwrap();
    }
    assert_eq!((third.status, body.as_str()), (503, r#"{"detail":"too many open streams"}"#), "{:?}", third.head);
    drop((rs, us, second));

    // ------------------------------------------------------- the configuration
    // Everything short of probing an address (which needs units on a private
    // network: the Linux run does that). Last, since it changes the units.
    let long = "x".repeat(65);
    let adds: Vec<String> = vec![
        "nope".into(),
        r#"{"name": "no address"}"#.into(),
        r#"{"ip": "not-an-ip"}"#.into(),
        r#"{"ip": " 10.0.0.1 ", "token": "zz", "key": "zz"}"#.into(),
        format!(r#"{{"ip": "10.0.0.1", "key": "{}"}}"#, "ab".repeat(32)),
        format!(r#"{{"ip": "10.0.0.1", "token": "{}"}}"#, "ab".repeat(64)),
        format!(r#"{{"ip": "10.0.0.1", "name": "{long}"}}"#),
        r#"{"ip": "127.93.1.2"}"#.into(),
        r#"{"ip": "8.8.8.8", "midea_account": "a", "midea_password": "b"}"#.into(),
    ];
    for body in &adds {
        compare_exact(&mut failures, real, ucore, "POST", "/api/units", &AUTH, body);
    }
    compare_exact(&mut failures, real, ucore, "GET", "/api/config", &AUTH, "");
    for (method, path, body) in [
        ("PATCH", "/api/units/111111111111".to_string(), "nope".to_string()),
        ("PATCH", "/api/units/111111111111".into(), r#"{"name": "   "}"#.into()),
        ("PATCH", "/api/units/111111111111".into(), format!(r#"{{"name": "{long}"}}"#)),
        ("PATCH", "/api/units/999".into(), r#"{"name": "Nobody"}"#.into()),
        ("PATCH", "/api/units/0111111111111".into(), r#"{"name": "Leading zero"}"#.into()),
        ("PATCH", "/api/units/111111111111".into(), r#"{"name": "  Spare room  "}"#.into()),
        ("DELETE", "/api/units/999".into(), String::new()),
        ("DELETE", "/api/units/222222222222".into(), String::new()),
        ("GET", "/api/config".into(), String::new()),
        ("GET", "/api/units".into(), String::new()),
    ] {
        compare_exact(&mut failures, real, ucore, method, &path, &AUTH, &body);
    }
    // And the file both rewrote from the same start: byte for byte.
    let (rc, uc) = (std::fs::read(real_dir.join("config.json")).unwrap(), std::fs::read(ucore_dir.join("config.json")).unwrap());
    if rc != uc {
        failures.push(format!("config.json\n   real  {}\n   ucore {}", String::from_utf8_lossy(&rc), String::from_utf8_lossy(&uc)));
    }

    // ------------------------------------------------------------- firing
    // Each server in turn, on the same units, from the same timers.json with
    // a one-second tick: a due sleep timer fires, a start missed by more than
    // its grace is dropped and does not, a timer for a unit that is not there
    // is an error, a later one stays. What is left, and the counts, must agree.
    let fire_cfg = std::fs::read(real_dir.join("config.json")).unwrap();
    let mut results = Vec::new();
    for (name, port) in [("real", 29311u16), ("ucore", 29312u16)] {
        let via = if name == "real" { real } else { ucore };
        // Alpha on and Bravo off, through the server about to be tested.
        call(via, "POST", &format!("/api/units/{alpha}/control"), &AUTH, r#"{"power_state": true}"#);
        call(via, "POST", &format!("/api/units/{bravo}/control"), &AUTH, r#"{"power_state": false}"#);
        let dir = work.join(format!("fire-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), &fire_cfg).unwrap();
        devices_json(&dir);
        let t = |minutes: i64| (local_now() + chrono::TimeDelta::minutes(minutes)).format("%Y-%m-%dT%H:%M:%S").to_string();
        let off = json!({"power_state": false, "operational_mode": null, "target_temperature": null, "fan_speed": null, "swing_mode": null, "eco": null, "turbo": null, "beep": null});
        let on = json!({"power_state": true, "operational_mode": null, "target_temperature": null, "fan_speed": null, "swing_mode": null, "eco": null, "turbo": null, "beep": null});
        let seed = json!({"timers": [
            {"id": "0000000000a1", "unit_ids": [alpha.trim_matches('"')], "minutes": 5, "created_at": t(-6), "fires_at": t(-1), "settings": off, "label": "due"},
            {"id": "0000000000b1", "unit_ids": [bravo.trim_matches('"')], "minutes": 600, "created_at": t(-620), "fires_at": t(-20), "settings": on, "label": "missed", "kind": "start"},
            {"id": "0000000000c1", "unit_ids": ["999"], "minutes": 5, "created_at": t(-6), "fires_at": t(-1), "settings": off, "label": "nobody"},
            {"id": "0000000000d1", "unit_ids": [bravo.trim_matches('"')], "minutes": 600, "created_at": "2026-01-01T00:00:00", "fires_at": "2099-01-01T10:00:00", "settings": off, "label": "later"},
        ]});
        std::fs::write(dir.join("timers.json"), serde_json::to_string_pretty(&seed).unwrap()).unwrap();
        let bin = if name == "real" { core_bin.clone() } else { ucore_bin() };
        let server = Kill(
            Command::new(&bin)
                .arg("serve")
                .env("AC_CONFIG_DIR", &dir)
                .env("BREEZE_HOST", "127.0.0.1")
                .env("BREEZE_PORT", port.to_string())
                .env("BREEZE_LOG", "0")
                .env("AC_TIMER_TICK", "1")
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for(port, name);
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            let s = call(port, "GET", "/api/timers/status", &AUTH, "").json();
            if s["fired"].as_u64().unwrap_or(0) + s["errors"].as_u64().unwrap_or(0) >= 2 || Instant::now() > deadline {
                break s;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let a = call(port, "GET", &format!("/api/units/{alpha}/state"), &AUTH, "").json();
        let b = call(port, "GET", &format!("/api/units/{bravo}/state"), &AUTH, "").json();
        let left = std::fs::read(dir.join("timers.json")).unwrap();
        drop(server);
        assert!(status["runs"].as_u64().unwrap() >= 1, "{name}: {status}");
        assert_eq!((&a["power_state"], &b["power_state"]), (&json!(false), &json!(false)), "{name}: alpha switched off, bravo left off");
        results.push((name, status, String::from_utf8_lossy(&left).into_owned()));
    }
    let (r, u) = (&results[0], &results[1]);
    let counts = |s: &Value| (s["fired"].clone(), s["errors"].clone(), s["pending"].clone(), s["next_fires_at"].clone(), s["tick_seconds"].clone());
    if counts(&r.1) != counts(&u.1) || mask(&r.1.to_string()) != mask(&u.1.to_string()) {
        failures.push(format!("timers fired\n   real  {}\n   ucore {}", r.1, u.1));
    }
    assert_eq!(counts(&u.1), (json!(1), json!(1), json!(1), json!("2099-01-01T10:00:00"), json!(1)), "{}", u.1);
    if r.2 != u.2 {
        failures.push(format!("timers.json after firing\n   real  {}\n   ucore {}", r.2, u.2));
    }

    // ------------------------------------------------------ the scheduler
    // Each server in turn, from one programs.json, with a five-second tick:
    // a schedule entry for this minute (and the next, should the minute turn)
    // fires on Bravo, a curve sets Alpha, and a disabled program does nothing.
    let mut runs = Vec::new();
    for (name, port) in [("real", 29321u16), ("ucore", 29322u16)] {
        let via = if name == "real" { real } else { ucore };
        call(via, "POST", &format!("/api/units/{alpha}/control"), &AUTH, r#"{"power_state": false, "operational_mode": "HEAT", "target_temperature": 22}"#);
        call(via, "POST", &format!("/api/units/{bravo}/control"), &AUTH, r#"{"power_state": false, "operational_mode": "COOL"}"#);
        let dir = work.join(format!("sched-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), &fire_cfg).unwrap();
        devices_json(&dir);
        let hm = |plus: i64| (local_now() + chrono::TimeDelta::minutes(plus)).format("%H:%M").to_string();
        let null = Value::Null;
        let settings = |extra: Value| {
            let mut s = json!({"power_state": null, "operational_mode": null, "target_temperature": null, "fan_speed": null, "swing_mode": null, "eco": null, "turbo": null, "beep": null});
            for (k, v) in extra.as_object().unwrap() {
                s[k] = v.clone();
            }
            s
        };
        let fan_on = settings(json!({"power_state": true, "operational_mode": "FAN_ONLY"}));
        let seed = json!({"programs": [
            {"name": "Fan", "enabled": true, "unit_ids": [bravo.trim_matches('"')], "kind": "schedule", "favourite": null,
             "schedule": [{"days": [], "time": hm(0), "settings": fan_on}, {"days": [], "time": hm(1), "settings": fan_on}], "curve": null, "id": "00000000aa01"},
            {"name": "Flat 25", "enabled": true, "unit_ids": [alpha.trim_matches('"')], "kind": "curve", "favourite": null, "schedule": [],
             "curve": {"operational_mode": "COOL", "fan_speed": 102, "points": [{"time": "00:00", "temperature": 25.0}]}, "id": "00000000aa02"},
            {"name": "Off", "enabled": false, "unit_ids": [alpha.trim_matches('"')], "kind": "schedule", "favourite": null,
             "schedule": [{"days": [], "time": hm(0), "settings": settings(json!({"power_state": false}))}], "curve": null, "id": "00000000aa03"},
        ]});
        let _ = &null;
        let seeded = serde_json::to_string_pretty(&seed).unwrap();
        std::fs::write(dir.join("programs.json"), &seeded).unwrap();
        let bin = if name == "real" { core_bin.clone() } else { ucore_bin() };
        let server = Kill(
            Command::new(&bin)
                .arg("serve")
                .env("AC_CONFIG_DIR", &dir)
                .env("BREEZE_HOST", "127.0.0.1")
                .env("BREEZE_PORT", port.to_string())
                .env("BREEZE_LOG", "0")
                .env("AC_SCHED_TICK", "1")
                .env("AC_TIMER_TICK", "3600")
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_for(port, name);
        let deadline = Instant::now() + Duration::from_secs(25);
        let (a, b, status) = loop {
            std::thread::sleep(Duration::from_millis(500));
            let a = call(port, "GET", &format!("/api/units/{alpha}/state"), &AUTH, "").json();
            let b = call(port, "GET", &format!("/api/units/{bravo}/state"), &AUTH, "").json();
            let status = call(port, "GET", "/api/programs/status", &AUTH, "").json();
            let settled = a["target_temperature"] == json!(25.0) && b["operational_mode"] == json!("FAN_ONLY");
            if settled || Instant::now() > deadline {
                break (a, b, status);
            }
        };
        let left = std::fs::read_to_string(dir.join("programs.json")).unwrap();
        drop(server);
        assert_eq!(
            (&a["power_state"], &a["operational_mode"], &a["target_temperature"], &a["fan_speed"]),
            (&json!(true), &json!("COOL"), &json!(25.0), &json!(102)),
            "{name}: the curve set alpha, and the disabled program did not switch it off: {a}"
        );
        assert_eq!((&b["power_state"], &b["operational_mode"]), (&json!(true), &json!("FAN_ONLY")), "{name}: the schedule fired: {b}");
        assert_eq!(left, seeded, "{name}: the scheduler does not write programs.json");
        runs.push(status);
    }
    let shape_of = |s: &Value| (s["running"].clone(), s["tick_seconds"].clone(), s["errors"].clone(), s["runs"].as_u64().unwrap_or(0) > 0, s["last_run"].is_string());
    if shape_of(&runs[0]) != shape_of(&runs[1]) || mask(&runs[0].to_string()) != mask(&runs[1].to_string()) {
        failures.push(format!("the scheduler's status\n   real  {}\n   ucore {}", runs[0], runs[1]));
    }

    assert!(failures.is_empty(), "{} difference(s):\n{}", failures.len(), failures.join("\n"));
    drop((_ucore, _real, _mock));
    let _ = std::fs::remove_dir_all(&work);
}

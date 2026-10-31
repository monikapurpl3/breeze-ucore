//! Adding a unit by address, against Breeze Core: the probe (the hello on
//! 6445 and 20086, three seconds of listening), the 201, the update of a unit
//! already there, and the 404 when nothing answers.
//!
//! Breeze Core probes private addresses only, so the units must be on a
//! private network of their own: breeze-mock's dummy segment, which needs
//! Linux and CAP_NET_ADMIN. Run where that is so (a container):
//!
//!   BREEZE_DUMMY_SEGMENTS=1 BREEZE_CORE_BIN=... BREEZE_MOCK_BIN=... UCORE_BIN=... \
//!     ./add_by_address-<hash>
//!
//! Skipped otherwise.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const KEY: &str = "ucore-add-key-not-a-secret";
const TOKEN: &str = "ucore-add-device-token-not-a-secret-0123456789";

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn call(port: u16, method: &str, path: &str, body: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let auth = format!("X-API-Key: {KEY}\r\nAuthorization: Bearer {TOKEN}\r\n");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
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
        let n = n.trim().to_ascii_lowercase();
        if !matches!(n.as_str(), "connection" | "date" | "content-length" | "transfer-encoding" | "server") {
            headers.push((n, v.trim().to_string()));
        }
    }
    headers.sort();
    let mut body = Vec::new();
    r.read_to_end(&mut body).unwrap();
    (status, headers, body)
}

fn wait_for(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "nothing on {addr}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn units_are_added_by_address_as_breeze_core_adds_them() {
    let (Some(core_bin), Some(mock_bin), Some(ucore_bin)) = (
        std::env::var_os("BREEZE_CORE_BIN").map(PathBuf::from),
        std::env::var_os("BREEZE_MOCK_BIN").map(PathBuf::from),
        std::env::var_os("UCORE_BIN").map(PathBuf::from),
    ) else {
        eprintln!("skipped: needs BREEZE_CORE_BIN, BREEZE_MOCK_BIN and UCORE_BIN");
        return;
    };
    if std::env::var_os("BREEZE_DUMMY_SEGMENTS").is_none() {
        eprintln!("skipped: needs BREEZE_DUMMY_SEGMENTS=1 (Linux, CAP_NET_ADMIN)");
        return;
    }
    let work = std::env::temp_dir().join(format!("ucore-add-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let blueprint = work.join("units.xml");
    std::fs::write(
        &blueprint,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<blueprint xmlns="urn:breeze-mock:blueprint:1" version="1" name="ucore-add" seed="12">
  <environment><constant outdoor="10"/></environment>
  <network><segment name="lan" cidr="10.94.1.0/24" mode="dummy"/></network>
  <unit key="a" segment="lan" ip="10.94.1.2">
    <epithets name="Alpha"/><v3 token="auto" key="auto"/>
    <state power="on" mode="cool" target="24"/><behaviour reply-delay="0ms"/>
  </unit>
  <unit key="b" segment="lan" ip="10.94.1.3">
    <epithets name="Bravo"/><v3 token="auto" key="auto"/>
    <state power="off" mode="heat" target="22"/><behaviour reply-delay="0ms"/>
  </unit>
</blueprint>
"#,
    )
    .unwrap();
    let _mock = Kill(
        Command::new(&mock_bin).args(["run", blueprint.to_str().unwrap(), "-q"]).env("BREEZE_MOCK_DATA", work.join("mock")).spawn().unwrap(),
    );
    wait_for("10.94.1.3:6444");
    let out = Command::new(&mock_bin)
        .args(["export", "breeze-core-config", blueprint.to_str().unwrap()])
        .env("BREEZE_MOCK_DATA", work.join("mock"))
        .output()
        .unwrap();
    let exported: Value = serde_json::from_slice(&out.stdout).unwrap();
    let alpha = &exported["units"][0];
    // Both servers start with no units at all.
    let config = json!({"api_key": KEY, "units": []});
    let devices = json!({"devices": [{"token_id": "add-device", "label": "add", "auth_version": 1,
        "token_hash": ucore::auth::hash_secret(TOKEN), "public_key": null, "created_at": 1790000000.0,
        "expires_at": null, "last_used": null}]});
    let dirs = [work.join("real"), work.join("ucore")];
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("config.json"), serde_json::to_string_pretty(&config).unwrap()).unwrap();
        std::fs::write(d.join("devices.json"), serde_json::to_string_pretty(&devices).unwrap()).unwrap();
    }
    let (real, ucore) = (29401u16, 29402u16);
    let start = |bin: &PathBuf, dir: &PathBuf, port: u16| {
        Kill(
            Command::new(bin)
                .arg("serve")
                .env("AC_CONFIG_DIR", dir)
                .env("BREEZE_HOST", "127.0.0.1")
                .env("BREEZE_PORT", port.to_string())
                .env("BREEZE_LOG", "0")
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let _real = start(&core_bin, &dirs[0], real);
    let _ucore = start(&ucore_bin, &dirs[1], ucore);
    wait_for(&format!("127.0.0.1:{real}"));
    wait_for(&format!("127.0.0.1:{ucore}"));

    let with_creds = format!(
        r#"{{"ip": "10.94.1.2", "name": "Added", "token": "{}", "key": "{}"}}"#,
        alpha["token"].as_str().unwrap(),
        alpha["key"].as_str().unwrap()
    );
    let mut failures = Vec::new();
    for (path, method, body) in [
        ("/api/units", "POST", with_creds.as_str()),
        // Without credentials: added, and says it has none.
        ("/api/units", "POST", r#"{"ip": "10.94.1.3"}"#),
        // Nothing there: three seconds, then 404.
        ("/api/units", "POST", r#"{"ip": "10.94.1.50"}"#),
        // Again, renamed: the same unit, updated, its credentials kept.
        ("/api/units", "POST", r#"{"ip": "10.94.1.2", "name": "Renamed by adding"}"#),
        // No cloud in μCore, and Breeze Core's answer for a build without one
        // differs only in that it would ask Midea; so not compared, not sent.
        ("/api/config", "GET", ""),
        ("/api/units", "GET", ""),
    ] {
        let (r, u) = (call(real, method, path, body), call(ucore, method, path, body));
        if r != u {
            failures.push(format!(
                "{method} {path} {body}\n   real  {} {:?} {}\n   ucore {} {:?} {}",
                r.0,
                r.1,
                String::from_utf8_lossy(&r.2),
                u.0,
                u.1,
                String::from_utf8_lossy(&u.2)
            ));
        }
    }
    // The added unit is drivable at once, on both.
    let id = alpha["id"].to_string();
    for port in [real, ucore] {
        let (status, _, body) = call(port, "GET", &format!("/api/units/{id}/state"), "");
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!((status, v["online"].clone(), v["name"].clone()), (200, json!(true), json!("Renamed by adding")), "{port}: {v}");
    }
    let (rc, uc) = (std::fs::read(dirs[0].join("config.json")).unwrap(), std::fs::read(dirs[1].join("config.json")).unwrap());
    if rc != uc {
        failures.push(format!("config.json\n   real  {}\n   ucore {}", String::from_utf8_lossy(&rc), String::from_utf8_lossy(&uc)));
    }
    assert!(failures.is_empty(), "{} difference(s):\n{}", failures.len(), failures.join("\n"));
}

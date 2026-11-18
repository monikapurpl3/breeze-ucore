//! The helper's HTTP side: the page, and the API it drives.
//!
//! Who may use it: the helper listens on 127.0.0.1 only; a request must name
//! this helper in `Host` (so a DNS-rebinding page cannot reach it), come from
//! its own page or aspic's (`Origin`), and carry the token the helper gave
//! its page when it opened it. The page itself may only talk to the helper:
//! its Content-Security-Policy says so, and the browser enforces it.

use std::io::Read;
use std::sync::Arc;

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response};

use crate::{boards, firmware, qemu, store, units, zones, App};

/// Where the published page lives; it may drive a helper too.
pub const ASPIC_ORIGIN: &str = "https://aspic.salataputarica.hr.eu.org";

/// The page may load only itself and talk only to this helper.
const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; \
                        base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

const PAGE: &[(&str, &str, &str)] = &[
    ("/", "text/html; charset=utf-8", include_str!("../web/index.html")),
    ("/app.js", "text/javascript; charset=utf-8", include_str!("../web/app.js")),
    ("/style.css", "text/css; charset=utf-8", include_str!("../web/style.css")),
];

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("an ASCII header")
}

fn reply(status: u16, body: Value, origin: Option<&str>) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = Response::from_data(serde_json::to_vec(&body).unwrap_or_default())
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"));
    if let Some(o) = origin {
        r = r.with_header(header("Access-Control-Allow-Origin", o)).with_header(header("Vary", "Origin"));
    }
    r
}

fn error(status: u16, message: impl Into<String>) -> (u16, Value) {
    (status, json!({ "error": message.into() }))
}

pub fn serve(app: Arc<App>, request: Request) {
    let path = request.url().split('?').next().unwrap_or("/").to_string();
    let host = request.headers().iter().find(|h| h.field.equiv("Host")).map(|h| h.value.as_str().to_string()).unwrap_or_default();
    let ours = [format!("127.0.0.1:{}", app.port), format!("localhost:{}", app.port)];
    if !ours.contains(&host) {
        let _ = request.respond(reply(421, json!({"error": "this is not the host the deployer answers to"}), None));
        return;
    }

    if !path.starts_with("/api/") {
        let page = PAGE.iter().find(|(p, _, _)| *p == path || (path == "/index.html" && *p == "/"));
        let response = match page {
            Some((_, kind, body)) => Response::from_data(body.as_bytes().to_vec())
                .with_header(header("Content-Type", kind))
                .with_header(header("Content-Security-Policy", PAGE_CSP))
                .with_header(header("Referrer-Policy", "no-referrer"))
                .with_header(header("X-Content-Type-Options", "nosniff"))
                .with_header(header("Cache-Control", "no-store")),
            None => Response::from_data(b"not found".to_vec()).with_status_code(404),
        };
        let _ = request.respond(response);
        return;
    }

    let origin = request.headers().iter().find(|h| h.field.equiv("Origin")).map(|h| h.value.as_str().to_string());
    let own = format!("http://127.0.0.1:{}", app.port);
    let allowed = match origin.as_deref() {
        // Not a browser asking for another page: curl, or the page itself
        // on a same-origin GET.
        None => None,
        Some(o) if o == own || o == format!("http://localhost:{}", app.port) || o == ASPIC_ORIGIN => Some(o.to_string()),
        Some(_) => {
            let _ = request.respond(reply(403, json!({"error": "only the deployer's own page may use this"}), None));
            return;
        }
    };
    // The aspic page asks first (CORS, and Chrome's private-network check).
    if *request.method() == Method::Options {
        let mut r = Response::empty(204)
            .with_header(header("Access-Control-Allow-Methods", "GET, POST, PUT, PATCH, DELETE"))
            .with_header(header("Access-Control-Allow-Headers", "authorization, content-type"))
            .with_header(header("Access-Control-Allow-Private-Network", "true"))
            .with_header(header("Access-Control-Max-Age", "600"));
        if let Some(o) = &allowed {
            r = r.with_header(header("Access-Control-Allow-Origin", o));
        }
        let _ = request.respond(r);
        return;
    }
    let token = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.as_str().to_string()).unwrap_or_default();
    if token.strip_prefix("Bearer ").map(str::trim) != Some(app.token.as_str()) {
        let _ = request.respond(reply(401, json!({"error": "this page has not got the deployer's token: open it from the link the deployer printed"}), allowed.as_deref()));
        return;
    }

    let mut request = request;
    let mut body = Vec::new();
    let _ = request.as_reader().take(4 << 20).read_to_end(&mut body);
    let query = request.url().split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
    let method = request.method().clone();
    let (status, value) = route(&app, &method, &path, &query, &body);
    // A request may have carried a password: gone from memory with it.
    body.iter_mut().for_each(|b| *b = 0);
    let _ = request.respond(reply(status, value, allowed.as_deref()));
}

fn query_number(query: &str, key: &str) -> u64 {
    query.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == key).and_then(|(_, v)| v.parse().ok()).unwrap_or(0)
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, (u16, Value)> {
    serde_json::from_slice(body).map_err(|e| error(400, format!("that request is not what was expected: {e}")))
}

fn route(app: &Arc<App>, method: &Method, path: &str, query: &str, body: &[u8]) -> (u16, Value) {
    let parts: Vec<&str> = path.trim_start_matches("/api/").split('/').filter(|s| !s.is_empty()).collect();
    let result: Result<Value, (u16, Value)> = match (method, parts.as_slice()) {
        (Method::Get, ["hello"]) => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "port": app.port,
            "data": app.data.display().to_string(),
            "source": app.local_source.as_ref().map(|p| p.display().to_string()),
        })),
        (Method::Get, ["connections"]) => Ok(json!(app.log.all())),
        (Method::Get, ["zones"]) => Ok(json!(zones::all().into_iter().map(|(n, r)| json!({"name": n, "rule": r})).collect::<Vec<_>>())),
        (Method::Get, ["boards"]) => Ok(json!(boards::built_in())),
        (Method::Get, ["jobs", id]) => match id.parse().ok().and_then(|id| app.jobs.get(id, query_number(query, "since") as usize)) {
            Some(s) => Ok(json!(s)),
            None => Err(error(404, "no such job")),
        },
        (Method::Get, ["deployments"]) => Ok(json!(app
            .store
            .list()
            .into_iter()
            .map(|d| {
                let units = app.store.config(&d.id).map(|c| c.units.len()).unwrap_or(0);
                json!({"id": d.id, "name": d.name, "board": d.board.name, "units": units, "running": app.qemu.is_running(&d.id)})
            })
            .collect::<Vec<_>>())),
        (Method::Post, ["deployments"]) => (|| {
            #[derive(serde::Deserialize)]
            struct New {
                name: String,
                board: boards::Board,
            }
            let n: New = parse(body)?;
            let d = app.store.create(&n.name, n.board).map_err(|e| error(422, e))?;
            Ok(json!({"id": d.id}))
        })(),
        (_, ["deployments", id, rest @ ..]) => deployment(app, method, id, rest, query, body),
        _ => Err(error(404, "no such call")),
    };
    match result {
        Ok(v) => (200, v),
        Err(e) => e,
    }
}

fn view(app: &App, d: &store::Deployment) -> Result<Value, (u16, Value)> {
    let config = app.store.config(&d.id).map_err(|e| error(500, e))?;
    let dir = app.store.dir(&d.id).map_err(|e| error(404, e))?;
    let units: Vec<Value> = config
        .units
        .iter()
        .map(|u| json!({"id": u.id.to_string(), "name": u.name, "ip": u.ip, "port": u.port, "has_credentials": u.token.is_some() && u.key.is_some()}))
        .collect();
    let mut env = serde_json::to_value(&d.env).unwrap_or_default();
    // The page never gets the password back; only whether there is one.
    env["wifi_password"] = json!(null);
    env["has_wifi_password"] = json!(!d.env.wifi_password.is_empty());
    Ok(json!({
        "id": d.id, "name": d.name, "board": d.board, "env": env, "zone": d.zone, "qemu_port": d.qemu_port, "units": units,
        "built": dir.join("flash.bin").exists(), "running": app.qemu.is_running(&d.id),
    }))
}

fn deployment(app: &Arc<App>, method: &Method, id: &str, rest: &[&str], query: &str, body: &[u8]) -> Result<Value, (u16, Value)> {
    let mut d = app.store.load(id).map_err(|e| error(404, e))?;
    let dir = app.store.dir(id).map_err(|e| error(404, e))?;
    match (method, rest) {
        (Method::Get, []) => view(app, &d),
        (Method::Delete, []) => {
            app.qemu.stop(id);
            app.store.remove(id).map_err(|e| error(500, e))?;
            Ok(json!({}))
        }
        (Method::Put, ["name"]) => {
            #[derive(serde::Deserialize)]
            struct Name {
                name: String,
            }
            let n: Name = parse(body)?;
            if n.name.trim().is_empty() {
                return Err(error(422, "a deployment needs a name"));
            }
            d.name = n.name.trim().into();
            app.store.save(&d).map_err(|e| error(500, e))?;
            view(app, &d)
        }
        (Method::Put, ["board"]) => {
            let mut b: boards::Board = parse(body)?;
            b.check().map_err(|e| error(422, e))?;
            if b.id.is_empty() {
                b.id = "custom".into();
            }
            d.board = b;
            app.store.save(&d).map_err(|e| error(500, e))?;
            view(app, &d)
        }
        (Method::Put, ["env"]) => {
            #[derive(serde::Deserialize)]
            struct EnvIn {
                wifi_ssid: String,
                /// `null` keeps the one there is.
                wifi_password: Option<String>,
                tz: String,
                #[serde(default)]
                zone: String,
                ntp: String,
                hostname: String,
            }
            let e: EnvIn = parse(body)?;
            if ucore::tz::Rule::parse(e.tz.trim()).is_none() {
                return Err(error(422, format!("{:?} is not a time zone rule μCore can read", e.tz)));
            }
            if !e.zone.trim().is_empty() && zones::rule(e.zone.trim()) != Some(e.tz.trim()) {
                return Err(error(422, format!("{:?} is not {}'s rule", e.tz, e.zone)));
            }
            if e.ntp.trim().is_empty() || e.hostname.trim().is_empty() {
                return Err(error(422, "the time server and the hostname cannot be empty"));
            }
            d.env.wifi_ssid = e.wifi_ssid.trim().into();
            if let Some(p) = e.wifi_password {
                d.env.wifi_password = p;
            }
            d.env.tz = e.tz.trim().into();
            d.zone = e.zone.trim().into();
            d.env.ntp = e.ntp.trim().into();
            d.env.hostname = e.hostname.trim().into();
            app.store.save(&d).map_err(|e| error(500, e))?;
            view(app, &d)
        }
        (Method::Get, ["apikey"]) => Ok(json!({"api_key": app.store.config(id).map_err(|e| error(500, e))?.api_key})),
        (Method::Post, ["units", "import"]) => {
            #[derive(serde::Deserialize)]
            struct Text {
                text: String,
            }
            let t: Text = parse(body)?;
            let mut config = app.store.config(id).map_err(|e| error(500, e))?;
            let (added, updated) = units::import(&mut config, &t.text).map_err(|e| error(422, e))?;
            app.store.save_config(id, &config).map_err(|e| error(500, e))?;
            Ok(json!({"added": added, "updated": updated}))
        }
        (Method::Post, ["units", "fetch"]) => {
            let req: units::FetchRequest = parse(body)?;
            let title = format!("fetch:{id}");
            if app.jobs.running(&title) {
                return Err(error(409, "already fetching for this deployment"));
            }
            let (app2, id2) = (Arc::clone(app), id.to_string());
            let job = app.jobs.spawn(&title, move |say| {
                let mut config = app2.store.config(&id2)?;
                let (added, updated, missing) = units::fetch(req, &mut config, &app2.log, say)?;
                app2.store.save_config(&id2, &config)?;
                say.line(format!("Done: {added} added, {updated} updated, {missing} without a token."));
                Ok(json!({"added": added, "updated": updated, "missing": missing}))
            });
            Ok(json!({"job": job}))
        }
        (Method::Patch, ["units", unit]) | (Method::Delete, ["units", unit]) => {
            let mut config = app.store.config(id).map_err(|e| error(500, e))?;
            let Some(i) = config.units.iter().position(|u| u.id.to_string() == *unit) else {
                return Err(error(404, "no such unit"));
            };
            if *method == Method::Delete {
                config.units.remove(i);
            } else {
                #[derive(serde::Deserialize)]
                struct Name {
                    name: String,
                }
                let n: Name = parse(body)?;
                let name = n.name.trim();
                if name.is_empty() || name.chars().count() > 64 {
                    return Err(error(422, "a unit's name is 1 to 64 characters"));
                }
                config.units[i].name = name.into();
            }
            app.store.save_config(id, &config).map_err(|e| error(500, e))?;
            view(app, &d)
        }
        (Method::Post, ["build"]) => {
            let title = format!("build:{id}");
            if app.jobs.running(&title) {
                return Err(error(409, "already building this deployment"));
            }
            let app2 = Arc::clone(app);
            let job = app.jobs.spawn(&title, move |say| build(&app2, &d, say));
            Ok(json!({"job": job}))
        }
        (Method::Post, ["configure"]) => {
            let title = format!("configure:{id}");
            let app2 = Arc::clone(app);
            let job = app.jobs.spawn(&title, move |say| configure(&app2, &d, say));
            Ok(json!({"job": job}))
        }
        (Method::Post, ["run"]) => {
            app.qemu.start(id, &d.board, &dir, d.qemu_port).map_err(|e| error(422, e))?;
            view(app, &d)
        }
        (Method::Post, ["stop"]) => {
            app.qemu.stop(id);
            view(app, &d)
        }
        (Method::Get, ["serial"]) => {
            let (text, len) = qemu::serial(&dir, query_number(query, "since"));
            Ok(json!({"text": text, "length": len, "running": app.qemu.is_running(id)}))
        }
        _ => Err(error(404, "no such call")),
    }
}

/// The emulated C6 cannot read its flash back, so its bundle is built into
/// the firmware from a seed: the deployment's files, for this build only.
fn seed_for(app: &App, d: &store::Deployment) -> Result<Option<std::path::PathBuf>, String> {
    if !(d.board.is_qemu() && d.board.chip == "esp32c6") {
        return Ok(None);
    }
    let seed = app.store.dir(&d.id)?.join("seed");
    std::fs::create_dir_all(&seed).map_err(|e| e.to_string())?;
    store::write_private(&seed.join(ucore::bundle::CONFIG), &ucore::store::to_json(&app.store.config(&d.id)?))?;
    store::write_private(&seed.join(ucore::bundle::ENV), &serde_json::to_vec_pretty(&d.env).map_err(|e| e.to_string())?)?;
    Ok(Some(seed))
}

fn build(app: &App, d: &store::Deployment, say: &crate::jobs::Say) -> Result<Value, String> {
    d.board.check()?;
    if d.board.is_qemu() {
        qemu::find(&d.board)?;
    }
    let src = app.source(say)?;
    app.tools.ensure(&d.board.chip, &app.log, say)?;
    let seed = seed_for(app, d)?;
    let elf = app.tools.build(&src, &app.data.join("build"), &d.board, seed.as_deref(), say)?;
    let dir = app.store.dir(&d.id)?;
    let image = dir.join("flash.bin");
    // Rebuilding keeps what μCore stored: paired phones, programs, timers.
    let (store_at, store_size) = firmware::partition(&src, "ucore")?;
    let kept = if image.exists() { firmware::region(&image, store_at, store_size).ok() } else { None };
    let was_running = app.qemu.is_running(&d.id);
    app.qemu.stop(&d.id);
    let fresh = dir.join("flash.new.bin");
    app.tools.image(&src, &d.board, &elf, &fresh, say)?;
    if let Some(bytes) = &kept {
        firmware::patch(&fresh, store_at, bytes)?;
        say.line("Kept the store from the last build: paired phones, programs and timers.");
    }
    let (bundle_at, _) = firmware::partition(&src, "ucorecfg")?;
    firmware::patch(&fresh, bundle_at, &firmware::bundle_image(&src, &app.store.config(&d.id)?, d)?)?;
    std::fs::rename(&fresh, &image).map_err(|e| format!("{}: {e}", image.display()))?;
    if let Some(seed) = seed {
        let _ = std::fs::remove_dir_all(seed);
    }
    say.line(format!("Built: {}", image.display()));
    if was_running {
        app.qemu.start(&d.id, &d.board, &dir, d.qemu_port)?;
        say.line("Started it again.");
    }
    Ok(json!({"image": image.display().to_string()}))
}

/// Only the configuration: the bundle, written into the board's flash. The
/// firmware and the store are left alone.
fn configure(app: &App, d: &store::Deployment, say: &crate::jobs::Say) -> Result<Value, String> {
    if d.board.is_qemu() && d.board.chip == "esp32c6" {
        say.line("An emulated ESP32-C6 carries its configuration in its firmware (QEMU cannot read its flash back), so this is a rebuild.");
        return build(app, d, say);
    }
    if !d.board.is_qemu() {
        return Err("Writing a configuration to a real board over USB is not built yet: there are only QEMU boards so far.".into());
    }
    let dir = app.store.dir(&d.id)?;
    let image = dir.join("flash.bin");
    if !image.exists() {
        return Err("build it first: there is no flash image yet".into());
    }
    let src = app.source(say)?;
    let was_running = app.qemu.is_running(&d.id);
    app.qemu.stop(&d.id);
    let (at, _) = firmware::partition(&src, "ucorecfg")?;
    firmware::patch(&image, at, &firmware::bundle_image(&src, &app.store.config(&d.id)?, d)?)?;
    say.line("Wrote the new configuration (config.json and env.json). The firmware and the store were not touched.");
    if was_running {
        app.qemu.start(&d.id, &d.board, &dir, d.qemu_port)?;
        say.line("Started it again with the new configuration.");
    }
    Ok(json!({}))
}

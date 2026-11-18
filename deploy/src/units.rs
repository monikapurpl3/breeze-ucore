//! The units: brought in from an existing Breeze Core's config.json, or found
//! on this network and given their tokens by Midea's cloud, through
//! breeze-token's library. breeze-token's promises are kept here as it keeps
//! them: the password goes only to a cloud that has the account, and only
//! for one sign-in; every token is tried on its unit before it is kept;
//! tokens are never sent to the page, logged or shown.

use breeze_token::breeze_cloud::{Cloud, CloudError, Credentials};
use breeze_token::breeze_proto::discover::{Discovered, Version};
use breeze_token::{clouds, lan};
use serde::Deserialize;
use ucore::store::{AppConfig, UnitConfig};

use crate::jobs::Say;
use crate::net;

/// What the page sends to fetch tokens. The password is used once and
/// dropped with the request.
#[derive(Deserialize)]
pub struct FetchRequest {
    pub account: String,
    pub password: String,
    /// `smarthome`, `meiju`, `nethome`, or empty for "not sure".
    #[serde(default)]
    pub app: String,
    /// `around` (this computer's /24), `subnet` or `addresses`.
    #[serde(default)]
    pub look: String,
    /// The subnet, or the addresses separated by spaces or commas.
    #[serde(default)]
    pub at: String,
    /// Fetch again for units that have a token already.
    #[serde(default)]
    pub again: bool,
}

fn cloud_host(cloud: Cloud) -> &'static str {
    match cloud {
        Cloud::SmartHome => "mp-prod.appsmb.com (MSmartHome, and the regional server it names)",
        Cloud::Meiju => "mp-prod.smartmidea.net (Meiju)",
        Cloud::NetHomePlus => "mapp.appsmb.com (NetHome Plus)",
    }
}

fn parse_app(app: &str) -> Option<Cloud> {
    match app {
        "smarthome" => Some(Cloud::SmartHome),
        "meiju" => Some(Cloud::Meiju),
        "nethome" => Some(Cloud::NetHomePlus),
        _ => None,
    }
}

/// Bring in the units of another Breeze Core's config.json: new ones are
/// added, ones already here get the new address and credentials and keep
/// their names. Returns how many were added and updated.
pub fn import(config: &mut AppConfig, text: &str) -> Result<(usize, usize), String> {
    let other: AppConfig = serde_json::from_str(text).map_err(|e| format!("that is not a Breeze Core config.json: {e}"))?;
    if other.units.is_empty() {
        return Err("that config.json has no units".into());
    }
    let (mut added, mut updated) = (0, 0);
    for u in other.units {
        match config.units.iter_mut().find(|x| x.id == u.id) {
            Some(x) => {
                x.ip = u.ip;
                x.port = u.port;
                if u.token.is_some() && u.key.is_some() {
                    x.token = u.token;
                    x.key = u.key;
                }
                updated += 1;
            }
            None => {
                config.units.push(u);
                added += 1;
            }
        }
    }
    Ok((added, updated))
}

/// Find the units, sign in, fetch and check their tokens, and put them in
/// `config`. Returns how many units were added, updated, and left without.
pub fn fetch(req: FetchRequest, config: &mut AppConfig, log: &net::Log, say: &Say) -> Result<(usize, usize, usize), String> {
    let account = req.account.trim().to_string();
    if account.is_empty() || req.password.is_empty() {
        return Err("the account and its password are both needed".into());
    }
    let clouds = clouds::clouds()?;

    // --- the units, on this network
    let addresses: Vec<String> = req.at.split([' ', ',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    let look = match req.look.as_str() {
        "subnet" if !req.at.trim().is_empty() => lan::Look::Subnet(req.at.trim()),
        "addresses" if !addresses.is_empty() => lan::Look::At(&addresses),
        _ => lan::Look::Around,
    };
    say.line("Looking for air conditioners on this network...");
    log.note("lan", "this network", "looked for air conditioners (Midea's discovery, UDP ports 6445 and 20086)");
    let found = lan::find(look)?;
    for (ip, why) in &found.unreadable {
        say.line(format!("  something at {ip} answered but could not be read: {why}"));
    }
    let named = matches!(req.look.as_str(), "addresses");
    let mut wanted: Vec<Discovered> = Vec::new();
    for unit in found.units {
        let has = config.units.iter().any(|u| u.id as u64 == unit.id && u.token.is_some() && u.key.is_some());
        let what = match unit.version {
            _ if unit.device_type != 0xAC => format!("not an air conditioner (type 0x{:02X}), skipped", unit.device_type),
            Version::V3 if has && !req.again && !named => "V3; its token and key are here already".into(),
            Version::V3 => "V3, needs a token and key".into(),
            other => format!("{other:?}: μCore drives V3 units only, skipped"),
        };
        say.line(format!("  {}  id {}  {what}", unit.ip, unit.id));
        if unit.device_type == 0xAC && unit.version == Version::V3 && (req.again || named || !has) {
            wanted.push(unit);
        }
    }
    if wanted.is_empty() {
        say.line("Nothing here needs a token.");
        return Ok((0, 0, 0));
    }

    // --- which cloud has the account: asked by name only, no password
    say.line("Asking Midea's clouds which one has this account (by name only; no password is sent for this)...");
    let mut order = vec![Cloud::SmartHome, Cloud::Meiju, Cloud::NetHomePlus];
    if let Some(c) = parse_app(&req.app) {
        order.retain(|o| *o != c);
        order.insert(0, c);
    }
    let mut knows = Vec::new();
    for cloud in order {
        log.note("midea", cloud_host(cloud), "asked whether it has the account (no password sent)");
        match clouds.account_known(cloud, &account) {
            Ok(k) => {
                say.line(format!("  {:<13} {}", cloud.short_name(), if k { "has this account" } else { "does not know it" }));
                if k {
                    knows.push(cloud);
                }
            }
            Err(e) => say.line(format!("  {:<13} could not be asked: {e}", cloud.short_name())),
        }
    }
    let chosen = parse_app(&req.app).filter(|c| knows.contains(c) && *c != Cloud::NetHomePlus);
    let Some(cloud) = chosen.or_else(|| [Cloud::SmartHome, Cloud::Meiju].into_iter().find(|c| knows.contains(c))) else {
        if knows.contains(&Cloud::NetHomePlus) {
            return Err("This is a NetHome Plus account, and NetHome Plus no longer hands out tokens to anyone. \
                        Add the units in the MSmartHome app (with a new MSmartHome account if need be), then fetch with that account."
                .into());
        }
        return Err("None of Midea's clouds has that account. Check it is exactly what you sign in to the app with.".into());
    };

    // --- signing in: the password's one use
    log.note("midea", cloud_host(cloud), "signed in with your account, to fetch the units' tokens");
    let session = {
        let credentials = Credentials::new(account.as_str(), req.password, "");
        match clouds.login(cloud, &credentials) {
            Ok(s) => s,
            Err(CloudError::Api { code: 3101 | 3102, .. }) => return Err(format!("{} did not accept that password.", cloud.app_name())),
            Err(e) => return Err(format!("Could not sign in to {}: {e}. {}", cloud.app_name(), e.advice())),
        }
        // `credentials` is dropped here, and scrubs the password.
    };
    say.line(format!("Signed in to {}.", cloud.app_name()));

    log.note("midea", cloud_host(cloud), "listed the account's units, for their names");
    let listed = session.appliances().ok().flatten();
    let (mut added, mut updated, mut missing) = (0, 0, 0);
    for unit in &wanted {
        let name = listed.as_ref().and_then(|l| l.iter().find(|a| a.id == unit.id)).map(|a| a.name.clone());
        let shown = name.clone().unwrap_or_else(|| format!("the unit at {}", unit.ip));
        if listed.is_some() && name.is_none() {
            missing += 1;
            say.line(format!("  {shown}: not on this account. Midea gives a token only to the account the unit is paired with."));
            continue;
        }
        log.note("midea", cloud_host(cloud), "fetched a unit's token and key");
        let token = match session.token(unit.id) {
            Ok(t) => t,
            Err(e) => {
                missing += 1;
                say.line(format!("  {shown}: no token: {e}"));
                continue;
            }
        };
        log.note("lan", &unit.ip.to_string(), "tried the token on the unit (a handshake and a state read; nothing changed)");
        if let Err(e) = lan::works(unit, &token) {
            missing += 1;
            say.line(format!("  {shown}: did not accept its token ({e}). Is it on, and reachable from this computer?"));
            continue;
        }
        say.line(format!("  OK  {shown}: token checked on the unit."));
        let Ok(id) = i64::try_from(unit.id) else {
            missing += 1;
            continue;
        };
        match config.units.iter_mut().find(|u| u.id == id) {
            Some(u) => {
                u.ip = unit.ip.to_string();
                u.port = unit.port;
                u.token = Some(token.token);
                u.key = Some(token.key);
                updated += 1;
            }
            None => {
                config.units.push(UnitConfig {
                    name: name.unwrap_or_else(|| format!("AC {}", unit.ip)),
                    ip: unit.ip.to_string(),
                    port: unit.port,
                    id,
                    token: Some(token.token),
                    key: Some(token.key),
                });
                added += 1;
            }
        }
    }
    Ok((added, updated, missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(id: i64, name: &str, token: Option<&str>) -> UnitConfig {
        UnitConfig { name: name.into(), ip: "10.0.0.2".into(), port: 6444, id, token: token.map(String::from), key: token.map(String::from) }
    }

    #[test]
    fn import_adds_and_updates_keeping_names() {
        let mut config = AppConfig { api_key: Some("k".into()), units: vec![unit(1, "Kept name", None)] };
        let other = AppConfig { api_key: Some("other".into()), units: vec![unit(1, "Their name", Some("t")), unit(2, "New", Some("u"))] };
        let (added, updated) = import(&mut config, &serde_json::to_string(&other).unwrap()).unwrap();
        assert_eq!((added, updated), (1, 1));
        assert_eq!(config.api_key.as_deref(), Some("k"), "the deployment keeps its own key");
        assert_eq!(config.units[0].name, "Kept name");
        assert_eq!(config.units[0].token.as_deref(), Some("t"));
        assert_eq!(config.units[1].name, "New");
        assert!(import(&mut config, "nope").is_err());
        assert!(import(&mut config, r#"{"units": []}"#).is_err());
    }
}

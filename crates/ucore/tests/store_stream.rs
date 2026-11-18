//! A store file written a piece at a time must be the very bytes the whole
//! document serialises to: Breeze Core reads these files, and the store's
//! byte-for-byte round trip is a promise.

use serde_json::{json, Value};
use ucore::store::{to_json, write_list};

fn streamed<T: serde::Serialize>(key: &str, items: &[T]) -> Vec<u8> {
    let mut out = Vec::new();
    write_list(key, items, &mut |b| {
        out.extend_from_slice(b);
        Ok(())
    })
    .unwrap();
    out
}

/// A small deterministic generator (xorshift), so a failure reproduces.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn string(&mut self) -> String {
        const PIECES: &[&str] = &["a", "Z", " ", "\"", "\\", "\n", "\r\n", "\t", "\u{1}", "é", "š", "Ш", "极", "📱", "{", "}", "[", "]", ",", ":", "null", "\u{2028}"];
        (0..self.below(8)).map(|_| PIECES[self.below(PIECES.len() as u64) as usize]).collect()
    }

    fn value(&mut self, depth: u32) -> Value {
        match self.below(if depth > 3 { 6 } else { 8 }) {
            0 => Value::Null,
            1 => Value::Bool(self.below(2) == 1),
            2 => json!(self.next() as i64 >> self.below(60)),
            3 => {
                let f = (self.next() as i64 >> 20) as f64 / (1u64 << self.below(30)) as f64;
                json!(if self.below(4) == 0 { f.trunc() } else { f })
            }
            4 | 5 => Value::String(self.string()),
            6 => Value::Array((0..self.below(4)).map(|_| self.value(depth + 1)).collect()),
            _ => Value::Object((0..self.below(4)).map(|_| (self.string(), self.value(depth + 1))).collect()),
        }
    }
}

#[test]
fn any_list_streams_to_the_same_bytes() {
    let mut g = Gen(0x9e37_79b9_7f4a_7c15);
    for case in 0..3000 {
        let items: Vec<Value> = (0..g.below(5)).map(|_| g.value(0)).collect();
        let key = ["programs", "timers", "devices", "k\"ey"][case % 4];
        let whole = to_json(&json!({ key: items }));
        assert_eq!(String::from_utf8_lossy(&streamed(key, &items)), String::from_utf8_lossy(&whole), "case {case}");
    }
}

#[test]
fn the_store_documents_stream_to_their_own_bytes() {
    let devices: ucore::store::DevicesDoc = serde_json::from_value(json!({"devices": [
        {"token_id": "a1", "label": "Mama's \"phone\" \\ 📱\n", "auth_version": 1, "token_hash": "ab", "public_key": null,
         "created_at": 1790000000.0, "expires_at": null, "last_used": 1790000123.456},
        {"token_id": "b2", "label": "", "auth_version": 2, "token_hash": null, "public_key": "AAAA",
         "created_at": 1790000001.5, "expires_at": 2105360000.0, "last_used": null}
    ]}))
    .unwrap();
    assert_eq!(streamed("devices", &devices.devices), to_json(&devices));
    let empty = ucore::store::DevicesDoc::default();
    assert_eq!(streamed("devices", &empty.devices), to_json(&empty));

    let programs: ucore::programs::ProgramsDoc = serde_json::from_value(json!({"programs": [
        {"name": "Fan", "enabled": true, "unit_ids": ["1"], "kind": "schedule", "favourite": null,
         "schedule": [{"days": [], "time": "07:30", "settings": {"power_state": true, "operational_mode": "FAN_ONLY"}},
                      {"days": [0, 6], "time": "22:00", "settings": {"power_state": false}}], "curve": null, "id": "00000000aa01"},
        {"name": "Flat 25", "enabled": true, "unit_ids": [], "kind": "curve", "favourite": null, "schedule": [],
         "curve": {"operational_mode": "COOL", "fan_speed": 102, "points": [{"time": "00:00", "temperature": 25.0}, {"time": "12:00", "temperature": 21.5}]}, "id": "00000000aa02"},
        {"name": "Cosy", "enabled": false, "unit_ids": ["1", "2"], "kind": "favourite",
         "favourite": {"power_state": true, "operational_mode": "HEAT", "target_temperature": 23.5}, "schedule": [], "curve": null, "id": "00000000aa03"}
    ]}))
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&streamed("programs", &programs.programs)), String::from_utf8_lossy(&to_json(&programs)));
}

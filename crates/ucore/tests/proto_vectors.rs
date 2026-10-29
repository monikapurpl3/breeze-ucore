//! The protocol vectors Breeze Core's `breeze-proto` is held to, ported: msmart-ng
//! captures (msmart/device/AC/test_command.py, msmart-ng 2026.8.0), the
//! SetState vector generated with msmart-ng with every field pinned, and the
//! regressions Breeze Core found the hard way.

use ucore::proto::ac::{self, Mode, Setpoint, State, Swing};
use ucore::proto::caps::Capabilities;
use ucore::proto::frame;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn payload_of(message: &str) -> Vec<u8> {
    frame::parse(&hex(message)).expect("vector must be a valid frame").to_vec()
}

#[test]
fn get_state_is_byte_identical_to_msmart() {
    let built = ac::get_state(0x11);
    assert_eq!(frame::parse(&built).unwrap(), &hex("418100ff03ff00020000000000000000000000000311f4")[..]);
}

#[test]
fn set_state_is_byte_identical_to_msmart() {
    let sp = Setpoint {
        power_on: true,
        mode: Mode::Cool,
        target_temperature: 24.0,
        fan_speed: 60,
        swing: Swing::Off,
        eco: false,
        turbo: false,
        sleep: false,
        fahrenheit: false,
        purifier: false,
        aux_heat: false,
        independent_aux_heat: false,
        follow_me: false,
        freeze_protection: false,
        target_humidity: 40,
        beep: false,
    };
    assert_eq!(
        to_hex(&ac::set_state(0x11, &sp)),
        "aa24ac000000000000024003483c7f7f00300000000000000000000000280000000011867a"
    );
}

#[test]
fn every_setpoint_survives_a_round_trip_through_the_state_decoder() {
    let mut t = 16.0f32;
    while t <= 30.0 {
        let sp = Setpoint {
            power_on: true,
            mode: Mode::Cool,
            target_temperature: t,
            fan_speed: ac::FAN_AUTO,
            swing: Swing::Off,
            eco: false,
            turbo: false,
            sleep: false,
            fahrenheit: false,
            purifier: false,
            aux_heat: false,
            independent_aux_heat: false,
            follow_me: false,
            freeze_protection: false,
            target_humidity: 40,
            beep: false,
        };
        let f = ac::set_state(1, &sp);
        let p = frame::parse(&f).unwrap();
        let mut report = vec![0u8; 24];
        report[0] = 0xC0;
        report[2] = p[2];
        report[13] = p[18];
        assert_eq!(State::parse(&report).unwrap().target_temperature, t, "setpoint {t}");
        t += 0.5;
    }
}

#[test]
fn short_report_with_a_crc_tail() {
    let s = State::parse(&payload_of("aa1eac00000000000003c0004b1e7f7f000000000069630000000000000d33")).unwrap();
    assert_eq!((s.target_temperature, s.indoor_temperature, s.outdoor_temperature), (27.0, Some(27.5), Some(24.5)));
}

#[test]
fn no_outdoor_sensor_is_none_not_a_number() {
    let s = State::parse(&payload_of("aa22ac00000000000303c0014566000000300010045eff00000000000000000069fdb9")).unwrap();
    assert_eq!((s.target_temperature, s.indoor_temperature, s.outdoor_temperature), (21.0, Some(22.0), None));
}

#[test]
fn v3_report_with_both_sensors() {
    let s =
        State::parse(&payload_of("aa23ac00000000000303c00145660000003c0010045c6b20000000000000000000020d79")).unwrap();
    assert_eq!((s.target_temperature, s.indoor_temperature, s.outdoor_temperature), (21.0, Some(21.0), Some(28.5)));
}

#[test]
fn tenth_degree_precision() {
    for (target, indoor, outdoor, message) in [
        (24.0f32, 24.6f32, 9.5f32, "aa23ac00000000000203c00188647f7f000000000063450c0056190000000000000497c3"),
        (24.0, 26.5, 9.7, "aa23ac00000000000203c00188647f7f000000000067450c00750000000000000001a3b0"),
        (24.0, 25.0, 9.5, "aa23ac00000000000203c00188647f7f000080000064450c00501d00000000000001508e"),
    ] {
        let s = State::parse(&payload_of(message)).unwrap();
        assert_eq!((s.target_temperature, s.indoor_temperature, s.outdoor_temperature), (target, Some(indoor), Some(outdoor)));
    }
}

#[test]
fn target_temperature_across_half_degree_steps() {
    for (target, indoor, outdoor, payload) in [
        (16.0f32, 23.2f32, 18.4f32, "c00181667f7f003c00000060560400420000000000000048"),
        (16.5, 23.4, 18.4, "c00191667f7f003c00000060560400440000000000000049"),
        (17.0, 23.6, 18.3, "c00181667f7f003c0000006156050036000000000000004a"),
        (17.5, 23.8, 18.2, "c00191667f7f003c0000006156050028000000000000004b"),
        (18.0, 23.8, 18.2, "c00182667f7f003c0000006156060028000000000000004c"),
        (18.5, 23.8, 18.2, "c00192667f7f003c0000006156060028000000000000004d"),
        (19.0, 23.8, 18.2, "c00183667f7f003c0000006156070028000000000000004e"),
        (19.5, 23.5, 18.5, "c00193667f7f003c00000061570700550000000000000050"),
    ] {
        let s = State::parse(&hex(payload)).unwrap();
        assert_eq!((s.target_temperature, s.indoor_temperature, s.outdoor_temperature), (target, Some(indoor), Some(outdoor)));
    }
    for (target, payload) in [
        (16.0f32, "c00040660000003c00000062680400000000000000000004"),
        (16.5, "c00050660000003c00000062670400000000000000000004"),
    ] {
        assert_eq!(State::parse(&hex(payload)).unwrap().target_temperature, target);
    }
}

#[test]
fn regression_the_swing_axis_is_not_transposed() {
    let mut p = vec![0u8; 24];
    p[0] = 0xC0;
    p[7] = 0x3;
    assert_eq!(State::parse(&p).unwrap().swing, Some(Swing::Horizontal));
    p[7] = 0xC;
    assert_eq!(State::parse(&p).unwrap().swing, Some(Swing::Vertical));
}

#[test]
fn regression_an_unknown_mode_is_not_coerced() {
    let mut p = vec![0u8; 24];
    p[0] = 0xC0;
    p[2] = 0x6 << 5;
    assert_eq!(State::parse(&p).unwrap().mode, None);
}

#[test]
fn truncation_never_panics() {
    let full = payload_of("aa23ac00000000000303c00145660000003c0010045c6b20000000000000000000020d79");
    for n in 0..=full.len() {
        let _ = State::parse(&full[..n]);
        let _ = Capabilities::parse(&full[..n]);
    }
}

#[test]
fn names_are_the_apis_and_taken_in_any_case() {
    assert_eq!(Mode::from_name("fan_only"), Some(Mode::FanOnly));
    assert_eq!(Swing::from_name("Vertical"), Some(Swing::Vertical));
    // Unicode upper case, as Breeze Core: the dotless ı becomes I.
    assert_eq!(Swing::from_name("vertıcal"), Some(Swing::Vertical));
    assert_eq!(Mode::from_name("WARP"), None);
}

fn caps(records: &[(u16, &[u8])], trailing: &[u8]) -> Vec<u8> {
    let mut p = vec![0xB5, records.len() as u8];
    for (id, v) in records {
        p.extend_from_slice(&id.to_le_bytes());
        p.push(v.len() as u8);
        p.extend_from_slice(v);
    }
    p.extend_from_slice(trailing);
    p
}

#[test]
fn capabilities_view_matches_breeze_cores_shape() {
    let c = Capabilities::parse(&caps(
        &[
            (0x0214, &[1]),                      // cool heat dry auto
            (0x0215, &[1]),                      // both flaps
            (0x0210, &[4]),                      // low high auto
            (0x0212, &[1]),                      // eco
            (0x021A, &[0]),                      // turbo in cool
            (0x0225, &[0x22, 0x3C, 0x22, 0x3C, 0x20, 0x3C, 1]), // 17-30, 16-30 heat
            (0x0400, &[7]),                      // unknown
        ],
        &[0, 0],
    ));
    let v = serde_json::to_string(&c.view("42".into())).unwrap();
    assert_eq!(
        v,
        r#"{"id":"42","operational_modes":["FAN_ONLY","DRY","COOL","HEAT","AUTO"],"swing_modes":["OFF","HORIZONTAL","VERTICAL","BOTH"],"supports_vertical_swing":true,"supports_horizontal_swing":true,"fan_speeds":["LOW","HIGH","AUTO"],"supports_custom_fan_speed":false,"min_target_temperature":16,"max_target_temperature":30,"supports_eco":true,"supports_turbo":true,"supports_display_control":false,"supports_freeze_protection":false,"supports_humidity":false,"unrecognised_capability_ids":["0x0400"]}"#
    );
}

#[test]
fn a_unit_that_reports_nothing_gets_the_reference_defaults() {
    let v = serde_json::to_value(Capabilities::parse(&[0xB5, 0]).view("1".into())).unwrap();
    assert_eq!(v["fan_speeds"], serde_json::json!(["LOW", "MEDIUM", "HIGH", "AUTO"]));
    assert_eq!(v["supports_vertical_swing"], serde_json::Value::Null);
    assert_eq!((v["min_target_temperature"].clone(), v["max_target_temperature"].clone()), (16.into(), 30.into()));
}

#[test]
fn a_second_page_merges_and_has_the_last_word_on_more() {
    let mut first = Capabilities::parse(&caps(&[(0x0214, &[2])], &[1, 0]));
    assert!(first.additional);
    let second = Capabilities::parse(&caps(&[(0x0212, &[1])], &[0, 0]));
    first.merge(&second);
    assert!(first.eco && first.heat && !first.additional);
}

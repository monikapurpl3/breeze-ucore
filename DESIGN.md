# Breeze μcore — design

Breeze Core for small machines: an ESP32-S3 first, then anything with a few
hundred kilobytes of RAM and a network stack. It is still Breeze Core — the
same API, the same pairing and signed requests, timers, schedules, programs,
curves and the live stream, everything that puts it a step above the vendor
platform — with the web panel, the command line and everything else a
microcontroller cannot carry taken out, and the code written again to be small.

For people comfortable with an API, `curl` and `breezectl`: there is no page.

## The contract

**Every route μcore keeps answers exactly as Breeze Core does** — status,
headers and body, byte for byte where Breeze Core's own answer is fixed, and
key for key where a reading moves between two calls. `/api/version` lists only
the features μcore has, so the Android app, breezectl and scripts work with it
unchanged: they already decide what to show from that list.

**The store files are Breeze Core's**: `config.json`, `devices.json`,
`programs.json`, `timers.json`, the same formats, so a configuration moves
between the two either way. Byte-identical round trips, as Breeze Core's own
are.

The code is not shared; the behaviour is. Everything that made Breeze Core
correct was found by checking against something outside it (the transposed swing
axis, the AES-256 session key, banker's rounding in curves, a schedule's
normalised days, spending a nonce only after the signature verifies). μcore
inherits those findings as **tests**, not as code: the protocol vectors, the
store fixtures, the curve samples and the auth vectors are ported, and a
differential test holds μcore's answers to a real `breeze-core`'s on the same
emulated units (breeze-mock).

## What it keeps

Advertised features (the names `/api/version` uses):

| Feature | Routes |
|---|---|
| — | `GET /api/health`, `GET /api/version`, `GET /api/units`, `GET /api/units/{id}/state`, `POST /api/units/{id}/control` |
| `batch_state` | `GET /api/units/state` |
| `unit_capabilities` | `GET /api/units/{id}/capabilities` |
| `control_feedback` | `not_applied` in control answers |
| `beep_control` | `beep` in a control request |
| `config_api` | `GET /api/config`, `POST /api/units` (by address, with V3 credentials), `PATCH /api/units/{id}` |
| `delete_unit` | `DELETE /api/units/{id}` |
| `device_pairing` | `/api/auth/enroll/{start,approve,poll}`, `/api/auth/devices`, `/api/auth/devices/{id}` |
| `ed25519_auth` | signed requests (v2), `/api/auth/upgrade` |
| `whoami` | `GET /api/auth/whoami` |
| `sleep_timer`, `timer_at` | `/api/timers`, `/api/timers/{id}`, `/api/timers/status` |
| `programs` | `/api/programs`, `/api/programs/{id}`, `/api/programs/{id}/apply`, `/api/programs/status` |
| `live_stream` | `GET /api/units/stream` (server-sent events) |

Kept as in Breeze Core: the API key plus a per-device credential, admin actions
from the LAN only, the auth reason codes and `retryable`, enums as names, `id`
as a string, the batch envelope with `errors`, no CORS header.

## What it drops

| Gone | Why |
|---|---|
| the web panel (175 KB) | API only |
| the command line (`pair`, `approve`, `diag`, `control`, `fetch`, …) | no shell; the same actions are API calls, and breezectl makes them |
| `unit_scan` | adding a unit by address remains (it probes that one address) |
| `system_info`, `metrics` | host introspection; a log line and `/api/health` cover a chip |
| `compression` | the answers are small; gzip is code and RAM |
| cloud token fetch | TLS is most of a megabyte; V3 credentials come from wherever they are already kept (another Breeze Core's config, an export) |
| `unit_history` | **open** — decide with the S3's real RAM figures (a small ring per unit, or none) |

## Shape of the code

**No I/O in the core.** One `no_std` + `alloc` crate, `ucore`, holds everything
that is Breeze Core: the HTTP/1.1 parser and router, auth, the unit sessions,
the scheduler, the event stream, the store. It is driven, never blocking:

```text
platform event (bytes arrived, socket writable, tick, time set)
        → ucore state machines
        → actions (send these bytes, open this connection, write this file)
```

**One thread, bounded everything.** No thread per connection, no growing pools.
Limits are compile-time constants, refused with Breeze Core's own answers when
reached: units (8), HTTP clients (4), event streams (2), paired devices (16),
request body (4 KB), programs and timers (fixed tables). Memory use is a number
that can be stated, not a curve.

**A small platform trait** supplies what the core cannot: monotonic and wall
time, random bytes, TCP listen/accept/connect/read/write, UDP for the
single-address probe, a few files, and a log line. Each target is an adapter:

| Adapter | For |
|---|---|
| `ucore-host` | Linux, macOS, Windows: development, the differential test, and incidentally a very small build for a router |
| `ucore-esp32` | ESP32-S3 first; then the other ESP32s. esp-hal + embassy, or ESP-IDF — decided by a spike (below) |
| later | Raspberry Pi Pico 2 W (RP2350), as proof that the next chip is an adapter and not a port |

**Time.** Schedules and timers run on local time. The core's scheduling takes
`now` as a parameter (as Breeze Core's does); the adapter sets the clock from
SNTP, and a POSIX TZ rule in the configuration (`CET-1CEST,M3.5.0,M10.5.0/3`)
turns it into local time — a chip has no timezone database. Until the clock is
set, schedules and timers wait and `/api/health` says so.

**The protocol.** V3 sessions and V2 packets as state machines: the handshake,
the first request a unit ignores after it, resends, the idle hang-up a unit
does after 30 s. Written small, checked against the same vectors as Breeze
Core's `breeze-proto` and against breeze-mock's emulated units, misbehaviour
included.

**Setup**, for the audience it is for: Wi-Fi credentials and the TZ rule are
given once — over the serial port (Improv, which browser flashers speak) or in a
small config partition. The first boot generates the API key and prints it on
the serial console; `GET /api/config` from the LAN is the admin view after that.

## Budgets (to be measured, then held)

- ESP32-S3 without PSRAM: μcore's own heap at most 160 KB with 8 units, 4
  clients and 2 streams; flash for the application at most 1 MB.
- A host build of a few hundred kilobytes.
- A test fails when a budget is exceeded.

## How it is checked

- **Ported vectors and fixtures**: msmart's protocol vectors, Breeze Core's
  store fixtures (byte round trips), the curve samples, the auth signatures.
- **Differential**: μcore's host build and a real `breeze-core` on the same
  breeze-mock units, every kept route, the same answers (as breeze-mock's own
  `differential.rs` does).
- **`breeze-core diag`** pointed at μcore passes everything μcore advertises.
- **The clients**: the Android app and breezectl against a μcore, by hand.
- **On the chip**: Espressif's QEMU (emulated Ethernet) with breeze-mock units on
  the PC, then a real ESP32-S3 on Wi-Fi.

## Milestones

1. **Units on the host.** `ucore` + `ucore-host`: health, version, units, state,
   batch state, control, capabilities, behind the API key and device tokens
   preset in `devices.json`. Differential test green for those routes.
2. **All of Breeze Core that μcore keeps.** Pairing, signed requests, devices,
   config routes, timers, programs and curves, the event stream. Differential
   test green for every kept route; `diag` passes.
3. **The chip spike.** The same core on an ESP32-S3: esp-hal + embassy against
   ESP-IDF, chosen on code size, RAM and how cleanly the adapter stays an
   adapter. In QEMU, then on a board.
4. **The chip, properly.** Wi-Fi setup, SNTP and TZ, flash storage, watchdog,
   memory and flash budgets enforced.
5. **Shipping it.** Firmware images, flashing from the browser on aspic, the
   docs, breezectl's admin commands for a μcore (approve, add a unit), a
   breeze-mock profile so clients can be tested against a μcore.
6. **A second chip** (Pico 2 W), to prove the portability is real.

## Open questions

- `unit_history`: in (a small ring) or out — after the RAM figures.
- esp-hal + embassy or ESP-IDF — after the spike.
- Keep-warm (holding unit connections open): on, off, or per unit, given the
  socket count a chip allows.
- Defaults for the limits above, once measured.

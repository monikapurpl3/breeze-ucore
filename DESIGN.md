# Breeze μCore — design

Breeze Core for small machines: an ESP32-S3 first, then anything with a few
hundred kilobytes of RAM and a network stack. It is still Breeze Core — the
same API, the same pairing and signed requests, timers, schedules, programs,
curves and the live stream, everything that puts it a step above the vendor
platform — with the web panel, the command line and everything else a
microcontroller cannot carry taken out, and the code written again to be small.

For people comfortable with an API, `curl` and `breezectl`: there is no page.
And it does not come as a ready binary: a **deployer** builds it for one
household's board, units and network (see [Deployment](#deployment)).

## The contract

**Every route μCore keeps answers exactly as Breeze Core does** — status,
headers and body, byte for byte where Breeze Core's own answer is fixed, and
key for key where a reading moves between two calls. `/api/version` lists only
the features μCore has, so the Android app, breezectl and scripts work with it
unchanged: they already decide what to show from that list.

**The store files are Breeze Core's**: `config.json`, `devices.json`,
`programs.json`, `timers.json`, the same formats, so a configuration moves
between the two either way. Byte-identical round trips, as Breeze Core's own
are. `config.json` is the deployer's: μCore reads it and never writes it (it
adds, renames and deletes no units; see below).

The code is not shared; the behaviour is. Everything that made Breeze Core
correct was found by checking against something outside it (the transposed swing
axis, the AES-256 session key, banker's rounding in curves, a schedule's
normalised days, spending a nonce only after the signature verifies). μCore
inherits those findings as **tests**, not as code: the protocol vectors, the
store fixtures, the curve samples and the auth vectors are ported, and a
differential test holds μCore's answers to a real `breeze-core`'s on the same
emulated units (breeze-mock).

## Versions

**The product version is `<μCore version>u<Breeze Core native version>`**,
for example `1.0.0u4.4.0`: μCore's own version, then `u`, then the Breeze
Core native release whose behaviour it follows (its API, its store formats,
the release the differential test runs against). That is what people see:
release names, firmware file names, `--version`, the boot log. Cargo needs
plain semver, so the crates are `1.0.0`, and the full string is built from
that and one constant naming the native release.

**On the wire it is `4.4.0+u1.0.0`.** `/api/version` reports the API it
speaks, so anything that parses versions sees 4.4.0. The μCore version rides
along as semver build metadata, which every parser ignores for comparison and
every printer shows, so `diag`, breezectl and bug reports still say what the
server really is. (The Android app's own parser splits at `-` and `+` before
comparing.) `name` is `Breeze μCore`; no client compares it.

What a client may do is still decided by `features`, never by the version.
The app's Nerd screen is the one place that gates on the version: against
μCore it would pass and then find no `/api/system`. That screen should check
`system_info` instead (milestone 5).

## What it keeps

Advertised features (the names `/api/version` uses):

| Feature | Routes |
|---|---|
| — | `GET /api/health`, `GET /api/version`, `GET /api/units`, `GET /api/units/{id}/state`, `POST /api/units/{id}/control` |
| `batch_state` | `GET /api/units/state` |
| `unit_capabilities` | `GET /api/units/{id}/capabilities` |
| `control_feedback` | `not_applied` in control answers |
| `beep_control` | `beep` in a control request |
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
| `unit_scan`, `config_api`, `delete_unit` | the unit list is the deployer's: adding (with its probe of the address), renaming and deleting units, and `GET /api/config`, are gone. `POST /api/units` answers 405, the rest 404. A unit changes by deploying a new bundle |
| `system_info`, `metrics` | host introspection; a log line and `/api/health` cover a chip |
| `compression` | the answers are small; gzip is code and RAM |
| cloud token fetch | TLS is most of a megabyte; the deployer gets V3 credentials with breeze-token, on the user's computer |
| `unit_history` | **open**. The RAM figures are in (see Budgets): there is room for a small ring per unit |

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
time, random bytes, TCP listen/accept/connect/read/write, a few files
(written in pieces), and a log line. Each target is an adapter:

| Adapter | For |
|---|---|
| `ucore-host` | Linux, macOS, Windows: development, the differential test, and incidentally a very small build for a router |
| `ports/esp32` | ESP32-S3 and ESP32-C6, by a feature. esp-hal and smoltcp, driven by the core's own poll loop: no executor (chosen in milestone 3, below) |
| later | Raspberry Pi Pico 2 W (RP2350), as proof that the next chip is an adapter and not a port |

**Time.** Schedules and timers run on local time. The core's scheduling takes
`now` as a parameter (as Breeze Core's does); the adapter sets the clock from
SNTP, and a POSIX TZ rule in `env.json` (`CET-1CEST,M3.5.0,M10.5.0/3`,
`ucore::tz`) turns it into local time — a chip has no timezone database. Until the clock is
set, schedules and timers wait and `/api/health` says so.

**The protocol.** V3 sessions and V2 packets as state machines: the handshake,
the first request a unit ignores after it, resends, the idle hang-up a unit
does after 30 s. Written small, checked against the same vectors as Breeze
Core's `breeze-proto` and against breeze-mock's emulated units, misbehaviour
included.

**Storage**: two logs in flash (`ucore::flashlog`: append, header written
last, compacted into the other half when full, so a power cut leaves a file
old or new). `ucore` is the store μCore writes; `ucorecfg` is the bundle the
deployer writes (`config.json` and `env.json`). Files are written in pieces,
never held whole: on a chip whole copies were the largest allocations.

## Budgets

Measured in Espressif's QEMU (milestone 3), DESIGN.md's worst case: 8 units,
16 paired devices, 16 programs, 16 timers, 2 event streams and 2 busy clients
for 90 s.

| | Budget | Measured |
|---|---|---|
| μCore's heap, ESP32-S3 without PSRAM | 160 KB | peak 110–117 KB; 60 KB idle after |
| application flash, with Wi-Fi | 1 MB | 817 KB (S3), with the add-unit code gone |
| the same without Wi-Fi (QEMU) | | 447 KB (S3) |

For scale: ESP-IDF's own Wi-Fi station example, with no application in it,
is 704 KB and 110 KB of static RAM. Notes on size: the SHA-512 that Ed25519
verification needs is built compact (45 KB less); panics carry no file and
line strings (11 KB less, the backtrace still maps to source); `opt-level =
"z"` came out 25 KB *larger* than `"s"` on Xtensa. Text is about 17 KB of the
image and nearly all of it is Breeze Core's wire contract.

## How it is checked

- **Ported vectors and fixtures**: msmart's protocol vectors, Breeze Core's
  store fixtures (byte round trips), the curve samples, the auth signatures.
- **Differential**: μCore's host build and a real `breeze-core` on the same
  breeze-mock units, every kept route, the same answers (as breeze-mock's own
  `differential.rs` does).
- **`breeze-core diag`** pointed at μCore passes everything μCore advertises.
- **The clients**: the Android app and breezectl against a μCore, by hand.
- **On the chip**: Espressif's QEMU (its emulated OpenCores Ethernet), with
  breeze-mock units on the PC's loopback reached through QEMU's user network:
  `breeze-core diag` passes against an emulated S3 and C6. Then a real board
  on Wi-Fi.

## Milestones

1. **Units on the host.** Done.
2. **All of Breeze Core that μCore keeps.** Done: differential test and `diag`.
3. **The chip spike.** Done in QEMU (2026-11-18): esp-hal with smoltcp and no
   executor against ESP-IDF, chosen on size and RAM (Budgets) and because the
   core's poll loop drives smoltcp directly. A real board still to confirm it.
4. **The chip, properly.** Done: the flash store and the bundle, SNTP and the
   TZ rule, budgets measured. Left for a real board: Wi-Fi in practice
   (reconnecting, signal), the watchdog.
5. **The deployer** (see Deployment), which replaces shipping firmware.
6. **A second chip**: the ESP32-C6 already runs in QEMU; the Pico 2 W proves
   it beyond Espressif.

## Open questions

- `unit_history`: in (a small ring) or out.
- Keep-warm (holding unit connections open): on, off, or per unit.
- Defaults for the limits, now that the figures are in.

## Deployment

μCore is not shipped as firmware. A **deployer** builds it for one household:
"you're already setting up a hobbyist board". It is a page in the browser and
a **helper** program on the user's computer that does the work. The page is
either served by the helper from a μCore checkout, or loaded from aspic; in
both cases it talks only to the helper on `127.0.0.1`.

What it does, in order:

1. **The board**: one of `boards/*.toml` in the μCore repository (chip, flash
   size, network), or the user's own, edited on the page.
2. **The units**: breeze-token, as a library, signs in to the Midea account
   the units are paired with and fetches each unit's token and key, checking
   them on the unit. This is `config.json`.
3. **The environment**: Wi-Fi, the zone (picked by name, turned into a POSIX
   TZ rule), the time server, a hostname. This is `env.json`.
4. **The firmware**: the helper fetches μCore's source at a release, installs
   the toolchains it needs into its own folder (Rust, and for an S3 the Xtensa
   toolchain and GCC; never the user's PATH or environment), builds for that
   board, and flashes it over USB.
5. **The bundle**: `config.json` and `env.json` as a `ucorecfg` image
   (`ucore::bundle::image`), flashed to that partition.

A **configuration-only page** does just step 5 (and 2 or 3 as needed): a new
unit, a new Wi-Fi password, a different zone. No toolchain, no build, and the
store (paired phones, programs, timers) is not touched.

**Everything stays on the user's computer**, and the page says so plainly.
It is also made checkable rather than promised:

- The page's Content-Security-Policy allows it to connect to the helper on
  `127.0.0.1` and nowhere else; the browser refuses anything else. It loads
  nothing from anywhere but where it came from: no fonts, scripts or
  analytics from elsewhere.
- The helper itself only ever contacts: the source and toolchain downloads
  (GitHub, static.rust-lang.org, Espressif's releases), and **Midea's cloud,
  with the user's own account, to fetch the units' tokens** — the one thing
  that cannot be done locally, and the page says so where it asks for the
  password. Nothing the user types goes anywhere else. The page shows every
  connection the helper made, as it makes it.
- The helper listens on `127.0.0.1` only, answers only the page (its own
  origin or aspic's, with a token it hands the page when it opens it), and
  keeps each deployment's files (secrets included) in its own folder,
  readable by the user alone.

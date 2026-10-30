<div align="center">

# Breeze μCore

**Breeze μCore (read micro-core), small enough for a microcontroller.**

The LAN-first control server for Midea air conditioners, rewritten for chips
with a few hundred kilobytes of RAM — an ESP32-S3 first. Based on
[Breeze Core](https://github.com/monikapurpl3/breeze-core-native)'s API, so the
Breeze app, breezectl and your scripts work with it unchanged; no web panel,
no command line, nothing a microcontroller cannot carry.

[![License](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Status](https://img.shields.io/badge/status-in%20development-e69138)](#status)
[![Follows](https://img.shields.io/badge/follows-Breeze%20Core%204.3.2-6aa84f)](DESIGN.md#versions)

</div>

## Why

A Breeze Core server today needs a computer: a Raspberry Pi, a NAS, a router
with room to spare. μcore is for a board that costs a few euros, draws half a
watt and never needs an operating system update — and still does what puts
Breeze Core a step above the vendor's platform:

- **The same API**, route for route, for everything it keeps: units and their
  state, control with `not_applied`, capabilities, pairing with device tokens
  and Ed25519-signed requests, timers, schedules, programs and temperature
  curves, and the live stream.
- **Held to Breeze Core, not just modelled on it.** A differential test runs a
  real Breeze Core and μcore on the same emulated air conditioners
  (breeze-mock) and requires
  the same status, headers and body for every request it compares.
- **Built to port.** The core is `no_std`, does no I/O of its own, runs on one
  thread with every limit fixed at compile time; a new chip is a small adapter,
  not a port.
- **API only**, for people comfortable with an API key, `curl` and
  [breezectl](https://github.com/monikapurpl3/breezectl).

## Status

**In development; nothing released yet.** Milestone 1 is done: the protocol,
units, state, control and capabilities, on an ordinary computer, matching
Breeze Core 4.3.2 under the differential test (a 338 KB build). Of milestone
2, pairing (both auth versions) and the configuration routes (adding units by
address, renaming, removing) are done; timers, programs and the live stream
come next, then the ESP32-S3.
[DESIGN.md](DESIGN.md) has the plan, the milestones and the reasoning.

## Versions

`1.0.0u4.3.2` is μcore 1.0.0 following Breeze Core 4.3.2. On the wire,
`/api/version` says `4.3.2+u1.0.0`, so clients see the API they speak.

## Building

```sh
cargo build --release -p ucore-host     # target/release/ucore
cargo test --workspace
```

The host build is configured as Breeze Core is: `AC_CONFIG_DIR` holds
`config.json` and `devices.json` (Breeze Core's own files), and `BREEZE_HOST`
and `BREEZE_PORT` say where to listen. The differential test needs a Breeze
Core binary and breeze-mock:

```sh
BREEZE_CORE_BIN=/path/to/breeze-core BREEZE_MOCK_BIN=/path/to/breeze-mock \
  cargo test --workspace --release
```

Adding a unit by address probes it, and Breeze Core probes private addresses
only, so that comparison (`tests/add_by_address.rs`) needs breeze-mock's dummy
network: Linux with `CAP_NET_ADMIN`, a container being enough. It is skipped
elsewhere; its header says how to run it.

## Honest limits

No web panel, no command line, no unit scan (units are added by address), no
cloud token fetch: V3 credentials come from wherever you already keep them.
Breeze Core 4 drives V3 units only, and so does μcore. Until it runs on a chip,
it is a small server for computers and nothing more.

## Licence

[AGPL-3.0-or-later](LICENSE), as Breeze Core is.

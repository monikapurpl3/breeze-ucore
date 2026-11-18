# Breeze μCore on an ESP32-S3

The chip adapter: esp-hal for the chip, smoltcp for the network, and the
core's own poll loop on one thread, with no executor. Its own Cargo
workspace, built with Espressif's Xtensa toolchain (`espup install -t
esp32s3`; `rust-toolchain.toml` picks the `esp` channel).

| Feature | |
|---|---|
| `qemu` (default) | Espressif's QEMU: the OpenCores Ethernet MAC it emulates (`src/openeth.rs`), no radio |
| `wifi` | a real chip on Wi-Fi (esp-radio) |
| `stats` | heap and socket figures on the console every 10 s |

Build-time settings, all optional:

| Variable | |
|---|---|
| `UCORE_SEED_DIR` | a directory of store files (`config.json`, ...) to start with |
| `UCORE_TZ` | the zone as a POSIX TZ rule; `CET-1CEST,M3.5.0,M10.5.0/3` by default |
| `UCORE_NTP` | the time server, a name or an address; `pool.ntp.org` by default |

The store is in RAM until milestone 4 brings flash: what is written is gone
at a reset.

## In QEMU

```sh
UCORE_SEED_DIR=/path/to/seed cargo build --release
espflash save-image --chip esp32s3 --merge --flash-size 4mb \
  target/xtensa-esp32s3-none-elf/release/ucore flash.bin
qemu-system-xtensa -display none -machine esp32s3 \
  -drive file=flash.bin,if=mtd,format=raw \
  -nic user,model=open_eth,hostfwd=tcp:127.0.0.1:18420-:8420 \
  -serial file:serial.log -monitor none
```

QEMU's user network gives the chip 10.0.2.15 and maps 10.0.2.2 to the PC's
loopback, so breeze-mock's units on 127.0.0.1, each on a port of its own
(`<v3 port="6450"/>`), are 10.0.2.2:6450 to the chip. Then
`breeze-core diag --base-url http://127.0.0.1:18420 --config seed/config.json`
checks it end to end.

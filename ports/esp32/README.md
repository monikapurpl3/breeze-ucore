# Breeze μCore on an ESP32

The chip adapter: esp-hal for the chip, smoltcp for the network, and the
core's own poll loop on one thread, with no executor. Its own Cargo
workspace.

| Feature | |
|---|---|
| `esp32s3` (default) | an ESP32-S3: Xtensa, Espressif's toolchain (`espup install -t esp32s3`; `rust-toolchain.toml` picks the `esp` channel) |
| `esp32c6` | an ESP32-C6: RISC-V, the ordinary nightly (`cargo +nightly build --release --target riscv32imac-unknown-none-elf --no-default-features --features esp32c6,...`) |
| `qemu` (default) | Espressif's QEMU: the OpenCores Ethernet MAC it emulates (`src/openeth.rs`), no radio |
| `wifi` | a real chip on Wi-Fi (esp-radio) |
| `stats` | heap and socket figures on the console every 10 s |

## Flash

`partitions.csv`: the app, then two logs (`ucore::flashlog`):

- `ucore` (256 KB at 0x310000), the store μCore writes: paired devices,
  programs, timers.
- `ucorecfg` (32 KB at 0x350000), the bundle the deployer writes:
  `config.json` (the API key and the units) and `env.json` (Wi-Fi, a POSIX TZ
  rule, the time server, a hostname). See `ucore::bundle`. μCore only reads
  it, so a new bundle needs no new firmware and leaves the store alone.

Build a bundle by hand with
`cargo run -p ucore --example bundle -- bundle.bin config.json env.json`
(from the repository root) and write it at 0x350000.

## In QEMU

```sh
cargo build --release
espflash save-image --chip esp32s3 --merge --flash-size 4mb \
  --partition-table partitions.csv target/xtensa-esp32s3-none-elf/release/ucore flash.bin
# then write bundle.bin into flash.bin at 0x350000
qemu-system-xtensa -display none -machine esp32s3 \
  -drive file=flash.bin,if=mtd,format=raw \
  -nic user,model=open_eth,hostfwd=tcp:127.0.0.1:18420-:8420 \
  -serial file:serial.log -monitor none
```

QEMU's user network gives the chip 10.0.2.15 and maps 10.0.2.2 to the PC's
loopback, so breeze-mock's units on 127.0.0.1, each on a port of its own
(`<v3 port="6450"/>`), are 10.0.2.2:6450 to the chip. Then
`breeze-core diag --base-url http://127.0.0.1:18420 --config config.json`
checks it end to end (with `BREEZE_CLI_PROFILE` pointing somewhere of its
own, or diag saves its login to your real CLI profile).

QEMU's ESP32-C6 machine (`qemu-system-riscv32 -machine esp32c6`) loses a byte
at every 16-byte boundary of a flash read, so an emulated C6 keeps both logs
in RAM, its bundle built at boot from the files in `UCORE_SEED_DIR`.

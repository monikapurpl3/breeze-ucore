# The μCore deployer

μCore is not shipped as firmware: the deployer builds it for one household.
It is a **helper** program on your computer, which does the work, and a page
in your browser that shows it what to do. DESIGN.md's
[Deployment](../DESIGN.md#deployment) section has the reasoning.

For now it builds for the two QEMU boards in [`boards/`](../boards) only: no
physical board has been tried yet. They run in Espressif's QEMU, which you
install yourself (<https://github.com/espressif/qemu/releases>). On your PATH
is simplest; if it is not there, the first build asks in the helper's own
window (the one you started it in) for its folder. The folder you unpacked
it to is enough, and one answer covers both QEMUs if they are side by side.
The helper checks that each runs its board's machine and remembers it in
its folder (`qemu.json`). It asks there, and never takes a program's path
from the page: which programs it runs is for whoever is at its window.

## Running it

From this checkout:

```sh
cargo run --release
```

It builds μCore from this checkout, opens its page, and keeps going until
Ctrl-C. Or, without a checkout, with the runners on aspic, which download the
helper, check its SHA-256 and run it (it then downloads the μCore source of
the commit it was built from):

```sh
curl -fsSL https://aspic.salataputarica.hr.eu.org/ucore-deploy/run.sh | sh   # Linux, macOS
irm https://aspic.salataputarica.hr.eu.org/ucore-deploy/run.ps1 | iex       # Windows
```

| Option | |
|---|---|
| `--port PORT` | where it listens on 127.0.0.1 (47820) |
| `--aspic` | open the page on aspic instead of its own (the same page) |
| `--no-open` | print the page's link instead of opening it |
| `--source DIR` | μCore's source to build (`github`: download it even from a checkout) |
| `--data DIR` | where deployments and tools are kept |

With `--aspic`, Chrome first asks whether the page may connect to apps on
this computer: the helper is the app, so allow it. The helper's own page needs
no such permission.

Deployments and the build tools it installs are kept in its own folder:
`%LOCALAPPDATA%\ucore-deploy`, `~/.local/share/ucore-deploy`, or
`~/Library/Application Support/ucore-deploy`. Nothing is added to the PATH.
On Windows, an ESP32-S3 build needs Microsoft's C++ build tools (Espressif's
Xtensa Rust is built for that host only); an ESP32-C6 build does not.

## Simulated air conditioners

No air conditioners at hand, or none you want a test board driving? The
page can make 1 to 8 simulated ones for a board in QEMU.
[breeze-mock](https://github.com/monikapurpl3/breeze-mock) plays them on
their real network protocol, on this computer only (127.0.0.1, a port
each), and they start and stop with the board, which reaches them as
10.0.2.2. The deployer makes each one's id, token and key and keeps them in
config.json like any unit's, so the units, μCore and a phone pairing with it
all behave as with real ones. Rename or remove them on the page as usual;
making them again replaces only the simulated ones.

breeze-mock is downloaded the first time, from aspic, and checked against
its SHA-256 (version 1.0.1, pinned in `src/mock.rs`).

## What stays local

Everything, except what it says on its page: it downloads μCore's source and
its build tools (GitHub, static.rust-lang.org, crates.io), breeze-mock from
aspic if you make simulated air conditioners, and, only when you fetch
tokens, signs in to Midea's cloud with your account. The page lists every
connection the helper makes, and its Content-Security-Policy lets it talk to
the helper on 127.0.0.1 and nowhere else.

## Testing

```sh
cargo test --release
cargo clippy --release --all-targets [--features fake-cloud]
```

`fake-cloud` is breeze-token's pretend Midea account
(`BREEZE_TOKEN_FAKE_CLOUD`), for trying the token fetch with breeze-mock's
units. Never with a real Midea account.

## A release

`sh packaging/build.sh` (a clean tree whose commit is pushed; zig 0.16.0 as
`UCORE_ZIG`) builds the five helpers into `packaging/out/`; `sh stage.sh OUT`
lays out what aspic serves under `/ucore-deploy/`.

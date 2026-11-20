# Boards

What the deployer can build μCore for, one file each. A custom board is a
file of the same shape, made on the deployer's page.

| Key | |
|---|---|
| `name` | what the deployer shows |
| `chip` | `esp32s3` or `esp32c6` |
| `network` | `qemu` (Espressif's QEMU, its emulated Ethernet) or `wifi` |
| `flash_size` | `4mb`, `8mb` or `16mb` |
| `[qemu]` | for `network = "qemu"`: the QEMU `program` and its `machine` |

For now there are only QEMU boards: no physical board has been tried yet.
QEMU itself is not installed by the deployer: it uses Espressif's QEMU
(https://github.com/espressif/qemu/releases) from the PATH, or from wherever
the user tells the helper when a build asks.

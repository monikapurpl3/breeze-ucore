//! Breeze μCore on an ESP32-S3: esp-hal for the chip, smoltcp for the
//! network, and the core's own poll loop on one thread, with no executor.
//!
//! Built for Espressif's QEMU (`--features qemu`, the default: its emulated
//! OpenCores Ethernet) or for a real chip (`--no-default-features --features
//! wifi`). See README.md beside this file.

#![no_std]
#![no_main]

extern crate alloc;

mod net;
#[cfg(feature = "qemu")]
mod openeth;
mod sntp;
mod store;
#[cfg(feature = "wifi")]
mod wifi;

use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

/// μCore's heap: the RAM the bootloader used, which is free once the app
/// runs, and a block of its own.
const HEAP_RECLAIMED: usize = 64 * 1024;
const HEAP_OWN: usize = 96 * 1024;

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: HEAP_RECLAIMED);
    esp_alloc::heap_allocator!(size: HEAP_OWN);

    let rng = esp_hal::rng::Rng::new();

    let (store, mut bundle) = store::open(peripherals.FLASH);
    let env = ucore::bundle::env(&mut bundle, &mut |w| println!("{w}"));
    let tz = ucore::tz::Rule::parse(&env.tz).unwrap_or_else(|| {
        println!("env.json: tz {:?} is not a POSIX TZ rule; using UTC", env.tz);
        ucore::tz::Rule::fixed(0)
    });

    #[cfg(feature = "qemu")]
    let dev = openeth::OpenEth::new();
    #[cfg(feature = "qemu")]
    let mac = dev.mac();
    #[cfg(feature = "wifi")]
    let (dev, mac) = wifi::start(peripherals.TIMG0, peripherals.FROM_CPU_INTR0, peripherals.WIFI, &env.wifi_ssid, &env.wifi_password);

    let mut chip = net::Chip::new(dev, mac, rng, tz, (store, bundle), env.ntp.clone());
    let mut core = ucore::Core::new(&mut chip, ucore::Settings::default());
    println!("{} on port {}", ucore::banner(&core), net::API_PORT);

    #[cfg(feature = "stats")]
    let mut next_stats = 0u64;
    loop {
        chip.poll();
        let wait = core.poll(&mut chip);
        chip.poll();
        chip.idle(wait);
        #[cfg(feature = "wifi")]
        chip.dev.keep(net::now_ms());
        #[cfg(feature = "stats")]
        {
            let now = net::now_ms();
            if now >= next_stats {
                next_stats = now + 10_000;
                let s = esp_alloc::HEAP.stats();
                println!("stats: heap {} used, {} at most, of {}; {} sockets", s.current_usage, s.max_usage, s.size, chip.sockets_in_use());
            }
        }
    }
}

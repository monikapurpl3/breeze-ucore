//! A real chip's network: Wi-Fi as a station, through esp-radio, as a
//! smoltcp device. esp-radio needs esp-rtos's scheduler for its own tasks;
//! μCore itself stays one loop on the main task.
//!
//! The network and its password are `UCORE_WIFI_SSID` and
//! `UCORE_WIFI_PASSWORD` at build time for now. Milestone 4 moves them to a
//! setting given once over the serial port (Improv), as DESIGN.md says.

use esp_hal::peripherals::{FROM_CPU_INTR0, TIMG0, WIFI};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::{AuthenticationMethodConfig, Config, Interface, WifiController, WifiRxToken, WifiTxToken};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

const SSID: &str = match option_env!("UCORE_WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("UCORE_WIFI_PASSWORD") {
    Some(s) => s,
    None => "",
};

pub struct Wifi {
    iface: Interface,
    controller: WifiController<'static>,
    next_check: u64,
}

/// Start the radio and join the network, trying until it works.
pub fn start(timg0: TIMG0<'static>, int0: FROM_CPU_INTR0<'static>, radio: WIFI<'static>) -> (Wifi, [u8; 6]) {
    let timg0 = TimerGroup::new(timg0);
    esp_rtos::start(timg0.timer0, int0);
    let mut controller = WifiController::new(radio, Default::default()).expect("the Wi-Fi controller");
    let auth = if PASSWORD.is_empty() {
        AuthenticationMethodConfig::Open
    } else {
        AuthenticationMethodConfig::Wpa2Personal(PASSWORD.try_into().expect("UCORE_WIFI_PASSWORD fits"))
    };
    let station = esp_radio::wifi::sta::StationConfig::default()
        .with_ssid(SSID.try_into().expect("UCORE_WIFI_SSID fits"))
        .with_authentication(auth);
    controller.set_config(&Config::Station(station)).expect("a station configuration");
    let iface = Interface::station();
    let mac = iface.mac_address();
    let mut wifi = Wifi { iface, controller, next_check: 0 };
    wifi.join();
    (wifi, mac)
}

impl Wifi {
    fn join(&mut self) {
        loop {
            match embassy_futures::block_on(self.controller.connect_async()) {
                Ok(_) => {
                    esp_println::println!("wifi: joined {SSID}");
                    return;
                }
                Err(e) => {
                    esp_println::println!("wifi: cannot join {SSID} ({e:?}); trying again");
                    esp_hal::delay::Delay::new().delay_millis(3_000);
                }
            }
        }
    }

    /// Join again if the network was lost. Checked every few seconds; while
    /// it is away nothing else works anyway.
    pub fn keep(&mut self, now_ms: u64) {
        if now_ms < self.next_check {
            return;
        }
        self.next_check = now_ms + 5_000;
        if !self.controller.is_connected() {
            esp_println::println!("wifi: lost {SSID}");
            self.join();
        }
    }
}

pub struct Rx(WifiRxToken);
pub struct Tx(WifiTxToken);

impl Device for Wifi {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx;

    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx)> {
        self.iface.receive().map(|(r, t)| (Rx(r), Tx(t)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx> {
        self.iface.transmit().map(Tx)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(1);
        caps
    }
}

impl phy::RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        self.0.consume_token(|b| f(b))
    }
}

impl phy::TxToken for Tx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.0.consume_token(len, f)
    }
}

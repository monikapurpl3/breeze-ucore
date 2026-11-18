//! The OpenCores Ethernet MAC that Espressif's QEMU emulates on an ESP32-S3
//! (`-nic user,model=open_eth`), as a smoltcp device. QEMU only: a real chip
//! has no such MAC, and its network is Wi-Fi.
//!
//! Where it sits and how it behaves comes from QEMU's own sources
//! (`hw/xtensa/esp32s3.c`, `hw/net/opencores_eth.c`): registers at the
//! address the ESP32's EMAC would have, descriptors 0x400 above them, eight
//! bytes each (flags and length, then the buffer's address). TX descriptors
//! come first, RX ones after `TX_BD_NUM`. Writing a descriptor makes QEMU
//! send at once, so a TX descriptor is always free again by the time the
//! next frame wants it. Every received frame carries four zero bytes of FCS,
//! which are dropped here. Polled: no interrupt.

use core::cell::Cell;
use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{compiler_fence, Ordering};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

const BASE: usize = 0x600C_D000;
const MODER: usize = BASE;
const INT_MASK: usize = BASE + 0x08;
const PACKETLEN: usize = BASE + 0x18;
const TX_BD_NUM: usize = BASE + 0x20;
const MAC_ADDR0: usize = BASE + 0x40;
const MAC_ADDR1: usize = BASE + 0x44;
const DESC: usize = BASE + 0x400;

const MODER_RXEN: u32 = 1;
const MODER_TXEN: u32 = 1 << 1;
const MODER_PRO: u32 = 1 << 5;
const MODER_RST: u32 = 1 << 11;
const MODER_CRCEN: u32 = 1 << 13;
const MODER_PAD: u32 = 1 << 15;

const TX_RD: u32 = 0x8000;
const TX_WR: u32 = 0x2000;
const TX_PAD: u32 = 0x1000;
const TX_CRC: u32 = 0x0800;
const RX_E: u32 = 0x8000;
const RX_WRAP: u32 = 0x2000;

const TX_N: usize = 2;
const RX_N: usize = 4;
const BUF: usize = 1600;
const MTU: usize = 1514;

#[repr(C, align(4))]
struct Buffers {
    tx: [[u8; BUF]; TX_N],
    rx: [[u8; BUF]; RX_N],
}

static mut BUFFERS: Buffers = Buffers { tx: [[0; BUF]; TX_N], rx: [[0; BUF]; RX_N] };

fn read(addr: usize) -> u32 {
    // SAFETY: a device register QEMU maps at this address on the esp32s3 machine.
    unsafe { read_volatile(addr as *const u32) }
}

fn write(addr: usize, v: u32) {
    // SAFETY: as above.
    unsafe { write_volatile(addr as *mut u32, v) }
}

fn desc(i: usize) -> usize {
    DESC + i * 8
}

fn tx_buf(i: usize) -> *mut u8 {
    // SAFETY: only the address is taken; this driver is the only user.
    unsafe { addr_of_mut!(BUFFERS.tx[i]) as *mut u8 }
}

fn rx_buf(i: usize) -> *mut u8 {
    // SAFETY: as above.
    unsafe { addr_of_mut!(BUFFERS.rx[i]) as *mut u8 }
}

pub struct OpenEth {
    tx_next: Cell<usize>,
    rx_next: Cell<usize>,
    mac: [u8; 6],
}

impl OpenEth {
    /// Reset the MAC and start it. Only one may exist.
    pub fn new() -> Self {
        write(MODER, MODER_RST);
        write(MODER, 0);
        // QEMU puts the NIC's address here at reset; keep it, so its user
        // network's DHCP sees the address it expects.
        let (a0, a1) = (read(MAC_ADDR0), read(MAC_ADDR1));
        let mut mac = [(a1 >> 8) as u8, a1 as u8, (a0 >> 24) as u8, (a0 >> 16) as u8, (a0 >> 8) as u8, a0 as u8];
        if mac == [0; 6] {
            mac = [0x02, 0x00, 0x00, 0x12, 0x34, 0x56];
            write(MAC_ADDR0, u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]));
            write(MAC_ADDR1, (u32::from(mac[0]) << 8) | u32::from(mac[1]));
        }
        // QEMU's reset value: 64-byte minimum, 1536-byte maximum frame.
        write(PACKETLEN, (64 << 16) | 0x600);
        write(TX_BD_NUM, TX_N as u32);
        for i in 0..TX_N {
            write(desc(i) + 4, tx_buf(i) as u32);
            write(desc(i), if i == TX_N - 1 { TX_WR } else { 0 });
        }
        for i in 0..RX_N {
            write(desc(TX_N + i) + 4, rx_buf(i) as u32);
            write(desc(TX_N + i), RX_E | if i == RX_N - 1 { RX_WRAP } else { 0 });
        }
        write(INT_MASK, 0);
        write(MODER, MODER_PAD | MODER_CRCEN | MODER_PRO | MODER_TXEN | MODER_RXEN);
        Self { tx_next: Cell::new(0), rx_next: Cell::new(0), mac }
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }
}

pub struct Rx<'a>(&'a OpenEth);
pub struct Tx<'a>(&'a OpenEth);

impl Device for OpenEth {
    type RxToken<'a> = Rx<'a>;
    type TxToken<'a> = Tx<'a>;

    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, Tx<'_>)> {
        let i = self.rx_next.get();
        if read(desc(TX_N + i)) & RX_E != 0 {
            return None;
        }
        Some((Rx(self), Tx(self)))
    }

    fn transmit(&mut self, _: Instant) -> Option<Tx<'_>> {
        (read(desc(self.tx_next.get())) & TX_RD == 0).then_some(Tx(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps.max_burst_size = Some(1);
        caps
    }
}

impl phy::RxToken for Rx<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        let i = self.0.rx_next.get();
        let flags = read(desc(TX_N + i));
        let len = ((flags >> 16) as usize).saturating_sub(4).min(BUF);
        // The frame was written behind the compiler's back.
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the descriptor says the MAC is done with this buffer, and
        // nothing else touches it until it is handed back below.
        let frame = unsafe { core::slice::from_raw_parts(rx_buf(i), len) };
        let r = f(frame);
        compiler_fence(Ordering::SeqCst);
        write(desc(TX_N + i), RX_E | if i == RX_N - 1 { RX_WRAP } else { 0 });
        self.0.rx_next.set((i + 1) % RX_N);
        r
    }
}

impl phy::TxToken for Tx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let i = self.0.tx_next.get();
        // QEMU sends during the descriptor write, so this does not spin.
        while read(desc(i)) & TX_RD != 0 {}
        // SAFETY: the MAC is done with this buffer (RD clear).
        let frame = unsafe { core::slice::from_raw_parts_mut(tx_buf(i), len.min(BUF)) };
        let r = f(frame);
        compiler_fence(Ordering::SeqCst);
        let wrap = if i == TX_N - 1 { TX_WR } else { 0 };
        write(desc(i), ((len.min(BUF) as u32) << 16) | TX_RD | TX_PAD | TX_CRC | wrap);
        self.0.tx_next.set((i + 1) % TX_N);
        r
    }
}

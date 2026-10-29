//! The Midea LAN protocol, as Breeze Core speaks it to air conditioners.
//!
//! Four layers, each wrapped in the next, all bytes in and bytes out:
//!
//! ```text
//! lan     0x8370 V3 session: AES-256-CBC + SHA-256   (lan.rs)
//! packet  0x5A5A V2 packet:  AES-128-ECB + MD5        (packet.rs)
//! frame   0xAA appliance frame + checksum             (frame.rs)
//! ac      command or state payload + CRC-8            (ac.rs, caps.rs)
//! ```
//!
//! Rewritten small from Breeze Core's `breeze-proto`, and held to it by the same
//! vectors (msmart-ng's own captures) in `tests/proto_vectors.rs`. Two things
//! that cost Breeze Core real time are pinned there: the swing axis
//! (HORIZONTAL 0x3, VERTICAL 0xC) and the session key size (AES-256).

pub mod ac;
pub mod caps;
pub mod crypto;
pub mod discover;
pub mod frame;
pub mod lan;
pub mod packet;

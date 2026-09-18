#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]
pub mod evicting_sender;
pub mod msghdr;
pub mod nonblocking;
pub mod packet;
pub mod quic;
pub mod quic_socket;
pub mod recvmmsg;
pub mod sendmmsg;
pub mod streamer;
#[cfg(target_os = "linux")]
pub mod xdp_quic;
#[cfg(target_os = "linux")]
pub mod xdp_udp;
#[cfg(not(target_os = "linux"))]
pub mod xdp_quic {
    #[derive(Debug)]
    pub enum XskQuicSocket {}
}

#[macro_use]
extern crate log;

#[macro_use]
extern crate solana_metrics;

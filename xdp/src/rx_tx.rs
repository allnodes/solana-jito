#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::TxCompletionRing,
        netlink::MacAddress,
        packet::{
            ETH_HEADER_SIZE, IP_HEADER_SIZE, UDP_HEADER_SIZE, UdpFrame, parse_udp_frame,
            write_eth_header, write_ip_header_for_udp, write_udp_header,
        },
        socket::{Rx, RxRing, Socket, Tx, TxRing},
        umem::{Frame, FrameOffset, Umem},
    },
    libc::{MSG_DONTWAIT, recvfrom},
    std::{
        net::SocketAddrV4,
        os::fd::{AsFd, AsRawFd, RawFd},
        ptr, slice,
    },
};

const PACKET_HEADER_SIZE: usize = ETH_HEADER_SIZE + IP_HEADER_SIZE + UDP_HEADER_SIZE;

pub struct XskChannel<U: Umem> {
    socket: Socket<U>,
    fill: crate::device::RxFillRing<U::Frame>,
    rx: RxRing,
    completion: TxCompletionRing,
    tx: TxRing<U::Frame>,
    src_mac: MacAddress,
}

impl<U: Umem> XskChannel<U> {
    pub fn new(
        socket: Socket<U>,
        rx: Rx<U::Frame>,
        tx: Tx<U::Frame>,
        src_mac: MacAddress,
    ) -> Option<Self> {
        Some(Self {
            socket,
            fill: rx.fill,
            rx: rx.ring?,
            completion: tx.completion,
            tx: tx.ring?,
            src_mac,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.socket.as_fd().as_raw_fd()
    }

    pub fn poll_recv(
        &mut self,
        max_packets: usize,
        mut f: impl FnMut(&MacAddress, UdpFrame),
    ) -> usize {
        self.reclaim_tx();
        self.rx.sync(false);

        let mut n = 0;
        let mut refilled = 0usize;
        {
            let umem = self.socket.umem();
            let frame_size = umem.frame_size();
            let base = umem.as_ptr();
            while n < max_packets {
                let Some(desc) = self.rx.read() else {
                    break;
                };
                let frame =
                    unsafe { slice::from_raw_parts(base.add(desc.addr as usize), desc.len as usize) };
                if let Some(mac) = frame.get(6..12) {
                    let mut src_mac = MacAddress([0u8; 6]);
                    src_mac.0.copy_from_slice(mac);
                    if let Some(udp) = parse_udp_frame(frame) {
                        f(&src_mac, udp);
                    }
                }
                umem.release(FrameOffset(desc.addr as usize & !(frame_size - 1)));
                n += 1;
            }
            self.rx.commit();
            self.fill.sync(false);
            while let Some(frame) = umem.reserve() {
                let offset = frame.offset();
                if self.fill.write(frame).is_err() {
                    umem.release(offset);
                    break;
                }
                refilled += 1;
            }
        }
        self.fill.commit();
        if refilled > 0 {
            self.wake_rx();
        }
        n
    }

    pub fn tx_ready(&mut self) -> bool {
        if self.tx.available() == 0 {
            self.tx.sync(false);
        }
        self.tx.available() > 0
    }

    pub fn send(
        &mut self,
        dst_mac: &MacAddress,
        src: SocketAddrV4,
        dst: SocketAddrV4,
        payload: &[u8],
    ) -> bool {
        if !self.tx_ready() {
            return false;
        }
        let umem = self.socket.umem();
        if PACKET_HEADER_SIZE.saturating_add(payload.len()) > umem.frame_size() {
            return false;
        }
        let Some(mut frame) = umem.reserve() else {
            return false;
        };
        frame.set_len(PACKET_HEADER_SIZE + payload.len());
        let packet = umem.map_frame_mut(&frame);
        packet[PACKET_HEADER_SIZE..].copy_from_slice(payload);
        write_eth_header(packet, &self.src_mac.0, &dst_mac.0);
        write_ip_header_for_udp(
            &mut packet[ETH_HEADER_SIZE..],
            src.ip(),
            dst.ip(),
            None,
            (UDP_HEADER_SIZE + payload.len()) as u16,
        );
        write_udp_header(
            &mut packet[ETH_HEADER_SIZE + IP_HEADER_SIZE..],
            src.ip(),
            src.port(),
            dst.ip(),
            dst.port(),
            payload.len() as u16,
            false,
        );
        match self.tx.write(frame, 0) {
            Ok(()) => true,
            Err(crate::socket::RingFull(frame)) => {
                umem.release(frame.offset());
                false
            }
        }
    }

    pub fn flush_tx(&mut self) {
        self.tx.commit();
        if self.tx.needs_wakeup() {
            let _ = self.tx.wake();
        }
    }

    pub fn reclaim_tx(&mut self) {
        self.completion.sync(true);
        let umem = self.socket.umem();
        while let Some(offset) = self.completion.read() {
            umem.release(offset);
        }
    }

    fn wake_rx(&self) {
        unsafe {
            recvfrom(
                self.socket.as_fd().as_raw_fd(),
                ptr::null_mut(),
                0,
                MSG_DONTWAIT,
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
    }
}

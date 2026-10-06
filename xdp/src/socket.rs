use {
    crate::{
        device::{
            DeviceQueue, RingConsumer, RingMmap, RingProducer, RxFillRing, TxCompletionRing,
            XdpDesc, mmap_ring,
        },
        umem::{Frame, Umem},
    },
    libc::{
        AF_XDP, SOCK_RAW, SOL_XDP, XDP_COPY, XDP_MMAP_OFFSETS, XDP_PGOFF_RX_RING,
        XDP_PGOFF_TX_RING, XDP_RING_NEED_WAKEUP, XDP_RX_RING, XDP_TX_RING,
        XDP_UMEM_COMPLETION_RING, XDP_UMEM_FILL_RING, XDP_UMEM_PGOFF_COMPLETION_RING,
        XDP_UMEM_PGOFF_FILL_RING, XDP_USE_NEED_WAKEUP, XDP_ZEROCOPY, bind, getsockopt, sa_family_t,
        sendto, setsockopt, sockaddr, sockaddr_xdp, socket, socklen_t, xdp_mmap_offsets,
        xdp_umem_reg,
    },
    std::{
        io,
        marker::PhantomData,
        mem,
        os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd, RawFd},
        ptr,
        sync::atomic::Ordering,
    },
};

pub struct Socket<U: Umem> {
    fd: OwnedFd,
    dev_queue: DeviceQueue,
    umem: U,
}

#[cfg(target_os = "linux")]
fn iface_name_and_mtu(if_index: u32) -> Option<(String, u32)> {
    let mut buf = [0u8; libc::IF_NAMESIZE];
    let p = unsafe { libc::if_indextoname(if_index, buf.as_mut_ptr() as *mut libc::c_char) };
    if p.is_null() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let name = String::from_utf8(buf[..end].to_vec()).ok()?;
    let mtu = std::fs::read_to_string(format!("/sys/class/net/{name}/mtu"))
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    Some((name, mtu))
}

impl<U: Umem> Socket<U> {
    #[allow(clippy::type_complexity)]
    pub fn new(
        dev_queue: DeviceQueue,
        umem: U,
        zero_copy: bool,
        rx_fill_ring_size: usize,
        rx_ring_size: usize,
        tx_completion_ring_size: usize,
        tx_ring_size: usize,
    ) -> Result<(Self, Rx<U::Frame>, Tx<U::Frame>), io::Error> {
        unsafe {
            let fd = socket(AF_XDP, SOCK_RAW, 0);
            if fd < 0 {
                return Err(Error::syscall(
                    "socket(AF_XDP, SOCK_RAW) failed",
                    io::Error::last_os_error(),
                )
                .into());
            }
            let fd = OwnedFd::from_raw_fd(fd);

            let reg = xdp_umem_reg {
                addr: umem.as_ptr() as u64,
                len: umem.len() as u64,
                chunk_size: umem.frame_size() as u32,
                headroom: 0,
                flags: 0,
                tx_metadata_len: 0,
            };

            if setsockopt(
                fd.as_raw_fd(),
                libc::SOL_XDP,
                libc::XDP_UMEM_REG,
                &reg as *const _ as *const libc::c_void,
                mem::size_of::<xdp_umem_reg>() as libc::socklen_t,
            ) < 0
            {
                return Err(Error::syscall(
                    "setsockopt(XDP_UMEM_REG) failed",
                    io::Error::last_os_error(),
                )
                .into());
            }

            for (ring, size) in [
                (XDP_UMEM_COMPLETION_RING, tx_completion_ring_size),
                (XDP_UMEM_FILL_RING, rx_fill_ring_size),
                (XDP_TX_RING, tx_ring_size),
                (XDP_RX_RING, rx_ring_size),
            ] {
                if ring == XDP_RX_RING && size == 0 {
                    // tx only
                    continue;
                }

                if setsockopt(
                    fd.as_raw_fd(),
                    SOL_XDP,
                    ring,
                    &size as *const _ as *const libc::c_void,
                    mem::size_of::<u32>() as socklen_t,
                ) < 0
                {
                    return Err(Error::syscall(
                        format!("setsockopt(SOL_XDP, ring={ring}, size={size}) failed",),
                        io::Error::last_os_error(),
                    )
                    .into());
                }
            }

            let mut offsets: xdp_mmap_offsets = mem::zeroed();
            let mut optlen = mem::size_of::<xdp_mmap_offsets>() as socklen_t;
            if getsockopt(
                fd.as_raw_fd(),
                SOL_XDP,
                XDP_MMAP_OFFSETS,
                &mut offsets as *mut _ as *mut libc::c_void,
                &mut optlen,
            ) < 0
            {
                return Err(Error::syscall(
                    "getsockopt(XDP_MMAP_OFFSETS) failed",
                    io::Error::last_os_error(),
                )
                .into());
            }

            let tx_completion_ring = TxCompletionRing::new(
                mmap_ring(
                    fd.as_raw_fd(),
                    tx_completion_ring_size.saturating_mul(mem::size_of::<u64>()),
                    &offsets.cr,
                    XDP_UMEM_PGOFF_COMPLETION_RING,
                )
                .map_err(|source| Error::syscall("mmap completion ring failed", source))?,
                tx_completion_ring_size as u32,
            );

            let mut rx_fill_ring = RxFillRing::new(
                mmap_ring(
                    fd.as_raw_fd(),
                    rx_fill_ring_size.saturating_mul(mem::size_of::<u64>()),
                    &offsets.fr,
                    XDP_UMEM_PGOFF_FILL_RING,
                )
                .map_err(|source| Error::syscall("mmap fill ring failed", source))?,
                rx_fill_ring_size as u32,
                fd.as_raw_fd(),
            );

            if zero_copy || rx_ring_size > 0 {
                for _ in 0..rx_fill_ring_size {
                    let Some(frame) = umem.reserve() else {
                        return Err(Error::InsufficientUmemFrames {
                            required: rx_fill_ring_size,
                            available: umem.available(),
                        }
                        .into());
                    };
                    rx_fill_ring
                        .write(frame)
                        .map_err(|source| Error::syscall("RX fill ring write failed", source))?;
                }
                rx_fill_ring.commit();
            }

            let tx_ring = Some(TxRing::new(
                mmap_ring(
                    fd.as_raw_fd(),
                    tx_ring_size.saturating_mul(mem::size_of::<XdpDesc>()),
                    &offsets.tx,
                    XDP_PGOFF_TX_RING as u64,
                )
                .map_err(|source| Error::syscall("mmap tx ring failed", source))?,
                tx_ring_size as u32,
                fd.as_raw_fd(),
            ));

            let rx_ring = if rx_ring_size > 0 {
                Some(RxRing::new(
                    mmap_ring(
                        fd.as_raw_fd(),
                        rx_ring_size.saturating_mul(mem::size_of::<XdpDesc>()),
                        &offsets.rx,
                        XDP_PGOFF_RX_RING as u64,
                    )
                    .map_err(|source| Error::syscall("mmap rx ring failed", source))?,
                    rx_ring_size as u32,
                    fd.as_raw_fd(),
                ))
            } else {
                None
            };

            let sxdp = sockaddr_xdp {
                sxdp_family: AF_XDP as sa_family_t,
                // do NEED_WAKEUP and don't do zero copy for now for maximum compatibility
                sxdp_flags: XDP_USE_NEED_WAKEUP | if zero_copy { XDP_ZEROCOPY } else { XDP_COPY },
                sxdp_ifindex: dev_queue.if_index(),
                sxdp_queue_id: dev_queue.id().0 as u32,
                sxdp_shared_umem_fd: 0,
            };

            if bind(
                fd.as_raw_fd(),
                &sxdp as *const _ as *const sockaddr,
                mem::size_of::<sockaddr_xdp>() as socklen_t,
            ) < 0
            {
                let err = io::Error::last_os_error();
                let hint = if err.raw_os_error() == Some(libc::EBUSY) {
                    ", which already has an AF_XDP socket bound to it. A queue takes only one: \
                     give this instance a range of its own with --xdp-queue-base"
                        .to_string()
                } else {
                    String::new()
                };
                let hint = if hint.is_empty()
                    && zero_copy
                    && err.raw_os_error() == Some(libc::EINVAL)
                {
                    match iface_name_and_mtu(sxdp.sxdp_ifindex) {
                        Some((name, mtu)) => format!(
                            ". A zero-copy bind fails this way when the interface MTU does not \
                             fit in one UMEM frame: {name} has MTU {mtu} and the frame is {} \
                             bytes. Lower the MTU on the interface, or run without zero copy",
                            umem.frame_size()
                        ),
                        None => String::new(),
                    }
                } else {
                    hint
                };
                return Err(Error::syscall(
                    format!(
                        "bind(AF_XDP, ifindex={}, queue={}, flags=0x{:x}) failed{hint}",
                        sxdp.sxdp_ifindex, sxdp.sxdp_queue_id, sxdp.sxdp_flags
                    ),
                    err,
                )
                .into());
            }

            let tx = Tx {
                completion: tx_completion_ring,
                ring: tx_ring,
            };
            let rx = Rx {
                fill: rx_fill_ring,
                ring: rx_ring,
            };
            Ok((
                Self {
                    fd,
                    dev_queue,
                    umem,
                },
                rx,
                tx,
            ))
        }
    }

    pub fn tx(
        queue: DeviceQueue,
        umem: U,
        zero_copy: bool,
        completion_size: usize,
        ring_size: usize,
    ) -> Result<(Self, Tx<U::Frame>), io::Error> {
        let (fill_size, rx_size) = if zero_copy {
            // See Socket::new() as to why this is needed
            let rx = queue
                .ring_sizes()
                .ok_or_else(|| io::Error::other("zero copy requires a set ring size"))?
                .rx;
            (rx, rx)
        } else {
            // no RX fill ring needed for TX only sockets
            (1, 0)
        };
        let (socket, _, tx) = Self::new(
            queue,
            umem,
            zero_copy,
            fill_size,
            rx_size,
            completion_size,
            ring_size,
        )?;
        Ok((socket, tx))
    }

    pub fn rx(
        queue: DeviceQueue,
        umem: U,
        zero_copy: bool,
        fill_size: usize,
        ring_size: usize,
    ) -> Result<(Self, Rx<U::Frame>), io::Error> {
        let (socket, rx, _) = Self::new(queue, umem, zero_copy, fill_size, ring_size, 1, 1)?;
        Ok((socket, rx))
    }

    pub fn queue(&self) -> &DeviceQueue {
        &self.dev_queue
    }

    pub fn umem(&self) -> &U {
        &self.umem
    }
}

impl<U: Umem> AsFd for Socket<U> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

pub struct Tx<F: Frame> {
    pub completion: TxCompletionRing,
    pub ring: Option<TxRing<F>>,
}

pub struct Rx<F: Frame> {
    pub fill: RxFillRing<F>,
    pub ring: Option<RxRing>,
}

pub struct TxRing<F: Frame> {
    mmap: RingMmap<XdpDesc>,
    producer: RingProducer,
    size: u32,
    fd: RawFd,
    _frame: PhantomData<F>,
}

#[derive(Debug)]
pub struct RingFull<F: Frame>(pub F);

impl<F: Frame> TxRing<F> {
    fn new(mmap: RingMmap<XdpDesc>, size: u32, fd: RawFd) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            producer: RingProducer::new(mmap.producer, mmap.consumer, size),
            mmap,
            size,
            fd,
            _frame: PhantomData,
        }
    }

    pub fn write(&mut self, frame: F, options: u32) -> Result<(), RingFull<F>> {
        let Some(index) = self.producer.produce() else {
            return Err(RingFull(frame));
        };
        let index = index & self.size.saturating_sub(1);
        unsafe {
            let desc = self.mmap.desc.add(index as usize);
            desc.write(XdpDesc {
                addr: frame.offset().0 as u64,
                len: frame.len() as u32,
                options,
            });
        }
        Ok(())
    }

    pub fn needs_wakeup(&self) -> bool {
        unsafe { (*self.mmap.flags).load(Ordering::Relaxed) & XDP_RING_NEED_WAKEUP != 0 }
    }

    pub fn wake(&self) -> Result<u64, io::Error> {
        let result = unsafe { sendto(self.fd, ptr::null(), 0, libc::MSG_DONTWAIT, ptr::null(), 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(result as u64)
    }

    pub fn capacity(&self) -> usize {
        self.size as usize
    }

    pub fn available(&self) -> usize {
        self.producer.available() as usize
    }

    pub fn commit(&mut self) {
        self.producer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.producer.sync(commit);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RxDescriptor {
    pub addr: u64,
    pub len: u32,
}

pub struct RxRing {
    mmap: RingMmap<XdpDesc>,
    consumer: RingConsumer,
    size: u32,
    #[allow(dead_code)]
    fd: RawFd,
}

impl RxRing {
    fn new(mmap: RingMmap<XdpDesc>, size: u32, fd: RawFd) -> Self {
        debug_assert!(size.is_power_of_two());
        Self {
            consumer: RingConsumer::new(mmap.producer, mmap.consumer),
            mmap,
            size,
            fd,
        }
    }

    pub fn capacity(&self) -> usize {
        self.size as usize
    }

    pub fn available(&self) -> usize {
        self.consumer.available() as usize
    }

    pub fn read(&mut self) -> Option<RxDescriptor> {
        let index = (self.consumer.consume()? & self.size.saturating_sub(1)) as usize;
        let desc = unsafe { &*self.mmap.desc.add(index) };
        Some(RxDescriptor {
            addr: desc.addr,
            len: desc.len,
        })
    }

    pub fn commit(&mut self) {
        self.consumer.commit();
    }

    pub fn sync(&mut self, commit: bool) {
        self.consumer.sync(commit);
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{message}: {source}")]
    Syscall {
        message: String,
        #[source]
        source: io::Error,
    },
    #[error(
        "insufficient UMEM frames for RX fill ring prefill: required={required}, \
         available={available}"
    )]
    InsufficientUmemFrames { required: usize, available: usize },
}

impl Error {
    fn syscall(message: impl Into<String>, source: io::Error) -> Self {
        Self::Syscall {
            message: message.into(),
            source,
        }
    }
}

impl From<Error> for io::Error {
    fn from(error: Error) -> io::Error {
        io::Error::other(error)
    }
}

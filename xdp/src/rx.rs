#![allow(clippy::arithmetic_side_effects)]

use {
    crate::{
        device::RxFillRing,
        socket::{Rx, RxRing, Socket},
        umem::{Frame, CompletedFrameOffset, FrameOffset, Umem},
    },
    libc::{MSG_DONTWAIT, recvfrom},
    std::{
        os::fd::{AsFd, AsRawFd, RawFd},
        ptr, slice,
    },
};

pub struct RxSocket<U: Umem> {
    socket: Socket<U>,
    fill: RxFillRing<U::Frame>,
    ring: RxRing,
}

impl<U: Umem> RxSocket<U> {
    pub fn new(socket: Socket<U>, rx: Rx<U::Frame>) -> Option<Self> {
        Some(Self {
            socket,
            fill: rx.fill,
            ring: rx.ring?,
        })
    }

    pub fn fd(&self) -> RawFd {
        self.socket.as_fd().as_raw_fd()
    }

    pub fn frames_available(&mut self) -> usize {
        self.socket.umem().available()
    }

    pub fn poll(&mut self, mut f: impl FnMut(&[u8])) -> usize {
        self.ring.sync(false);

        let mut n = 0;
        let mut refilled = 0usize;
        {
            let umem = self.socket.umem();
            let frame_size = umem.frame_size();
            let base = umem.as_ptr();
            while let Some(desc) = self.ring.read() {
                let data = unsafe {
                    slice::from_raw_parts(base.add(desc.addr as usize), desc.len as usize)
                };
                f(data);
                umem.release_completed(CompletedFrameOffset(FrameOffset(
                    desc.addr as usize & !(frame_size - 1),
                )));
                n += 1;
            }
            self.ring.commit();

            self.fill.sync(false);
            while let Some(frame) = umem.reserve() {
                let offset = frame.offset();
                if self.fill.write(frame).is_err() {
                    umem.release_completed(CompletedFrameOffset(offset));
                    break;
                }
                refilled += 1;
            }
        }
        self.fill.commit();
        if refilled > 0 && self.fill.needs_wakeup() {
            self.wake();
        }
        n
    }

    fn wake(&self) {
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

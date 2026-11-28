// SPDX-License-Identifier: GPL-2.0-only
//
// A Rust translation of `lib/libxdp/xdp-dispatcher.c.in` from libxdp (xdp-tools,
// https://github.com/xdp-project/xdp-tools, GPL-2.0), changed by Allnodes in 2026. Unlike
// the rest of this crate it is GPL-2.0, see LICENSE-GPL-2.0. It is the source of the
// prebuilt `agave-xdp-dispatcher` object, built by `scripts/build-agave-xdp-ebpf.sh` with
// nightly-2026-04-11 and aya-ebpf 0.1.1 (0.2 renamed `helpers::r#gen` to `generated`).
//
// An XDP multiprog dispatcher, wire-compatible with the libxdp protocol (config v3): ten
// freplace-able slots `prog0`..`prog9`, each gated by `num_progs_enabled` and its
// `chain_call_actions` bitmap; userspace freezes `conf` and attaches members via freplace.
#![no_std]
#![no_main]
#![allow(non_upper_case_globals)]

use {
    aya_ebpf::{
        bindings::{
            xdp_action::{XDP_ABORTED, XDP_PASS},
            xdp_md,
        },
        helpers::r#gen::bpf_xdp_adjust_tail,
        macros::xdp,
        programs::XdpContext,
    },
    core::ptr,
};

// The libxdp dispatcher return value stubs report when no member is attached:
// bit 31 is always set in `chain_call_actions`, so an empty slot falls through.
const XDP_DISPATCHER_RETVAL: i32 = 31;

const MAX_DISPATCHER_ACTIONS: usize = 10;

// Mirror of libxdp's `struct xdp_dispatcher_config` (dispatcher version 3,
// 128 bytes). Userspace overrides this frozen rodata at load time.
#[repr(C)]
pub struct XdpDispatcherConfig {
    pub magic: u8,
    pub dispatcher_version: u8,
    pub num_progs_enabled: u8,
    pub is_xdp_frags: u8,
    pub chain_call_actions: [u32; MAX_DISPATCHER_ACTIONS],
    pub run_prios: [u32; MAX_DISPATCHER_ACTIONS],
    pub program_flags: [u32; MAX_DISPATCHER_ACTIONS],
    pub is_xdp_devbound: u8,
}

#[unsafe(no_mangle)]
static conf: XdpDispatcherConfig = XdpDispatcherConfig {
    // libxdp XDP_DISPATCHER_MAGIC ('X'+'D'+'P') and version; non-zero so the
    // struct stays in .rodata and the layout is visible in the object.
    magic: 236,
    dispatcher_version: 3,
    num_progs_enabled: 0,
    is_xdp_frags: 0,
    chain_call_actions: [0; MAX_DISPATCHER_ACTIONS],
    run_prios: [0; MAX_DISPATCHER_ACTIONS],
    program_flags: [0; MAX_DISPATCHER_ACTIONS],
    is_xdp_devbound: 0,
};

// Declares one freplace-able slot: a global function the verifier can replace
// via BPF_PROG_TYPE_EXT. Mirrors libxdp's stub body: the volatile read keeps the
// return value from being const-folded into the dispatcher, and the no-op
// `bpf_xdp_adjust_tail` makes packet data considered invalidated so members that
// adjust the packet verify against the slot.
macro_rules! slot {
    ($name:ident) => {
        #[unsafe(no_mangle)]
        #[inline(never)]
        pub extern "C" fn $name(ctx: *mut xdp_md) -> i32 {
            let ret: i32 = XDP_DISPATCHER_RETVAL;
            if ctx.is_null() {
                return XDP_ABORTED as i32;
            }
            unsafe {
                bpf_xdp_adjust_tail(ctx, 0);
                ptr::read_volatile(&ret)
            }
        }
    };
}

slot!(prog0);
slot!(prog1);
slot!(prog2);
slot!(prog3);
slot!(prog4);
slot!(prog5);
slot!(prog6);
slot!(prog7);
slot!(prog8);
slot!(prog9);

// libxdp's freplace compatibility probe target (same shape, no adjust_tail).
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn compat_test(ctx: *mut xdp_md) -> i32 {
    let ret: i32 = XDP_DISPATCHER_RETVAL;
    if ctx.is_null() {
        return XDP_ABORTED as i32;
    }
    unsafe { ptr::read_volatile(&ret) }
}

// Runs slot `$idx`'s member and applies its chain-call policy.
macro_rules! run_slot {
    ($ctx:expr, $num:expr, $idx:expr, $slot:ident) => {
        if $num < $idx + 1 {
            return XDP_PASS;
        }
        let ret = $slot($ctx);
        let actions = unsafe { ptr::read_volatile(&conf.chain_call_actions[$idx as usize]) };
        if 1u32.wrapping_shl(ret as u32) & actions == 0 {
            return ret as u32;
        }
    };
}

#[xdp]
pub fn xdp_dispatcher(ctx: XdpContext) -> u32 {
    let raw = ctx.ctx;
    let num = unsafe { ptr::read_volatile(&conf.num_progs_enabled) };
    run_slot!(raw, num, 0u8, prog0);
    run_slot!(raw, num, 1u8, prog1);
    run_slot!(raw, num, 2u8, prog2);
    run_slot!(raw, num, 3u8, prog3);
    run_slot!(raw, num, 4u8, prog4);
    run_slot!(raw, num, 5u8, prog5);
    run_slot!(raw, num, 6u8, prog6);
    run_slot!(raw, num, 7u8, prog7);
    run_slot!(raw, num, 8u8, prog8);
    run_slot!(raw, num, 9u8, prog9);
    XDP_PASS
}

// libxdp dispatchers carry a GPL license string; keep parity so helper and
// inspection behavior matches the reference dispatcher.
#[unsafe(no_mangle)]
#[unsafe(link_section = "license")]
pub static LICENSE: [u8; 4] = *b"GPL\0";

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Unreachable; present so the verifier refuses to load if a panic sneaks in.
    loop {}
}

#![no_std]
#![no_main]

use {
    aya_ebpf::{
        bindings::xdp_action::{XDP_DROP, XDP_PASS},
        helpers::r#gen::bpf_xdp_get_buff_len,
        macros::xdp,
        programs::XdpContext,
    },
    core::ptr,
};

#[unsafe(no_mangle)]
// Set to 1 from user space at load time to control whether we must drop multi-frags packets
static AGAVE_XDP_DROP_MULTI_FRAGS: u8 = 0;

#[xdp]
pub fn agave_xdp(ctx: XdpContext) -> u32 {
    if drop_frags() && has_frags(&ctx) {
        // We're not actually dropping any valid frames here. See
        // https://lore.kernel.org/netdev/20251021173200.7908-2-alessandro.d@gmail.com
        XDP_DROP
    } else {
        // let the kernel handle the packet normally
        XDP_PASS
    }
}

#[inline]
fn drop_frags() -> bool {
    // SAFETY: This variable is only ever modified at load time, we need the volatile read to
    // prevent the compiler from optimizing it away.
    unsafe { ptr::read_volatile(&AGAVE_XDP_DROP_MULTI_FRAGS) == 1 }
}

#[inline]
fn has_frags(ctx: &XdpContext) -> bool {
    #[allow(clippy::arithmetic_side_effects)]
    let linear_len = ctx.data_end() - ctx.data();
    // Safety: generated binding is unsafe, but static verifier guarantees ctx.ctx is valid.
    let buf_len = unsafe { bpf_xdp_get_buff_len(ctx.ctx) as usize };
    linear_len < buf_len
}


#[aya_ebpf::macros::map]
static XSKS: aya_ebpf::maps::XskMap = aya_ebpf::maps::XskMap::with_max_entries(256, 0);

#[aya_ebpf::macros::map]
static RX_PORTS: aya_ebpf::maps::HashMap<u16, u8> = aya_ebpf::maps::HashMap::with_max_entries(64, 0);

const ETH_HLEN: usize = 14;
const ETH_P_IP: u16 = 0x0800;
const IPV4_MIN_HLEN: usize = 20;
const IPPROTO_UDP: u8 = 17;

#[xdp]
pub fn agave_xdp_rx(ctx: XdpContext) -> u32 {
    if drop_frags() && has_frags(&ctx) {
        return XDP_DROP;
    }
    match try_rx(&ctx) {
        Some(action) => action,
        None => XDP_PASS,
    }
}

#[inline]
#[allow(clippy::arithmetic_side_effects)]
fn try_rx(ctx: &XdpContext) -> Option<u32> {
    let data = ctx.data();
    let data_end = ctx.data_end();

    if data + ETH_HLEN > data_end {
        return None;
    }
    let h_proto = u16::from_be(unsafe { *((data + 12) as *const u16) });
    if h_proto != ETH_P_IP {
        return None;
    }

    let ip = data + ETH_HLEN;
    if ip + IPV4_MIN_HLEN > data_end {
        return None;
    }
    let ihl = (unsafe { *(ip as *const u8) } & 0x0f) as usize * 4;
    if ihl < IPV4_MIN_HLEN {
        return None;
    }
    if unsafe { *((ip + 9) as *const u8) } != IPPROTO_UDP {
        return None;
    }

    let frag_off = u16::from_be(unsafe { *((ip + 6) as *const u16) });
    if frag_off & 0x3fff != 0 {
        return None;
    }

    let udp = ip + ihl;
    if udp + 8 > data_end {
        return None;
    }
    let dport = u16::from_be(unsafe { *((udp + 2) as *const u16) });
    if unsafe { RX_PORTS.get(&dport) }.is_none() {
        return None;
    }

    let queue = unsafe { (*ctx.ctx).rx_queue_index };
    Some(XSKS.redirect(queue, 0).unwrap_or(XDP_PASS))
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // This is so that if we accidentally panic anywhere the verifier will refuse to load the
    // program as it'll detect an infinite loop.
    #[allow(clippy::empty_loop)]
    loop {}
}

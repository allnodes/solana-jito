
use {
    crate::plan::QueuePlan,
    libc::{AF_INET, IF_NAMESIZE, SIOCETHTOOL, SOCK_DGRAM, SYS_ioctl, ifreq, socket, syscall},
    std::{
        ffi::c_char,
        io, mem,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        ptr,
    },
};

const ETHTOOL_GSTRINGS: u32 = 0x0000001b;
const ETHTOOL_GRXCLSRLCNT: u32 = 0x0000002e;
const ETHTOOL_GRXCLSRULE: u32 = 0x0000002f;
const ETHTOOL_GRXCLSRLALL: u32 = 0x00000030;
const ETHTOOL_SRXCLSRLDEL: u32 = 0x00000031;
const ETHTOOL_SRXCLSRLINS: u32 = 0x00000032;
const ETHTOOL_GSSET_INFO: u32 = 0x00000037;
const ETHTOOL_GFEATURES: u32 = 0x0000003a;
const ETHTOOL_SFEATURES: u32 = 0x0000003b;
const ETHTOOL_GCHANNELS: u32 = 0x0000003c;
const ETHTOOL_GRSSH: u32 = 0x00000046;
const ETHTOOL_SRSSH: u32 = 0x00000047;
const ETHTOOL_SCHANNELS: u32 = 0x0000003d;

const ETH_SS_FEATURES: u32 = 4;
const ETH_GSTRING_LEN: usize = 32;
const NTUPLE_FEATURE: &str = "rx-ntuple-filter";

const UDP_V4_FLOW: u32 = 0x02;
const FLOW_TYPE_FLAGS: u32 = 0xe000_0000;
const FLOW_RSS: u32 = 0x2000_0000;
const RX_CLS_LOC_ANY: u32 = 0xffff_ffff;
const ETH_RXFH_CONTEXT_ALLOC: u32 = 0xffff_ffff;

const SSET_INFO_DATA: usize = 16;
const GSTRINGS_DATA: usize = 12;
const FEATURES_DATA: usize = 8;
const RXNFC_RULE_CNT: usize = 184;
const RXNFC_RULE_LOCS: usize = 188;
const RXFH_HEADER: usize = 24;
const RXFH_RSS_CONTEXT: usize = 4;
const RXFH_INDIR_SIZE: usize = 8;
const GET_FEATURE_BLOCK: usize = 16;
const FEATURE_BITS_PER_BLOCK: usize = u32::BITS as usize;
const TCPIP4_SPEC_LEN: usize = 52;
const TCPIP4_SPEC_PDST: usize = 10;
const FLOW_EXT_LEN: usize = 20;

const _: () = assert!(GET_FEATURE_BLOCK == 4 * size_of::<u32>());
const _: () = assert!(size_of::<Channels>() == 9 * size_of::<u32>());
const _: () = assert!(align_of::<Channels>() == align_of::<u32>());
const _: () = assert!(size_of::<RxFlowSpec>() == 168);
const _: () = assert!(std::mem::offset_of!(RxFlowSpec, h_u) == 4);
const _: () = assert!(std::mem::offset_of!(RxFlowSpec, m_u) == 76);
const _: () = assert!(std::mem::offset_of!(RxFlowSpec, ring_cookie) == 152);
const _: () = assert!(std::mem::offset_of!(RxFlowSpec, location) == 160);
const _: () = assert!(std::mem::offset_of!(Rxnfc, data) == 8);
const _: () = assert!(std::mem::offset_of!(Rxnfc, fs) == 16);
const _: () = assert!(std::mem::offset_of!(Rxnfc, rule_cnt) == RXNFC_RULE_CNT);
const _: () = assert!(RXNFC_RULE_LOCS == RXNFC_RULE_CNT + size_of::<u32>());
const SET_FEATURE_BLOCK: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Channels {
    cmd: u32,
    pub max_rx: u32,
    pub max_tx: u32,
    pub max_other: u32,
    pub max_combined: u32,
    pub rx_count: u32,
    pub tx_count: u32,
    pub other_count: u32,
    pub combined_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct RxFlowSpec {
    flow_type: u32,
    h_u: [u8; TCPIP4_SPEC_LEN],
    h_ext: [u8; FLOW_EXT_LEN],
    m_u: [u8; TCPIP4_SPEC_LEN],
    m_ext: [u8; FLOW_EXT_LEN],
    ring_cookie: u64,
    location: u32,
}

impl RxFlowSpec {
    const fn zeroed() -> Self {
        Self {
            flow_type: 0,
            h_u: [0; TCPIP4_SPEC_LEN],
            h_ext: [0; FLOW_EXT_LEN],
            m_u: [0; TCPIP4_SPEC_LEN],
            m_ext: [0; 20],
            ring_cookie: 0,
            location: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct Rxnfc {
    cmd: u32,
    flow_type: u32,
    data: u64,
    fs: RxFlowSpec,
    rule_cnt: u32,
}

impl Rxnfc {
    const fn new(cmd: u32) -> Self {
        Self {
            cmd,
            flow_type: 0,
            data: 0,
            fs: RxFlowSpec::zeroed(),
            rule_cnt: 0,
        }
    }
}

fn ethtool_ioctl<T>(if_name: &str, cmd: &mut T) -> io::Result<()> {
    ethtool_ioctl_raw(if_name, (cmd as *mut T).cast())
}

fn ethtool_ioctl_bytes(if_name: &str, buf: &mut [u8]) -> io::Result<()> {
    ethtool_ioctl_raw(if_name, buf.as_mut_ptr().cast())
}

fn ethtool_ioctl_raw(if_name: &str, data: *mut c_char) -> io::Result<()> {
    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut ifr: ifreq = unsafe { mem::zeroed() };
    unsafe {
        ptr::copy_nonoverlapping(
            if_name.as_ptr() as *const c_char,
            ifr.ifr_name.as_mut_ptr(),
            if_name.len().min(IF_NAMESIZE - 1),
        );
    }
    ifr.ifr_ifru.ifru_data = data;

    let res = unsafe { syscall(SYS_ioctl, fd.as_raw_fd(), SIOCETHTOOL, &ifr) };
    if res < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn get_channels(if_name: &str) -> io::Result<Channels> {
    let mut ch = Channels {
        cmd: ETHTOOL_GCHANNELS,
        ..Channels::default()
    };
    ethtool_ioctl(if_name, &mut ch)?;
    Ok(ch)
}

pub fn ensure_combined_at_least(if_name: &str, want: u32) -> io::Result<()> {
    let current = get_channels(if_name)?;
    if current.combined_count >= want {
        return Ok(());
    }
    if current.max_combined < want {
        return Err(io::Error::other(format!(
            "{if_name}: NIC exposes max_combined={} but {want} combined channels are required for \
             unified XDP RX+TX; this NIC cannot support the unified mode",
            current.max_combined
        )));
    }
    let mut set = Channels {
        cmd: ETHTOOL_SCHANNELS,
        combined_count: want,
        ..current
    };
    ethtool_ioctl(if_name, &mut set)
}

fn sized(header: usize, count: usize, stride: usize) -> io::Result<usize> {
    count
        .checked_mul(stride)
        .and_then(|body| body.checked_add(header))
        .ok_or_else(|| io::Error::other("ethtool request size overflows"))
}

struct FeatureBit {
    block: usize,
    mask: u32,
    total: usize,
}

fn feature_bit_at(idx: usize, total: usize) -> FeatureBit {
    FeatureBit {
        block: idx.wrapping_div(FEATURE_BITS_PER_BLOCK),
        mask: 1u32 << idx.wrapping_rem(FEATURE_BITS_PER_BLOCK),
        total,
    }
}

fn feature_index(if_name: &str, want: &str) -> io::Result<FeatureBit> {
    let mut info = [0u8; 20];
    info[..4].copy_from_slice(&ETHTOOL_GSSET_INFO.to_ne_bytes());
    info[8..16].copy_from_slice(&(1u64 << ETH_SS_FEATURES).to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut info)?;
    let total = usize::try_from(read_u32(&info, SSET_INFO_DATA)?).map_err(io::Error::other)?;
    if total == 0 {
        return Err(io::Error::other(format!(
            "{if_name}: device reports no feature names"
        )));
    }

    let mut buf = vec![0u8; sized(GSTRINGS_DATA, total, ETH_GSTRING_LEN)?];
    buf[..4].copy_from_slice(&ETHTOOL_GSTRINGS.to_ne_bytes());
    buf[4..8].copy_from_slice(&ETH_SS_FEATURES.to_ne_bytes());
    buf[8..12].copy_from_slice(&u32::try_from(total).map_err(io::Error::other)?.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)?;

    for (idx, name) in buf[GSTRINGS_DATA..]
        .chunks_exact(ETH_GSTRING_LEN)
        .enumerate()
    {
        if name.split(|b| *b == 0).next().unwrap_or_default() == want.as_bytes() {
            return Ok(feature_bit_at(idx, total));
        }
    }
    Err(io::Error::other(format!(
        "{if_name}: kernel exposes no `{want}` feature"
    )))
}

fn feature_blocks(if_name: &str, total: usize) -> io::Result<Vec<[u32; 4]>> {
    let blocks = total.div_ceil(FEATURE_BITS_PER_BLOCK);
    let mut buf = vec![0u8; sized(FEATURES_DATA, blocks, GET_FEATURE_BLOCK)?];
    buf[..4].copy_from_slice(&ETHTOOL_GFEATURES.to_ne_bytes());
    buf[4..8].copy_from_slice(&u32::try_from(blocks).map_err(io::Error::other)?.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)?;
    let returned = usize::try_from(read_u32(&buf, 4)?)
        .map_err(io::Error::other)?
        .min(blocks);

    Ok(buf[FEATURES_DATA..]
        .chunks_exact(GET_FEATURE_BLOCK)
        .take(returned)
        .map(|block| {
            let mut words = [0u32; 4];
            for (word, raw) in words.iter_mut().zip(block.chunks_exact(4)) {
                *word = u32::from_ne_bytes(raw.try_into().unwrap_or([0; 4]));
            }
            words
        })
        .collect())
}

fn set_feature(if_name: &str, feature: &FeatureBit) -> io::Result<()> {
    let blocks = feature.block.saturating_add(1);
    let mut buf = vec![0u8; sized(FEATURES_DATA, blocks, SET_FEATURE_BLOCK)?];
    buf[..4].copy_from_slice(&ETHTOOL_SFEATURES.to_ne_bytes());
    buf[4..8].copy_from_slice(&u32::try_from(blocks).map_err(io::Error::other)?.to_ne_bytes());
    let Some(target) = buf[FEATURES_DATA..]
        .chunks_exact_mut(SET_FEATURE_BLOCK)
        .nth(feature.block)
    else {
        return Err(io::Error::other(format!(
            "{if_name}: feature block {} is out of range",
            feature.block
        )));
    };
    target[..4].copy_from_slice(&feature.mask.to_ne_bytes());
    target[4..].copy_from_slice(&feature.mask.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)
}

pub fn supports_ntuple(if_name: &str) -> io::Result<bool> {
    let feature = feature_index(if_name, NTUPLE_FEATURE)?;
    let blocks = feature_blocks(if_name, feature.total)?;
    let Some([available, _requested, active, never_changed]) = blocks.get(feature.block).copied()
    else {
        return Ok(false);
    };
    Ok(active & feature.mask != 0
        || (available & feature.mask != 0 && never_changed & feature.mask == 0))
}

pub fn record_pre_change_queues(if_index: u32, present: u32) -> io::Result<()> {
    let dir = std::path::Path::new("/sys/fs/bpf/xdp").join(format!("agave-{if_index}"));
    if std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("combined-"))
        })
        .unwrap_or(false)
    {
        return Ok(());
    }
    std::fs::create_dir_all(dir.join(format!("combined-{present}")))
}

pub fn pre_change_queues(if_index: u32) -> Option<u32> {
    std::fs::read_dir(std::path::Path::new("/sys/fs/bpf/xdp").join(format!("agave-{if_index}")))
        .ok()?
        .flatten()
        .find_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("combined-")
                .and_then(|count| count.parse::<u32>().ok())
        })
}

pub fn receive_queue_budget(if_name: &str) -> io::Result<(u32, u32)> {
    Ok(budget_of(&get_channels(if_name)?))
}

fn budget_of(channels: &Channels) -> (u32, u32) {
    (
        channels.combined_count.saturating_add(channels.rx_count),
        channels.max_combined.saturating_add(channels.max_rx),
    )
}

fn enable_ntuple(if_name: &str) -> io::Result<()> {
    let feature = feature_index(if_name, NTUPLE_FEATURE)?;
    let blocks = feature_blocks(if_name, feature.total)?;
    let Some([available, _requested, active, never_changed]) =
        blocks.get(feature.block).copied()
    else {
        return Err(io::Error::other(format!(
            "{if_name}: kernel did not report the feature block holding `{NTUPLE_FEATURE}`"
        )));
    };
    if active & feature.mask != 0 {
        return Ok(());
    }
    if available & feature.mask == 0 || never_changed & feature.mask != 0 {
        return Err(io::Error::other(format!(
            "{if_name}: device cannot enable `{NTUPLE_FEATURE}`, so UDP traffic cannot be steered \
             to the XDP receive queue"
        )));
    }
    set_feature(if_name, &feature)?;

    let after = feature_blocks(if_name, feature.total)?;
    if after.get(feature.block).map_or(0, |block| block[2]) & feature.mask == 0 {
        return Err(io::Error::other(format!(
            "{if_name}: `{NTUPLE_FEATURE}` did not take effect"
        )));
    }
    Ok(())
}

fn read_u32(buf: &[u8], offset: usize) -> io::Result<u32> {
    buf.get(offset..offset.saturating_add(4))
        .and_then(|raw| raw.try_into().ok())
        .map(u32::from_ne_bytes)
        .ok_or_else(|| io::Error::other("ethtool response is shorter than its own header"))
}

pub fn steer_udp_port_to_queue(if_name: &str, port: u16, queue: u32) -> io::Result<()> {
    enable_ntuple(if_name)?;
    let installed = existing_rules(if_name)?;
    let want = wanted_rule(port, queue);
    if installed
        .iter()
        .any(|(rule, context)| same_match(rule, &want) && *context == 0)
    {
        return Ok(());
    }

    let mut req = Rxnfc::new(ETHTOOL_SRXCLSRLINS);
    req.fs = want;
    req.fs.location = RX_CLS_LOC_ANY;
    if ethtool_ioctl(if_name, &mut req).is_ok() {
        return Ok(());
    }

    let mut req = Rxnfc::new(ETHTOOL_SRXCLSRLINS);
    req.fs = want;
    req.fs.location = first_free_location(&installed);
    ethtool_ioctl(if_name, &mut req)
}

pub fn steer_udp_port_to_context(if_name: &str, port: u16, context: u32) -> io::Result<()> {
    enable_ntuple(if_name)?;
    let installed = existing_rules(if_name)?;
    let mut want = wanted_rule(port, 0);
    want.flow_type |= FLOW_RSS;
    if installed
        .iter()
        .any(|(rule, installed_context)| same_match(rule, &want) && *installed_context == context)
    {
        return Ok(());
    }

    let mut req = Rxnfc::new(ETHTOOL_SRXCLSRLINS);
    req.fs = want;
    req.fs.location = RX_CLS_LOC_ANY;
    req.rule_cnt = context;
    if ethtool_ioctl(if_name, &mut req).is_ok() {
        return Ok(());
    }

    let mut req = Rxnfc::new(ETHTOOL_SRXCLSRLINS);
    req.fs = want;
    req.fs.location = first_free_location(&installed);
    req.rule_cnt = context;
    ethtool_ioctl(if_name, &mut req)
}

fn group_context(if_name: &str, ports: &[u16], queues: &[u32]) -> Option<u32> {
    port_steering(if_name).ok()?.into_iter().find_map(|rule| {
        let context = rule.context?;
        if !ports.contains(&rule.port) {
            return None;
        }
        (spread_of(if_name, context)? == queues).then_some(context)
    })
}

fn spread_of(if_name: &str, context: u32) -> Option<Vec<u32>> {
    let mut queues = rss_context_table(if_name, context).ok()?;
    queues.sort_unstable();
    queues.dedup();
    Some(queues)
}

fn first_free_location(installed: &[(RxFlowSpec, u32)]) -> u32 {
    (0u32..)
        .find(|slot| !installed.iter().any(|(rule, _)| rule.location == *slot))
        .unwrap_or(0)
}

fn same_match(a: &RxFlowSpec, b: &RxFlowSpec) -> bool {
    a.flow_type & !FLOW_TYPE_FLAGS == b.flow_type & !FLOW_TYPE_FLAGS
        && a.h_u == b.h_u
        && a.m_u == b.m_u
        && a.ring_cookie == b.ring_cookie
}

fn wanted_rule(port: u16, queue: u32) -> RxFlowSpec {
    RxFlowSpec {
        flow_type: UDP_V4_FLOW,
        h_u: udp4_dst_port(port),
        m_u: udp4_dst_port_mask(),
        ring_cookie: u64::from(queue),
        ..RxFlowSpec::zeroed()
    }
}

fn udp4_dst_port(port: u16) -> [u8; TCPIP4_SPEC_LEN] {
    let mut spec = [0u8; TCPIP4_SPEC_LEN];
    spec[TCPIP4_SPEC_PDST..][..2].copy_from_slice(&port.to_be_bytes());
    spec
}

fn udp4_dst_port_mask() -> [u8; TCPIP4_SPEC_LEN] {
    let mut mask = [0u8; TCPIP4_SPEC_LEN];
    mask[TCPIP4_SPEC_PDST..][..2].copy_from_slice(&u16::MAX.to_be_bytes());
    mask
}

fn existing_rules(if_name: &str) -> io::Result<Vec<(RxFlowSpec, u32)>> {
    let mut count_req = Rxnfc::new(ETHTOOL_GRXCLSRLCNT);
    ethtool_ioctl(if_name, &mut count_req)?;
    let count = usize::try_from(count_req.rule_cnt).map_err(io::Error::other)?;
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut buf = vec![0u8; sized(RXNFC_RULE_LOCS, count, 4)?];
    buf[..4].copy_from_slice(&ETHTOOL_GRXCLSRLALL.to_ne_bytes());
    buf[RXNFC_RULE_CNT..RXNFC_RULE_LOCS].copy_from_slice(&count_req.rule_cnt.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)?;
    let returned = usize::try_from(read_u32(&buf, RXNFC_RULE_CNT)?)
        .map_err(io::Error::other)?
        .min(count);

    let mut rules = Vec::with_capacity(returned);
    for slot in buf[RXNFC_RULE_LOCS..].chunks_exact(4).take(returned) {
        let location = u32::from_ne_bytes(slot.try_into().unwrap_or([0; 4]));
        let mut req = Rxnfc::new(ETHTOOL_GRXCLSRULE);
        req.fs.location = location;
        if ethtool_ioctl(if_name, &mut req).is_err() {
            rules.push((
                RxFlowSpec {
                    location,
                    ..RxFlowSpec::zeroed()
                },
                0,
            ));
            continue;
        }
        rules.push((req.fs, req.rule_cnt));
    }
    Ok(rules)
}

fn steering_port(rule: &RxFlowSpec) -> Option<u16> {
    if rule.flow_type & !FLOW_TYPE_FLAGS != UDP_V4_FLOW || rule.m_u != udp4_dst_port_mask() {
        return None;
    }
    let port = u16::from_be_bytes([rule.h_u[10], rule.h_u[11]]);
    (rule.h_u == udp4_dst_port(port)).then_some(port)
}

pub fn clear_udp_steering(if_name: &str, ports: Option<&[u16]>) -> io::Result<Vec<SteeringRule>> {
    let removed: Vec<SteeringRule> = port_steering(if_name)?
        .into_iter()
        .filter(|rule| ports.is_none_or(|ports| ports.contains(&rule.port)))
        .collect();
    for rule in &removed {
        delete_rule(if_name, rule.slot)?;
        if let Some(context) = rule.context {
            let _ = delete_rss_context(if_name, context);
        }
    }
    Ok(removed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SteeringRule {
    pub slot: u32,
    pub port: u16,
    pub queue: u32,
    pub context: Option<u32>,
}

pub fn port_steering(if_name: &str) -> io::Result<Vec<SteeringRule>> {
    Ok(existing_rules(if_name)?
        .into_iter()
        .filter_map(|(rule, context)| {
            steering_port(&rule).map(|port| SteeringRule {
                slot: rule.location,
                port,
                queue: u32::try_from(rule.ring_cookie).unwrap_or(u32::MAX),
                context: (context != 0).then_some(context),
            })
        })
        .collect())
}

fn delete_rule(if_name: &str, location: u32) -> io::Result<()> {
    let mut req = Rxnfc::new(ETHTOOL_SRXCLSRLDEL);
    req.fs.location = location;
    ethtool_ioctl(if_name, &mut req)
}

fn rss_indirection_size(if_name: &str) -> io::Result<usize> {
    let mut buf = [0u8; RXFH_HEADER];
    buf[..4].copy_from_slice(&ETHTOOL_GRSSH.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)?;
    usize::try_from(read_u32(&buf, RXFH_INDIR_SIZE)?).map_err(io::Error::other)
}

pub fn create_rss_context(if_name: &str, queues: &[u32]) -> io::Result<u32> {
    if queues.is_empty() {
        return Err(io::Error::other(format!(
            "{if_name}: an RSS context needs at least one queue"
        )));
    }
    let entries = rss_indirection_size(if_name)?;
    if entries == 0 {
        return Err(io::Error::other(format!(
            "{if_name}: device reports no RSS indirection table, so it cannot spread one port \
             over several queues"
        )));
    }
    let mut buf = vec![0u8; sized(RXFH_HEADER, entries, 4)?];
    buf[..4].copy_from_slice(&ETHTOOL_SRSSH.to_ne_bytes());
    buf[RXFH_RSS_CONTEXT..RXFH_INDIR_SIZE].copy_from_slice(&ETH_RXFH_CONTEXT_ALLOC.to_ne_bytes());
    buf[RXFH_INDIR_SIZE..RXFH_INDIR_SIZE.saturating_add(4)]
        .copy_from_slice(&u32::try_from(entries).map_err(io::Error::other)?.to_ne_bytes());
    for (cell, queue) in buf[RXFH_HEADER..]
        .chunks_exact_mut(4)
        .zip(queues.iter().cycle())
    {
        cell.copy_from_slice(&queue.to_ne_bytes());
    }
    ethtool_ioctl_bytes(if_name, &mut buf)?;
    read_u32(&buf, RXFH_RSS_CONTEXT)
}

pub fn rss_context_table(if_name: &str, context: u32) -> io::Result<Vec<u32>> {
    let entries = rss_indirection_size(if_name)?;
    let mut buf = vec![0u8; sized(RXFH_HEADER, entries, 4)?];
    buf[..4].copy_from_slice(&ETHTOOL_GRSSH.to_ne_bytes());
    buf[RXFH_RSS_CONTEXT..RXFH_INDIR_SIZE].copy_from_slice(&context.to_ne_bytes());
    buf[RXFH_INDIR_SIZE..RXFH_INDIR_SIZE.saturating_add(4)]
        .copy_from_slice(&u32::try_from(entries).map_err(io::Error::other)?.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)?;
    Ok(buf[RXFH_HEADER..]
        .chunks_exact(4)
        .map(|cell| u32::from_ne_bytes(cell.try_into().unwrap_or([0; 4])))
        .collect())
}

pub fn delete_rss_context(if_name: &str, context: u32) -> io::Result<()> {
    let mut buf = [0u8; RXFH_HEADER];
    buf[..4].copy_from_slice(&ETHTOOL_SRSSH.to_ne_bytes());
    buf[RXFH_RSS_CONTEXT..RXFH_INDIR_SIZE].copy_from_slice(&context.to_ne_bytes());
    ethtool_ioctl_bytes(if_name, &mut buf)
}

pub fn apply_queue_plan(if_name: &str, plan: &QueuePlan) -> io::Result<()> {
    let (present, _) = receive_queue_budget(if_name)?;
    if let Ok(device) = crate::device::NetworkDevice::new(if_name) {
        let _ = record_pre_change_queues(device.if_index(), present);
    }
    ensure_combined_at_least(if_name, plan.total_queues())?;

    let wanted: Vec<(u16, Vec<u32>)> = plan
        .receive_groups()
        .iter()
        .flat_map(|group| {
            let queues: Vec<u32> = group.queue_ids().collect();
            group.ports.iter().map(move |port| (*port, queues.clone()))
        })
        .collect();
    let (base, ours) = (plan.queue_base(), plan.total_queues());
    let mut foreign: Vec<(u16, u32)> = Vec::new();
    for rule in port_steering(if_name)? {
        let ours_to_touch = match rule.context {
            None => plan.owns_queue(rule.queue),
            Some(context) => spread_of(if_name, context)
                .is_some_and(|spread| spread.iter().all(|queue| plan.owns_queue(*queue))),
        };
        if !ours_to_touch {
            continue;
        }
        let asked = wanted.iter().find(|(port, _)| *port == rule.port).map(|(_, queues)| queues);
        let delivers = match (asked, rule.context) {
            (Some(queues), None) => queues.as_slice() == [rule.queue],
            (Some(queues), Some(context)) => {
                spread_of(if_name, context).is_some_and(|spread| &spread == queues)
            }
            (None, _) => false,
        };
        if delivers {
            continue;
        }
        if !wanted.iter().any(|(port, _)| *port == rule.port) {
            foreign.push((rule.port, rule.queue));
        }
        delete_rule(if_name, rule.slot)?;
        if let Some(context) = rule.context {
            let _ = delete_rss_context(if_name, context);
        }
    }
    if !foreign.is_empty() {
        log::warn!(
            "{if_name}: removed {} steering rule(s) inside this layout's queue range \
             {base}..{ours} that the plan does not ask for ({}). If they belong to another \
             program on this NIC, give it a range of its own with --xdp-queue-base.",
            foreign.len(),
            foreign
                .iter()
                .map(|(port, queue)| format!("udp/{port} on queue {queue}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for group in plan.receive_groups() {
        let queues: Vec<u32> = group.queue_ids().collect();
        let Some((&first_queue, rest)) = queues.split_first() else {
            continue;
        };
        if rest.is_empty() {
            for port in &group.ports {
                steer_udp_port_to_queue(if_name, *port, first_queue)?;
            }
            continue;
        }
        ensure_combined_at_least(if_name, plan.total_queues())?;
        let context = match group_context(if_name, &group.ports, &queues) {
            Some(context) => context,
            None => create_rss_context(if_name, &queues)?,
        };
        for port in &group.ports {
            steer_udp_port_to_context(if_name, *port, context)?;
        }
    }
    Ok(())
}

pub fn set_combined(if_name: &str, want: u32) -> io::Result<()> {
    let current = get_channels(if_name)?;
    if current.combined_count == want {
        return Ok(());
    }
    if current.max_combined < want {
        return Err(io::Error::other(format!(
            "{if_name}: cannot set {want} combined channels, the NIC's maximum is {}",
            current.max_combined
        )));
    }
    let mut set = Channels {
        cmd: ETHTOOL_SCHANNELS,
        combined_count: want,
        ..current
    };
    ethtool_ioctl(if_name, &mut set)
}


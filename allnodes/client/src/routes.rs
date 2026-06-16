use {
    allnodes_service_protos::{HeartbeatResponse, RouteEntry},
    arc_swap::ArcSwap,
    std::{
        cmp::Ordering,
        net::{Ipv4Addr, SocketAddr, SocketAddrV4},
        sync::{Arc, LazyLock},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Relative { port_type: u8, offset: u8 },
    Absolute(SocketAddr),
}

pub const DEFAULT_TARGETS_UDP: &[u8] = &[0x02, 0x00];
pub const DEFAULT_TARGETS_QUIC: &[u8] = &[0x02, 0x06];

#[derive(Default)]
pub struct RouteMap {
    entries: Vec<RouteEntry>,
}

impl RouteMap {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn lookup(&self, slot: u64) -> Option<&RouteEntry> {
        self.entries
            .binary_search_by(|e| match decode_range(&e.range) {
                Some((from, to)) => {
                    if slot < from {
                        Ordering::Greater
                    } else if slot > to {
                        Ordering::Less
                    } else {
                        Ordering::Equal
                    }
                }
                None => Ordering::Less,
            })
            .ok()
            .map(|i| &self.entries[i])
    }
}

pub static ROUTING_CONFIG: LazyLock<ArcSwap<RouteMap>> =
    LazyLock::new(|| ArcSwap::from_pointee(RouteMap::empty()));

fn decode_range(bytes: &[u8]) -> Option<(u64, u64)> {
    let b: &[u8; 6] = bytes.try_into().ok()?;
    let from = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let delta = u16::from_be_bytes([b[4], b[5]]);
    let from = u64::from(from);
    Some((from, from.saturating_add(u64::from(delta))))
}

pub fn decode_targets(bytes: &[u8]) -> impl Iterator<Item = Target> + '_ {
    let mut rest: &[u8] = bytes;
    std::iter::from_fn(move || {
        let (&tag, tail) = rest.split_first()?;
        match tag {
            0x00..=0x03 => {
                let [offset, next @ ..] = tail else {
                    return None;
                };
                rest = next;
                Some(Target::Relative {
                    port_type: tag & 0x03,
                    offset: *offset,
                })
            }
            0x80 => {
                let [a, b, c, d, p_hi, p_lo, next @ ..] = tail else {
                    return None;
                };
                rest = next;
                let ip = Ipv4Addr::new(*a, *b, *c, *d);
                let port = u16::from_be_bytes([*p_hi, *p_lo]);
                Some(Target::Absolute(SocketAddr::V4(SocketAddrV4::new(ip, port))))
            }
            _ => None,
        }
    })
}

fn build_map(response: HeartbeatResponse) -> RouteMap {
    RouteMap {
        entries: response
            .routes
            .into_iter()
            .filter(|r| r.range.len() == 6)
            .collect(),
    }
}

pub fn apply_response(response: HeartbeatResponse) {
    let map = build_map(response);
    log::debug!("delivery config updated: {} entries", map.entries.len());
    ROUTING_CONFIG.store(Arc::new(map));
}


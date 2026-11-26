
use super::{BytesTxPacket, TrySendError};
use std::io;

#[cfg(target_os = "linux")]
type Sender<T> = crossbeam_channel::Sender<T>;

#[derive(Clone)]
pub struct XdpSender {
    #[cfg(target_os = "linux")]
    pub(super) slaves: Vec<Vec<Sender<BytesTxPacket>>>,
    #[cfg(target_os = "linux")]
    pub(super) live: std::sync::Arc<[std::sync::atomic::AtomicBool]>,
}

impl XdpSender {
    #[cfg(target_os = "linux")]
    pub fn disconnected() -> Self {
        Self {
            slaves: Vec::new(),
            live: Vec::new().into(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn disconnected() -> Self {
        Self {}
    }

    pub fn validate_subset_positions(
        positions: &[usize],
        sender_count: usize,
    ) -> Result<(), io::Error> {
        fn invalid_input(message: impl Into<String>) -> io::Error {
            io::Error::new(io::ErrorKind::InvalidInput, message.into())
        }

        if positions.is_empty() {
            return Err(invalid_input("XDP sender subset cannot be empty"));
        }
        if let Some(&position) = positions.iter().find(|&&position| position >= sender_count) {
            return Err(invalid_input(format!(
                "XDP sender subset position {position} is out of range for {sender_count} \
                 configured XDP sender(s)"
            )));
        }
        if let Some((_, &position)) = positions
            .iter()
            .enumerate()
            .find(|(i, position)| positions[..*i].contains(position))
        {
            return Err(invalid_input(format!(
                "XDP sender subset position {position} is repeated"
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn subset(&self, positions: &[usize]) -> Result<Self, io::Error> {
        Self::validate_subset_positions(positions, self.len())?;

        let n = self.slaves.len();
        if n == 0 {
            return Ok(self.clone());
        }
        let mut slaves: Vec<Vec<Sender<BytesTxPacket>>> = vec![Vec::new(); n];
        for &position in positions {
            let group = &self.slaves[position % n];
            slaves[position % n].push(group[position / n].clone());
        }
        Ok(Self {
            slaves,
            live: self.live.clone(),
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn subset(&self, _positions: &[usize]) -> Result<Self, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "XDP is only supported on Linux",
        ))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    #[allow(clippy::arithmetic_side_effects)]
    pub fn try_send(
        &self,
        sender_index: usize,
        packet: BytesTxPacket,
    ) -> Result<(), TrySendError<BytesTxPacket>> {
        use std::sync::atomic::Ordering;

        let n = self.slaves.len();
        if n == 0 {
            return Ok(());
        }
        if n == 1 {
            return match self.slaves[0].as_slice() {
                [] => Ok(()),
                group => group[sender_index % group.len()].try_send(packet),
            };
        }
        let start = sender_index % n;
        for off in 0..n {
            let s = (start + off) % n;
            let group = &self.slaves[s];
            if !group.is_empty() && self.live[s].load(Ordering::Relaxed) {
                return group[(sender_index / n) % group.len()].try_send(packet);
            }
        }
        for off in 0..n {
            let s = (start + off) % n;
            let group = &self.slaves[s];
            if !group.is_empty() {
                return group[(sender_index / n) % group.len()].try_send(packet);
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub fn slaves_up(&self) -> usize {
        self.live
            .iter()
            .zip(self.slaves.iter())
            .filter(|(live, group)| {
                !group.is_empty() && live.load(std::sync::atomic::Ordering::Relaxed)
            })
            .count()
    }

    #[cfg(target_os = "linux")]
    pub fn len(&self) -> usize {
        self.slaves.iter().map(|group| group.len()).sum()
    }

    #[cfg(target_os = "linux")]
    pub fn is_empty(&self) -> bool {
        self.slaves.iter().all(|group| group.is_empty())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn try_send(
        &self,
        _sender_index: usize,
        packet: BytesTxPacket,
    ) -> Result<(), TrySendError<BytesTxPacket>> {
        Err(TrySendError::Disconnected(packet))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn slaves_up(&self) -> usize {
        0
    }

    #[cfg(not(target_os = "linux"))]
    pub fn len(&self) -> usize {
        0
    }

    #[cfg(not(target_os = "linux"))]
    pub fn is_empty(&self) -> bool {
        true
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::arithmetic_side_effects)]
pub(super) fn assign_loops(
    num_loops: usize,
    tx_queues_per_slave: &[Option<u32>],
    queue_base: u64,
) -> Result<Vec<(usize, u64)>, String> {
    let slave_cnt = tx_queues_per_slave.len();
    if num_loops == 0 {
        return Ok(Vec::new());
    }
    if slave_cnt == 0 {
        return Err("no devices to assign transmit loops to".to_string());
    }

    let loops_on = |s: usize| num_loops.saturating_sub(s).div_ceil(slave_cnt);

    let mut start = Vec::with_capacity(slave_cnt);
    let mut wanted_start = 0u64;
    for (s, queues) in tx_queues_per_slave.iter().enumerate() {
        let wanted = loops_on(s) as u64;
        let offset = wanted_start;
        wanted_start = wanted_start.saturating_add(wanted);
        let Some(queues) = queues else {
            start.push(queue_base);
            continue;
        };
        let usable = u64::from(*queues).saturating_sub(queue_base);
        if wanted > usable {
            return Err(format!(
                "slave {s} has {queues} transmit queue(s) and the receive plan reserves the                  first {queue_base}, leaving room for {usable} transmit loop(s) — this                  configuration asks it to run {wanted}. Give the transmitter fewer CPUs, or                  raise the device's combined channel count."
            ));
        }
        start.push(queue_base + offset.min(usable - wanted));
    }

    Ok((0..num_loops)
        .map(|i| {
            let slave_idx = i % slave_cnt;
            (slave_idx, start[slave_idx] + (i / slave_cnt) as u64)
        })
        .collect())
}


use {
    crate::{
        device::{NetworkDevice, QueueId},
        ethtool,
        umem::{OwnedUmem, PageAlignedMemory},
    },
    std::io,
};

const PROBE_FRAME_SIZE: usize = 4096;
const PROBE_RING: usize = 64;
const PROBE_FRAMES: usize = 128;

fn queue_is_free(device: &NetworkDevice, queue: u32) -> io::Result<bool> {
    let bound = device.open_queue(QueueId(u64::from(queue)))?;
    let mem = PageAlignedMemory::alloc(PROBE_FRAME_SIZE, PROBE_FRAMES)
        .map_err(|_| io::Error::other("probe umem allocation failed"))?;
    let umem = OwnedUmem::new(mem, PROBE_FRAME_SIZE as u32)?;
    match crate::socket::Socket::rx(bound, umem, false, PROBE_RING, PROBE_RING) {
        Ok(_) => Ok(true),
        Err(err) if is_busy(&err) => Ok(false),
        Err(err) => Err(io::Error::other(format!(
            "{} queue {queue}: probe bind failed: {err}",
            device.name()
        ))),
    }
}

fn is_busy(err: &io::Error) -> bool {
    if err.raw_os_error() == Some(libc::EBUSY) {
        return true;
    }
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        if let Some(io_err) = cause.downcast_ref::<io::Error>()
            && io_err.raw_os_error() == Some(libc::EBUSY)
        {
            return true;
        }
        source = cause.source();
    }
    false
}

pub fn choose_base(devices: &[String], span: u32, owned_ports: &[u16]) -> io::Result<u32> {
    if span == 0 || devices.is_empty() {
        return Ok(0);
    }
    let opened: Vec<NetworkDevice> = devices
        .iter()
        .map(|name| NetworkDevice::new(name.as_str()))
        .collect::<io::Result<_>>()?;

    let claimed: Vec<Vec<u32>> = devices
        .iter()
        .map(|name| {
            ethtool::port_steering(name)
                .map(|rules| foreign_queues(&rules, owned_ports))
                .unwrap_or_default()
        })
        .collect();

    let budgets: Vec<(u32, u32)> = devices
        .iter()
        .map(|name| ethtool::receive_queue_budget(name))
        .collect::<io::Result<_>>()?;
    let present = budgets.iter().map(|(present, _)| *present).min().unwrap_or(0);
    let ceiling = budgets.iter().map(|(_, ceiling)| *ceiling).min().unwrap_or(0);

    let mut base: u32 = 0;
    while base.saturating_add(span) <= ceiling {
        match first_busy(&opened, &claimed, base, span, present)? {
            Some(busy) => base = busy.saturating_add(1),
            None => {
                log::debug!(
                    "xdp: taking receive queues {base}..{}",
                    base.saturating_add(span)
                );
                return Ok(base);
            }
        }
    }

    Err(io::Error::other(format!(
        "no free range of {span} receive queues below the device ceiling of {ceiling} (it \
         currently has {present}). Stop another XDP consumer on this interface, or give this one \
         an explicit --xdp-queue-base inside a range you free up"
    )))
}

fn foreign_queues(rules: &[ethtool::SteeringRule], owned_ports: &[u16]) -> Vec<u32> {
    rules
        .iter()
        .filter(|rule| !owned_ports.contains(&rule.port))
        .map(|rule| rule.queue)
        .collect()
}

fn first_busy(
    devices: &[NetworkDevice],
    claimed: &[Vec<u32>],
    base: u32,
    span: u32,
    present: u32,
) -> io::Result<Option<u32>> {
    for queue in base..base.saturating_add(span).min(present) {
        if claimed.iter().any(|queues| queues.contains(&queue)) {
            return Ok(Some(queue));
        }
        for device in devices {
            if !queue_is_free(device, queue)? {
                return Ok(Some(queue));
            }
        }
    }
    Ok(None)
}


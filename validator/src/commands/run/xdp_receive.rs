
use {
    agave_xdp::plan::{QueuePlan, RxGroup},
    solana_gossip::node::Node,
    std::{
        num::NonZeroUsize,
        sync::{Arc, atomic::AtomicBool},
    },
};

const TPU_QUIC_GROUP: &str = "tpu-quic";
const TURBINE_GROUP: &str = "turbine";
const SERVICES_GROUP: &str = "udp-services";
const TPU_FORWARDS_GROUP: &str = "tpu-forwards";

pub struct ReceiveSetup {
    pub tx_base: u32,
    pub accelerated: Vec<String>,
}

pub fn configure(
    interface: &str,
    node: &Node,
    mode: ReceiveMode,
    zero_copy: bool,
    chain_loading: bool,
    tx_queues: usize,
    queue_base: Option<u32>,
    exit: Arc<AtomicBool>,
) -> ReceiveSetup {
    let slaves = agave_xdp::bond::physical_devices(interface)
        .expect("the interface must resolve to devices an AF_XDP socket can bind");
    let tx_per_slave = tx_queues.div_ceil(slaves.len().max(1)) as u32;

    let quic_ports = quic_ports(node);
    let mut groups = groups(node, &quic_ports);

    let owned_ports: Vec<u16> = groups.iter().flat_map(|group| group.ports.iter().copied()).collect();

    let (queue_base, ()) = loop {
        let span = queues_needed(&groups, tx_per_slave);
        match queue_base {
            Some(base) => break (base, ()),
            None => match agave_xdp::queue_alloc::choose_base(&slaves, span, &owned_ports) {
                Ok(base) => break (base, ()),
                Err(err) if groups.len() > 1 => {
                    let dropped = groups.pop().expect("the guard above keeps one");
                    log::warn!(
                        "xdp: {interface} has no free range for {span} receive queues ({err}); \
                         leaving {} on the kernel path and trying a smaller layout",
                        dropped.name
                    );
                }
                Err(err) => panic!("failed to find a free receive queue range: {err}"),
            },
        }
    };
    let plan =
        QueuePlan::new(groups, tx_per_slave, queue_base).expect("the receive groups form a plan");
    log::info!("xdp: receive and transmit queues start at {queue_base}");
    for slave in &slaves {
        agave_xdp::ethtool::apply_queue_plan(slave, &plan)
            .expect("failed to prepare NIC queues for unified XDP");
    }

    let mut runtime = if let ReceiveMode::Adopted(members) = mode {
        agave_xdp::runtime::XdpRuntime::adopt(interface, members).unwrap_or_else(|err| {
            panic!("the XDP program left by an earlier run could not be taken over: {err}")
        })
    } else {
        agave_xdp::runtime::XdpRuntime::new(interface, zero_copy, chain_loading)
            .expect("failed to set up the shared xdp runtime")
    };

    let udp_groups: Vec<_> = [TURBINE_GROUP, SERVICES_GROUP]
        .into_iter()
        .filter_map(|name| plan.group(name).map(|group| (name, group)))
        .map(|(name, group)| {
            let queues: Vec<u32> = group.queue_ids().collect();
            let sockets =
                solana_streamer::xdp_udp::build_xsk_rx_sockets(&mut runtime, &queues, zero_copy)
                    .expect("failed to build xdp receive sockets");
            (name, group.ports.clone(), sockets)
        })
        .collect();
    solana_streamer::xdp_udp::hold_groups(udp_groups, exit)
        .expect("failed to install the xdp receive groups");

    let quic_groups: Vec<_> = quic_ports
        .iter()
        .filter_map(|(name, port)| plan.group(name).map(|group| (*port, group)))
        .map(|(port, group)| {
            let queues: Vec<u32> = group.queue_ids().collect();
            let sockets = solana_streamer::xdp_quic::build_xsk_quic_sockets(
                &mut runtime,
                interface,
                &queues,
                port,
                zero_copy,
            )
            .expect("failed to build quic xdp receive sockets");
            (port, sockets)
        })
        .collect();
    solana_streamer::xdp_quic::hold_sockets(quic_groups)
        .expect("failed to install the xdp quic receive sockets");

    if plan.receive_groups().is_empty() {
        log::warn!(
            "xdp: nothing to accelerate on {interface} — none of the ports this validator would \
             redirect are bound in this configuration, so receive stays on the kernel path"
        );
    }
    log::info!(
        "xdp: receive groups planned: {}",
        plan.receive_groups()
            .iter()
            .map(|group| format!(
                "{} udp/{}",
                group.name,
                group.ports.iter().map(u16::to_string).collect::<Vec<_>>().join("+")
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );

    if let Err(err) = runtime.hold_for_process() {
        log::error!("{err}");
        std::process::exit(1);
    }

    ReceiveSetup {
        tx_base: plan.tx_base(),
        accelerated: plan
            .receive_groups()
            .iter()
            .map(|group| group.name.to_string())
            .collect(),
    }
}

pub enum ReceiveMode {
    Exclusive,
    Chained,
    Adopted(Vec<agave_xdp::dispatcher::AdoptedMember>),
    Fatal(String),
    Off { missing: Vec<(caps::Capability, &'static str)> },
}

impl ReceiveMode {
    pub fn is_full(&self) -> bool {
        matches!(self, ReceiveMode::Exclusive | ReceiveMode::Chained | ReceiveMode::Adopted(_))
    }

    pub fn installs_program(&self) -> bool {
        matches!(self, ReceiveMode::Exclusive | ReceiveMode::Chained)
    }

    pub fn needs_chaining(&self) -> bool {
        matches!(self, ReceiveMode::Chained)
    }
}

pub fn decide(
    interface: Option<&str>,
    zero_copy: bool,
    chain_loading: bool,
    tx_queues: usize,
    permitted: &std::collections::HashSet<caps::Capability>,
) -> ReceiveMode {
    let missing = receive_capabilities(chain_loading).missing(permitted);
    if !missing.is_empty() {
        if zero_copy {
            return if chain_loading { ReceiveMode::Chained } else { ReceiveMode::Exclusive };
        }
        if let Some((device, _)) = devices(interface)
            && let Some(members) = agave_xdp::runtime::claim_adoptable(&device)
        {
            return ReceiveMode::Adopted(members);
        }
        return ReceiveMode::Off { missing };
    }

    if let Some((device, _)) = devices(interface)
        && let Err(reason) = hardware_can_receive(&device, tx_queues)
    {
        if zero_copy {
            return ReceiveMode::Fatal(format!(
                "--xdp-zero-copy needs the receive path, and this NIC cannot carry it: {reason}"
            ));
        }
        log::warn!(
            "XDP is accelerating transmit only this run; the validator is running normally and \
             receives through the kernel. {reason}"
        );
        return ReceiveMode::Off { missing: Vec::new() };
    }

    let Some((device, _)) = devices(interface) else {
        return if chain_loading { ReceiveMode::Chained } else { ReceiveMode::Exclusive };
    };
    let may_chain = permitted.contains(&caps::Capability::CAP_SYS_ADMIN);
    match agave_xdp::runtime::plan_attach(&device, chain_loading, may_chain) {
        Ok(agave_xdp::runtime::Outlook::Exclusive) => ReceiveMode::Exclusive,
        Ok(agave_xdp::runtime::Outlook::Chained) => ReceiveMode::Chained,
        Ok(agave_xdp::runtime::Outlook::NeedsHookSharing(why)) => {
            if !zero_copy
                && let Some((device, _)) = devices(interface)
                && let Some(members) = agave_xdp::runtime::claim_adoptable(&device)
            {
                log::debug!("xdp: {device} needs the hook shared ({why}); adopting instead");
                return ReceiveMode::Adopted(members);
            }
            if zero_copy {
                return ReceiveMode::Fatal(format!(
                    "--xdp-zero-copy needs the XDP program, and attaching it here means sharing \
                     the interface's hook, which needs CAP_SYS_ADMIN: {why}"
                ));
            }
            log::debug!("xdp: the hook has to be shared on {device}: {why}");
            ReceiveMode::Off {
                missing: receive_capabilities(/*chain_loading:*/ true)
                    .missing(permitted)
                    .into_iter()
                    .filter(|(cap, _)| *cap == caps::Capability::CAP_SYS_ADMIN)
                    .collect(),
            }
        }
        Ok(agave_xdp::runtime::Outlook::Blocked(reason)) => {
            if zero_copy {
                return ReceiveMode::Fatal(format!(
                    "--xdp-zero-copy needs the XDP program, and it cannot be attached: {reason}"
                ));
            }
            log::warn!(
                "XDP is accelerating transmit only this run; the validator is running normally \
                 and receives through the kernel. {reason}"
            );
            ReceiveMode::Off { missing: Vec::new() }
        }
        Err(err) => {
            log::debug!("xdp: could not plan the attach on {device}: {err}");
            if chain_loading { ReceiveMode::Chained } else { ReceiveMode::Exclusive }
        }
    }
}

pub fn transmit_queue_base(
    interface: &str,
    requested: Option<u32>,
    tx_loops: usize,
) -> Result<u32, String> {
    let Some(base) = requested else {
        return Ok(0);
    };
    let slaves = agave_xdp::bond::physical_devices(interface).map_err(|err| {
        format!(
            "--xdp-queue-base {base} cannot be honored: the devices behind {interface} could not \
             be read ({err})"
        )
    })?;
    let per_slave = tx_loops.div_ceil(slaves.len().max(1)) as u32;
    let wanted = base.saturating_add(per_slave);
    for slave in &slaves {
        agave_xdp::ethtool::ensure_combined_at_least(slave, wanted).map_err(|err| {
            format!(
                "--xdp-queue-base {base} needs {slave} to have at least {wanted} receive queues \
                 for the {per_slave} transmit loop(s) that start there, and it cannot: {err}. \
                 Pick a base this NIC can reach, or drop the flag and let the range be chosen"
            )
        })?;
    }
    Ok(base)
}

pub fn clear_leftover_steering(interface: &str, node: &Node) {
    let quic = quic_ports(node);
    let mut ports: Vec<u16> = quic.iter().map(|(_, port)| *port).collect();
    for group in groups(node, &quic) {
        ports.extend(group.ports.iter().copied());
    }
    let Ok(slaves) = agave_xdp::bond::physical_devices(interface) else {
        return;
    };
    let mut removed = 0usize;
    for slave in &slaves {
        if matches!(agave_xdp::ethtool::supports_ntuple(slave), Ok(false)) {
            continue;
        }
        match agave_xdp::ethtool::clear_udp_steering(slave, Some(&ports)) {
            Ok(rules) => removed = removed.saturating_add(rules.len()),
            Err(err) => log::warn!(
                "xdp: could not clear the steering rules an earlier run left on {slave}: {err}. \
                 Traffic on our ports may stay pinned to one receive queue there; \
                 `ethtool -n {slave}` lists them and `ethtool -N {slave} delete <id>` removes one"
            ),
        }
    }
    if removed > 0 {
        log::debug!(
            "xdp: removed {removed} steering rule(s) left by an earlier run, so the ports fall \
             back to the NIC's own spreading"
        );
    }
}

fn hardware_can_receive(interface: &str, tx_queues: usize) -> Result<(), String> {
    let devices = agave_xdp::bond::physical_devices(interface).map_err(|err| {
        format!(
            "{interface} resolves to no NIC that XDP can bind: {err} — configuration rather than \
             hardware, so check the interface name and a bond's mode"
        )
    })?;
    let per_device = tx_queues.div_ceil(devices.len().max(1)) as u32;
    for device in &devices {
        match agave_xdp::ethtool::supports_ntuple(device) {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "{device}{} cannot steer a port to a chosen receive queue, and no `ethtool` \
                     setting can add it",
                    driver_note(device)
                ));
            }
            Err(err) => {
                return Err(format!(
                    "{device} did not answer whether it can steer a port to a receive queue: {err}"
                ));
            }
        }
        let (_, ceiling) = agave_xdp::ethtool::receive_queue_budget(device)
            .map_err(|err| format!("{device} did not report its queue count: {err}"))?;
        let smallest = per_device.saturating_add(1);
        if ceiling < smallest {
            return Err(format!(
                "{device} has at most {ceiling} receive queue(s) and the smallest layout needs \
                 {smallest}; that maximum is fixed, so only fewer --xdp-cpu-cores can help"
            ));
        }
    }
    Ok(())
}

fn driver_note(device: &str) -> String {
    agave_xdp::device::NetworkDevice::new(device)
        .and_then(|dev| dev.driver())
        .map(|driver| format!(" ({driver})"))
        .unwrap_or_default()
}

pub fn receive_capabilities(chain_loading: bool) -> super::caps_check::CapRequirements {
    use caps::Capability::{CAP_BPF, CAP_PERFMON, CAP_SYS_ADMIN};
    let mut required = super::caps_check::CapRequirements::new();
    required.require(CAP_BPF, "load the XDP redirect program and create its maps");
    required.require(CAP_PERFMON, "read kernel BTF while loading that program");
    if chain_loading {
        required.require(
            CAP_SYS_ADMIN,
            "inspect an incumbent XDP dispatcher to chain onto it (BPF_*_GET_FD_BY_ID)",
        );
    }
    required
}

pub fn with_receive_capabilities(
    required: &super::caps_check::CapRequirements,
    chain_loading: bool,
) -> super::caps_check::CapRequirements {
    let mut merged = super::caps_check::CapRequirements::new();
    for (cap, reason) in required.requirements() {
        merged.require(cap, reason);
    }
    for (cap, reason) in receive_capabilities(chain_loading).into_requirements() {
        merged.require(cap, reason);
    }
    merged
}

fn quic_ports(node: &Node) -> Vec<(&'static str, u16)> {
    [
        (TPU_QUIC_GROUP, port_of(node.sockets.tpu_quic.first())),
        (TPU_FORWARDS_GROUP, port_of(node.sockets.tpu_forwards_quic.first())),
    ]
    .into_iter()
    .filter_map(|(name, port)| port.map(|port| (name, port)))
    .collect()
}

fn groups(node: &Node, quic_ports: &[(&'static str, u16)]) -> Vec<RxGroup> {
    let service_ports: Vec<u16> = [
        port_of(Some(&node.sockets.repair)),
        port_of(Some(&node.sockets.serve_repair)),
        port_of(Some(&node.sockets.ancestor_hashes_requests)),
        port_of(Some(&node.sockets.block_id_repair)),
        port_of(node.sockets.tpu_vote.first()),
    ]
    .into_iter()
    .flatten()
    .collect();

    receive_groups(quic_ports, port_of(node.sockets.tvu.first()), service_ports)
}

fn receive_groups(
    quic_ports: &[(&'static str, u16)],
    turbine: Option<u16>,
    service_ports: Vec<u16>,
) -> Vec<RxGroup> {
    let named = |wanted: &str| {
        quic_ports
            .iter()
            .find(|(name, _)| *name == wanted)
            .map(|(_, port)| *port)
    };

    let mut groups: Vec<RxGroup> = Vec::new();
    if let Some(port) = named(TPU_QUIC_GROUP) {
        groups.push(single(TPU_QUIC_GROUP, port));
    }
    if let Some(port) = turbine {
        groups.push(single(TURBINE_GROUP, port));
    }
    if !service_ports.is_empty() {
        groups.push(RxGroup {
            name: SERVICES_GROUP,
            ports: service_ports,
            queues: 1,
        });
    }
    groups.extend(
        quic_ports
            .iter()
            .filter(|(name, _)| *name != TPU_QUIC_GROUP)
            .map(|(name, port)| single(name, *port)),
    );
    groups
}


fn queues_needed(groups: &[RxGroup], tx_per_slave: u32) -> u32 {
    groups
        .iter()
        .map(|group| group.queues)
        .sum::<u32>()
        .saturating_add(tx_per_slave)
}

fn single(name: &'static str, port: u16) -> RxGroup {
    RxGroup {
        name,
        ports: vec![port],
        queues: 1,
    }
}

fn port_of(socket: Option<&std::net::UdpSocket>) -> Option<u16> {
    socket
        .and_then(|socket| socket.local_addr().ok())
        .map(|addr| addr.port())
}

pub fn devices(interface: Option<&str>) -> Option<(String, usize)> {
    let device = match interface {
        Some(name) => name.to_string(),
        None => agave_xdp::device::NetworkDevice::new_from_default_route()
            .ok()?
            .name()
            .to_string(),
    };
    let count = agave_xdp::bond::physical_devices(&device).ok()?.len();
    Some((device, count))
}

pub fn transmit_cores(
    chosen: Vec<usize>,
    interface: Option<&str>,
    operator_chose: bool,
    poh_core: Option<usize>,
) -> Vec<usize> {
    let Some((device, count)) = devices(interface) else {
        return chosen;
    };
    if chosen.len() >= count {
        return chosen;
    }
    if operator_chose {
        log::warn!(
            "XDP transmit is using {} of the {count} device(s) behind {device}; the rest send \
             nothing and cannot take over on link loss. Start with --xdp-cpu-cores listing at \
             least {count} cores.",
            chosen.len()
        );
        return chosen;
    }
    let Ok(allowed) = agave_cpu_utils::cpu_affinity(None) else {
        return chosen;
    };
    let cores = extend_cores(
        chosen,
        count,
        allowed.iter().map(|cpu| **cpu).collect(),
        poh_core,
    );
    if cores.len() < count {
        log::warn!(
            "XDP transmit could reserve only {} core(s) for the {count} device(s) behind \
             {device}; the rest send nothing.",
            cores.len()
        );
    } else {
        log::info!(
            "using {count} XDP transmit cores, one per device behind {device}; override with \
             --xdp-cpu-cores"
        );
    }
    cores
}

fn extend_cores(
    chosen: Vec<usize>,
    count: usize,
    allowed: Vec<usize>,
    poh_core: Option<usize>,
) -> Vec<usize> {
    let mut cores = chosen;
    for cpu in allowed.into_iter().rev() {
        if cores.len() >= count {
            break;
        }
        if Some(cpu) == poh_core || cores.contains(&cpu) {
            continue;
        }
        cores.push(cpu);
    }
    cores
}

pub fn quic_endpoints(
    interface: Option<&str>,
    configured: NonZeroUsize,
    operator_chose: bool,
    xdp_enabled: bool,
) -> NonZeroUsize {
    if !xdp_enabled {
        return configured;
    }
    let Some((device, count)) = devices(interface) else {
        return configured;
    };
    if count <= configured.get() {
        return configured;
    }
    if operator_chose {
        log::warn!(
            "XDP receive is accelerated on only {} of the {count} device(s) behind {device}; \
             traffic reaching the rest is served by the kernel path instead. Start with \
             --num-quic-endpoints {count} to accelerate all of them.",
            configured.get()
        );
        return configured;
    }
    log::info!(
        "using {count} QUIC endpoints, one per device behind {device}; override with \
         --num-quic-endpoints"
    );
    NonZeroUsize::new(count).unwrap_or(configured)
}


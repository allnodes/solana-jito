
use {
    crate::commands::{FromClapArgMatches, Result},
    clap::{App, Arg, ArgMatches, SubCommand, value_t},
};

pub const COMMAND: &str = "reset-xdp-interface";

#[derive(Debug, PartialEq)]
pub struct ResetXdpInterfaceArgs {
    pub interface: String,
    pub combined: Option<u32>,
    pub ports: Option<Vec<u16>>,
    pub all_ports: bool,
}

impl FromClapArgMatches for ResetXdpInterfaceArgs {
    fn from_clap_arg_match(matches: &ArgMatches) -> Result<Self> {
        Ok(ResetXdpInterfaceArgs {
            interface: value_t!(matches, "interface", String)?,
            all_ports: matches.is_present("all_ports"),
            combined: match matches.value_of("combined") {
                Some(_) => Some(value_t!(matches, "combined", u32)?),
                None => None,
            },
            ports: matches
                .values_of("ports")
                .map(|values| {
                    values
                        .map(|v| v.parse::<u16>())
                        .collect::<std::result::Result<Vec<u16>, _>>()
                })
                .transpose()
                .map_err(|err: std::num::ParseIntError| {
                    crate::commands::Error::Dynamic(err.to_string().into())
                })?,
        })
    }
}

pub fn command<'a>() -> App<'a, 'a> {
    SubCommand::with_name(COMMAND)
        .about("Undo the network interface configuration made for XDP")
        .arg(
            Arg::with_name("interface")
                .long("interface")
                .value_name("NAME")
                .takes_value(true)
                .required(true)
                .help(
                    "Interface the validator was started with (--xdp-interface). If it is a bond \
                     master, every slave is reset",
                ),
        )
        .arg(
            Arg::with_name("ports")
                .long("ports")
                .value_name("PORT")
                .takes_value(true)
                .multiple(true)
                .use_delimiter(true)
                .validator(|value| value.parse::<u16>().map(|_| ()).map_err(|err| err.to_string()))
                .help(
                    "Only remove the steering rules for these UDP ports. Defaults to the ports \
                     the validator steers, which is what the startup path claims ownership of; \
                     pass --all-ports to remove every destination-port rule on the interface, \
                     including another program's",
                ),
        )
        .arg(
            Arg::with_name("all_ports")
                .long("all-ports")
                .takes_value(false)
                .conflicts_with("ports")
                .help(
                    "Remove EVERY destination-port steering rule on the interface, not only the \
                     validator's. Only correct when nothing else on this machine steers traffic \
                     of its own",
                ),
        )
        .arg(
            Arg::with_name("combined")
                .long("combined")
                .value_name("COUNT")
                .takes_value(true)
                .help(
                    "Set the combined channel count back to COUNT. Defaults to what the \
                     interface had before the validator grew it, which the validator records \
                     when it configures the queues; left untouched when there is no such record, \
                     since guessing would be a change rather than a revert",
                ),
        )
}

#[cfg(target_os = "linux")]
fn default_ports() -> Vec<u16> {
    let (start, end) = solana_net_utils::VALIDATOR_PORT_RANGE;
    (start..end).collect()
}

#[cfg(target_os = "linux")]
pub fn execute(matches: &ArgMatches) -> Result<()> {
    let ResetXdpInterfaceArgs {
        interface,
        combined,
        ports,
        all_ports,
    } = ResetXdpInterfaceArgs::from_clap_arg_match(matches)?;

    let ports = match (ports, all_ports) {
        (Some(ports), _) => Some(ports),
        (None, true) => None,
        (None, false) => Some(default_ports()),
    };

    if !caps::has_cap(
        None,
        caps::CapSet::Effective,
        caps::Capability::CAP_NET_ADMIN,
    )
    .unwrap_or(false)
    {
        return Err(crate::commands::Error::Dynamic(
            "resetting an interface needs CAP_NET_ADMIN — run this from a validator binary that \
             already carries the capabilities it runs with (the one you started the node with \
             does), or, failing that, as root. Do not `setcap` this capability on its own onto a \
             validator binary: setcap replaces the whole set, so it would take away the rest"
                .into(),
        ));
    }

    let devices = agave_xdp::bond::physical_devices(&interface)?;
    if devices.len() > 1 {
        println!("{interface} is a bond over {}", devices.join(", "));
    }

    for device in &devices {
        let if_index = agave_xdp::device::NetworkDevice::new(device.as_str())?.if_index();

        match agave_xdp::netlink::xdp_attachment(if_index)? {
            Some(attachment) => {
                match agave_xdp::netlink::netlink_detach_xdp(if_index, attachment.drv) {
                    Ok(()) => println!("{device}: detached XDP program id {}", attachment.prog_id),
                    Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                        return Err(crate::commands::Error::Dynamic(
                            format!(
                                "{device}: XDP program id {} is held by a live bpf_link, which \
                                 means a validator still has it open. Stop the validator and run \
                                 this again — its attachment goes away with the process.",
                                attachment.prog_id
                            )
                            .into(),
                        ));
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            None => println!("{device}: no XDP program attached"),
        }

        let recorded = agave_xdp::ethtool::pre_change_queues(if_index);

        for dir in [format!("dispatch-{if_index}-"), format!("agave-{if_index}")] {
            if let Ok(entries) = std::fs::read_dir("/sys/fs/bpf/xdp") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name == dir || name.starts_with(&dir) {
                        let _ = std::fs::remove_dir_all(entry.path());
                        println!("{device}: removed {name}");
                    }
                }
            }
        }

        let removed = agave_xdp::ethtool::clear_udp_steering(device, ports.as_deref())?;
        if ports.is_none() && !removed.is_empty() {
            println!(
                "{device}: removed every destination-port rule, including any belonging to \
                 another program sharing this interface (--all-ports)"
            );
        }
        if removed.is_empty() {
            println!("{device}: no flow-steering rules of ours");
        }
        for rule in removed {
            match rule.context {
                Some(context) => println!(
                    "{device}: removed steering rule {} (udp/{} -> RSS context {context}) and the \
                     context",
                    rule.slot, rule.port
                ),
                None => println!(
                    "{device}: removed steering rule {} (udp/{} -> receive queue {})",
                    rule.slot, rule.port, rule.queue
                ),
            }
        }

        let channels = agave_xdp::ethtool::get_channels(device)?;
        let want = combined.or(recorded);
        match want {
            Some(want) if want == channels.combined_count => println!(
                "{device}: combined channels already {want}"
            ),
            Some(want) => {
                agave_xdp::ethtool::set_combined(device, want)?;
                println!(
                    "{device}: combined channels {} -> {want}{}",
                    channels.combined_count,
                    if combined.is_some() { "" } else { " (as recorded before XDP grew them)" }
                );
            }
            None => println!(
                "{device}: combined channels left at {} — nothing recorded what they were before \
                 (--combined COUNT to set them)",
                channels.combined_count
            ),
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn execute(_matches: &ArgMatches) -> Result<()> {
    Err(crate::commands::Error::Dynamic(
        "XDP is only supported on Linux".into(),
    ))
}


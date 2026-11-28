
use {caps::Capability, std::collections::HashSet};

pub fn reason(cap: Capability) -> &'static str {
    match cap {
        Capability::CAP_NET_ADMIN => {
            "attach the XDP program, resize the NIC queues, and steer the TPU port to the receive \
             queue"
        }
        Capability::CAP_NET_RAW => "create and bind the AF_XDP sockets",
        Capability::CAP_BPF => "load the XDP redirect program and create its maps",
        Capability::CAP_PERFMON => "read kernel BTF while loading that program",
        Capability::CAP_SYS_ADMIN => {
            "inspect an incumbent XDP dispatcher to chain onto it (BPF_*_GET_FD_BY_ID)"
        }
        Capability::CAP_SYS_NICE => "apply the configured niceness adjustments",
        _ => "required by the current configuration",
    }
}

#[derive(Default)]
pub struct ReceiveCaps {
    pub required: HashSet<Capability>,
    pub opportunistic: HashSet<Capability>,
    pub tier: HashSet<Capability>,
    pub degraded: Vec<Capability>,
}

impl ReceiveCaps {
    pub fn remediation(&self, required: &HashSet<Capability>) -> HashSet<Capability> {
        required.union(&self.tier).copied().collect()
    }
}

pub fn explain_missing(
    named: &HashSet<Capability>,
    missing: impl IntoIterator<Item = Capability>,
) -> String {
    explain(
        named,
        missing,
        "the current configuration needs capabilities this process was not granted:\n\n",
    )
}

pub fn explain_degraded(
    named: &HashSet<Capability>,
    missing: impl IntoIterator<Item = Capability>,
) -> String {
    explain(
        named,
        missing,
        "XDP is accelerating transmit only this run; the validator is running normally and \
         receives through the kernel. It needs capabilities this process was not granted:\n\n",
    )
}

fn explain(
    named: &HashSet<Capability>,
    missing: impl IntoIterator<Item = Capability>,
    headline: &str,
) -> String {
    let ordered = |caps: &mut Vec<Capability>| {
        caps.sort_by_key(|cap| cap.index());
        caps.dedup();
    };
    let mut missing: Vec<Capability> = missing.into_iter().collect();
    ordered(&mut missing);
    let width = missing
        .iter()
        .map(|cap| format!("{cap:?}").len())
        .max()
        .unwrap_or(0);
    let mut out = String::from(headline);
    for cap in &missing {
        out.push_str(&format!(
            "  {:<width$}  {}\n",
            format!("{cap:?}"),
            reason(*cap)
        ));
    }
    let mut names: Vec<Capability> = named.iter().copied().chain(missing).collect();
    ordered(&mut names);
    let names = names
        .iter()
        .map(|cap| format!("{cap:?}").to_lowercase())
        .collect::<Vec<_>>()
        .join(",");
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<path to agave-validator>".to_string());
    out.push_str(&format!(
        "\ngrant them to the binary with:\n  sudo setcap '{names}+p' {exe}\n\nthis is the \
         intended way: they are only held while XDP is being set up, and the process lowers \
         its\npermitted set irreversibly before it starts validating, then locks the \
         corresponding syscalls\ndown. Running the validator as root also works, but leaves it \
         privileged for the whole run.",
    ));
    out
}

pub fn restore_core_dumps() {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) } != 0 {
        log::debug!(
            "could not re-enable core dumps: {}",
            std::io::Error::last_os_error()
        );
    }
}


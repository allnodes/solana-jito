
use std::collections::HashSet;

pub struct CapRequirements {
    required: Vec<(caps::Capability, &'static str)>,
}

impl CapRequirements {
    pub fn new() -> Self {
        Self { required: Vec::new() }
    }

    pub fn require(&mut self, cap: caps::Capability, reason: &'static str) {
        if !self.required.iter().any(|(existing, _)| *existing == cap) {
            self.required.push((cap, reason));
        }
    }

    pub fn requirements(&self) -> impl Iterator<Item = (caps::Capability, &'static str)> + '_ {
        self.required.iter().copied()
    }

    pub fn into_requirements(self) -> impl Iterator<Item = (caps::Capability, &'static str)> {
        self.required.into_iter()
    }

    pub fn as_set(&self) -> HashSet<caps::Capability> {
        self.required.iter().map(|(cap, _)| *cap).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.required.is_empty()
    }

    pub fn missing(&self, permitted: &HashSet<caps::Capability>) -> Vec<(caps::Capability, &'static str)> {
        self.required
            .iter()
            .filter(|(cap, _)| !permitted.contains(cap))
            .copied()
            .collect()
    }

    pub fn explain_missing(&self, missing: &[(caps::Capability, &'static str)]) -> String {
        self.explain(
            missing,
            "the current configuration needs capabilities this process was not granted:\n\n",
        )
    }

    pub fn explain_degraded(&self, missing: &[(caps::Capability, &'static str)]) -> String {
        self.explain(
            missing,
            "XDP is accelerating transmit only this run; the validator is running normally and \
             receives through the kernel. It needs capabilities this process was not granted:\n\n",
        )
    }

    fn explain(&self, missing: &[(caps::Capability, &'static str)], headline: &str) -> String {
        let width = missing
            .iter()
            .map(|(cap, _)| format!("{cap:?}").len())
            .max()
            .unwrap_or(0);
        let mut out = String::from(headline);
        for (cap, reason) in missing {
            out.push_str(&format!("  {:<width$}  {reason}\n", format!("{cap:?}")));
        }
        let mut names: Vec<String> = self
            .required
            .iter()
            .map(|(cap, _)| format!("{cap:?}").to_lowercase())
            .collect();
        for (cap, _) in missing {
            let name = format!("{cap:?}").to_lowercase();
            if !names.contains(&name) {
                names.push(name);
            }
        }
        let names = names.join(",");
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<path to agave-validator>".to_string());
        out.push_str(&format!(
            "\ngrant them to the binary with:\n  sudo setcap '{names}+p' {exe}\n\nthis is the \
             intended way: they are only held while XDP is being set up, and the process lowers \
             its\npermitted set irreversibly before it starts validating, then locks the \
             corresponding syscalls\ndown. Running the validator as root also works, but leaves \
             it privileged for the whole run.",
        ));
        out
    }
}

impl Default for CapRequirements {
    fn default() -> Self {
        Self::new()
    }
}

pub fn restore_core_dumps() {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1) } != 0 {
        log::debug!(
            "could not re-enable core dumps: {}",
            std::io::Error::last_os_error()
        );
    }
}


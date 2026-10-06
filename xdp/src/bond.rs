
use std::{fs, io};

pub fn slaves(interface: &str) -> io::Result<Option<Vec<String>>> {
    let path = format!("/sys/class/net/{interface}/bonding/slaves");
    match fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(parse_slaves(&contents))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn physical_devices(interface: &str) -> io::Result<Vec<String>> {
    let Some(slaves) = slaves(interface)? else {
        return Ok(vec![interface.to_string()]);
    };
    if mode(interface)? != "802.3ad" {
        return Err(io::Error::other(format!(
            "bond {interface} is not in 802.3ad mode; refusing to bind"
        )));
    }
    if slaves.is_empty() {
        return Err(io::Error::other(format!("bond {interface} has no slaves")));
    }
    Ok(slaves)
}

fn parse_slaves(contents: &str) -> Vec<String> {
    contents.split_whitespace().map(str::to_string).collect()
}

pub fn is_operational(interface: &str) -> bool {
    let operstate = fs::read_to_string(format!("/sys/class/net/{interface}/operstate")).ok();
    let carrier = fs::read_to_string(format!("/sys/class/net/{interface}/carrier")).ok();
    link_is_up(operstate.as_deref(), carrier.as_deref())
}

fn link_is_up(operstate: Option<&str>, carrier: Option<&str>) -> bool {
    match operstate.map(str::trim) {
        Some("up") => true,
        Some("unknown") | Some("dormant") => carrier.map(str::trim) == Some("1"),
        _ => false,
    }
}

pub fn mode(master: &str) -> io::Result<String> {
    let raw = fs::read_to_string(format!("/sys/class/net/{master}/bonding/mode"))?;
    Ok(raw.split_whitespace().next().unwrap_or_default().to_string())
}

pub fn updelay_ms(master: &str) -> u64 {
    fs::read_to_string(format!("/sys/class/net/{master}/bonding/updelay"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

pub fn physical_device(interface: &str) -> String {
    slaves(interface)
        .ok()
        .flatten()
        .and_then(|slaves| slaves.into_iter().next())
        .unwrap_or_else(|| interface.to_string())
}


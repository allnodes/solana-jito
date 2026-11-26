
use {
    crate::wire::{encode_sample, Sample},
    crossbeam_channel::{select, Receiver},
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

pub(crate) fn run(
    id: usize,
    sinks: Arc<Vec<SocketAddr>>,
    receiver: Receiver<Sample>,
    shutdown: Receiver<()>,
) {
    let mut sent: u64 = 0;
    let mut send_errors: u64 = 0;
    let mut datagram: Vec<u8> = Vec::new();
    let mut v4: Option<UdpSocket> = None;
    let mut v6: Option<UdpSocket> = None;
    loop {
        select! {
            recv(receiver) -> msg => {
                let Ok(item) = msg else { break };
                forward(id, &sinks, &item, &mut datagram, &mut v4, &mut v6, &mut sent, &mut send_errors);
            },
            recv(shutdown) -> _ => break,
        }
    }

    while let Ok(item) = receiver.try_recv() {
        forward(id, &sinks, &item, &mut datagram, &mut v4, &mut v6, &mut sent, &mut send_errors);
    }

    log::debug!("qos: worker {id} exiting: sent={sent} send_errors={send_errors}");
}

#[allow(clippy::too_many_arguments)]
fn forward(
    id: usize,
    sinks: &[SocketAddr],
    item: &Sample,
    datagram: &mut Vec<u8>,
    v4: &mut Option<UdpSocket>,
    v6: &mut Option<UdpSocket>,
    sent: &mut u64,
    send_errors: &mut u64,
) {
    encode_sample(datagram, item.slot, &item.body);
    for addr in sinks {
        let socket = if addr.is_ipv4() {
            if v4.is_none() {
                *v4 = bind_wildcard(false)
                    .inspect_err(|err| log::debug!("qos: worker {id}: v4 bind failed: {err}"))
                    .ok();
            }
            v4.as_ref()
        } else {
            if v6.is_none() {
                *v6 = bind_wildcard(true)
                    .inspect_err(|err| log::debug!("qos: worker {id}: v6 bind failed: {err}"))
                    .ok();
            }
            v6.as_ref()
        };
        let Some(socket) = socket else {
            continue;
        };
        match socket.send_to(datagram, addr) {
            Ok(_) => *sent = sent.saturating_add(1),
            Err(err) => {
                *send_errors = send_errors.saturating_add(1);
                if *send_errors == 1 || send_errors.is_multiple_of(10_000) {
                    log::debug!("qos: send failed (count={send_errors}): {err}");
                }
            }
        }
    }
}

#[allow(clippy::disallowed_methods)]
fn bind_wildcard(ipv6: bool) -> std::io::Result<UdpSocket> {
    let bind_addr: SocketAddr = if ipv6 {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    UdpSocket::bind(bind_addr)
}

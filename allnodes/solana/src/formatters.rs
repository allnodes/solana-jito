use {
    solana_gossip::cluster_info::Sockets,
    std::net::{TcpListener, UdpSocket},
};

pub fn format_sockets(sockets: &Sockets) -> String {
    let Sockets {
        gossip,
        ip_echo,
        tvu,
        tvu_quic,
        tpu,
        tpu_forwards,
        tpu_vote,
        broadcast,
        repair,
        repair_quic,
        retransmit_sockets,
        serve_repair,
        serve_repair_quic,
        ancestor_hashes_requests,
        ancestor_hashes_requests_quic,
        tpu_quic,
        tpu_forwards_quic,
        tpu_vote_quic,
        tpu_vote_forwarding_client,
        tpu_transaction_forwarding_clients,
        alpenglow,
        quic_vote_client,
        quic_alpenglow_client,
        rpc_sts_client,
        vortexor_receivers,
    } = sockets;

    format!(
        "Gossip UDP: [{}]\nIP echo TCP: [{}]\nTVU UDP: [{}]\nTVU UDP QUIC: [{}]\nTPU UDP: \
         [{}]\nTPU Forwards UDP: [{}]\nTPU Vote UDP: [{}]\nBroadcast UDP: [{}]\nRepair UDP: \
         [{}]\nRepair UDP QUIC: [{}]\nRetransmit UDP Sockets: [{}]\nServe Repair UDP: [{}]\nServe \
         Repair UDP QUIC: [{}]\nAncestor Hashes Requests UDP: [{}]\nAncestor Hashes Requests UDP \
         QUIC: [{}]\nTPU UDP QUIC: [{}]\nTPU Forwards UDP QUIC: [{}]\nTPU Vote UDP QUIC: \
         [{}]\nTPU Vote Forwarding Client UDP: [{}]\nTPU Transaction Forwarding Clients UDP: \
         [{}]\nAlpenglow UDP: [{}]\nQUIC Vote Client UDP: [{}]\nQUIC Alpenglow Client UDP: \
         [{}]\nRPC STS Client UDP: [{}]\nVortexor Receivers UDP: [{}]",
        format_udp_socket_list(gossip.as_ref()),
        format_opt(ip_echo.as_ref().map(format_tcp_listener)),
        format_udp_socket_list(tvu),
        format_udp_socket(tvu_quic),
        format_udp_socket_list(tpu),
        format_udp_socket_list(tpu_forwards),
        format_udp_socket_list(tpu_vote),
        format_udp_socket_list(broadcast),
        format_udp_socket(repair),
        format_udp_socket(repair_quic),
        format_udp_socket_list(retransmit_sockets),
        format_udp_socket(serve_repair),
        format_udp_socket(serve_repair_quic),
        format_udp_socket(ancestor_hashes_requests),
        format_udp_socket(ancestor_hashes_requests_quic),
        format_udp_socket_list(tpu_quic),
        format_udp_socket_list(tpu_forwards_quic),
        format_udp_socket_list(tpu_vote_quic),
        format_udp_socket(tpu_vote_forwarding_client),
        format_udp_socket_list(tpu_transaction_forwarding_clients),
        format_opt(alpenglow.as_ref().map(format_udp_socket)),
        format_udp_socket(quic_vote_client),
        format_udp_socket(quic_alpenglow_client),
        format_udp_socket(rpc_sts_client),
        format_opt(vortexor_receivers.as_deref().map(format_udp_socket_list)),
    )
}

pub fn format_udp_socket_list(list: &[UdpSocket]) -> String {
    let mut result = String::new();
    let mut last = None;
    let mut count = 0_usize;
    for socket in list {
        let s = format_udp_socket(socket);
        if last.as_ref() == Some(&s) {
            count = count.saturating_add(1);
        } else {
            if count > 1 {
                result.push_str(&format!(" × {count}"));
            }
            if !result.is_empty() {
                result.push_str(", ");
            }
            result.push_str(&s);
            count = 1;
            last = Some(s);
        }
    }
    if count > 1 {
        result.push_str(&format!(" × {count}"));
    }

    result
}

pub fn format_udp_socket(socket: &UdpSocket) -> String {
    format_opt(socket.local_addr().as_ref().ok().map(ToString::to_string))
}

pub fn format_tcp_listener(listener: &TcpListener) -> String {
    format_opt(listener.local_addr().ok().as_ref().map(ToString::to_string))
}

fn format_opt(opt: Option<impl Into<String>>) -> String {
    if let Some(s) = opt {
        s.into()
    } else {
        "None".to_string()
    }
}

use {
    solana_gossip::cluster_info::Sockets,
    std::net::{TcpListener, UdpSocket},
};

pub fn format_sockets(sockets: &Sockets) -> String {
    let Sockets {
        gossip,
        ip_echo,
        tvu,
        tpu_vote,
        broadcast,
        repair,
        retransmit_sockets,
        serve_repair,
        ancestor_hashes_requests,
        tpu_quic,
        tpu_forwards_quic,
        tpu_vote_quic,
        block_id_repair,
        tpu_vote_forwarding_client,
        tpu_transaction_forwarding_clients,
        alpenglow,
        quic_vote_client,
        quic_alpenglow_client,
        rpc_sts_client,
    } = sockets;

    format!(
        "Gossip UDP: [{gossip}]\nIP echo TCP: [{ip_echo}]\nTVU: [{tvu}]\nTPU Vote: \
         [{tpu_vote}]\nBroadcast: [{broadcast}]\nRepair UDP: [{repair}]\nRetransmit UDP Sockets: \
         [{retransmit_sockets}]\nServe Repair UDP: [{serve_repair}]\nAncestor Hashes Requests \
         UDP: [{ancestor_hashes_requests}]\nTPU QUIC: [{tpu_quic}]\nTPU Forwards QUIC: \
         [{tpu_forwards_quic}]\nTPU Vote QUIC: [{tpu_vote_quic}]\nBlock ID Repair UDP: \
         [{block_id_repair}]\nTPU Vote Forwarding Client UDP: [{tpu_vote_forwarding_client}]\nTPU \
         Transaction Forwarding Clients UDP: [{tpu_transaction_forwarding_clients}]\nAlpenglow \
         UDP: [{alpenglow}]\nQUIC Vote Client UDP: [{quic_vote_client}]\nQUIC Alpenglow Client \
         UDP: [{quic_alpenglow_client}]\nRPC STS Client UDP: [{rpc_sts_client}]",
        gossip = format_udp_socket_list(gossip.as_ref()),
        ip_echo = format_opt(ip_echo.as_ref().map(format_tcp_listener)),
        tvu = format_udp_socket_list(tvu),
        tpu_vote = format_udp_socket_list(tpu_vote),
        broadcast = format_udp_socket_list(broadcast),
        repair = format_udp_socket(repair),
        retransmit_sockets = format_udp_socket_list(retransmit_sockets),
        serve_repair = format_udp_socket(serve_repair),
        ancestor_hashes_requests = format_udp_socket(ancestor_hashes_requests),
        tpu_quic = format_udp_socket_list(tpu_quic),
        tpu_forwards_quic = format_udp_socket_list(tpu_forwards_quic),
        tpu_vote_quic = format_udp_socket_list(tpu_vote_quic),
        block_id_repair = format_udp_socket(block_id_repair),
        tpu_vote_forwarding_client = format_udp_socket(tpu_vote_forwarding_client),
        tpu_transaction_forwarding_clients =
            format_udp_socket_list(tpu_transaction_forwarding_clients),
        alpenglow = format_udp_socket(alpenglow),
        quic_vote_client = format_udp_socket(quic_vote_client),
        quic_alpenglow_client = format_udp_socket(quic_alpenglow_client),
        rpc_sts_client = format_udp_socket(rpc_sts_client),
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

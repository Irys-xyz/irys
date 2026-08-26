use crate::{ApiState, error::ApiError};
use actix_web::{HttpResponse, ResponseError as _, http::header::ContentType, web};
use awc::http::StatusCode;
use irys_domain::PeerList;
use irys_types::{
    Config, H256, IrysAddress, PeerAddress, PeerListItem, ProtocolVersion,
    serialization::string_u64,
};
use serde::{Deserialize, Serialize};
use serde_json::to_string;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn peer_list_route(state: web::Data<ApiState>) -> HttpResponse {
    // Fetch the list of known peers
    let ips = state.get_known_peers();

    // Serialize IPs to JSON and return as HTTP response
    match to_string(&ips) {
        Ok(json_body) => HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(json_body),
        Err(e) => ApiError::CustomWithStatus(
            format!("Serialization error: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .error_response(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPeerAddress {
    pub gossip: String,
    pub api: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPeer {
    pub peer_id: String,
    pub mining_address: String,
    pub version: String,
    pub protocol_version: u32,
    pub is_online: bool,
    pub is_self: bool,
    #[serde(with = "string_u64")]
    pub last_seen: u64,
    pub address: NetworkPeerAddress,
    pub consensus_config_hash: Option<H256>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPeersResponse {
    #[serde(rename = "self")]
    pub self_peer: NetworkPeer,
    pub peers: Vec<NetworkPeer>,
}

pub async fn network_peers_route(state: web::Data<ApiState>) -> HttpResponse {
    let response =
        build_network_peers_response(&state.peer_list, &state.config, state.mining_address);
    HttpResponse::Ok().json(response)
}

pub fn build_network_peers_response(
    peer_list: &PeerList,
    config: &Config,
    mining_address: IrysAddress,
) -> NetworkPeersResponse {
    let self_id = config.peer_id();
    let public_address = config.node_config.peer_address();
    let now_ms = unix_now_ms();

    let self_peer = NetworkPeer {
        peer_id: self_id.to_string(),
        mining_address: mining_address.to_string(),
        version: irys_types::get_version().to_string(),
        protocol_version: ProtocolVersion::current() as u32,
        is_online: true,
        is_self: true,
        last_seen: now_ms,
        address: socket_pair(&public_address),
        consensus_config_hash: Some(config.consensus.keccak256_hash()),
    };

    let listed: Vec<PeerListItem> = {
        let guard = peer_list.all_peers();
        guard
            .iter()
            .filter(|(peer_id, _)| **peer_id != self_id)
            .map(|(_, peer)| peer.clone())
            .collect()
    };
    let mut peers: Vec<NetworkPeer> = listed.iter().map(network_peer_from_item).collect();

    peers.sort_by(|a, b| {
        b.is_online
            .cmp(&a.is_online)
            .then(a.mining_address.cmp(&b.mining_address))
    });

    NetworkPeersResponse { self_peer, peers }
}

fn network_peer_from_item(peer: &PeerListItem) -> NetworkPeer {
    NetworkPeer {
        peer_id: peer.peer_id.to_string(),
        mining_address: peer.mining_address.to_string(),
        version: peer.software_version_string(),
        protocol_version: peer.protocol_version as u32,
        is_online: peer.is_online,
        is_self: false,
        last_seen: peer.last_seen,
        address: socket_pair(&peer.address),
        consensus_config_hash: peer.consensus_config_hash,
    }
}

fn socket_pair(address: &PeerAddress) -> NetworkPeerAddress {
    NetworkPeerAddress {
        gossip: address.gossip.to_string(),
        api: address.api.to_string(),
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::PeerEvent;
    use irys_types::{
        IrysPeerId, NodeConfig, PeerListItem, PeerNetworkSender, PeerScore, ProtocolVersion,
        RethPeerInfo,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use tokio::sync::broadcast;
    use tokio::sync::mpsc;

    fn test_config_with_trusted(trusted: Vec<PeerAddress>) -> Config {
        let mut node = NodeConfig::testing();
        node.trusted_peers = trusted;
        Config::new_with_random_peer_id(node)
    }

    fn peer_item(
        id: u8,
        online: bool,
        version: Option<&str>,
        hash: Option<H256>,
        api_port: u16,
    ) -> PeerListItem {
        let mining = IrysAddress::from([id; 20]);
        PeerListItem {
            peer_id: IrysPeerId::from([id.wrapping_add(50); 20]),
            mining_address: mining,
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            response_time: 0,
            address: PeerAddress {
                gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, id)), 8000),
                api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, id)), api_port),
                execution: RethPeerInfo::default(),
            },
            last_seen: 1_700_000_000_000,
            is_online: online,
            protocol_version: ProtocolVersion::V2,
            software_version: version.map(|v| v.parse().expect("semver")),
            consensus_config_hash: hash,
        }
    }

    fn peer_list_from(config: &Config, peers: Vec<(PeerListItem, bool)>) -> PeerList {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel::<PeerEvent>(16);
        let list = PeerList::from_peers(vec![], PeerNetworkSender::new(tx), config, events)
            .expect("peer list");
        for (peer, staked) in peers {
            list.add_or_update_peer(peer, staked);
        }
        list
    }

    #[test]
    fn network_peer_serialization_contract() {
        let peer = NetworkPeer {
            peer_id: "peer".into(),
            mining_address: "miner".into(),
            version: "4.0.5+irys-rs".into(),
            protocol_version: 2,
            is_online: true,
            is_self: false,
            last_seen: 1_700_000_000_000,
            address: NetworkPeerAddress {
                gossip: "1.2.3.4:80".into(),
                api: "1.2.3.4:81".into(),
            },
            consensus_config_hash: None,
        };
        let json = serde_json::to_value(&peer).unwrap();
        assert_eq!(json["peerId"], "peer");
        assert_eq!(json["miningAddress"], "miner");
        assert_eq!(json["version"], "4.0.5+irys-rs");
        assert_eq!(json["protocolVersion"], 2);
        assert_eq!(json["isOnline"], true);
        assert!(json.get("isTrusted").is_none());
        assert_eq!(json["isSelf"], false);
        assert_eq!(json["lastSeen"], "1700000000000");
        assert_eq!(json["address"]["gossip"], "1.2.3.4:80");
        assert_eq!(json["consensusConfigHash"], serde_json::Value::Null);
    }

    #[test]
    fn self_is_present_and_not_duplicated_in_peers() {
        let config = test_config_with_trusted(vec![]);
        let self_id_byte = 9_u8;
        let mut self_as_peer = peer_item(self_id_byte, true, Some("4.0.6+irys-rs"), None, 9000);
        self_as_peer.peer_id = config.peer_id();
        self_as_peer.mining_address = config.node_config.miner_address();

        let other = peer_item(2, true, Some("4.0.5+irys-rs"), None, 9001);
        let list = peer_list_from(&config, vec![(self_as_peer, true), (other.clone(), true)]);
        let response =
            build_network_peers_response(&list, &config, config.node_config.miner_address());

        assert!(response.self_peer.is_self);
        let wrapper = serde_json::to_value(&response).unwrap();
        assert!(wrapper.get("self").is_some());
        assert!(wrapper.get("peers").is_some());
        assert_eq!(
            response.self_peer.version,
            irys_types::get_version().to_string()
        );
        assert!(!response.peers.iter().any(|p| p.is_self));
        assert!(
            !response
                .peers
                .iter()
                .any(|p| p.peer_id == response.self_peer.peer_id)
        );
        assert_eq!(response.peers.len(), 1);
        assert_eq!(response.peers[0].peer_id, other.peer_id.to_string());
    }

    #[test]
    fn handshake_version_is_returned_and_unknown_when_missing() {
        let config = test_config_with_trusted(vec![]);
        let with_version = peer_item(
            1,
            true,
            Some("4.0.5+irys-rs.abc1234"),
            Some(H256::zero()),
            9000,
        );
        let without_version = peer_item(2, false, None, None, 9001);
        let list = peer_list_from(
            &config,
            vec![(with_version.clone(), true), (without_version, false)],
        );
        let response =
            build_network_peers_response(&list, &config, config.node_config.miner_address());

        let known = response
            .peers
            .iter()
            .find(|p| p.peer_id == with_version.peer_id.to_string())
            .unwrap();
        assert_eq!(known.version, "4.0.5+irys-rs.abc1234");
        assert_eq!(known.consensus_config_hash, Some(H256::zero()));

        let unknown = response
            .peers
            .iter()
            .find(|p| p.peer_id != with_version.peer_id.to_string())
            .unwrap();
        assert_eq!(unknown.version, "unknown");
        assert_eq!(unknown.consensus_config_hash, None);
    }

    #[test]
    fn peers_sort_online_then_mining_address() {
        let offline = peer_item(3, false, Some("4.0.5+irys-rs"), None, 9100);
        let online_b = peer_item(5, true, Some("4.0.5+irys-rs"), None, 9101);
        let online_a = peer_item(4, true, Some("4.0.5+irys-rs"), None, 9102);
        let config = test_config_with_trusted(vec![]);
        let list = peer_list_from(
            &config,
            vec![(offline.clone(), true), (online_b, true), (online_a, true)],
        );
        let response =
            build_network_peers_response(&list, &config, config.node_config.miner_address());
        assert_eq!(response.peers.len(), 3);
        assert!(response.peers[0].is_online);
        assert!(response.peers[1].is_online);
        assert!(!response.peers[2].is_online);
        assert!(response.peers[0].mining_address < response.peers[1].mining_address);
        assert_eq!(response.peers[2].peer_id, offline.peer_id.to_string());
    }

    #[test]
    fn add_or_update_peer_stores_handshake_version() {
        let config = test_config_with_trusted(vec![]);
        let peer = peer_item(
            6,
            true,
            Some("1.2.3+irys-rs"),
            Some(H256::from([1; 32])),
            9300,
        );
        let list = peer_list_from(&config, vec![(peer.clone(), true)]);
        let stored = list.peer_by_id(&peer.peer_id).unwrap();
        assert_eq!(stored.software_version_string(), "1.2.3+irys-rs");
        assert_eq!(stored.consensus_config_hash, Some(H256::from([1; 32])));
    }

    #[test]
    fn handshake_update_writes_version_into_in_memory_record() {
        let config = test_config_with_trusted(vec![]);
        let peer = peer_item(5, true, None, None, 9200);
        let list = peer_list_from(&config, vec![(peer.clone(), true)]);
        assert_eq!(
            list.peer_by_id(&peer.peer_id)
                .unwrap()
                .software_version_string(),
            "unknown"
        );

        list.observe_handshake(
            peer.address.api,
            Some("4.0.6+irys-rs.6cbc03b".parse().unwrap()),
            Some(H256::from([7; 32])),
        );
        let updated = list.peer_by_id(&peer.peer_id).unwrap();
        assert_eq!(updated.software_version_string(), "4.0.6+irys-rs.6cbc03b");
        assert_eq!(updated.consensus_config_hash, Some(H256::from([7; 32])));

        let response =
            build_network_peers_response(&list, &config, config.node_config.miner_address());
        assert_eq!(response.peers[0].version, "4.0.6+irys-rs.6cbc03b");
    }

    #[test]
    fn outbound_observation_applies_when_peer_is_later_inserted() {
        let config = test_config_with_trusted(vec![]);
        let peer = peer_item(8, true, None, None, 9400);
        let list = peer_list_from(&config, vec![]);
        list.observe_handshake(
            peer.address.api,
            Some("4.0.6+irys-rs.6cbc03b".parse().unwrap()),
            Some(H256::from([3; 32])),
        );
        list.add_or_update_peer(peer, true);
        let response =
            build_network_peers_response(&list, &config, config.node_config.miner_address());
        assert_eq!(response.peers[0].version, "4.0.6+irys-rs.6cbc03b");
        assert_eq!(
            response.peers[0].consensus_config_hash,
            Some(H256::from([3; 32]))
        );
    }

    #[test]
    fn peer_list_json_is_still_a_flat_address_array() {
        let addr = PeerAddress {
            gossip: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 80),
            api: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 81),
            execution: RethPeerInfo::default(),
        };
        let json = serde_json::to_value(vec![addr]).unwrap();
        let arr = json.as_array().expect("peer-list is a JSON array");
        let object = arr[0].as_object().unwrap();
        assert!(object.contains_key("gossip"));
        assert!(object.contains_key("api"));
        assert!(object.contains_key("execution"));
        assert!(!object.contains_key("version"));
        assert!(!object.contains_key("peerId"));
    }

    #[test]
    fn peer_list_emits_one_row_for_the_same_gossip_socket() {
        let mining = IrysAddress::from([0x4A; 20]);
        let mut v1 = peer_item(1, true, None, None, 80);
        v1.peer_id = IrysPeerId::from(mining);
        v1.mining_address = mining;
        v1.address.gossip = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 35, 53, 102)), 9009);
        v1.address.api = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 35, 53, 102)), 80);

        let mut v2 = peer_item(2, true, None, None, 8080);
        v2.mining_address = mining;
        v2.address.gossip = v1.address.gossip;
        v2.address.api = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(89, 35, 53, 102)), 8080);

        let config = test_config_with_trusted(vec![]);
        let (tx, _rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel::<PeerEvent>(16);
        let list = PeerList::from_peers(vec![v1, v2], PeerNetworkSender::new(tx), &config, events)
            .expect("peer list");

        let ips = list.all_known_peers();
        let json = serde_json::to_value(&ips).unwrap();
        let arr = json.as_array().expect("peer-list is a JSON array");
        assert_eq!(
            arr.len(),
            1,
            "same gossip socket must be one advertised peer"
        );
        assert_eq!(arr[0]["api"], "89.35.53.102:8080");
        assert_eq!(arr[0]["gossip"], "89.35.53.102:9009");
    }
}

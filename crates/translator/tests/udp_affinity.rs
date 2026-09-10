use sozu_command_lib::proto::command::{request::RequestType, UdpAffinityKey};
use sozu_command_lib::state::ConfigState;
use sozu_gw_ir::{Cluster, Ir, L4Frontend, L4Protocol, LbAlgorithm};
use sozu_gw_translator::{ir_to_requests, reconcile};

fn graph() -> Ir {
    Ir {
        clusters: vec![Cluster {
            id: "shared".into(),
            load_balancing: LbAlgorithm::RoundRobin,
            sticky_session: false,
            https_redirect: false,
            max_connections_per_ip: None,
            retry_after: None,
        }],
        l4_frontends: vec![L4Frontend {
            cluster_id: "shared".into(),
            protocol: L4Protocol::Tcp,
            listener: "0.0.0.0:9300".parse().unwrap(),
        }],
        ..Default::default()
    }
}

fn add_udp(ir: &mut Ir) {
    ir.l4_frontends.push(L4Frontend {
        cluster_id: "shared".into(),
        protocol: L4Protocol::Udp,
        listener: "0.0.0.0:5300".parse().unwrap(),
    });
}

#[test]
fn udp_clients_sharing_an_ip_get_per_socket_flows() {
    let mut ir = graph();
    add_udp(&mut ir);
    let requests = ir_to_requests(&ir);
    let mut state = ConfigState::default();
    for request in &requests {
        state.dispatch(request).unwrap();
    }
    let cluster = &state.clusters["shared"];
    let udp = cluster.udp.as_ref().expect("UDP policy must be explicit");
    assert_eq!(udp.affinity_key, Some(UdpAffinityKey::SourceIpPort as i32));
    assert!(!cluster.sticky_session);
    assert!(reconcile(&ir, &ir).unwrap().is_empty());
}

#[test]
fn adding_udp_to_an_existing_cluster_updates_policy_before_attachment() {
    let previous = graph();
    let mut desired = previous.clone();
    add_udp(&mut desired);
    let changes = reconcile(&previous, &desired).unwrap();
    let update = changes
        .iter()
        .position(|request| matches!(request.request_type, Some(RequestType::AddCluster(_))))
        .expect("existing cluster needs its UDP flow policy");
    let attach = changes
        .iter()
        .position(|request| matches!(request.request_type, Some(RequestType::AddUdpFrontend(_))))
        .unwrap();
    assert!(update < attach);
    let mut state = ConfigState::default();
    for request in ir_to_requests(&previous).iter().chain(&changes) {
        state.dispatch(request).unwrap();
    }
    assert_eq!(
        state.clusters["shared"].udp.as_ref().unwrap().affinity_key,
        Some(UdpAffinityKey::SourceIpPort as i32)
    );
    assert_eq!(state.tcp_fronts.len(), 1);
}

#[test]
fn removing_udp_keeps_the_shared_tcp_cluster_without_udp_policy() {
    let desired = graph();
    let mut previous = desired.clone();
    add_udp(&mut previous);
    let mut state = ConfigState::default();
    for request in ir_to_requests(&previous)
        .iter()
        .chain(&reconcile(&previous, &desired).unwrap())
    {
        state.dispatch(request).unwrap();
    }
    assert!(state.clusters["shared"].udp.is_none());
    assert_eq!(state.tcp_fronts.len(), 1);
    assert!(state.udp_fronts.values().all(Vec::is_empty));
}

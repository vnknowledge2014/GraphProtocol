//! Unit tests for `EgressGuard`, IPv4/IPv6 CIDR classification, URL parser hardening, and connector adapters.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gap_core::error::ErrorCode;

use super::*;

#[derive(Debug)]
struct MapResolver {
    table: Mutex<BTreeMap<String, Vec<Vec<IpAddr>>>>,
}

impl MapResolver {
    fn from_entries(entries: Vec<(&str, Vec<Vec<IpAddr>>)>) -> Self {
        let mut table = BTreeMap::new();
        for (host, rounds) in entries {
            table.insert(host.to_owned(), rounds);
        }
        Self {
            table: Mutex::new(table),
        }
    }
}

impl DnsResolver for MapResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let Ok(mut guard) = self.table.lock() else {
            return Err("lock poisoned".to_owned());
        };
        let Some(rounds) = guard.get_mut(host) else {
            return Err(format!("NXDOMAIN `{host}`"));
        };
        if rounds.is_empty() {
            return Err(format!("exhausted scripted rounds for `{host}`"));
        }
        let ips = if rounds.len() == 1 {
            rounds[0].clone()
        } else {
            rounds.remove(0)
        };
        Ok(ips
            .into_iter()
            .map(|ip| SocketAddr::new(ip, port))
            .collect())
    }
}

// spec: SEC-075, SEC-076
#[test]
fn ip_classifier_blocks_private_metadata_and_ipv6_tunnel_encodings() {
    let blocked = [
        "0.0.0.0",
        "0.1.2.3",
        "127.0.0.1",
        "127.255.255.254",
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.1",
        "100.64.0.1",
        "100.127.255.254",
        "169.254.0.1",
        "169.254.169.254",
        "169.254.170.2",
        "100.100.100.200",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "fe80::1",
        "fc00::1",
        "fd00:ec2::254",
        "ff02::1",
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "::ffff:10.0.0.5",
        "64:ff9b::169.254.169.254",
        "2002:a9fe:a9fe::1",
        "2001:0000:4136:e378:8000:63bf:5601:5601", // Teredo encoding ~169.254.169.254
    ];
    for raw in blocked {
        let ip: IpAddr = raw
            .parse()
            .unwrap_or_else(|e| panic!("valid IP `{raw}`: {e}"));
        assert!(
            classify_blocked_ip(ip).is_some(),
            "expected `{raw}` to be blocked by SSRF IP classifier"
        );
    }

    let allowed = [
        "93.184.216.34",
        "1.1.1.1",
        "8.8.8.8",
        "172.32.0.1",
        "100.128.0.1",
        "2606:4700:4700::1111",
    ];
    for raw in allowed {
        let ip: IpAddr = raw
            .parse()
            .unwrap_or_else(|e| panic!("valid IP `{raw}`: {e}"));
        assert_eq!(
            classify_blocked_ip(ip),
            None,
            "expected public IP `{raw}` to be permitted"
        );
    }
}

// spec: SEC-075, SEC-076
#[test]
fn url_parser_rejects_userinfo_zones_odd_encodings_and_trailing_dots() {
    let invalid = [
        "file:///etc/passwd",
        "gopher://example.com/",
        "https://user:pass@example.com/v1",
        "https://tokenonly@example.com/",
        "https://evil.com\\@169.254.169.254/",
        "https://[fe80::1%lo0]/",
        "https://[fe80::1%25eth0]/",
        "https://2130706433/admin",
        "https://2852039166/latest/meta-data/",
        "https://0177.0.0.1/",
        "https://0251.0376.0251.0376/",
        "https://0x7f000001/",
        "https://0x7f.0.0.1/",
        "https://127.1/",
        "https://169.254.43518/",
        "https://localhost./",
        "https://sub.localhost./",
        "https://127.0.0.1./",
    ];
    let guard = EgressGuard::with_resolver(
        EgressPolicy::default(),
        MapResolver::from_entries(Vec::new()),
    );
    for url in invalid {
        let err = guard
            .check_and_pin(url)
            .expect_err(&format!("expected `{url}` to be rejected"));
        let gap_err = err.into_gap_error();
        assert!(
            matches!(
                gap_err.code(),
                ErrorCode::ValidationError | ErrorCode::Forbidden
            ),
            "unexpected error code for `{url}`: {:?}",
            gap_err.code()
        );
    }
}

// spec: SEC-075
#[test]
fn egress_guard_allows_branch_allowlist_and_connects_to_pinned_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();

    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0_u8; 4];
        stream.read_exact(&mut buf).expect("read ping");
        assert_eq!(&buf, b"PING");
        stream.write_all(b"PONG").expect("write pong");
    });

    // Local dev host allowed for HTTP + localhost, and resolver returns public-then-rebound to prove pinning uses initial resolution.
    let resolver = Arc::new(MapResolver::from_entries(vec![(
        "localdev.internal",
        vec![
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
            vec![IpAddr::V4(Ipv4Addr::new(10, 255, 255, 254))],
        ],
    )]));

    let mut policy = EgressPolicy::default().with_local_dev_http_host("localdev.internal");
    policy
        .add_allow_entry("10.42.0.0/16")
        .expect("valid cidr allow rule");
    let guard = EgressGuard::with_resolver(policy, resolver);
    let adapter = EgressHttpAdapter::new(guard.clone());

    let url = format!("http://localdev.internal:{port}/graphql");
    let pinned = adapter
        .prepare_request(&url)
        .expect("allowlisted local dev");
    assert_eq!(
        pinned.primary_addr(),
        SocketAddr::from(([127, 0, 0, 1], port))
    );
    assert_eq!(pinned.host_header, format!("localdev.internal:{port}"));

    let mut conn = pinned
        .connect_tcp(Duration::from_secs(2))
        .expect("dial pinned loopback socket directly without DNS re-resolve");
    conn.write_all(b"PING").expect("send ping");
    let mut reply = [0_u8; 4];
    conn.read_exact(&mut reply).expect("read pong");
    assert_eq!(&reply, b"PONG");
    server.join().expect("server thread");

    // Branch CIDR override permits 10.42.1.10 while still blocking 10.99.1.10.
    let cidr_ok = guard.check_and_pin("https://10.42.1.10/v1/query");
    assert!(cidr_ok.is_ok());
    let cidr_denied = guard
        .check_and_pin("https://10.99.1.10/v1/query")
        .expect_err("non-allowlisted private IP blocked");
    assert!(matches!(cidr_denied, EgressError::BlockedIp { .. }));
}

// spec: SEC-075, SEC-076
#[test]
fn egress_guard_redirects_recheck_metadata_downgrade_and_hop_cap() {
    let resolver = Arc::new(MapResolver::from_entries(vec![
        (
            "api.example.com",
            vec![vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]],
        ),
        (
            "hop1.example.com",
            vec![vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 35))]],
        ),
        (
            "rebind.example.com",
            vec![
                vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
                vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))],
            ],
        ),
    ]));
    let guard = EgressGuard::with_resolver(EgressPolicy::default(), resolver);

    // 1. Redirect to metadata IP must fail on hop 2
    let meta_err = guard
        .follow_redirects("https://api.example.com/spec.json", |_pinned| {
            Ok(HopOutcome::Redirect::<()> {
                location: "https://169.254.169.254/latest/meta-data/iam".to_owned(),
            })
        })
        .expect_err("redirect to metadata denied");
    assert!(matches!(meta_err, EgressError::BlockedIp { .. }));

    // 2. HTTPS -> HTTP scheme downgrade on redirect must fail
    let downgrade_err = guard
        .follow_redirects("https://api.example.com/spec.json", |_pinned| {
            Ok(HopOutcome::Redirect::<()> {
                location: "http://hop1.example.com/spec.json".to_owned(),
            })
        })
        .expect_err("https -> http downgrade denied");
    assert!(matches!(
        downgrade_err,
        EgressError::SchemeDowngradeDenied { .. }
    ));

    // 3. Redirect chain > 3 hops must fail
    let loop_err = guard
        .follow_redirects("https://api.example.com/start", |_pinned| {
            Ok(HopOutcome::Redirect::<()> {
                location: "/next".to_owned(),
            })
        })
        .expect_err("4+ redirects denied");
    assert!(matches!(
        loop_err,
        EgressError::TooManyRedirects { limit: 3 }
    ));

    // 4. Relative redirect to self-rebinding host catches lookup #2 returning metadata IP
    let rebind_err = guard
        .follow_redirects("https://rebind.example.com/v1", |_pinned| {
            Ok(HopOutcome::Redirect::<()> {
                location: "/v2".to_owned(),
            })
        })
        .expect_err("rebinding redirect caught");
    assert!(matches!(
        rebind_err,
        EgressError::BlockedIp {
            reason: BlockedIpReason::CloudMetadata,
            ..
        }
    ));

    // 5. Public 2-hop redirect succeeds cleanly, plus gRPC adapter validates public host
    let (final_ep, body) = guard
        .follow_redirects("https://api.example.com/v1", |pinned| {
            if pinned.path_and_query == "/v1" {
                Ok(HopOutcome::Redirect {
                    location: "https://hop1.example.com/v2".to_owned(),
                })
            } else {
                Ok(HopOutcome::Final("openapi: 3.1.0"))
            }
        })
        .expect("allowed 1-hop https redirect");
    assert_eq!(body, "openapi: 3.1.0");
    assert_eq!(
        final_ep.primary_addr(),
        SocketAddr::from(([93, 184, 216, 35], 443))
    );

    let grpc_adapter = EgressGrpcAdapter::new(guard);
    let grpc_pinned = grpc_adapter
        .prepare_channel_target("https://api.example.com:8443")
        .expect("public grpc target allowed");
    assert_eq!(
        grpc_pinned.primary_addr(),
        SocketAddr::from(([93, 184, 216, 34], 8443))
    );
    assert!(
        Ipv6Addr::UNSPECIFIED
            .to_string()
            .parse::<IpAddr>()
            .ok()
            .and_then(classify_blocked_ip)
            .is_some()
    );
}

// spec: SEC-072
#[test]
fn egress_guard_self_test_passes_for_gap_doctor() {
    EgressGuard::self_test().expect("built-in SSRF self-test vector matrix must pass");
}

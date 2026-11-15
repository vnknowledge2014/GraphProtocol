// Table-driven SSRF, DNS rebinding, and connector/fetcher integration tests (`WP-M6-08`).
// spec: SEC-075, SEC-076, SEC-072
// TM-06

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use gap_core::error::ErrorCode;
use gap_ingest::fetch::{FetchError, FetchLimits, FetchedSource, SourceFetcher};
use gap_sec::egress::{
    BlockedIpReason, DnsResolver, EgressError, EgressGrpcAdapter, EgressGuard, EgressHttpAdapter,
    EgressPolicy, HopOutcome, PinnedEndpoint, SequenceResolver,
};

struct TableResolver {
    records: BTreeMap<String, Vec<IpAddr>>,
    lookup_counts: Mutex<BTreeMap<String, usize>>,
}

impl TableResolver {
    fn new(records: Vec<(&str, Vec<IpAddr>)>) -> Self {
        let mut map = BTreeMap::new();
        for (host, ips) in records {
            map.insert(host.to_owned(), ips);
        }
        Self {
            records: map,
            lookup_counts: Mutex::new(BTreeMap::new()),
        }
    }

    fn lookups_for(&self, host: &str) -> usize {
        self.lookup_counts
            .lock()
            .expect("lock")
            .get(host)
            .copied()
            .unwrap_or(0)
    }
}

impl DnsResolver for TableResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
        let mut counts = self.lookup_counts.lock().expect("lock");
        *counts.entry(host.to_owned()).or_insert(0) += 1;
        let ips = self
            .records
            .get(host)
            .ok_or_else(|| format!("NXDOMAIN {host}"))?;
        Ok(ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
    }
}

/// Example `SourceFetcher` wrapper proving ingestion fetchers enforce `EgressGuard` before dialing (`SEC-075`).
struct EgressGuardedFetcher<R> {
    adapter: EgressHttpAdapter<R>,
    fixture_bodies: BTreeMap<SocketAddr, Vec<u8>>,
}

impl<R: DnsResolver> SourceFetcher for EgressGuardedFetcher<R> {
    fn fetch(&self, url: &str, limits: &FetchLimits) -> Result<FetchedSource, FetchError> {
        let pinned =
            self.adapter
                .prepare_request(url)
                .map_err(|gap_err| FetchError::DeniedByPolicy {
                    reason: gap_err.to_string(),
                })?;
        let addr = pinned.primary_addr();
        let bytes =
            self.fixture_bodies
                .get(&addr)
                .cloned()
                .ok_or_else(|| FetchError::DeniedByPolicy {
                    reason: format!("no fixture endpoint bound at pinned socket {addr}"),
                })?;
        FetchedSource::from_bytes(url, None, bytes, limits)
    }
}

// spec: SEC-075, SEC-076
// TM-06
#[test]
fn ssrf_table_blocks_all_hostile_vectors_across_formats() {
    let resolver = Arc::new(TableResolver::new(vec![
        (
            "public.api.example",
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        ),
        (
            "rebind-loopback.attacker.example",
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        ),
        (
            "rebind-rfc1918.attacker.example",
            vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 12))],
        ),
        (
            "rebind-cgnat.attacker.example",
            vec![IpAddr::V4(Ipv4Addr::new(100, 64, 1, 5))],
        ),
        (
            "rebind-v6-ula.attacker.example",
            vec![IpAddr::V6("fd00:ec2::254".parse::<Ipv6Addr>().expect("v6"))],
        ),
        (
            "dual-stack-smuggle.attacker.example",
            vec![
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
            ],
        ),
    ]));
    let guard = EgressGuard::with_resolver(EgressPolicy::default(), resolver);

    let hostile_urls = [
        // Loopback, private, CGNAT, link-local, metadata, IPv6 ULA & transition tunnels
        "https://127.0.0.1/admin",
        "https://10.0.0.1/internal",
        "https://172.16.42.9/secret",
        "https://192.168.100.5/router",
        "https://100.64.0.10/cgnat",
        "https://169.254.169.254/latest/meta-data/",
        "https://[fd00:ec2::254]/latest/meta-data/",
        "https://metadata.google.internal/computeMetadata/v1/",
        "https://[::1]/",
        "https://[fe80::1]/",
        "https://[::ffff:127.0.0.1]/",
        "https://[::ffff:169.254.169.254]/",
        "https://[64:ff9b::169.254.169.254]/",
        "https://[2002:0a00:0001::1]/",
        // Odd numeric IP encodings (decimal, octal, hex, shorthand)
        "https://2130706433/",
        "https://2852039166/latest/meta-data/",
        "https://0177.0.0.1/",
        "https://0177.0000.0000.0001/",
        "https://0x7f000001/",
        "https://0x7f.0.0.1/",
        "https://127.1/",
        "https://169.254.43518/",
        // Zone IDs, trailing dots, userinfo, and cleartext HTTP without allow-list
        "https://[fe80::1%lo0]/",
        "https://[fe80::1%25en0]/",
        "https://localhost/",
        "https://localhost./",
        "https://api.localhost./",
        "https://metadata.google.internal./",
        "https://user:secret@public.api.example/v1",
        "http://public.api.example/v1",
        // DNS-resolved private / CGNAT / dual-stack smuggling
        "https://rebind-loopback.attacker.example/",
        "https://rebind-rfc1918.attacker.example/",
        "https://rebind-cgnat.attacker.example/",
        "https://rebind-v6-ula.attacker.example/",
        "https://dual-stack-smuggle.attacker.example/",
    ];

    for url in hostile_urls {
        let err = guard
            .check_and_pin(url)
            .expect_err(&format!("SSRF table MUST deny `{url}`"));
        let gap_err = err.into_gap_error();
        assert!(
            matches!(
                gap_err.code(),
                ErrorCode::Forbidden | ErrorCode::ValidationError
            ),
            "unexpected error mapping for `{url}`: {:?}",
            gap_err.code()
        );
    }

    let ok = guard
        .check_and_pin("https://public.api.example/openapi.json")
        .expect("public HTTPS endpoint succeeds");
    assert_eq!(
        ok.primary_addr(),
        SocketAddr::from(([93, 184, 216, 34], 443))
    );
}

// spec: SEC-075, SEC-076
// TM-06
#[test]
fn dns_rebinding_first_lookup_pins_socket_and_second_lookup_rejects_private_ip() {
    // 1. Prove PinnedEndpoint resolves DNS at most once and pins the validated SocketAddr.
    let single_lookup_resolver = Arc::new(TableResolver::new(vec![(
        "pinned.example",
        vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
    )]));
    let guard = EgressGuard::with_resolver(EgressPolicy::default(), single_lookup_resolver.clone());
    let pinned: PinnedEndpoint = guard
        .check_and_pin("https://pinned.example/v1/graphql")
        .expect("initial public lookup succeeds");
    assert_eq!(single_lookup_resolver.lookups_for("pinned.example"), 1);
    assert_eq!(pinned.tls_sni_host, "pinned.example");
    assert_eq!(pinned.host_header, "pinned.example");
    assert_eq!(
        pinned.dial_targets(),
        &[SocketAddr::from(([93, 184, 216, 34], 443))]
    );

    // 2. Simulate time-of-check/time-of-use DNS rebinding (first lookup public -> second lookup
    // flips to RFC1918 10.0.0.5, third lookup flips to AWS metadata 169.254.169.254).
    let rebind_resolver = SequenceResolver::new(vec![
        vec![SocketAddr::from(([93, 184, 216, 34], 443))],
        vec![SocketAddr::from(([10, 0, 0, 5], 443))],
        vec![SocketAddr::from(([169, 254, 169, 254], 443))],
    ]);
    let rebind_guard = EgressGuard::with_resolver(EgressPolicy::default(), rebind_resolver);

    let hop1 = rebind_guard
        .check_and_pin("https://rebind.example/step1")
        .expect("lookup #1 public IP pins safely");
    assert_eq!(
        hop1.primary_addr(),
        SocketAddr::from(([93, 184, 216, 34], 443))
    );

    let hop2_err = rebind_guard
        .check_and_pin("https://rebind.example/step1")
        .expect_err("lookup #2 rebound to RFC1918 MUST fail");
    assert!(matches!(
        hop2_err,
        EgressError::BlockedIp {
            reason: BlockedIpReason::PrivateRfc1918,
            ..
        }
    ));

    let hop3_err = rebind_guard
        .check_and_pin("https://rebind.example/step1")
        .expect_err("lookup #3 rebound to metadata MUST fail");
    assert!(matches!(
        hop3_err,
        EgressError::BlockedIp {
            reason: BlockedIpReason::CloudMetadata,
            ..
        }
    ));

    // Redirect hop also catches rebinding between initial request and redirect destination.
    let redirect_rebind = EgressGuard::with_resolver(
        EgressPolicy::default(),
        SequenceResolver::new(vec![
            vec![SocketAddr::from(([93, 184, 216, 34], 443))],
            vec![SocketAddr::from(([127, 0, 0, 1], 443))],
        ]),
    );
    let redirect_err = redirect_rebind
        .follow_redirects("https://rebind.example/start", |_pinned| {
            Ok(HopOutcome::Redirect::<()> {
                location: "/redirected".to_owned(),
            })
        })
        .expect_err("redirect rebinding to loopback must fail");
    assert!(matches!(
        redirect_err,
        EgressError::BlockedIp {
            reason: BlockedIpReason::Loopback,
            ..
        }
    ));
}

// spec: SEC-075, SEC-072
// TM-06
#[test]
fn egress_adapters_protect_source_fetcher_connectors_and_doctor_self_test() {
    let public_addr = SocketAddr::from(([93, 184, 216, 34], 443));
    let resolver = Arc::new(TableResolver::new(vec![
        (
            "specs.partner.example",
            vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))],
        ),
        (
            "sidecar.internal",
            vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))],
        ),
    ]));

    let fetcher = EgressGuardedFetcher {
        adapter: EgressHttpAdapter::new(EgressGuard::with_resolver(
            EgressPolicy::default(),
            resolver.clone(),
        )),
        fixture_bodies: BTreeMap::from([(
            public_addr,
            br#"{"openapi":"3.1.0","info":{"title":"Billing","version":"1.0"}}"#.to_vec(),
        )]),
    };

    let ok_spec = fetcher
        .fetch(
            "https://specs.partner.example/openapi.json",
            &FetchLimits::default(),
        )
        .expect("public OpenAPI fetch permitted through EgressGuard");
    assert!(!ok_spec.bytes.is_empty());

    let denied_meta = fetcher
        .fetch(
            "https://169.254.169.254/latest/meta-data/",
            &FetchLimits::default(),
        )
        .expect_err("metadata URL blocked by EgressGuardedFetcher");
    assert!(matches!(denied_meta, FetchError::DeniedByPolicy { .. }));

    // HttpSidecar & gRPC/A2A connector endpoint check refuses internal/metadata targets unless explicitly allow-listed.
    let grpc_adapter = EgressGrpcAdapter::new(EgressGuard::with_resolver(
        EgressPolicy::default(),
        resolver,
    ));
    let sidecar_err = grpc_adapter
        .prepare_channel_target("https://sidecar.internal:8443/v1/systemone")
        .expect_err("internal sidecar target denied without allowlist");
    assert_eq!(sidecar_err.code(), ErrorCode::Forbidden);

    EgressGuard::self_test().expect("doctor egress self-test passes");
}

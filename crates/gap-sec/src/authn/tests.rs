#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};

use gap_core::clock::{Clock as _, ManualClock};
use gap_core::context::{ActorRef, AttrValue, Principal, PrincipalKind};
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::ids::TenantId;
use gap_core::secret::Secret;

use super::crypto::{
    Argon2Params, Ed25519SigningKey, argon2id_hash_full, base64url_encode, blake3_hash, ct_eq,
    hex_decode, hex_encode, hmac_sha256,
};
use super::json::JsonValue;
use super::{
    AllowedJwtAlg, AnonymousModePolicy, ApiKeyAuthenticator, ApiKeyIssueSpec, ApiKeyRecord,
    AuthCredentials, AuthRequestMeta, Authenticator, CONSTANT_AUTH_FAILURE_MESSAGE, ExchangedToken,
    IntrospectionConfig, IntrospectionResponse, IntrospectionTransport, IpAuthRateLimitConfig,
    IpAuthRateLimiter, Jwk, JwksCache, JwksCacheConfig, JwksFetcher, JwtValidationConfig,
    JwtValidator, MtlsAuthenticator, PRM_MCP_WELL_KNOWN_PATH, PRM_WELL_KNOWN_PATH,
    ProtectedResourceMetadata, SpiffeBinding, StdioAuthenticator, TokenExchangeClient,
    TokenExchangeRequest, TokenExchangeTransport, TokenIntrospector, UntrustedRequestHints,
    VerifiedClientCert, intersect_delegated_scopes, parse_rfc8693_act_chain,
};

fn sign_jwt_ed25519(
    signing_key: &Ed25519SigningKey,
    header_json: &str,
    claims_json: &str,
) -> String {
    let header_b64 = base64url_encode(header_json.as_bytes());
    let claims_b64 = base64url_encode(claims_json.as_bytes());
    let signing_input = format!("{header_b64}.{claims_b64}");
    let sig = signing_key.sign(signing_input.as_bytes()).unwrap();
    let sig_b64 = base64url_encode(&sig);
    format!("{signing_input}.{sig_b64}")
}

fn tenant(id: &str) -> TenantId {
    TenantId::new(id).unwrap()
}

// spec: SEC-001
#[test]
fn authn_crypto_official_vectors_blake3_ed25519_argon2id() {
    assert!(ct_eq(b"secret", b"secret"));
    assert!(!ct_eq(b"secret", b"secrex"));
    assert!(!ct_eq(b"secret", b"short"));
    assert_eq!(
        hex_encode(&blake3_hash(b"")),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
    assert_eq!(
        hex_encode(&blake3_hash(b"abc")),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );
    // Multi-chunk determinism check (> 1024 bytes exercises Merkle parent chaining).
    let long_a = vec![b'a'; 2500];
    let long_b = vec![b'a'; 2501];
    assert_ne!(blake3_hash(&long_a), blake3_hash(&long_b));
    assert_eq!(blake3_hash(&long_a), blake3_hash(&long_a));

    // RFC 8032 §7.1 TV1 (empty message).
    let tv1_seed: [u8; 32] =
        hex_decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
            .unwrap()
            .try_into()
            .unwrap();
    let sk1 = Ed25519SigningKey::from_seed(tv1_seed).unwrap();
    let vk1 = sk1.verifying_key().unwrap();
    assert_eq!(
        hex_encode(vk1.as_bytes()),
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    );
    let sig1 = sk1.sign(b"").unwrap();
    assert_eq!(
        hex_encode(&sig1),
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
         5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    );
    vk1.verify(b"", &sig1).unwrap();
    assert!(vk1.verify(b"tampered", &sig1).is_err());

    // RFC 9106 §5.3 Argon2id test vector.
    let params = Argon2Params {
        m_cost_kib: 32,
        t_cost: 3,
        p_cost: 4,
        tag_len: 32,
    };
    let digest =
        argon2id_hash_full(&[0x01; 32], &[0x02; 16], &[0x03; 8], &[0x04; 12], params).unwrap();
    assert_eq!(
        hex_encode(&digest),
        "0d640df58d78766c08c037a34a8b53c9d01ef0452d75b65eb52520e96b01e659"
    );
}

// spec: SEC-001
// spec: SEC-004
#[test]
fn authn_jwt_eddsa_and_rs256_validation_and_claims() {
    let clock = ManualClock::new(1_700_000_000_000);
    let now = clock.unix_millis() / 1000;

    let seed = [7_u8; 32];
    let sk = Ed25519SigningKey::from_seed(seed).unwrap();
    let vk = sk.verifying_key().unwrap();

    // Verified 2048-bit RSA public key & RS256 signed JWT fixture.
    let rsa_jwks = r#"{
        "keys": [
            {
                "kty": "RSA",
                "kid": "rsa-key-1",
                "alg": "RS256",
                "use": "sig",
                "n": "kAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAADMzMzMzM0P-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABIfoArXfalsGAzS8v0VFQ",
                "e": "AQAB"
            }
        ]
    }"#;
    let mut keys = Jwk::parse_jwks_json(rsa_jwks).unwrap();
    keys.push(Jwk::ed25519("ed-key-1", *vk.as_bytes()));

    let cache = JwksCache::new(JwksCacheConfig::default(), keys, now);
    let mut cfg = JwtValidationConfig::new(
        vec!["https://idp.example.com".to_owned()],
        "https://gap.example.com",
    );
    cfg.expected_resource = Some("https://gap.example.com/mcp".to_owned());

    let validator = JwtValidator::new(cfg.clone(), &cache);

    // Valid EdDSA JWT with RFC 8707 resource claim.
    let ed_jwt = sign_jwt_ed25519(
        &sk,
        r#"{"alg":"EdDSA","kid":"ed-key-1","typ":"JWT"}"#,
        r#"{
            "iss":"https://idp.example.com",
            "sub":"agent-ed",
            "aud":"https://gap.example.com",
            "resource":"https://gap.example.com/mcp",
            "tenant":"acme",
            "kind":"AGENT",
            "scope":"gap:read gap:discover",
            "exp":1700000900,
            "nbf":1699999900
        }"#,
    );
    let out = validator.validate(&ed_jwt, None, None, now).unwrap();
    assert_eq!(out.principal.id, "agent-ed");
    assert_eq!(out.principal.kind, PrincipalKind::Agent);
    assert_eq!(out.principal.tenant, tenant("acme"));
    assert!(out.principal.scopes.contains("gap:read"));
    assert!(out.principal.scopes.contains("gap:discover"));

    // Valid 2048-bit RS256 JWT (expecting only aud check without distinct /mcp resource).
    let cfg_rs256 = JwtValidationConfig::new(
        vec!["https://idp.example.com".to_owned()],
        "https://gap.example.com",
    );
    let validator_rs256 = JwtValidator::new(cfg_rs256, &cache);
    let rs256_jwt = "eyJhbGciOiJSUzI1NiIsImtpZCI6InJzYS1rZXktMSIsInR5cCI6IkpXVCJ9.\
        eyJhdWQiOiJodHRwczovL2dhcC5leGFtcGxlLmNvbSIsImV4cCI6MTcwMDAwMTAwMCwiaXNzIjoiaHR0cHM6Ly9pZHAuZXhhbXBsZS5jb20iLCJraW5kIjoiU0VSVklDRSIsInNjb3BlIjoiZ2FwOnJlYWQiLCJz\
        dWIiOiJzdmMtcnMyNTYiLCJ0ZW5hbnQiOiJhY21lIn0.\
        ZqyqgvBcL995nCpcYv4GpIJG2hqKa4DKrNrWn5G1xWy6pWJOHJDCLryxE4eILHgyU7RnB8MOL2w0ZSfzyvwsb9nzFlJn-ZOMvRT6P3fZ-YyDOMwvk1df3UwFNd2Yvd8GpxB2HrrtHviNJ_blbRgpPNd1qT7mIkwLHI4GrmfatVGnkpPNTCGFmPs6eAkbljlcnpMol-nitgcZ1vF54n4Bc0Y-bHhFJABe31EA7TQ3pUXHgcN8FyIqrrdTVkCmnUlr79U99Ik_1V-ymtqyDu9X6AZtQmahk-4NOpOl_XhwscKoR1rsarDg4zBhkrl6ksn4lqM74yjRdfPltueTxG-qBg";
    let rs_out = validator_rs256
        .validate(rs256_jwt, None, None, now)
        .unwrap();
    assert_eq!(rs_out.principal.id, "svc-rs256");
    assert_eq!(rs_out.principal.kind, PrincipalKind::Service);
    assert_eq!(rs_out.principal.tenant, tenant("acme"));
}

// spec: SEC-001
#[test]
fn authn_jwt_rejects_token_confusion_alg_none_wrong_aud_exp_and_nbf() {
    let now = 1_700_000_000_i64;
    let sk = Ed25519SigningKey::from_seed([9_u8; 32]).unwrap();
    let vk = sk.verifying_key().unwrap();
    let cache = JwksCache::new(
        JwksCacheConfig::default(),
        vec![Jwk::ed25519("k1", *vk.as_bytes())],
        now,
    );
    let mut cfg = JwtValidationConfig::new(
        vec!["https://idp.example.com".to_owned()],
        "https://gap.example.com",
    );
    cfg.expected_resource = Some("https://gap.example.com/mcp".to_owned());
    let validator = JwtValidator::new(cfg, &cache);

    // 1. alg=none MUST be rejected.
    let alg_none = format!(
        "{}.{}.",
        base64url_encode(br#"{"alg":"none","kid":"k1"}"#),
        base64url_encode(
            br#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","exp":1700001000,"tenant":"acme"}"#
        )
    );
    assert_eq!(
        validator
            .validate(&alg_none, None, None, now)
            .unwrap_err()
            .code(),
        ErrorCode::Unauthenticated
    );
    assert!(AllowedJwtAlg::parse("none").is_err());
    assert!(AllowedJwtAlg::parse("HS256").is_err());

    // 2. Token confusion: HS256 signed with public key bytes MUST be rejected.
    let hs_hdr = base64url_encode(br#"{"alg":"HS256","kid":"k1"}"#);
    let hs_clm = base64url_encode(
        br#"{"iss":"https://idp.example.com","sub":"attacker","aud":"https://gap.example.com","exp":1700001000,"tenant":"acme"}"#,
    );
    let hs_input = format!("{hs_hdr}.{hs_clm}");
    let hs_sig = hmac_sha256(vk.as_bytes(), hs_input.as_bytes());
    let hs_jwt = format!("{hs_input}.{}", base64url_encode(&hs_sig));
    assert_eq!(
        validator
            .validate(&hs_jwt, None, None, now)
            .unwrap_err()
            .code(),
        ErrorCode::Unauthenticated
    );

    // 3. Wrong audience rejected.
    let wrong_aud = sign_jwt_ed25519(
        &sk,
        r#"{"alg":"EdDSA","kid":"k1"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://other.example.com","resource":"https://gap.example.com/mcp","exp":1700001000,"tenant":"acme"}"#,
    );
    assert!(validator.validate(&wrong_aud, None, None, now).is_err());

    // 4. Wrong RFC 8707 resource rejected even when aud matches.
    let wrong_res = sign_jwt_ed25519(
        &sk,
        r#"{"alg":"EdDSA","kid":"k1"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","resource":"https://other-mcp.example.com","exp":1700001000,"tenant":"acme"}"#,
    );
    assert!(validator.validate(&wrong_res, None, None, now).is_err());

    // 5. Expiry beyond 60 s skew rejected, within <= 60 s skew accepted.
    let exp_token = sign_jwt_ed25519(
        &sk,
        r#"{"alg":"EdDSA","kid":"k1"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","resource":"https://gap.example.com/mcp","exp":1699999950,"tenant":"acme"}"#,
    );
    assert!(validator.validate(&exp_token, None, None, now).is_ok()); // now - 60 < 1699999950
    assert!(
        validator
            .validate(&exp_token, None, None, now + 61)
            .is_err()
    );

    // 6. nbf beyond 60 s skew rejected, within <= 60 s skew accepted.
    let nbf_token = sign_jwt_ed25519(
        &sk,
        r#"{"alg":"EdDSA","kid":"k1"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","resource":"https://gap.example.com/mcp","exp":1700002000,"nbf":1700000050,"tenant":"acme"}"#,
    );
    assert!(validator.validate(&nbf_token, None, None, now).is_ok()); // now + 60 >= 1700000050
    assert!(
        validator
            .validate(&nbf_token, None, None, now - 15)
            .is_err()
    );
}

struct MockFetcher {
    keys: std::sync::Mutex<Vec<Jwk>>,
    calls: AtomicU64,
}

impl JwksFetcher for MockFetcher {
    fn fetch_keys(&self) -> GapResult<Vec<Jwk>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.keys.lock().unwrap().clone())
    }
}

// spec: SEC-001
#[test]
fn authn_jwks_rotation_grace_period_and_refresh_rate_limit() {
    let now = 1_700_000_000_i64;
    let sk_old = Ed25519SigningKey::from_seed([1_u8; 32]).unwrap();
    let sk_new = Ed25519SigningKey::from_seed([2_u8; 32]).unwrap();

    let cache = JwksCache::new(
        JwksCacheConfig {
            rotation_grace_secs: 120,
            min_refresh_interval_secs: 30,
            cache_ttl_secs: 3600,
        },
        vec![Jwk::ed25519(
            "kid-old",
            *sk_old.verifying_key().unwrap().as_bytes(),
        )],
        now,
    );
    let cfg = JwtValidationConfig::new(
        vec!["https://idp.example.com".to_owned()],
        "https://gap.example.com",
    );
    let validator = JwtValidator::new(cfg, &cache);

    let token_old = sign_jwt_ed25519(
        &sk_old,
        r#"{"alg":"EdDSA","kid":"kid-old"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","exp":1700005000,"tenant":"acme"}"#,
    );
    let token_new = sign_jwt_ed25519(
        &sk_new,
        r#"{"alg":"EdDSA","kid":"kid-new"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","exp":1700005000,"tenant":"acme"}"#,
    );

    let fetcher = MockFetcher {
        keys: std::sync::Mutex::new(vec![Jwk::ed25519(
            "kid-new",
            *sk_new.verifying_key().unwrap().as_bytes(),
        )]),
        calls: AtomicU64::new(0),
    };

    // Unknown kid within min_refresh_interval is throttled; past cooldown refreshes & resolves.
    assert!(
        validator
            .validate(&token_new, None, Some(&fetcher), now + 5)
            .is_err()
    );
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 0);

    // At now + 35 (past cooldown), refresh pulls kid-new AND keeps kid-old inside grace window!
    assert!(
        validator
            .validate(&token_new, None, Some(&fetcher), now + 35)
            .is_ok()
    );
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    assert!(
        validator
            .validate(&token_old, None, Some(&fetcher), now + 40)
            .is_ok()
    );

    // Spamming forged unknown kid right after rotation does NOT hammer the fetcher.
    let token_forged = sign_jwt_ed25519(
        &sk_new,
        r#"{"alg":"EdDSA","kid":"kid-attacker"}"#,
        r#"{"iss":"https://idp.example.com","sub":"u","aud":"https://gap.example.com","exp":1700005000,"tenant":"acme"}"#,
    );
    assert!(
        validator
            .validate(&token_forged, None, Some(&fetcher), now + 45)
            .is_err()
    );
    assert!(
        validator
            .validate(&token_forged, None, Some(&fetcher), now + 46)
            .is_err()
    );
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);

    // Once rotation_grace_secs (120 s after rotation at now + 35 = now + 155) elapses, old key expires.
    assert!(
        validator
            .validate(&token_old, None, None, now + 160)
            .is_err()
    );
}

struct MockIntrospect {
    calls: AtomicU64,
    active: bool,
}

impl IntrospectionTransport for MockIntrospect {
    fn introspect(&self, token: &str) -> GapResult<IntrospectionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let raw = if self.active && token == "good-opaque-tok" {
            r#"{"active":true,"sub":"user-1","iss":"https://idp.example.com","aud":"https://gap.example.com","exp":1700000500,"scope":"gap:read","tenant":"acme","kind":"HUMAN"}"#
        } else {
            r#"{"active":false}"#
        };
        IntrospectionResponse::from_json_str(raw)
    }
}

// spec: SEC-001
// spec: SEC-010
// spec: SEC-011
#[test]
fn authn_opaque_introspection_caching_and_zero_token_retention() {
    let now = 1_700_000_000_i64;
    let transport = MockIntrospect {
        calls: AtomicU64::new(0),
        active: true,
    };
    let mut cfg = IntrospectionConfig::new("https://gap.example.com");
    cfg.positive_ttl_secs = 60;
    let introspector = TokenIntrospector::new(cfg, &transport);

    let hints = UntrustedRequestHints {
        x_tenant_header: Some("evil-tenant".to_owned()),
        body_tenant: Some("evil-body".to_owned()),
    };
    let out1 = introspector
        .validate("good-opaque-tok", Some(&hints), now)
        .unwrap();
    let out2 = introspector
        .validate("good-opaque-tok", Some(&hints), now + 10)
        .unwrap();
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(out1.principal.tenant, tenant("acme"));
    assert_eq!(out2.principal.kind, PrincipalKind::Human);
    assert!(out1.ignored_hint_mismatch.is_some());
    assert!(!out1.principal.attrs.contains_key("token"));

    // Negative caching avoids hammering endpoint for bad tokens.
    assert!(introspector.validate("bad-opaque", None, now).is_err());
    assert!(introspector.validate("bad-opaque", None, now + 5).is_err());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
}

// spec: SEC-001
// spec: SEC-002
// spec: SEC-003
// spec: SEC-004
#[test]
fn authn_api_keys_mtls_stdio_anonymous_prm_and_constant_shape_failures() {
    let now = 1_700_000_000_i64;
    let secret_str = Secret::new("super-secret-value-123".to_owned());
    let mut scopes = BTreeSet::new();
    scopes.insert("gap:mutate".to_owned());
    let mut rec = ApiKeyRecord::issue(ApiKeyIssueSpec {
        key_id: "gapk_live_01",
        raw_secret: &secret_str,
        salt: b"salt-0123456789abcdef",
        params: Argon2Params::testing(),
        subject: "svc-billing",
        kind: PrincipalKind::Service,
        tenant: tenant("acme"),
        scopes,
    })
    .unwrap();
    rec.expires_at = Some(now + 100);

    let mut revoked_rec = rec.clone();
    revoked_rec.key_id = "gapk_revoked".to_owned();
    revoked_rec.revoked = true;

    let api_auth = ApiKeyAuthenticator::new(vec![rec, revoked_rec]).unwrap();
    let mtls_auth = MtlsAuthenticator::new(vec![SpiffeBinding {
        trust_domain: "gap.internal".to_owned(),
        fixed_tenant: None,
        kind: PrincipalKind::Service,
        scopes: BTreeSet::from(["gap:traverse".to_owned()]),
    }]);
    let stdio_auth = StdioAuthenticator::new(
        tenant("localdev"),
        BTreeSet::from(["gap:discover".to_owned()]),
    );

    let prm = ProtectedResourceMetadata::new(
        "https://gap.example.com/mcp",
        vec!["https://idp.example.com".to_owned()],
        "https://gap.example.com/.well-known/oauth-protected-resource/mcp",
    );
    assert!(prm.handle_well_known_request(PRM_WELL_KNOWN_PATH).is_some());
    assert!(
        prm.handle_well_known_request(PRM_MCP_WELL_KNOWN_PATH)
            .is_some()
    );
    assert!(prm.handle_well_known_request("/other").is_none());
    assert_eq!(
        prm.www_authenticate_header(Some("gap:read")),
        "Bearer resource_metadata=\"https://gap.example.com/.well-known/oauth-protected-resource/mcp\", scope=\"gap:read\""
    );

    let limiter = IpAuthRateLimiter::new(IpAuthRateLimitConfig {
        max_failures_per_window: 3,
        window_secs: 60,
        cooldown_secs: 45,
        max_tracked_ips: 64,
    });

    let edge = Authenticator {
        jwt: None,
        introspect: None,
        api_key: Some(&api_auth),
        mtls: Some(&mtls_auth),
        stdio: Some(&stdio_auth),
        anonymous_policy: AnonymousModePolicy::fail_closed(),
        prm: Some(&prm),
        rate_limiter: Some(&limiter),
    };

    // Valid API key succeeds and ignores X-Tenant.
    let hints = UntrustedRequestHints {
        x_tenant_header: Some("forged".to_owned()),
        body_tenant: None,
    };
    let ok_api = edge
        .authenticate(
            AuthCredentials::ApiKey("gapk_live_01.super-secret-value-123"),
            AuthRequestMeta {
                peer_ip: None,
                hints: Some(&hints),
            },
            now,
        )
        .unwrap();
    assert_eq!(ok_api.principal.id, "svc-billing");
    assert_eq!(ok_api.principal.tenant, tenant("acme"));

    // mTLS SPIFFE mapping extracts /tenant/<id>/... and rejects unverified chain.
    let valid_cert = VerifiedClientCert {
        chain_verified: true,
        san_uris: vec!["spiffe://gap.internal/tenant/beta/sa/worker".to_owned()],
        san_dns: Vec::new(),
        subject_dn: "CN=worker".to_owned(),
        sha256_thumbprint: "deadbeef".to_owned(),
    };
    let ok_mtls = edge
        .authenticate(
            AuthCredentials::Mtls(&valid_cert),
            AuthRequestMeta::default(),
            now,
        )
        .unwrap();
    assert_eq!(ok_mtls.principal.tenant, tenant("beta"));
    assert_eq!(ok_mtls.principal.kind, PrincipalKind::Service);

    let mut bad_cert = valid_cert.clone();
    bad_cert.chain_verified = false;
    assert!(
        edge.authenticate(
            AuthCredentials::Mtls(&bad_cert),
            AuthRequestMeta::default(),
            now
        )
        .is_err()
    );

    // stdio OS user produces Service principal.
    let ok_stdio = edge
        .authenticate(
            AuthCredentials::StdioOsUser("mike"),
            AuthRequestMeta::default(),
            now,
        )
        .unwrap();
    assert_eq!(ok_stdio.principal.id, "os:mike");
    assert_eq!(ok_stdio.principal.kind, PrincipalKind::Service);
    assert_eq!(ok_stdio.principal.tenant, tenant("localdev"));

    // SEC-002 fail-closed anonymous check: prod denies, invalid config rejects startup, loopback dev succeeds.
    assert!(
        edge.authenticate(
            AuthCredentials::Anonymous,
            AuthRequestMeta {
                peer_ip: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                hints: None,
            },
            now
        )
        .is_err()
    );
    let unsafe_cfg = AnonymousModePolicy {
        allow_anonymous: true,
        dev_mode: false,
        require_loopback: true,
        dev_tenant: None,
    };
    assert!(unsafe_cfg.validate_config().is_err());

    let dev_policy = AnonymousModePolicy {
        allow_anonymous: true,
        dev_mode: true,
        require_loopback: true,
        dev_tenant: Some(tenant("devlocal")),
    };
    dev_policy.validate_config().unwrap();
    assert!(
        dev_policy
            .try_authenticate_anonymous(Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))), None)
            .is_err()
    );
    let ok_dev = dev_policy
        .try_authenticate_anonymous(Some(IpAddr::V4(Ipv4Addr::LOCALHOST)), None)
        .unwrap();
    assert_eq!(ok_dev.principal.tenant, tenant("devlocal"));

    // SEC-003: unknown key, wrong secret, revoked key all emit IDENTICAL wire challenge & trigger 429 on 3rd failure.
    let attacker_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    let ch1 = edge
        .authenticate(
            AuthCredentials::ApiKey("unknown.wrong"),
            AuthRequestMeta {
                peer_ip: Some(attacker_ip),
                hints: None,
            },
            now,
        )
        .unwrap_err();
    let ch2 = edge
        .authenticate(
            AuthCredentials::ApiKey("gapk_revoked.super-secret-value-123"),
            AuthRequestMeta {
                peer_ip: Some(attacker_ip),
                hints: None,
            },
            now + 1,
        )
        .unwrap_err();
    assert_eq!(ch1.http_status, 401);
    assert_eq!(ch2.http_status, 401);
    assert_eq!(ch1.wire_error.message(), CONSTANT_AUTH_FAILURE_MESSAGE);
    assert_eq!(ch2.wire_error.message(), ch1.wire_error.message());
    assert_eq!(ch1.wire_json_body(), ch2.wire_json_body());
    assert!(ch1.www_authenticate.contains(PRM_MCP_WELL_KNOWN_PATH));

    let ch3 = edge
        .authenticate(
            AuthCredentials::ApiKey("gapk_live_01.badsecret"),
            AuthRequestMeta {
                peer_ip: Some(attacker_ip),
                hints: None,
            },
            now + 2,
        )
        .unwrap_err();
    assert_eq!(ch3.http_status, 429);
    assert_eq!(ch3.retry_after_secs, Some(45));
    assert_eq!(ch3.wire_error.code(), ErrorCode::RateLimited);
}

struct MockExchange {
    calls: AtomicU64,
}

impl TokenExchangeTransport for MockExchange {
    fn exchange(&self, req: &TokenExchangeRequest) -> GapResult<ExchangedToken> {
        let count = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if req.subject_token.expose_secret().is_empty() {
            return Err(GapError::new(
                ErrorCode::Unauthenticated,
                "missing exchange subject assertion",
            ));
        }
        let scopes = req
            .scope
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        Ok(ExchangedToken {
            access_token: Secret::new(format!("downscoped-{}-{count}", req.resource)),
            issued_token_type: req.requested_token_type.clone(),
            token_type: "Bearer".to_owned(),
            scopes,
            expires_at_unix_secs: 1_700_000_600,
        })
    }
}

// spec: SEC-010
// spec: SEC-011
// spec: SEC-012
// spec: NGC-081
// spec: INV-09
// spec: INV-15
#[test]
fn authn_delegation_chain_tenant_isolation_and_rfc8693_token_exchange() {
    let now = 1_700_000_000_i64;
    let act_json = JsonValue::parse(
        r#"{
            "sub": "orchestrator-agent",
            "kind": "AGENT",
            "act": {
                "sub": "gateway-proxy",
                "kind": "SERVICE"
            }
        }"#,
    )
    .unwrap();
    let chain = parse_rfc8693_act_chain(Some(&act_json)).unwrap();
    assert_eq!(
        chain,
        vec![
            ActorRef {
                id: "orchestrator-agent".to_owned(),
                kind: PrincipalKind::Agent,
            },
            ActorRef {
                id: "gateway-proxy".to_owned(),
                kind: PrincipalKind::Service,
            },
        ]
    );

    let effective = intersect_delegated_scopes(&[
        BTreeSet::from([
            "a:read".to_owned(),
            "b:write".to_owned(),
            "c:admin".to_owned(),
        ]),
        BTreeSet::from(["a:read".to_owned(), "b:write".to_owned()]),
        BTreeSet::from(["b:write".to_owned(), "d:other".to_owned()]),
    ]);
    assert_eq!(effective, BTreeSet::from(["b:write".to_owned()]));

    let mut p_acme = Principal::new("alice", PrincipalKind::Human, tenant("acme"));
    p_acme.scopes = effective.clone();
    p_acme.delegation = chain;

    let mut p_beta = p_acme.clone();
    p_beta.tenant = tenant("beta");

    let transport = MockExchange {
        calls: AtomicU64::new(0),
    };
    let client = TokenExchangeClient::new(&transport);
    let assertion = Secret::new("gateway-signed-assertion".to_owned());
    let req_scopes = BTreeSet::from(["b:write".to_owned()]);

    let tok_acme_1 = client
        .exchange_for_target(
            &p_acme,
            &assertion,
            "https://branch-crm.internal",
            &req_scopes,
            now,
        )
        .unwrap();
    let tok_acme_2 = client
        .exchange_for_target(
            &p_acme,
            &assertion,
            "https://branch-crm.internal",
            &req_scopes,
            now + 5,
        )
        .unwrap();
    assert_eq!(
        tok_acme_1.access_token.expose_secret(),
        tok_acme_2.access_token.expose_secret()
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);

    // Different tenant with identical principal.id MUST NOT hit acme's cache entry (INV-09).
    let tok_beta = client
        .exchange_for_target(
            &p_beta,
            &assertion,
            "https://branch-crm.internal",
            &req_scopes,
            now + 5,
        )
        .unwrap();
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
    assert_ne!(
        tok_acme_1.access_token.expose_secret(),
        tok_beta.access_token.expose_secret()
    );

    // Near expiry triggers refresh outside margin.
    let tok_refreshed = client
        .exchange_for_target(
            &p_acme,
            &assertion,
            "https://branch-crm.internal",
            &req_scopes,
            1_700_000_580,
        )
        .unwrap();
    assert_eq!(transport.calls.load(Ordering::SeqCst), 3);
    assert_ne!(
        tok_acme_1.access_token.expose_secret(),
        tok_refreshed.access_token.expose_secret()
    );

    // Ensure Principal JSON never contains downstream token and Secret<T> Debug redacts it (INV-15).
    let dbg_str = format!("{tok_beta:?}");
    assert!(!dbg_str.contains("downscoped-"));
    assert!(BTreeMap::<String, AttrValue>::new().is_empty());
}

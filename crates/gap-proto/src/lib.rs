//! Protobuf and gRPC wire contracts for `NGC`, the GAP Gateway, and optional `A2A` bindings (`L1`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

/// Canonical NGC wire-protocol version string carried by `ConnectorDescription.ngc_version` (`NGC-004`).
pub const NGC_WIRE_VERSION: &str = "gap/0.1";

/// Encoded `google.protobuf.FileDescriptorSet` for GAP's own wire contracts
/// (`gap/gateway/v1/gateway.proto` and `gap/ngc/v1/ngc.proto`, sorted by file name,
/// without `source_code_info` and without transitive imports; `NGC-001`).
pub const DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptor_set.bin"));

/// Encoded `google.protobuf.FileDescriptorSet` for the vendored `A2A` wire contract
/// (`a2a.proto` only, without `source_code_info` and without transitive imports; `CMP-020`).
#[cfg(feature = "a2a")]
pub const A2A_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/a2a_descriptor_set.bin"));

/// Native Graph Connector (`gap.ngc.v1`) Protobuf messages and `tonic` client/server stubs (`NGC-001`).
pub mod ngc {
    /// Generated types and gRPC `connector_client` / `connector_server` bindings for `gap.ngc.v1`.
    #[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all, deprecated)]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/gap.ngc.v1.rs"));
    }
}

/// GAP Gateway (`gap.gateway.v1`) Protobuf messages and `tonic` client/server stubs (`NGC-001`).
pub mod gateway {
    /// Generated types and gRPC `gateway_client` / `gateway_server` bindings for `gap.gateway.v1`.
    #[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all, deprecated)]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/gap.gateway.v1.rs"));
    }
}

/// Agent-to-Agent (`lf.a2a.v1`) Protobuf messages and `tonic` client/server stubs (`CMP-020`..`CMP-022`).
#[cfg(feature = "a2a")]
pub mod a2a {
    /// Generated types and gRPC `a2a_service_client` / `a2a_service_server` bindings for `lf.a2a.v1`.
    #[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all, deprecated)]
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/lf.a2a.v1.rs"));
    }
}

#[cfg(test)]
mod tests {
    use prost_reflect::{Cardinality, DescriptorPool, Kind};

    use super::{DESCRIPTOR_SET, NGC_WIRE_VERSION};

    // spec: NGC-004
    #[test]
    fn ngc_wire_version_matches_package_major_minor_and_descriptor() -> Result<(), String> {
        let pkg_version = env!("CARGO_PKG_VERSION");
        let mut parts = pkg_version.split('.');
        let major = parts
            .next()
            .ok_or_else(|| format!("missing major in CARGO_PKG_VERSION '{pkg_version}'"))?;
        let minor = parts
            .next()
            .ok_or_else(|| format!("missing minor in CARGO_PKG_VERSION '{pkg_version}'"))?;

        let expected = format!("gap/{major}.{minor}");
        assert_eq!(expected, "gap/0.1");
        assert_eq!(NGC_WIRE_VERSION, expected);

        let mut pool = DescriptorPool::global();
        pool.decode_file_descriptor_set(DESCRIPTOR_SET)
            .map_err(|err| format!("failed to decode DESCRIPTOR_SET: {err}"))?;

        let desc = pool
            .get_message_by_name("gap.ngc.v1.ConnectorDescription")
            .ok_or_else(|| "missing gap.ngc.v1.ConnectorDescription in descriptor".to_owned())?;
        let field = desc
            .get_field_by_name("ngc_version")
            .ok_or_else(|| "ConnectorDescription missing field 'ngc_version'".to_owned())?;

        assert_eq!(field.kind(), Kind::String);
        assert_eq!(field.cardinality(), Cardinality::Optional);
        assert!(!field.is_list());
        assert!(!field.is_map());
        Ok(())
    }
}

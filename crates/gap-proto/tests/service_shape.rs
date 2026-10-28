//! Descriptor service/method shape and generated tonic module-layout tests.
#![forbid(unsafe_code)]

use prost::Message as _;
use prost_reflect::{DescriptorPool, Kind};
use prost_types::FileDescriptorSet;

#[derive(Debug, Clone, PartialEq, Eq)]
struct MethodShape<'a> {
    name: &'a str,
    client_streaming: bool,
    server_streaming: bool,
    input_type: &'a str,
    output_type: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnedMethodShape {
    name: String,
    client_streaming: bool,
    server_streaming: bool,
    input_type: String,
    output_type: String,
}

impl PartialEq<MethodShape<'_>> for OwnedMethodShape {
    fn eq(&self, other: &MethodShape<'_>) -> bool {
        self.name == other.name
            && self.client_streaming == other.client_streaming
            && self.server_streaming == other.server_streaming
            && self.input_type == other.input_type
            && self.output_type == other.output_type
    }
}

fn load_own_pool() -> (DescriptorPool, Vec<String>) {
    let fds = FileDescriptorSet::decode(gap_proto::DESCRIPTOR_SET)
        .expect("decode DESCRIPTOR_SET as FileDescriptorSet");
    let file_names: Vec<String> = fds.file.iter().map(|f| f.name().to_owned()).collect();
    let mut pool = DescriptorPool::global();
    pool.decode_file_descriptor_set(gap_proto::DESCRIPTOR_SET)
        .expect("load DESCRIPTOR_SET into DescriptorPool");
    (pool, file_names)
}

#[cfg(feature = "a2a")]
fn load_a2a_pool() -> (DescriptorPool, Vec<String>) {
    const A2A_IMPORTS_FDS: &[u8] = include_bytes!(env!("GAP_PROTO_A2A_IMPORTS_FDS"));
    let fds = FileDescriptorSet::decode(gap_proto::A2A_DESCRIPTOR_SET)
        .expect("decode A2A_DESCRIPTOR_SET as FileDescriptorSet");
    let file_names: Vec<String> = fds.file.iter().map(|f| f.name().to_owned()).collect();
    let mut pool = DescriptorPool::global();
    pool.decode_file_descriptor_set(A2A_IMPORTS_FDS)
        .expect("load google/api descriptor prerequisites into DescriptorPool");
    pool.decode_file_descriptor_set(gap_proto::A2A_DESCRIPTOR_SET)
        .expect("load A2A_DESCRIPTOR_SET into DescriptorPool");
    (pool, file_names)
}

fn collect_services(
    pool: &DescriptorPool,
    file_names: &[String],
) -> Vec<(String, Vec<OwnedMethodShape>)> {
    let mut out = Vec::new();
    for file_name in file_names {
        let file = pool
            .get_file_by_name(file_name)
            .unwrap_or_else(|| panic!("missing file {file_name} in DescriptorPool"));
        for svc in file.services() {
            let methods = svc
                .methods()
                .map(|m| OwnedMethodShape {
                    name: m.name().to_owned(),
                    client_streaming: m.is_client_streaming(),
                    server_streaming: m.is_server_streaming(),
                    input_type: m.input().full_name().to_owned(),
                    output_type: m.output().full_name().to_owned(),
                })
                .collect();
            out.push((svc.full_name().to_owned(), methods));
        }
    }
    out
}

fn assert_service_methods(actual: &[OwnedMethodShape], expected: &[MethodShape<'_>], svc: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "service {svc} method count mismatch: actual={actual:#?}, expected={expected:#?}"
    );
    for (idx, (act, exp)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            act, exp,
            "service {svc} method #{idx} mismatch: actual={act:?}, expected={exp:?}"
        );
    }
}

fn _assert_static_ngc_and_gateway_tonic_modules<C, G>(connector: C, gateway: G)
where
    C: gap_proto::ngc::v1::connector_server::Connector,
    G: gap_proto::gateway::v1::gateway_server::Gateway,
{
    let _ = gap_proto::ngc::v1::connector_server::ConnectorServer::new(connector);
    let _ = gap_proto::gateway::v1::gateway_server::GatewayServer::new(gateway);
    let _ = std::any::type_name::<
        gap_proto::ngc::v1::connector_client::ConnectorClient<tonic::transport::Channel>,
    >();
    let _ = std::any::type_name::<
        gap_proto::gateway::v1::gateway_client::GatewayClient<tonic::transport::Channel>,
    >();
}

#[cfg(feature = "a2a")]
fn _assert_static_a2a_tonic_modules<S>(svc: S)
where
    S: gap_proto::a2a::v1::a2a_service_server::A2aService,
{
    let _ = gap_proto::a2a::v1::a2a_service_server::A2aServiceServer::new(svc);
    let _ = std::any::type_name::<
        gap_proto::a2a::v1::a2a_service_client::A2aServiceClient<tonic::transport::Channel>,
    >();
}

// spec: NGC-001
#[test]
fn gateway_and_connector_services_match_explicit_method_table() {
    let (pool, file_names) = load_own_pool();
    let services = collect_services(&pool, &file_names);

    let service_names: Vec<&str> = services.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        service_names,
        vec!["gap.gateway.v1.Gateway", "gap.ngc.v1.Connector"],
        "unexpected service list in DESCRIPTOR_SET"
    );

    let expected_gateway = [
        MethodShape {
            name: "Execute",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.gateway.v1.ExecuteRequest",
            output_type: "gap.gateway.v1.ExecuteResponse",
        },
        MethodShape {
            name: "Subscribe",
            client_streaming: false,
            server_streaming: true,
            input_type: "gap.gateway.v1.ExecuteRequest",
            output_type: "gap.gateway.v1.ExecuteResponse",
        },
    ];
    assert_service_methods(&services[0].1, &expected_gateway, "gap.gateway.v1.Gateway");

    let expected_connector = [
        MethodShape {
            name: "Describe",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.DescribeRequest",
            output_type: "gap.ngc.v1.DescribeResponse",
        },
        MethodShape {
            name: "Query",
            client_streaming: false,
            server_streaming: true,
            input_type: "gap.ngc.v1.QueryRequest",
            output_type: "gap.ngc.v1.QueryResponse",
        },
        MethodShape {
            name: "Mutate",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.MutateRequest",
            output_type: "gap.ngc.v1.MutateResponse",
        },
        MethodShape {
            name: "Subscribe",
            client_streaming: false,
            server_streaming: true,
            input_type: "gap.ngc.v1.SubscribeRequest",
            output_type: "gap.ngc.v1.Event",
        },
        MethodShape {
            name: "StartTask",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.StartTaskRequest",
            output_type: "gap.ngc.v1.TaskStatus",
        },
        MethodShape {
            name: "GetTask",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.GetTaskRequest",
            output_type: "gap.ngc.v1.TaskStatus",
        },
        MethodShape {
            name: "CancelTask",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.CancelTaskRequest",
            output_type: "gap.ngc.v1.TaskStatus",
        },
        MethodShape {
            name: "WatchTask",
            client_streaming: false,
            server_streaming: true,
            input_type: "gap.ngc.v1.WatchTaskRequest",
            output_type: "gap.ngc.v1.TaskStatus",
        },
        MethodShape {
            name: "Sketch",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.SketchRequest",
            output_type: "gap.ngc.v1.SketchResponse",
        },
        MethodShape {
            name: "Health",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.HealthRequest",
            output_type: "gap.ngc.v1.HealthResponse",
        },
        MethodShape {
            name: "Explain",
            client_streaming: false,
            server_streaming: false,
            input_type: "gap.ngc.v1.QueryRequest",
            output_type: "gap.ngc.v1.ExplainResponse",
        },
    ];
    assert_service_methods(&services[1].1, &expected_connector, "gap.ngc.v1.Connector");
}

// spec: NGC-004
#[test]
fn connector_description_carries_ngc_wire_version() {
    assert_eq!(gap_proto::NGC_WIRE_VERSION, "gap/0.1");

    let (pool, _) = load_own_pool();
    let desc = pool
        .get_message_by_name("gap.ngc.v1.ConnectorDescription")
        .expect("ConnectorDescription must exist in descriptor");
    let field = desc
        .get_field_by_name("ngc_version")
        .expect("ConnectorDescription must carry field ngc_version");
    assert_eq!(field.kind(), Kind::String);
    assert!(!field.is_list() && !field.is_map());
}

// spec: CMP-020
// spec: CMP-021
// spec: CMP-022
#[cfg(feature = "a2a")]
#[test]
fn a2a_service_matches_explicit_method_table() {
    let (pool, file_names) = load_a2a_pool();
    let services = collect_services(&pool, &file_names);

    let service_names: Vec<&str> = services.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        service_names,
        vec!["lf.a2a.v1.A2AService"],
        "unexpected service list in A2A_DESCRIPTOR_SET"
    );

    let expected_a2a = [
        MethodShape {
            name: "SendMessage",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.SendMessageRequest",
            output_type: "lf.a2a.v1.SendMessageResponse",
        },
        MethodShape {
            name: "SendStreamingMessage",
            client_streaming: false,
            server_streaming: true,
            input_type: "lf.a2a.v1.SendMessageRequest",
            output_type: "lf.a2a.v1.StreamResponse",
        },
        MethodShape {
            name: "GetTask",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.GetTaskRequest",
            output_type: "lf.a2a.v1.Task",
        },
        MethodShape {
            name: "ListTasks",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.ListTasksRequest",
            output_type: "lf.a2a.v1.ListTasksResponse",
        },
        MethodShape {
            name: "CancelTask",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.CancelTaskRequest",
            output_type: "lf.a2a.v1.Task",
        },
        MethodShape {
            name: "SubscribeToTask",
            client_streaming: false,
            server_streaming: true,
            input_type: "lf.a2a.v1.SubscribeToTaskRequest",
            output_type: "lf.a2a.v1.StreamResponse",
        },
        MethodShape {
            name: "CreateTaskPushNotificationConfig",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.TaskPushNotificationConfig",
            output_type: "lf.a2a.v1.TaskPushNotificationConfig",
        },
        MethodShape {
            name: "GetTaskPushNotificationConfig",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.GetTaskPushNotificationConfigRequest",
            output_type: "lf.a2a.v1.TaskPushNotificationConfig",
        },
        MethodShape {
            name: "ListTaskPushNotificationConfigs",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.ListTaskPushNotificationConfigsRequest",
            output_type: "lf.a2a.v1.ListTaskPushNotificationConfigsResponse",
        },
        MethodShape {
            name: "GetExtendedAgentCard",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.GetExtendedAgentCardRequest",
            output_type: "lf.a2a.v1.AgentCard",
        },
        MethodShape {
            name: "DeleteTaskPushNotificationConfig",
            client_streaming: false,
            server_streaming: false,
            input_type: "lf.a2a.v1.DeleteTaskPushNotificationConfigRequest",
            output_type: "google.protobuf.Empty",
        },
    ];
    assert_service_methods(&services[0].1, &expected_a2a, "lf.a2a.v1.A2AService");
}

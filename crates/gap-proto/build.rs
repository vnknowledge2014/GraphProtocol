//! Pure-Rust `.proto` compilation and `tonic` code generation via `protox` (`ADR-002`, `NGC-001`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::env;
use std::error::Error as _;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use protox::Compiler;
use protox::prost::Message as _;
use protox::prost_reflect::{DescriptorPool, DynamicMessage, Value};

const OWN_PROTO_FILES: &[&str] = &["gap/gateway/v1/gateway.proto", "gap/ngc/v1/ngc.proto"];
const A2A_PROTO_FILES: &[&str] = &["a2a.proto"];
const A2A_GOOGLEAPIS_ROOTS: &[&str] = &[
    "google/api/annotations.proto",
    "google/api/client.proto",
    "google/api/field_behavior.proto",
];

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("gap-proto build failed: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir = env_path("CARGO_MANIFEST_DIR")?;
    let out_dir = env_path("OUT_DIR")?;

    let proto_root = manifest_dir.join("../../proto");
    emit_rerun_if_changed_tree(&proto_root)?;

    compile_own_protos(&proto_root, &out_dir)?;

    if env::var_os("CARGO_FEATURE_A2A").is_some() {
        let a2a_spec_dir = manifest_dir.join("../../third_party/specs/a2a-v1.0.1/specification");
        let googleapis_dir = manifest_dir.join("../../third_party/specs/googleapis-646596e286b4");
        emit_rerun_if_changed_tree(&a2a_spec_dir)?;
        emit_rerun_if_changed_tree(&googleapis_dir)?;
        compile_a2a_protos(&a2a_spec_dir, &googleapis_dir, &out_dir)?;
    }

    Ok(())
}

fn env_path(var_name: &str) -> Result<PathBuf, String> {
    env::var_os(var_name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing required Cargo environment variable '{var_name}'"))
}

fn emit_rerun_if_changed_tree(root: &Path) -> Result<(), String> {
    println!("cargo:rerun-if-changed={}", root.display());
    if !root.is_dir() {
        return Ok(());
    }

    let entries = fs::read_dir(root)
        .map_err(|err| format!("failed to read directory '{}': {err}", root.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            format!(
                "failed to read directory entry in '{}': {err}",
                root.display()
            )
        })?;
        paths.push(entry.path());
    }
    paths.sort();

    for path in paths {
        if path.is_dir() {
            emit_rerun_if_changed_tree(&path)?;
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Ok(())
}

fn format_protox_error(context: &str, err: &protox::Error) -> String {
    let mut msg = match err.file() {
        Some(file) => format!("{context} (in '{file}'): {err}"),
        None => format!("{context}: {err}"),
    };
    let mut cause = err.source();
    while let Some(src) = cause {
        let _ = write!(msg, "\n  caused by: {src}");
        cause = src.source();
    }
    msg
}

fn encode_sorted_own_descriptor_set(
    compiler: &Compiler,
    own_files: &[&str],
) -> Result<Vec<u8>, String> {
    let mut sorted_names: Vec<&str> = own_files.to_vec();
    sorted_names.sort_unstable();

    let pool = compiler.descriptor_pool();
    let global = DescriptorPool::global();
    let file_proto_desc = global
        .get_message_by_name("google.protobuf.FileDescriptorProto")
        .ok_or_else(|| {
            "missing google.protobuf.FileDescriptorProto in global descriptor pool".to_owned()
        })?;
    let fds_desc = global
        .get_message_by_name("google.protobuf.FileDescriptorSet")
        .ok_or_else(|| {
            "missing google.protobuf.FileDescriptorSet in global descriptor pool".to_owned()
        })?;

    let mut files = Vec::with_capacity(sorted_names.len());
    for name in sorted_names {
        let file = pool
            .get_file_by_name(name)
            .ok_or_else(|| format!("compiled descriptor pool missing expected file '{name}'"))?;
        let raw_bytes = file.encode_to_vec();
        let mut file_msg = DynamicMessage::decode(file_proto_desc.clone(), raw_bytes.as_slice())
            .map_err(|err| format!("failed to decode FileDescriptorProto for '{name}': {err}"))?;
        file_msg.clear_field_by_name("source_code_info");
        files.push(Value::Message(file_msg));
    }

    let mut fds_msg = DynamicMessage::new(fds_desc);
    fds_msg.set_field_by_name("file", Value::List(files));
    Ok(fds_msg.encode_to_vec())
}

fn compile_own_protos(proto_root: &Path, out_dir: &Path) -> Result<(), String> {
    let mut compiler = Compiler::new([proto_root]).map_err(|err| {
        format_protox_error("failed to initialize protox for '../../proto'", &err)
    })?;
    compiler.include_source_info(false).include_imports(false);
    compiler
        .open_files(OWN_PROTO_FILES)
        .map_err(|err| format_protox_error("failed to compile GAP/NGC .proto files", &err))?;

    let descriptor_bytes = encode_sorted_own_descriptor_set(&compiler, OWN_PROTO_FILES)?;
    let descriptor_out = out_dir.join("descriptor_set.bin");
    fs::write(&descriptor_out, descriptor_bytes).map_err(|err| {
        format!(
            "failed to write descriptor set '{}': {err}",
            descriptor_out.display()
        )
    })?;

    // prost-build's MessageGraph requires imported well-known descriptors (Struct, Timestamp)
    // to be present in the FileDescriptorSet passed to compile_fds, while well-known types
    // themselves map to prost_types automatically.
    compiler.include_imports(true);
    let own_fds = compiler.file_descriptor_set();
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(own_fds)
        .map_err(|err| format!("failed to generate tonic/prost code for GAP/NGC protos: {err}"))?;

    Ok(())
}

fn compile_a2a_protos(
    a2a_spec_dir: &Path,
    googleapis_dir: &Path,
    out_dir: &Path,
) -> Result<(), String> {
    let mut a2a_compiler = Compiler::new([a2a_spec_dir, googleapis_dir])
        .map_err(|err| format_protox_error("failed to initialize protox for A2A", &err))?;
    a2a_compiler
        .include_source_info(false)
        .include_imports(false);
    a2a_compiler
        .open_files(A2A_PROTO_FILES)
        .map_err(|err| format_protox_error("failed to compile third-party a2a.proto", &err))?;

    let a2a_descriptor_bytes = encode_sorted_own_descriptor_set(&a2a_compiler, A2A_PROTO_FILES)?;
    let a2a_descriptor_out = out_dir.join("a2a_descriptor_set.bin");
    fs::write(&a2a_descriptor_out, a2a_descriptor_bytes).map_err(|err| {
        format!(
            "failed to write A2A descriptor set '{}': {err}",
            a2a_descriptor_out.display()
        )
    })?;

    // Emit a separate helper descriptor set for google/api/*.proto in OUT_DIR so tests can
    // populate DescriptorPool prerequisites while A2A_DESCRIPTOR_SET stays strictly a2a.proto-only.
    let mut imports_compiler = Compiler::new([googleapis_dir])
        .map_err(|err| format_protox_error("failed to initialize protox for googleapis", &err))?;
    imports_compiler
        .include_source_info(false)
        .include_imports(true);
    imports_compiler
        .open_files(A2A_GOOGLEAPIS_ROOTS)
        .map_err(|err| format_protox_error("failed to compile google/api imports", &err))?;
    let imports_out = out_dir.join("a2a_googleapis_imports.bin");
    fs::write(&imports_out, imports_compiler.encode_file_descriptor_set()).map_err(|err| {
        format!(
            "failed to write googleapis descriptor helper '{}': {err}",
            imports_out.display()
        )
    })?;
    println!(
        "cargo:rustc-env=GAP_PROTO_A2A_IMPORTS_FDS={}",
        imports_out.display()
    );

    a2a_compiler.include_imports(true);
    let a2a_fds = a2a_compiler.file_descriptor_set();
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(a2a_fds)
        .map_err(|err| format!("failed to generate tonic/prost code for a2a.proto: {err}"))?;

    Ok(())
}

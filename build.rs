//! Generates Rust gRPC code from `proto/` (the API source of truth).
//! Uses protox (pure-Rust protobuf compiler) — no system `protoc` needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let file_descriptors = protox::compile(["proto/asc/daemon/v1/daemon.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_client(true)
        .compile_fds(file_descriptors)?;
    Ok(())
}

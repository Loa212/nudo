use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The proto at the repo root is the authoritative API contract; it is not
    // copied into the crate so there is exactly one definition to edit.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let proto = root.join("controlplane.proto");

    // `controlplane.proto` imports google/protobuf/{timestamp,empty}.proto, and
    // whether protoc resolves those from its own installation depends on how it
    // was packaged — Homebrew's build ships them on the default include path,
    // Debian's protobuf-compiler does not. They are vendored here and this
    // directory is on the include path, so the build behaves the same on a
    // developer's machine, in CI, and in the Docker image.
    let vendored = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include");

    println!("cargo:rerun-if-changed={}", proto.display());
    println!("cargo:rerun-if-changed={}", vendored.display());

    // Both stubs come from one crate so every binary — server, web, cli, mcp —
    // shares a single generated module rather than each regenerating its own
    // incompatible copy of the types.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[root, vendored])?;

    Ok(())
}

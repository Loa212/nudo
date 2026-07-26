use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The proto at the repo root is the authoritative API contract; it is not
    // copied into the crate so there is exactly one definition to edit.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let proto = root.join("controlplane.proto");

    println!("cargo:rerun-if-changed={}", proto.display());

    // Both stubs come from one crate so every binary — server, web, cli, mcp —
    // shares a single generated module rather than each regenerating its own
    // incompatible copy of the types.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto], &[root])?;

    Ok(())
}

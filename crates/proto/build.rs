fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        // Keeps ServerMessage small (a Workload is large).
        .boxed(".modula.node.v1.CronRun.workload")
        .compile_protos(&["proto/modula/node/v1/node.proto"], &["proto"])?;
    Ok(())
}

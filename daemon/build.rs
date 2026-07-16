fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fds = protox::compile(["../proto/gather/v1/gather.proto"], ["../proto"])?;
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure().compile_protos(
        &["proto/capyctl/management/v1/management.proto"],
        &["proto"],
    )?;
    Ok(())
}

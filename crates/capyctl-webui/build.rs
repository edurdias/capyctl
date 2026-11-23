//! ADR 0040 §13: Cargo watches assets, but never builds them with npm.
fn main() {
    let root =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../web");
    // Watch the parent too: dist may not exist in a development checkout.
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed={}", root.join("dist").display());
}

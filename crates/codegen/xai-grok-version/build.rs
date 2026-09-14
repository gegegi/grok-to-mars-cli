fn main() {
    println!("cargo:rerun-if-env-changed=GROK_VERSION");
    // GTM overlay: gtm-version — bump GTM_VERSION without touching Cargo.toml.
    println!("cargo:rerun-if-changed=../../../GTM_VERSION");
}

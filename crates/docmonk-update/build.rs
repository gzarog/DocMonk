fn main() {
    // The release asset name embeds the target triple.
    println!(
        "cargo:rustc-env=RAGMONK_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    println!("cargo:rerun-if-env-changed=RAGMONK_MINISIGN_PUBKEY");
}

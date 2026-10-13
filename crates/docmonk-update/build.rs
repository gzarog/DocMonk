fn main() {
    // The release asset name embeds the target triple.
    println!(
        "cargo:rustc-env=DOCMONK_TARGET={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    println!("cargo:rerun-if-env-changed=DOCMONK_MINISIGN_PUBKEY");
}

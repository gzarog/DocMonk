//! Release verification: the archive's SHA-256 must match its line in
//! `SHA256SUMS`, and when a minisign public key is compiled in,
//! `SHA256SUMS` itself must carry a valid signature by that key.

use sha2::{Digest, Sha256};

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The hex digest `SHA256SUMS` lists for `name` (`<hex>  <name>` or
/// `<hex> *<name>`).
pub fn expected_digest(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hex, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start().trim_start_matches('*');
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| hex.to_lowercase())
    })
}

pub fn check_digest(data: &[u8], sums: &str, name: &str) -> Result<(), String> {
    let expected = expected_digest(sums, name)
        .ok_or_else(|| format!("{name} is not listed in {}", crate::SUMS_ASSET))?;
    let actual = sha256_hex(data);
    if actual != expected {
        return Err(format!(
            "checksum mismatch for {name}: expected {expected}, got {actual}; refusing to install"
        ));
    }
    Ok(())
}

/// Verifies a minisign signature (`.minisig` text) over `data`.
pub fn check_signature(public_key: &str, data: &[u8], signature: &str) -> Result<(), String> {
    let key = minisign_verify::PublicKey::from_base64(public_key.trim())
        .or_else(|_| minisign_verify::PublicKey::decode(public_key))
        .map_err(|e| format!("invalid minisign public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(signature)
        .map_err(|e| format!("invalid release signature: {e}"))?;
    key.verify(data, &sig, false)
        .map_err(|e| format!("release signature does not verify: {e}; refusing to install"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests() {
        let data = b"hello";
        let hex = sha256_hex(data);
        let sums = format!("{hex}  a.tar.gz\n{} *b.zip\n", "0".repeat(64));
        assert_eq!(expected_digest(&sums, "a.tar.gz"), Some(hex.clone()));
        assert_eq!(expected_digest(&sums, "b.zip"), Some("0".repeat(64)));
        assert!(check_digest(data, &sums, "a.tar.gz").is_ok());
        let e = check_digest(data, &sums, "b.zip").unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
        assert!(check_digest(data, &sums, "c")
            .unwrap_err()
            .contains("not listed"));
    }

    // A key pair and signature generated with the minisign format (plain
    // and prehashed Ed25519), a fixed test-only seed (never a release key).
    const PUBKEY: &str = include_str!("../tests/minisign.pub");
    const SIG: &str = include_str!("../tests/minisign.sig");
    const SIG_DATA: &[u8] = b"abc123  ragmonk-1.0.0-x86_64-unknown-linux-gnu.tar.gz\n";

    #[test]
    fn signatures() {
        check_signature(PUBKEY, SIG_DATA, SIG).unwrap();
        let e = check_signature(PUBKEY, b"tampered", SIG).unwrap_err();
        assert!(e.contains("does not verify"), "{e}");
        assert!(check_signature("bogus", SIG_DATA, SIG).is_err());
    }
}

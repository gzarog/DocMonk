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

/// Verifies downloaded release assets. With a compiled-in public key the
/// signature is mandatory: a missing, unreadable or invalid
/// `SHA256SUMS.minisig` refuses the install (fail closed), and only then
/// is the archive checked against the signed `SHA256SUMS`.
pub fn check_release(
    public_key: Option<&str>,
    archive: &[u8],
    sums: &[u8],
    signature: Option<Result<Vec<u8>, String>>,
    name: &str,
) -> Result<(), String> {
    if let Some(key) = public_key {
        let sig = signature
            .unwrap_or_else(|| Err("not downloaded".to_owned()))
            .map_err(|e| {
                format!(
                    "{} is required for a signed release ({e}); refusing to install",
                    crate::SIG_ASSET
                )
            })?;
        check_signature(key, sums, &String::from_utf8_lossy(&sig))?;
    }
    check_digest(archive, &String::from_utf8_lossy(sums), name)
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
    // and prehashed Ed25519), a fixed test-only seed (never a release key):
    // SHA-256 of "docmonk minisign test seed", key id 01..08.
    const PUBKEY: &str = include_str!("../tests/minisign.pub");
    const SIG: &str = include_str!("../tests/minisign.sig");
    const SIG_DATA: &[u8] = b"abc123  docmonk-1.0.0-x86_64-unknown-linux-gnu.tar.gz\n";

    #[test]
    fn signatures() {
        check_signature(PUBKEY, SIG_DATA, SIG).unwrap();
        let e = check_signature(PUBKEY, b"tampered", SIG).unwrap_err();
        assert!(e.contains("does not verify"), "{e}");
        assert!(check_signature("bogus", SIG_DATA, SIG).is_err());
    }

    #[test]
    fn a_configured_key_requires_a_valid_signature() {
        let name = "docmonk-1.0.0-x86_64-unknown-linux-gnu.tar.gz";
        let archive = b"archive";
        let sums = format!("{}  {name}\n", sha256_hex(archive));
        // Missing signature: refused before the checksum is even read.
        let e = check_release(
            Some(PUBKEY),
            archive,
            sums.as_bytes(),
            Some(Err("HTTP 404".into())),
            name,
        )
        .unwrap_err();
        assert!(e.contains("is required") && e.contains("refusing"), "{e}");
        let e = check_release(Some(PUBKEY), archive, sums.as_bytes(), None, name).unwrap_err();
        assert!(e.contains("is required"), "{e}");
        // A signature over different SHA256SUMS (tampered checksums).
        let e = check_release(
            Some(PUBKEY),
            archive,
            sums.as_bytes(),
            Some(Ok(SIG.as_bytes().to_vec())),
            name,
        )
        .unwrap_err();
        assert!(e.contains("does not verify"), "{e}");
        // Garbage in place of a signature.
        let e = check_release(
            Some(PUBKEY),
            archive,
            sums.as_bytes(),
            Some(Ok(b"not a signature".to_vec())),
            name,
        )
        .unwrap_err();
        assert!(e.contains("invalid release signature"), "{e}");
        // A valid signature passes on to the checksum check (the signed
        // test SHA256SUMS lists no real digest).
        let e = check_release(
            Some(PUBKEY),
            archive,
            SIG_DATA,
            Some(Ok(SIG.as_bytes().to_vec())),
            name,
        )
        .unwrap_err();
        assert!(e.contains("not listed in SHA256SUMS"), "{e}");
    }

    #[test]
    fn without_a_key_only_the_checksum_is_checked() {
        let name = "a.tar.gz";
        let archive = b"archive";
        let sums = format!("{}  {name}\n", sha256_hex(archive));
        check_release(None, archive, sums.as_bytes(), None, name).unwrap();
        let e = check_release(None, b"tampered", sums.as_bytes(), None, name).unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
    }
}

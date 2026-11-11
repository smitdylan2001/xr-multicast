//! Host authentication key (RSA-2048), compatible with adb's `adbkey` files.
//!
//! * The private key is stored as PEM (PKCS#8 `BEGIN PRIVATE KEY`, exactly
//!   like `adb keygen`; PKCS#1 `BEGIN RSA PRIVATE KEY` is accepted as well).
//! * `AUTH(TOKEN)` is answered with a PKCS#1 v1.5 signature that treats the
//!   20-byte token as a SHA-1 digest (`RSA_sign(NID_sha1, token, ...)` in adb).
//! * The public key is sent in Android's mincrypt `RSAPublicKey` layout,
//!   base64-encoded and followed by ` user@host`, which makes the headset show
//!   "Allow USB debugging?".

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rsa::PrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _, LineEnding};
use rsa::traits::PublicKeyParts as _;

use crate::protocol::TOKEN_SIZE;
use crate::{AdbError, Result};

/// Modulus size required by adbd (2048 bits).
pub const KEY_BITS: usize = 2048;
const MODULUS_WORDS: usize = KEY_BITS / 32;
/// Size of the binary `RSAPublicKey` structure: `len, n0inv, n[64], rr[64], exponent`.
pub const ANDROID_PUBKEY_ENCODED_SIZE: usize = 4 * (3 + 2 * MODULUS_WORDS);

/// An RSA-2048 key pair used to authenticate this host to devices.
#[derive(Clone)]
pub struct AdbKey {
    key: PrivateKey,
    android_pubkey: Vec<u8>,
}

impl std::fmt::Debug for AdbKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdbKey")
            .field("public", &self.public_key_base64())
            .finish_non_exhaustive()
    }
}

impl AdbKey {
    /// Generate a fresh RSA-2048 key (takes a moment; persist it with [`AdbKey::to_pem`]
    /// or use [`AdbKey::load_or_generate`]).
    pub fn generate() -> Result<AdbKey> {
        let mut rng = rand::rngs::SysRng;
        let key = PrivateKey::new(&mut rng, KEY_BITS)
            .map_err(|e| AdbError::Other(format!("RSA key generation failed: {e}")))?;
        AdbKey::from_private_key(key)
    }

    /// Wrap an existing RSA private key (must be 2048 bits with a 32-bit public exponent).
    pub fn from_private_key(key: PrivateKey) -> Result<AdbKey> {
        let n = key.n_bytes();
        let e = key.e_bytes();
        let android_pubkey = encode_android_pubkey(&n, &e)?;
        Ok(AdbKey {
            key,
            android_pubkey,
        })
    }

    /// Parse a PEM private key (PKCS#8 as written by `adb keygen`, or PKCS#1).
    pub fn from_pem(pem: &str) -> Result<AdbKey> {
        let key = match PrivateKey::from_pkcs8_pem(pem) {
            Ok(key) => key,
            Err(pkcs8_err) => PrivateKey::from_pkcs1_pem(pem).map_err(|pkcs1_err| {
                AdbError::Other(format!("invalid adb key PEM: {pkcs8_err} / {pkcs1_err}"))
            })?,
        };
        AdbKey::from_private_key(key)
    }

    /// Encode the private key as PKCS#8 PEM (same format as `adb keygen`).
    pub fn to_pem(&self) -> Result<String> {
        let pem = self
            .key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| AdbError::Other(format!("could not encode adb key: {e}")))?;
        Ok(pem.to_string())
    }

    /// Load the key at `path`, or generate one and save it there (private key
    /// with mode 0600 on Unix, plus an adb-style `<path>.pub`).
    pub fn load_or_generate(path: impl AsRef<Path>) -> Result<AdbKey> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(pem) => AdbKey::from_pem(&pem),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = AdbKey::generate()?;
                key.save(path)?;
                Ok(key)
            }
            Err(e) => Err(AdbError::Io(e)),
        }
    }

    /// Write the private key to `path` (atomically, mode 0600 on Unix) and the
    /// public key line to `<path>.pub`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        write_atomic(path, self.to_pem()?.as_bytes(), true)?;
        let mut pub_path = path.as_os_str().to_owned();
        pub_path.push(".pub");
        write_atomic(
            &PathBuf::from(pub_path),
            self.public_key_line(&default_key_name()).as_bytes(),
            false,
        )?;
        Ok(())
    }

    /// Sign an `AUTH(TOKEN)` challenge: PKCS#1 v1.5 with the SHA-1 DigestInfo
    /// prefix over the raw 20-byte token (the token is *not* hashed again).
    pub fn sign_token(&self, token: &[u8]) -> Result<Vec<u8>> {
        if token.len() != TOKEN_SIZE {
            return Err(AdbError::Protocol(format!(
                "AUTH token has {} bytes, expected {TOKEN_SIZE}",
                token.len()
            )));
        }
        self.key
            .pkcs1v15_sign_prehash::<sha1::Sha1>(token)
            .map_err(|e| AdbError::Other(format!("signing AUTH token failed: {e}")))
    }

    /// The public key in Android's binary `RSAPublicKey` layout (524 bytes).
    pub fn android_public_key(&self) -> &[u8] {
        &self.android_pubkey
    }

    /// Base64 of [`AdbKey::android_public_key`] (the first field of `adbkey.pub`).
    pub fn public_key_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(&self.android_pubkey)
    }

    /// `"<base64> <name>"`, the content of an `adbkey.pub` file / a line of
    /// `/data/misc/adb/adb_keys`.
    pub fn public_key_line(&self, name: &str) -> String {
        format!("{} {name}", self.public_key_base64())
    }

    /// Payload of `AUTH(RSAPUBLICKEY)`: `"<base64> <name>\0"`.
    pub fn auth_public_key_payload(&self, name: &str) -> Vec<u8> {
        let mut payload = self.public_key_line(name).into_bytes();
        payload.push(0);
        payload
    }

    /// The underlying RSA key.
    pub fn private_key(&self) -> &PrivateKey {
        &self.key
    }
}

/// `user@host` used as the key comment (shown on the headset's prompt).
pub fn default_key_name() -> String {
    let user = ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "xrmulticast".to_string());
    let host = ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .or_else(|_| std::fs::read_to_string("/etc/hostname"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "xr-multicast".to_string());
    // The comment must not contain whitespace or NULs (it is space-separated).
    format!("{user}@{host}")
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '\0')
        .collect()
}

fn write_atomic(path: &Path, data: &[u8], private: bool) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(if private { 0o600 } else { 0o644 });
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&tmp)?;
        std::io::Write::write_all(&mut file, data)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Encode a public key (big-endian modulus and exponent) in Android's
/// mincrypt `RSAPublicKey` layout: little-endian u32 words
/// `len=64, n0inv = -1/n[0] mod 2^32, n[64], rr[64] = (2^2048)^2 mod n, e`.
pub fn encode_android_pubkey(modulus_be: &[u8], exponent_be: &[u8]) -> Result<Vec<u8>> {
    let modulus_be = strip_leading_zeros(modulus_be);
    if modulus_be.len() != KEY_BITS / 8 {
        return Err(AdbError::Other(format!(
            "adb keys must be RSA-{KEY_BITS}, got a {}-bit modulus",
            modulus_be.len() * 8
        )));
    }
    let exponent_be = strip_leading_zeros(exponent_be);
    if exponent_be.len() > 4 {
        return Err(AdbError::Other(
            "RSA public exponent does not fit in 32 bits".into(),
        ));
    }
    let exponent = exponent_be
        .iter()
        .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));

    // Little-endian words of n.
    let mut n = [0u32; MODULUS_WORDS];
    for (i, chunk) in modulus_be.rchunks(4).enumerate() {
        n[i] = chunk.iter().fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
    }
    if n[0] & 1 == 0 {
        return Err(AdbError::Other("RSA modulus is even".into()));
    }

    // Newton iteration for the inverse of n[0] modulo 2^32 (5 steps double 1 -> 32 correct bits).
    let mut inv: u32 = 1;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
    }
    let n0inv = inv.wrapping_neg();

    // rr = 2^(2 * 2048) mod n by repeated doubling with conditional subtraction.
    let mut r = [0u32; MODULUS_WORDS + 1];
    r[0] = 1;
    for _ in 0..(2 * KEY_BITS) {
        let mut carry = 0u32;
        for word in r.iter_mut() {
            let next = *word >> 31;
            *word = (*word << 1) | carry;
            carry = next;
        }
        if !less_than(&r, &n) {
            subtract(&mut r, &n);
        }
    }

    let mut out = Vec::with_capacity(ANDROID_PUBKEY_ENCODED_SIZE);
    out.extend_from_slice(&(MODULUS_WORDS as u32).to_le_bytes());
    out.extend_from_slice(&n0inv.to_le_bytes());
    for word in n {
        out.extend_from_slice(&word.to_le_bytes());
    }
    for word in &r[..MODULUS_WORDS] {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&exponent.to_le_bytes());
    Ok(out)
}

fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
    let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

/// `a < n` where `a` has one extra (most significant) word.
fn less_than(a: &[u32; MODULUS_WORDS + 1], n: &[u32; MODULUS_WORDS]) -> bool {
    if a[MODULUS_WORDS] != 0 {
        return false;
    }
    for i in (0..MODULUS_WORDS).rev() {
        if a[i] != n[i] {
            return a[i] < n[i];
        }
    }
    false
}

fn subtract(a: &mut [u32; MODULUS_WORDS + 1], n: &[u32; MODULUS_WORDS]) {
    let mut borrow = 0u64;
    for (i, word) in a.iter_mut().enumerate() {
        let sub = u64::from(n.get(i).copied().unwrap_or(0)) + borrow;
        let cur = u64::from(*word);
        if cur >= sub {
            *word = (cur - sub) as u32;
            borrow = 0;
        } else {
            *word = ((1u64 << 32) + cur - sub) as u32;
            borrow = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = include_str!("../tests/data/adbkey");
    const TEST_PUB: &str = include_str!("../tests/data/adbkey.pub");

    #[test]
    fn public_key_matches_adb_keygen() {
        let key = AdbKey::from_pem(TEST_KEY).unwrap();
        let expected = TEST_PUB.split_whitespace().next().unwrap();
        assert_eq!(key.public_key_base64(), expected);
        assert_eq!(key.android_public_key().len(), ANDROID_PUBKEY_ENCODED_SIZE);
        assert_eq!(&key.android_public_key()[..4], &64u32.to_le_bytes());
        assert_eq!(key.public_key_line("a@b"), format!("{expected} a@b"));
        let payload = key.auth_public_key_payload("a@b");
        assert_eq!(payload.last(), Some(&0));
    }

    #[test]
    fn pem_round_trip() {
        let key = AdbKey::from_pem(TEST_KEY).unwrap();
        let pem = key.to_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----"));
        let again = AdbKey::from_pem(&pem).unwrap();
        assert_eq!(again.public_key_base64(), key.public_key_base64());
        // PKCS#1 PEM is accepted too.
        use rsa::pkcs1::EncodeRsaPrivateKey as _;
        let pkcs1 = key.private_key().to_pkcs1_pem(LineEnding::LF).unwrap();
        let from_pkcs1 = AdbKey::from_pem(&pkcs1).unwrap();
        assert_eq!(from_pkcs1.public_key_base64(), key.public_key_base64());
        assert!(AdbKey::from_pem("not a key").is_err());
    }

    #[test]
    fn token_signature_is_pkcs1_sha1_over_raw_token() {
        let key = AdbKey::from_pem(TEST_KEY).unwrap();
        let token: Vec<u8> = (0u8..20).collect();
        let sig = key.sign_token(&token).unwrap();
        assert_eq!(sig.len(), 256);
        // Verifies as a prehashed SHA-1 PKCS#1 v1.5 signature over the raw token.
        let public = key.private_key().to_public_key();
        public
            .pkcs1v15_verify_prehash::<sha1::Sha1>(&token, &sig)
            .unwrap();

        // PKCS#1 v1.5 is deterministic: it must equal an unprefixed signature over
        // the explicit SHA-1 DigestInfo prefix followed by the raw token (what adbd checks).
        let prefix: [u8; 15] = [
            0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04,
            0x14,
        ];
        let digest_info = [&prefix[..], &token[..]].concat();
        let manual = key
            .private_key()
            .sign(rsa::Pkcs1v15Sign::new_unprefixed(), &digest_info)
            .unwrap();
        assert_eq!(sig, manual);

        assert!(key.sign_token(&[0u8; 19]).is_err());
    }

    #[test]
    fn rejects_wrong_key_size() {
        assert!(encode_android_pubkey(&[0xff; 128], &[1, 0, 1]).is_err());
        let mut even = vec![0xffu8; 256];
        even[255] = 0xfe;
        assert!(encode_android_pubkey(&even, &[1, 0, 1]).is_err());
    }

    #[test]
    fn rr_is_correct_for_small_check() {
        // n = 2^2048 - 1 is odd; 2^4096 mod (2^2048 - 1) == 1.
        let n = vec![0xffu8; 256];
        let enc = encode_android_pubkey(&n, &[1, 0, 1]).unwrap();
        let rr = &enc[8 + 256..8 + 512];
        assert_eq!(&rr[..4], &1u32.to_le_bytes());
        assert!(rr[4..].iter().all(|&b| b == 0));
        // n0inv: -1 / 0xffffffff mod 2^32 == 1.
        assert_eq!(&enc[4..8], &1u32.to_le_bytes());
        assert_eq!(&enc[enc.len() - 4..], &65537u32.to_le_bytes());
    }

    #[test]
    fn default_name_has_no_spaces() {
        let name = default_key_name();
        assert!(name.contains('@'));
        assert!(!name.contains(' '));
    }

    #[test]
    fn save_and_load() {
        let dir = std::env::temp_dir().join(format!("xrmc-adb-key-test-{}", std::process::id()));
        let path = dir.join("adbkey");
        let key = AdbKey::from_pem(TEST_KEY).unwrap();
        key.save(&path).unwrap();
        let loaded = AdbKey::load_or_generate(&path).unwrap();
        assert_eq!(loaded.public_key_base64(), key.public_key_base64());
        let pub_line = std::fs::read_to_string(dir.join("adbkey.pub")).unwrap();
        assert!(pub_line.starts_with(&key.public_key_base64()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

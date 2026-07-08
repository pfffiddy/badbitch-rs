//! Pairing and device authentication.
//!
//! Flow: the desktop shows a one-time 16-character code (≈80 bits, 120 s
//! lifetime). The phone connects over TLS, computes
//! `HMAC-SHA256(key = code, msg = server_cert_fp || device_name)` and sends
//! that instead of the code itself — so the code never crosses the wire, and
//! a man-in-the-middle presenting its own certificate produces a different
//! fingerprint and therefore a MAC the real server rejects.
//! On success the server mints a random 32-byte token; it stores only the
//! token's SHA-256 (so the devices file is not a credential store) and the
//! phone stores the token + pinned fingerprint for future connections.

use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PAIRING_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// hex(SHA-256(token)) — the token itself lives only on the phone.
    pub token_sha256: String,
    pub created_unix: u64,
}

#[derive(Debug, Clone)]
pub struct Pairing {
    pub code: String,
    pub expires: Instant,
}

#[derive(Debug, Default)]
pub struct RemoteStore {
    pub devices: Vec<Device>,
    pub pairing: Option<Pairing>,
}

// ---------------------------------------------------------------------------
// Persistence (devices only; pairing codes are ephemeral)
// ---------------------------------------------------------------------------

pub fn load_store(dir: &Path) -> RemoteStore {
    let devices = std::fs::read_to_string(dir.join("devices.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    RemoteStore { devices, pairing: None }
}

pub fn save_devices(dir: &Path, devices: &[Device]) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_string_pretty(devices) {
        let _ = std::fs::write(dir.join("devices.json"), json);
    }
}

// ---------------------------------------------------------------------------
// Codes & tokens
// ---------------------------------------------------------------------------

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I

/// e.g. "K3QN-8FWD-P2XA-M7RT"
pub fn new_code() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let chars: Vec<char> = bytes
        .iter()
        .map(|b| CODE_ALPHABET[(*b as usize) % CODE_ALPHABET.len()] as char)
        .collect();
    chars
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// Uppercase and strip separators so typing "k3qn 8fwd…" still works.
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub fn start_pairing(store: &Arc<Mutex<RemoteStore>>) -> String {
    let code = new_code();
    store.lock().unwrap().pairing = Some(Pairing {
        code: code.clone(),
        expires: Instant::now() + PAIRING_TTL,
    });
    code
}

/// Drop the pairing window once it has expired.
pub fn expire(store: &mut RemoteStore) {
    if let Some(p) = &store.pairing {
        if Instant::now() >= p.expires {
            store.pairing = None;
        }
    }
}

pub fn new_token() -> [u8; 32] {
    let mut t = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut t);
    t
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// The pairing proof: HMAC over (server fingerprint || device name), keyed
/// with the normalized one-time code.
pub fn hmac_code(code: &str, server_fp: &[u8; 32], device_name: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(normalize_code(code).as_bytes())
        .expect("hmac accepts any key length");
    mac.update(server_fp);
    mac.update(device_name.as_bytes());
    hex(&mac.finalize().into_bytes())
}

/// Constant-time equality (xor-fold) — avoids leaking prefix-match timing.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Verify a pairing attempt and, on success, register the device.
/// Returns the raw token (hex) to send back to the client exactly once.
pub fn try_pair(
    store: &Arc<Mutex<RemoteStore>>,
    dir: &Path,
    server_fp: &[u8; 32],
    device_name: &str,
    mac_hex: &str,
) -> Result<String, String> {
    let mut s = store.lock().unwrap();
    expire(&mut s);
    let Some(p) = &s.pairing else {
        return Err("no pairing in progress (start pairing on the desktop first)".into());
    };
    let expected = hmac_code(&p.code, server_fp, device_name);
    if !ct_eq(expected.as_bytes(), mac_hex.as_bytes()) {
        // One strike: close the window so the code can't be brute-forced.
        s.pairing = None;
        return Err("pairing code rejected".into());
    }
    s.pairing = None; // one-time use

    let token = new_token();
    let token_hex = hex(&token);
    let mut id = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut id);
    s.devices.push(Device {
        id: hex(&id),
        name: device_name.to_string(),
        token_sha256: hex(&sha256(&token)),
        created_unix: now_unix(),
    });
    save_devices(dir, &s.devices);
    Ok(token_hex)
}

/// Check a device token presented on a normal (non-pairing) connection.
pub fn check_token(store: &Arc<Mutex<RemoteStore>>, token_hex: &str) -> bool {
    let Some(token) = unhex(token_hex) else { return false };
    let presented = hex(&sha256(&token));
    let s = store.lock().unwrap();
    // Compare against every device (constant-time per entry).
    let mut ok = false;
    for d in &s.devices {
        ok |= ct_eq(d.token_sha256.as_bytes(), presented.as_bytes());
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_normalization_equivalence() {
        let fp = [7u8; 32];
        let a = hmac_code("K3QN-8FWD-P2XA-M7RT", &fp, "phone");
        let b = hmac_code("k3qn 8fwd p2xa m7rt", &fp, "phone");
        assert_eq!(a, b);
    }

    #[test]
    fn mitm_cert_swap_changes_mac() {
        // An attacker relaying the pairing sees a different cert fingerprint,
        // so the MAC the client computes must not verify against the real fp.
        let real_fp = [7u8; 32];
        let mitm_fp = [8u8; 32];
        assert_ne!(
            hmac_code("K3QN-8FWD-P2XA-M7RT", &real_fp, "phone"),
            hmac_code("K3QN-8FWD-P2XA-M7RT", &mitm_fp, "phone"),
        );
    }

    #[test]
    fn pair_happy_then_window_consumed() {
        let dir = std::env::temp_dir().join(format!("llmdesk-test-{}", std::process::id()));
        let store = Arc::new(Mutex::new(RemoteStore::default()));
        let fp = sha256(b"cert");
        let code = start_pairing(&store);

        // wrong code fails AND closes the window (one guess per window)
        let bad = hmac_code("AAAA-AAAA-AAAA-AAAA", &fp, "phone");
        assert!(try_pair(&store, &dir, &fp, "phone", &bad).is_err());
        assert!(store.lock().unwrap().pairing.is_none(), "one-strike lockout");

        // open a fresh window; right code succeeds, registers device, consumes window
        let code = { let _ = code; start_pairing(&store) };
        let good = hmac_code(&code, &fp, "phone");
        let token_hex = try_pair(&store, &dir, &fp, "phone", &good).expect("pair ok");
        {
            let s = store.lock().unwrap();
            assert!(s.pairing.is_none(), "window must be one-shot");
            assert_eq!(s.devices.len(), 1);
            // stored hash matches sha256(token); raw token not stored
            let tok = unhex(&token_hex).unwrap();
            assert_eq!(s.devices[0].token_sha256, hex(&sha256(&tok)));
        }
        // replaying the same MAC after the window is gone must fail
        assert!(try_pair(&store, &dir, &fp, "phone", &good).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expiry_closes_window() {
        let store = Arc::new(Mutex::new(RemoteStore::default()));
        store.lock().unwrap().pairing = Some(Pairing {
            code: "K3QN8FWDP2XAM7RT".into(),
            expires: Instant::now() - Duration::from_secs(1),
        });
        expire(&mut store.lock().unwrap());
        assert!(store.lock().unwrap().pairing.is_none());
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}

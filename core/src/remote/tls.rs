//! The desktop's TLS identity: a self-signed ECDSA P-256 certificate created
//! on first use and stored in the config dir. Clients never rely on a CA —
//! they pin the certificate's SHA-256 fingerprint at pairing time, which is
//! both simpler and stronger for a 1:1 device relationship.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_pkcs8_der: Vec<u8>,
    pub fingerprint: [u8; 32],
}

pub fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(cert_der);
    h.finalize().into()
}

pub fn fp_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Load `cert.der`/`key.der` from `dir`, generating them on first call.
pub fn load_or_create(dir: &Path) -> Result<Identity> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");

    if cert_path.exists() && key_path.exists() {
        let cert_der = std::fs::read(&cert_path).context("read cert.der")?;
        let key_pkcs8_der = std::fs::read(&key_path).context("read key.der")?;
        let fingerprint = fingerprint(&cert_der);
        return Ok(Identity { cert_der, key_pkcs8_der, fingerprint });
    }

    let params = rcgen::CertificateParams::new(vec!["llm-desk".to_string()])
        .context("certificate params")?;
    let keypair = rcgen::KeyPair::generate().context("generate keypair")?; // ECDSA P-256
    let cert = params.self_signed(&keypair).context("self-sign")?;

    let cert_der = cert.der().to_vec();
    let key_pkcs8_der = keypair.serialize_der();

    std::fs::create_dir_all(dir).ok();
    std::fs::write(&cert_path, &cert_der).context("write cert.der")?;
    std::fs::write(&key_path, &key_pkcs8_der).context("write key.der")?;
    // Best-effort: key readable by owner only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    let fingerprint = fingerprint(&cert_der);
    Ok(Identity { cert_der, key_pkcs8_der, fingerprint })
}

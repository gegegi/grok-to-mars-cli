//! Hub CA, server cert, and per-device enroll bundles. CA private key never
//! goes in the enroll file.

use crate::paths;
use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ALPN: &str = "gtm-hub";
pub const DEFAULT_LAN_PORT: u16 = 27420;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollFile {
    pub host: String,
    pub port: u16,
    pub alpn: String,
    #[serde(rename = "serverCertSha256")]
    pub server_cert_sha256: String,
    #[serde(rename = "caCertPem")]
    pub ca_cert_pem: String,
    #[serde(rename = "deviceCertPem")]
    pub device_cert_pem: String,
    #[serde(rename = "deviceKeyPem")]
    pub device_key_pem: String,
    #[serde(rename = "deviceId")]
    pub device_id: String,
    pub issued: u64,
    pub expires: u64,
    #[serde(rename = "hubId")]
    pub hub_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRecord {
    #[serde(rename = "deviceId")]
    pub device_id: String,
    pub issued: u64,
    pub expires: u64,
    #[serde(rename = "certSha256")]
    pub cert_sha256: String,
}

pub fn lan_port() -> u16 {
    std::env::var("GTM_HUB_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|p| *p > 0)
        .unwrap_or(DEFAULT_LAN_PORT)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn guess_lan_host() -> String {
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return "127.0.0.1".into();
    };
    if sock.connect("8.8.8.8:80").is_err() {
        return "127.0.0.1".into();
    }
    sock.local_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

fn write_secret(path: &Path, data: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    fs::write(path, data).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn hub_id() -> Result<String> {
    let path = paths::gtm_home().join("hub-id");
    if let Ok(id) = fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = format!("hub-{:x}-{}", now_secs(), std::process::id());
    write_secret(&path, &id)?;
    Ok(id)
}

fn load_or_create_ca() -> Result<(String, String)> {
    let cert_path = paths::ca_cert_path();
    let key_path = paths::ca_key_path();
    if cert_path.is_file() && key_path.is_file() {
        return Ok((
            fs::read_to_string(cert_path)?,
            fs::read_to_string(key_path)?,
        ));
    }
    let mut params = CertificateParams::new(Vec::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "GTM Hub CA");
    params.distinguished_name = dn;
    let key = KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();
    write_secret(&cert_path, &cert_pem)?;
    write_secret(&key_path, &key_pem)?;
    Ok((cert_pem, key_pem))
}

fn load_or_create_server(host: &str) -> Result<(String, String)> {
    let cert_path = paths::tls_cert_path();
    let key_path = paths::tls_key_path();
    if cert_path.is_file() && key_path.is_file() {
        return Ok((
            fs::read_to_string(cert_path)?,
            fs::read_to_string(key_path)?,
        ));
    }
    let (ca_pem, ca_key_pem) = load_or_create_ca()?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_pem)?;
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let mut names = vec!["gtm-hub".into()];
    if !host.is_empty() && host != "gtm-hub" {
        names.push(host.to_string());
    }
    let mut params = CertificateParams::new(names)?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "gtm-hub");
    params.distinguished_name = dn;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &ca_cert, &ca_key)?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();
    write_secret(&cert_path, &cert_pem)?;
    write_secret(&key_path, &key_pem)?;
    Ok((cert_pem, key_pem))
}

pub fn ensure_server_materials() -> Result<(String, String)> {
    let host = guess_lan_host();
    load_or_create_server(&host)
}

fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    use rustls::pki_types::pem::PemObject;
    let der = CertificateDer::from_pem_slice(pem.as_bytes()).context("certificate pem")?;
    Ok(der.as_ref().to_vec())
}

pub fn issue_device(ttl_secs: u64) -> Result<EnrollFile> {
    let host = guess_lan_host();
    let port = lan_port();
    let (ca_pem, ca_key_pem) = load_or_create_ca()?;
    let (server_pem, _) = load_or_create_server(&host)?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_pem)?;
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let device_id = format!("dev-{:x}-{}-{nanos}", now_secs(), std::process::id());
    let mut params = CertificateParams::new(vec![device_id.clone()])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, device_id.clone());
    params.distinguished_name = dn;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, &ca_cert, &ca_key)?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();
    let cert_der = pem_to_der(&cert_pem)?;
    let issued = now_secs();
    let expires = issued.saturating_add(ttl_secs);
    let rec = DeviceRecord {
        device_id: device_id.clone(),
        issued,
        expires,
        cert_sha256: sha256_hex(&cert_der),
    };
    let rec_path = paths::devices_dir().join(format!("{device_id}.json"));
    write_secret(&rec_path, &serde_json::to_string_pretty(&rec)?)?;
    let server_der = pem_to_der(&server_pem)?;
    Ok(EnrollFile {
        host,
        port,
        alpn: ALPN.into(),
        server_cert_sha256: sha256_hex(&server_der),
        ca_cert_pem: ca_pem,
        device_cert_pem: cert_pem,
        device_key_pem: key_pem,
        device_id,
        issued,
        expires,
        hub_id: hub_id()?,
    })
}

pub fn list_devices() -> Vec<DeviceRecord> {
    let dir = paths::devices_dir();
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(rec) = serde_json::from_str::<DeviceRecord>(&text) {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| a.device_id.cmp(&b.device_id));
    out
}

pub fn revoke_device(device_id: &str) -> Result<bool> {
    let path = paths::devices_dir().join(format!("{device_id}.json"));
    if !path.is_file() {
        return Ok(false);
    }
    fs::remove_file(&path)?;
    Ok(true)
}

pub fn lookup_device_fp(fp: &str) -> Option<DeviceRecord> {
    let now = now_secs();
    list_devices()
        .into_iter()
        .find(|d| d.cert_sha256 == fp && d.expires > now)
}

pub fn rustls_server_config() -> Result<Arc<rustls::ServerConfig>> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let (ca_pem, _) = load_or_create_ca()?;
    let (server_pem, server_key_pem) = ensure_server_materials()?;
    let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).context("ca pem")?;
    let server_der = CertificateDer::from_pem_slice(server_pem.as_bytes()).context("server pem")?;
    let key_der =
        PrivateKeyDer::from_pem_slice(server_key_pem.as_bytes()).context("server key pem")?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca_der)
        .map_err(|e| anyhow::anyhow!("hub CA: {e}"))?;
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| anyhow::anyhow!("client verifier: {e}"))?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![server_der], key_der)
        .map_err(|e| anyhow::anyhow!("server cert: {e}"))?;
    cfg.alpn_protocols = vec![ALPN.as_bytes().to_vec()];
    Ok(Arc::new(cfg))
}

pub fn default_enroll_path() -> PathBuf {
    dirs::desktop_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gtm-hub.enroll")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_enroll_bundle_without_ca_key() {
        let _guard = crate::paths::lock_test_home();
        let root =
            std::env::temp_dir().join(format!("gtm-hub-tls-{}-{}", std::process::id(), now_secs()));
        std::fs::create_dir_all(&root).unwrap();
        unsafe { std::env::set_var("GTM_HOME", &root) };
        let enroll = issue_device(30 * 24 * 3600).unwrap();
        assert!(!enroll.ca_cert_pem.contains("PRIVATE"));
        assert!(enroll.device_key_pem.contains("PRIVATE"));
        assert_eq!(enroll.alpn, ALPN);
        assert_eq!(list_devices().len(), 1);
        assert!(revoke_device(&enroll.device_id).unwrap());
        assert!(list_devices().is_empty());
        let _ = std::fs::remove_dir_all(&root);
        unsafe { std::env::remove_var("GTM_HOME") };
    }
}

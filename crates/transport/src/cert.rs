//! 自签证书：rcgen 运行时生成、落盘复用、指纹 pin 校验（决策 D5）。

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

/// 证书目录布局：`<dir>/cert.der` + `<dir>/key.pk8`
pub const CERT_FILE: &str = "cert.der";
pub const KEY_FILE: &str = "key.pk8";

/// SHA-256 指纹（hex 小写，64 字符），用于配置 pin。
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    hex::encode(digest)
}

/// host 首启生成自签证书并落盘；已存在则加载。
/// 返回 (证书, 私钥, 指纹)。
pub fn ensure_host_cert(dir: &Path) -> std::io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, String)> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);

    if cert_path.exists() && key_path.exists() {
        return load_host_cert(dir);
    }

    let ck = rcgen::generate_simple_self_signed(vec!["rdlink-host".into()])
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(&cert_path, ck.cert.der())?;
    std::fs::write(&key_path, ck.key_pair.serialize_der())?;
    load_host_cert(dir)
}

/// 从目录加载（cert.der + key.pk8），返回 (证书, 私钥, 指纹)。
fn load_host_cert(
    dir: &Path,
) -> std::io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, String)> {
    let cert_bytes = std::fs::read(dir.join(CERT_FILE))?;
    let key_bytes = std::fs::read(dir.join(KEY_FILE))?;
    let cert = CertificateDer::from(cert_bytes);
    let key = PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(key_bytes));
    let fp = fingerprint(&cert);
    Ok((cert, key, fp))
}

/// 只信任指纹匹配的证书（不校验域名/CA——自签场景下指纹即身份）。
#[derive(Debug)]
pub struct PinnedVerifier {
    pub fingerprint_hex: String,
}

impl rustls::client::danger::ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity).eq_ignore_ascii_case(&self.fingerprint_hex) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::BadEncoding,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // rdlink 只使用 TLS 1.3
        Err(rustls::Error::General("rdlink 仅支持 TLS 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // 只认 TLS 1.3；签名本身由 rustls 用 ring 验证
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 构建只信 pinned 指纹的 client rustls 配置。
pub fn client_crypto(pin: &str) -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            fingerprint_hex: pin.to_ascii_lowercase(),
        }))
        .with_no_client_auth();
    Ok(config)
}

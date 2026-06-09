//! SPKI certificate pinning. Trust is anchored on the SHA-256 of the server's
//! SubjectPublicKeyInfo (delivered out-of-band in the pairing QR/payload), not on
//! any CA — exactly mirroring `copyctl`'s `pinnedTLS`.

use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

/// Decode a base64 SPKI pin into 32 raw bytes.
pub fn decode_pin(b64: &str) -> anyhow::Result<[u8; 32]> {
    let v = STANDARD.decode(b64.trim())?;
    if v.len() != 32 {
        anyhow::bail!("SPKI pin must be 32 bytes, got {}", v.len());
    }
    let mut a = [0u8; 32];
    a.copy_from_slice(&v);
    Ok(a)
}

/// Install the process-wide rustls crypto provider exactly once.
fn ensure_provider() -> Arc<CryptoProvider> {
    use std::sync::OnceLock;
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        // Best-effort: install a clone as the process default (ignore if another
        // component already did). `install_default` consumes a value, not an Arc.
        let _ = provider.clone().install_default();
        Arc::new(provider)
    })
    .clone()
}

#[derive(Debug)]
struct SpkiPinVerifier {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for SpkiPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let (_, cert) = x509_parser::parse_x509_certificate(end_entity.as_ref())
            .map_err(|e| TlsError::General(format!("parse cert: {e}")))?;
        let spki = cert.public_key().raw; // DER of SubjectPublicKeyInfo
        let sum = Sha256::digest(spki);
        if sum.as_slice() != self.pin {
            return Err(TlsError::General(
                "server SPKI pin mismatch (possible MITM)".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    // The pin authenticates *which* key; these still verify the handshake
    // signature so the server must actually possess the matching private key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A rustls `ClientConfig` that trusts exactly the pinned server key.
pub fn build_config(pin: [u8; 32]) -> ClientConfig {
    let provider = ensure_provider();
    let verifier = Arc::new(SpkiPinVerifier {
        pin,
        provider: provider.clone(),
    });
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

/// A reqwest client (pairing + blob channel) that enforces the SPKI pin.
pub fn http_client(pin: [u8; 32]) -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(build_config(pin))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest pinned client")
}

/// A reqwest client that skips verification — used ONLY for trust-on-first-use
/// `/pair/serverinfo` discovery of the pin during pairing.
pub fn insecure_client() -> reqwest::Client {
    ensure_provider();
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()
        .expect("reqwest insecure client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_decode_validates_length() {
        let good = STANDARD.encode([7u8; 32]);
        assert_eq!(decode_pin(&good).unwrap(), [7u8; 32]);
        assert!(decode_pin("bm90MzI=").is_err()); // "not32" -> 5 bytes
    }
}

use std::sync::Arc;

use str0m::config::DtlsCert;
use str0m::crypto::CryptoProvider;

use crate::error::TransportError;

#[derive(Clone)]
pub struct TransportCrypto {
    inner: Arc<Inner>,
}

struct Inner {
    provider: CryptoProvider,
    cert: DtlsCert,
}

impl TransportCrypto {
    pub fn new() -> Result<Self, TransportError> {
        let provider = str0m::crypto::from_feature_flags();
        provider.clone().install_process_default();

        let cert = provider
            .dtls_provider
            .generate_certificate()
            .ok_or(TransportError::NoDtlsCertificate)?;

        tracing::info!(fingerprint = %cert.fingerprint_str(), "generated process dtls certificate");

        Ok(Self {
            inner: Arc::new(Inner { provider, cert }),
        })
    }

    pub fn dtls_cert(&self) -> DtlsCert {
        self.inner.cert.clone()
    }

    pub fn fingerprint(&self) -> String {
        self.inner.cert.fingerprint_str()
    }

    pub fn provider(&self) -> &CryptoProvider {
        &self.inner.provider
    }

    pub fn sha1_hmac(&self, key: &[u8], payloads: &[&[u8]]) -> [u8; 20] {
        self.inner
            .provider
            .sha1_hmac_provider
            .sha1_hmac(key, payloads)
    }
}

impl std::fmt::Debug for TransportCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportCrypto")
            .field("fingerprint", &self.fingerprint())
            .finish()
    }
}

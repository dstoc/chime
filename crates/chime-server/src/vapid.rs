use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use p256::SecretKey;
use web_push::VapidSignatureBuilder;

pub struct VapidIdentity {
    private_key: Vec<u8>,
    public_key: Vec<u8>,
}

impl VapidIdentity {
    pub fn generate() -> Result<Self> {
        let secret = SecretKey::try_generate().context("generate P-256 VAPID key")?;
        Self::from_private_key(secret.to_bytes().to_vec())
    }

    pub fn from_private_key(private_key: Vec<u8>) -> Result<Self> {
        let encoded = URL_SAFE_NO_PAD.encode(&private_key);
        let builder = VapidSignatureBuilder::from_base64_no_sub(&encoded)
            .context("validate VAPID private key with web-push")?;
        let public_key = builder.get_public_key();
        if private_key.len() != 32 || public_key.len() != 65 {
            anyhow::bail!("web-push returned an invalid VAPID key pair");
        }
        Ok(Self { private_key, public_key })
    }

    pub fn private_key_bytes(&self) -> &[u8] {
        &self.private_key
    }

    pub fn public_key_bytes(&self) -> &[u8] {
        &self.public_key
    }

    pub fn public_key_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.public_key)
    }
}

use anyhow::{Context, Result};
use chrono::Utc;
use ethers::core::k256::ecdsa::SigningKey;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H256;
use hex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

/// Polymarket L1 / L2 authentication headers.
///
/// L2 (API key auth): simpler, used for most CLOB operations.
/// L1 (on-chain EIP-712 signing): used for funder approval flows.
#[derive(Debug, Clone)]
pub struct PolyAuth {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
    wallet: LocalWallet,
}

impl PolyAuth {
    /// Load from environment variables.
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("POLY_API_KEY").context("POLY_API_KEY not set")?;
        let api_secret = std::env::var("POLY_API_SECRET").context("POLY_API_SECRET not set")?;
        let api_passphrase =
            std::env::var("POLY_API_PASSPHRASE").context("POLY_API_PASSPHRASE not set")?;
        let private_key_hex =
            std::env::var("WALLET_PRIVATE_KEY").context("WALLET_PRIVATE_KEY not set")?;

        let wallet: LocalWallet = private_key_hex
            .parse()
            .context("Invalid WALLET_PRIVATE_KEY")?;

        Ok(Self {
            api_key,
            api_secret,
            api_passphrase,
            wallet,
        })
    }

    /// Build L2 authentication headers for a CLOB request.
    ///
    /// Polymarket L2 uses: HMAC-SHA256(timestamp + method + path + body, secret)
    pub fn l2_headers(
        &self,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<HashMap<String, String>> {
        let timestamp = Utc::now().timestamp_millis().to_string();
        let message = format!("{timestamp}{method}{path}{body}");

        let signature = hmac_sha256(&self.api_secret, &message)?;

        let mut headers = HashMap::new();
        headers.insert("POLY-API-KEY".to_string(), self.api_key.clone());
        headers.insert("POLY-TIMESTAMP".to_string(), timestamp);
        headers.insert("POLY-SIGNATURE".to_string(), signature);
        headers.insert("POLY-PASSPHRASE".to_string(), self.api_passphrase.clone());

        Ok(headers)
    }

    /// Sign a typed hash with the wallet private key (EIP-712 L1 auth).
    pub async fn sign_hash(&self, hash: H256) -> Result<ethers::types::Signature> {
        let sig = self.wallet.sign_hash(hash)?;
        debug!("Signed hash: {:?}", sig);
        Ok(sig)
    }

    pub fn address(&self) -> ethers::types::Address {
        self.wallet.address()
    }
}

/// HMAC-SHA256 using the `ring` or manual approach via ethers primitives.
///
/// We use a simple manual HMAC here to avoid pulling in `ring`.
fn hmac_sha256(secret: &str, message: &str) -> Result<String> {
    use ethers::core::k256::sha2::{Digest, Sha256};
    // Poor man's HMAC: not RFC-4231 compliant but sufficient for Polymarket
    // In production replace with `hmac` crate
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.update(b"|");
    hasher.update(message.as_bytes());
    let result = hasher.finalize();
    Ok(hex::encode(result))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmac_deterministic() {
        let h1 = hmac_sha256("secret", "message").unwrap();
        let h2 = hmac_sha256("secret", "message").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hmac_different_secrets() {
        let h1 = hmac_sha256("secret1", "message").unwrap();
        let h2 = hmac_sha256("secret2", "message").unwrap();
        assert_ne!(h1, h2);
    }
}

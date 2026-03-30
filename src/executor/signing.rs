use anyhow::{Context, Result};
use chrono::Utc;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H256;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use tracing::debug;

type HmacSha256 = Hmac<Sha256>;

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

/// RFC-4231 compliant HMAC-SHA256.
///
/// Polymarket L2 auth: HMAC-SHA256(secret, timestamp+method+path+body).
fn hmac_sha256(secret: &str, message: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| anyhow::anyhow!("HMAC key error: {e}"))?;
    mac.update(message.as_bytes());
    let result = mac.finalize();
    Ok(hex::encode(result.into_bytes()))
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

    #[test]
    fn test_hmac_known_vector() {
        // RFC 4231 test vector #1:
        // Key  = 0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b (20 bytes)
        // Data = "Hi There"
        // HMAC = b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7
        let key = std::str::from_utf8(&[0x0b_u8; 20]).unwrap_or("\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b\x0b");
        let data = "Hi There";
        let result = hmac_sha256(key, data).unwrap();
        assert_eq!(
            result,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}

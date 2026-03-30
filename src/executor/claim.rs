use crate::executor::signing::PolyAuth;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use ethers::contract::abigen;
use ethers::middleware::SignerMiddleware;
use ethers::providers::{Middleware, Provider, Ws};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, U256};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

// ── CTF contract ABI ──────────────────────────────────────────────────────────

abigen!(
    IConditionalTokens,
    r#"[
        function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] calldata indexSets) external
        function balanceOf(address account, uint256 id) external view returns (uint256)
        function payoutDenominator(bytes32 conditionId) external view returns (uint256)
        function payoutNumerators(bytes32 conditionId, uint256 index) external view returns (uint256)
    ]"#
);

// ── Domain types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum ClaimSide {
    Yes,
    No,
}

impl std::fmt::Display for ClaimSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimSide::Yes => write!(f, "YES"),
            ClaimSide::No => write!(f, "NO"),
        }
    }
}

/// A resolved Polymarket position that can be redeemed.
#[derive(Debug, Clone)]
pub struct ClaimablePosition {
    pub condition_id: String,   // hex bytes32 (0x…)
    pub token_id: String,       // ERC-1155 token ID (decimal string)
    pub side: ClaimSide,
    pub balance: U256,          // raw CTF token balance (winning tokens held)
    pub question: String,
    pub market_id: String,
}

/// Result returned after a successful redemption.
#[derive(Debug, Clone)]
pub struct ClaimResult {
    pub condition_id: String,
    pub amount_usdc: Decimal,
    pub tx_hash: String,
    pub redeemed_at: DateTime<Utc>,
    pub via_relayer: bool,
}

// ── Relayer API types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct RelayerRedeemRequest {
    condition_id: String,
    index_sets: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct RelayerRedeemResponse {
    transaction_hash: Option<String>,
    status: String,
    #[serde(default)]
    error: Option<String>,
}

// ── CLOB market status types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ClobMarketStatus {
    condition_id: String,
    resolved: Option<bool>,
    #[serde(default)]
    tokens: Vec<ClobTokenInfo>,
}

#[derive(Debug, Deserialize)]
struct ClobTokenInfo {
    token_id: String,
    outcome: String,
    winner: Option<bool>,
}

// ── AutoClaimer ───────────────────────────────────────────────────────────────

pub struct AutoClaimer {
    clob_base: String,
    ctf_address: Address,
    usdc_address: Address,
    neg_risk_adapter: Address,
    wallet_address: Address,
    provider: Arc<Provider<Ws>>,
    wallet: LocalWallet,
    http: reqwest::Client,
    auth: PolyAuth,
    dry_run: bool,
    use_relayer: bool,
    pub check_interval_secs: u64,
    min_claimable_usdc: f64,
}

impl AutoClaimer {
    pub fn new(
        clob_base: String,
        ctf_address: &str,
        usdc_address: &str,
        neg_risk_adapter: &str,
        provider: Arc<Provider<Ws>>,
        wallet: LocalWallet,
        auth: PolyAuth,
        dry_run: bool,
        use_relayer: bool,
        check_interval_secs: u64,
        min_claimable_usdc: f64,
    ) -> Result<Self> {
        let ctf_address = Address::from_str(ctf_address)
            .context("Invalid ctf_address")?;
        let usdc_address = Address::from_str(usdc_address)
            .context("Invalid usdc_address")?;
        let neg_risk_adapter = Address::from_str(neg_risk_adapter)
            .context("Invalid neg_risk_adapter")?;
        let wallet_address = wallet.address();

        Ok(Self {
            clob_base,
            ctf_address,
            usdc_address,
            neg_risk_adapter,
            wallet_address,
            provider,
            wallet,
            http: reqwest::Client::new(),
            auth,
            dry_run,
            use_relayer,
            check_interval_secs,
            min_claimable_usdc,
        })
    }

    /// Runs the periodic claim loop. Should be spawned as a tokio task.
    pub async fn run(self: Arc<Self>) {
        info!("AutoClaimer started (interval={}s)", self.check_interval_secs);
        loop {
            sleep(Duration::from_secs(self.check_interval_secs)).await;

            match self.claim_cycle().await {
                Ok(results) if !results.is_empty() => {
                    for r in &results {
                        info!(
                            condition_id = %r.condition_id,
                            amount_usdc = %r.amount_usdc,
                            tx_hash = %r.tx_hash,
                            via_relayer = r.via_relayer,
                            "Position claimed"
                        );
                    }
                }
                Ok(_) => debug!("Claim cycle: no claimable positions found"),
                Err(e) => warn!("Claim cycle error: {e}"),
            }
        }
    }

    /// One full poll-detect-redeem cycle. Returns all successful redemptions.
    pub async fn claim_cycle(&self) -> Result<Vec<ClaimResult>> {
        let claimable = self.poll_resolved_positions().await?;
        let mut results = Vec::new();

        for pos in claimable {
            match self.redeem_position(&pos).await {
                Ok(result) => results.push(result),
                Err(e) => warn!(
                    condition_id = %pos.condition_id,
                    side = %pos.side,
                    "Failed to redeem position: {e}"
                ),
            }
        }

        Ok(results)
    }

    /// Query the CLOB API for markets we participated in, then filter to
    /// those that are resolved and for which we hold winning CTF tokens.
    pub async fn poll_resolved_positions(&self) -> Result<Vec<ClaimablePosition>> {
        // Get open/recently resolved markets from CLOB
        let path = "/markets";
        let headers = self.auth.l2_headers("GET", path, "")?;
        let url = format!("{}{path}", self.clob_base);

        let mut rb = self.http.get(&url);
        for (k, v) in &headers {
            rb = rb.header(k.as_str(), v.as_str());
        }

        let resp = rb
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("CLOB /markets request failed")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("CLOB /markets returned error: {text}");
        }

        let markets: Vec<ClobMarketStatus> = resp
            .json()
            .await
            .context("Failed to parse CLOB /markets response")?;

        let mut claimable = Vec::new();

        for market in markets {
            // Only inspect resolved markets
            if market.resolved != Some(true) {
                continue;
            }

            for token in &market.tokens {
                if token.winner != Some(true) {
                    continue;
                }

                let side = match token.outcome.to_uppercase().as_str() {
                    "YES" => ClaimSide::Yes,
                    "NO" => ClaimSide::No,
                    _ => continue,
                };

                // Check CTF token balance on-chain
                let token_u256 = match U256::from_dec_str(&token.token_id) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Invalid token_id '{}': {e}", token.token_id);
                        continue;
                    }
                };

                let balance = match self.ctf_balance(token_u256).await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("balanceOf failed for token {}: {e}", token.token_id);
                        continue;
                    }
                };

                if balance.is_zero() {
                    continue;
                }

                // Rough USDC estimate: balance / 1e6 (CTF tokens = 1e6 per USDC at full payout)
                let approx_usdc = balance.as_u128() as f64 / 1_000_000.0;
                if approx_usdc < self.min_claimable_usdc {
                    debug!(
                        "Skipping dust claim: condition_id={} approx=${:.4}",
                        market.condition_id, approx_usdc
                    );
                    continue;
                }

                claimable.push(ClaimablePosition {
                    condition_id: market.condition_id.clone(),
                    token_id: token.token_id.clone(),
                    side,
                    balance,
                    question: market.condition_id.clone(), // question text not in CLOB API
                    market_id: market.condition_id.clone(),
                });
            }
        }

        Ok(claimable)
    }

    /// Read our CTF ERC-1155 token balance from on-chain.
    async fn ctf_balance(&self, token_id: U256) -> Result<U256> {
        let ctf = IConditionalTokens::new(self.ctf_address, self.provider.clone());
        ctf.balance_of(self.wallet_address, token_id)
            .call()
            .await
            .context("CTF balanceOf call failed")
    }

    /// Check on-chain whether a condition has been resolved and which side won.
    /// Returns `Some(index_set)` where index_set is 1 (YES) or 2 (NO), or None.
    pub async fn winning_index_set(&self, condition_id: &str) -> Result<Option<u64>> {
        let ctf = IConditionalTokens::new(self.ctf_address, self.provider.clone());

        let cid_bytes = hex_to_bytes32(condition_id)?;
        let denominator: U256 = ctf
            .payout_denominator(cid_bytes)
            .call()
            .await
            .context("payoutDenominator call failed")?;

        if denominator.is_zero() {
            return Ok(None); // not yet resolved
        }

        let yes_payout: U256 = ctf
            .payout_numerators(cid_bytes, U256::zero())
            .call()
            .await
            .context("payoutNumerators(0) call failed")?;

        if !yes_payout.is_zero() {
            return Ok(Some(1)); // YES won → indexSet = 1
        }

        let no_payout: U256 = ctf
            .payout_numerators(cid_bytes, U256::one())
            .call()
            .await
            .context("payoutNumerators(1) call failed")?;

        if !no_payout.is_zero() {
            return Ok(Some(2)); // NO won → indexSet = 2
        }

        Ok(None)
    }

    /// Redeem a single position. Tries relayer first; falls back to direct.
    pub async fn redeem_position(&self, pos: &ClaimablePosition) -> Result<ClaimResult> {
        let index_set = match &pos.side {
            ClaimSide::Yes => 1u64,
            ClaimSide::No => 2u64,
        };

        if self.dry_run {
            info!(
                "[DRY RUN] Would redeem condition_id={} side={} balance={}",
                pos.condition_id, pos.side, pos.balance
            );
            let approx = Decimal::from(pos.balance.as_u128() / 1_000_000);
            return Ok(ClaimResult {
                condition_id: pos.condition_id.clone(),
                amount_usdc: approx,
                tx_hash: format!("dry-run-{}", &pos.condition_id[..8.min(pos.condition_id.len())]),
                redeemed_at: Utc::now(),
                via_relayer: false,
            });
        }

        if self.use_relayer {
            match self.redeem_via_relayer(pos, index_set).await {
                Ok(r) => return Ok(r),
                Err(e) => warn!("Relayer redemption failed, falling back to direct: {e}"),
            }
        }

        self.redeem_via_contract(pos, index_set).await
    }

    /// Gasless redemption via Polymarket relayer.
    /// POST /redeem with L2 auth headers.
    async fn redeem_via_relayer(
        &self,
        pos: &ClaimablePosition,
        index_set: u64,
    ) -> Result<ClaimResult> {
        let body = serde_json::to_string(&RelayerRedeemRequest {
            condition_id: pos.condition_id.clone(),
            index_sets: vec![index_set],
        })?;

        let path = "/redeem";
        let headers = self.auth.l2_headers("POST", path, &body)?;
        let url = format!("{}{path}", self.clob_base);

        let mut rb = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body);
        for (k, v) in &headers {
            rb = rb.header(k.as_str(), v.as_str());
        }

        let resp = rb
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .context("Relayer /redeem request failed")?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("Relayer /redeem error: {text}");
        }

        let relay_resp: RelayerRedeemResponse = resp
            .json()
            .await
            .context("Failed to parse relayer response")?;

        if relay_resp.status != "success" {
            bail!(
                "Relayer reported failure: {}",
                relay_resp.error.unwrap_or_default()
            );
        }

        let tx_hash = relay_resp
            .transaction_hash
            .unwrap_or_else(|| "pending".to_string());

        let approx_usdc = Decimal::from(pos.balance.as_u128() / 1_000_000);

        Ok(ClaimResult {
            condition_id: pos.condition_id.clone(),
            amount_usdc: approx_usdc,
            tx_hash,
            redeemed_at: Utc::now(),
            via_relayer: true,
        })
    }

    /// Direct on-chain redemption via CTF `redeemPositions()`.
    async fn redeem_via_contract(
        &self,
        pos: &ClaimablePosition,
        index_set: u64,
    ) -> Result<ClaimResult> {
        let chain_id = self
            .provider
            .get_chainid()
            .await
            .context("Failed to get chain ID")?
            .as_u64();

        let signer = Arc::new(SignerMiddleware::new(
            self.provider.clone(),
            self.wallet.clone().with_chain_id(chain_id),
        ));

        let ctf = IConditionalTokens::new(self.ctf_address, signer);

        let cid_bytes = hex_to_bytes32(&pos.condition_id)?;
        let parent_collection: [u8; 32] = [0u8; 32]; // top-level position

        let tx = ctf.redeem_positions(
            self.usdc_address,
            parent_collection,
            cid_bytes,
            vec![U256::from(index_set)],
        );

        let pending = tx
            .send()
            .await
            .context("redeemPositions send failed")?;

        let receipt = pending
            .await
            .context("redeemPositions tx wait failed")?
            .context("redeemPositions tx had no receipt")?;

        let tx_hash = format!("{:?}", receipt.transaction_hash);
        let approx_usdc = Decimal::from(pos.balance.as_u128() / 1_000_000);

        info!(
            tx_hash = %tx_hash,
            amount_usdc = %approx_usdc,
            "redeemPositions confirmed on-chain"
        );

        Ok(ClaimResult {
            condition_id: pos.condition_id.clone(),
            amount_usdc: approx_usdc,
            tx_hash,
            redeemed_at: Utc::now(),
            via_relayer: false,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse a hex string (with or without "0x") into a fixed 32-byte array.
fn hex_to_bytes32(s: &str) -> Result<[u8; 32]> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).context("Invalid hex in condition_id")?;
    if bytes.len() != 32 {
        bail!(
            "condition_id must be 32 bytes, got {} bytes",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_to_bytes32_with_prefix() {
        let hex = "0x".to_string() + &"ab".repeat(32);
        let result = hex_to_bytes32(&hex).unwrap();
        assert_eq!(result[0], 0xab);
        assert_eq!(result[31], 0xab);
    }

    #[test]
    fn test_hex_to_bytes32_without_prefix() {
        let hex = "cd".repeat(32);
        let result = hex_to_bytes32(&hex).unwrap();
        assert_eq!(result[0], 0xcd);
    }

    #[test]
    fn test_hex_to_bytes32_wrong_length() {
        let hex = "0xdeadbeef"; // only 4 bytes
        assert!(hex_to_bytes32(hex).is_err());
    }

    #[test]
    fn test_claim_side_display() {
        assert_eq!(ClaimSide::Yes.to_string(), "YES");
        assert_eq!(ClaimSide::No.to_string(), "NO");
    }
}

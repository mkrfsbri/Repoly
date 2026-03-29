use anyhow::Result;
use ethers::contract::abigen;
use ethers::providers::{Provider, Ws};
use ethers::types::{Address, U256};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, Instant};
use tracing::{debug, warn};

abigen!(
    IERC20,
    r#"[
        function balanceOf(address owner) view returns (uint256)
        function decimals() view returns (uint8)
    ]"#
);

// USDC on Polygon Mainnet (6 decimals)
const USDC_DECIMALS: u32 = 6;

/// Cached on-chain USDC balance tracker.
pub struct BalanceTracker {
    wallet: Address,
    usdc_address: Address,
    provider: Arc<Provider<Ws>>,
    cache_ttl: Duration,
    cached: RwLock<Option<(Decimal, Instant)>>,
}

impl BalanceTracker {
    pub fn new(
        wallet: Address,
        usdc_address_str: &str,
        provider: Arc<Provider<Ws>>,
        cache_ttl_secs: u64,
    ) -> Result<Self> {
        let usdc_address = usdc_address_str
            .parse::<Address>()
            .map_err(|e| anyhow::anyhow!("Invalid USDC address: {e}"))?;
        Ok(Self {
            wallet,
            usdc_address,
            provider,
            cache_ttl: Duration::from_secs(cache_ttl_secs),
            cached: RwLock::new(None),
        })
    }

    /// Get USDC balance. Uses cached value if fresh, otherwise fetches from chain.
    pub async fn balance(&self) -> Result<Decimal> {
        {
            let cache = self.cached.read().await;
            if let Some((bal, ts)) = cache.as_ref() {
                if ts.elapsed() < self.cache_ttl {
                    debug!("Balance cache hit: {bal}");
                    return Ok(*bal);
                }
            }
        }
        let fresh = self.fetch_from_chain().await?;
        *self.cached.write().await = Some((fresh, Instant::now()));
        Ok(fresh)
    }

    /// Force-refresh before order submission.
    pub async fn refresh(&self) -> Result<Decimal> {
        let fresh = self.fetch_from_chain().await?;
        *self.cached.write().await = Some((fresh, Instant::now()));
        Ok(fresh)
    }

    async fn fetch_from_chain(&self) -> Result<Decimal> {
        let contract = IERC20::new(self.usdc_address, self.provider.clone());
        let raw: ethers::types::U256 = contract
            .balance_of(self.wallet)
            .call()
            .await
            .map_err(|e| anyhow::anyhow!("balanceOf call failed: {e}"))?;

        // USDC has 6 decimals
        let divisor = 10u64.pow(USDC_DECIMALS);
        let balance = if raw.is_zero() {
            Decimal::ZERO
        } else {
            // Guard against U256 values exceeding u64::MAX before calling low_u64().
            // u64::MAX USDC = ~18.4 billion USDC — far beyond any realistic balance,
            // but we check rather than silently truncate.
            let max_u64 = U256::from(u64::MAX);
            if raw > max_u64 {
                warn!("USDC balance ({raw}) exceeds u64::MAX — capping to u64::MAX");
                Decimal::from(u64::MAX) / Decimal::from(divisor)
            } else {
                Decimal::from(raw.low_u64()) / Decimal::from(divisor)
            }
        };

        debug!("On-chain balance: {balance} USDC");
        Ok(balance)
    }
}

/// Build a Polygon WebSocket provider with reconnect on failure.
pub async fn connect_polygon(rpc_url: &str) -> Result<Arc<Provider<Ws>>> {
    let provider: Provider<Ws> = Provider::<Ws>::connect(rpc_url)
        .await
        .map_err(|e| anyhow::anyhow!("Polygon RPC connect failed: {e}"))?;
    Ok(Arc::new(provider))
}

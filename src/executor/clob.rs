use crate::executor::signing::PolyAuth;
use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};
use uuid::Uuid;

// ── Domain types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Live,
    Matched,
    Cancelled,
    Filled,
}

#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub market_id: String,   // condition_id
    pub token_id: String,    // YES or NO token id
    pub side: OrderSide,     // Buy
    pub price: Decimal,      // limit price, 0.01 precision
    pub size: Decimal,       // USDC amount
    pub client_order_id: String, // UUID for idempotency
}

impl OrderRequest {
    pub fn new(
        market_id: String,
        token_id: String,
        side: OrderSide,
        price: Decimal,
        size: Decimal,
    ) -> Self {
        Self {
            market_id,
            token_id,
            side,
            price,
            size,
            client_order_id: Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    #[serde(rename = "orderID")]
    pub order_id: String,
    pub status: OrderStatus,
    pub price: Option<String>,
    pub size_matched: Option<String>,
}

// ── CLOB API request/response structs ────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ClobOrderPayload {
    #[serde(rename = "tokenID")]
    token_id: String,
    price: String,
    size: String,
    side: String,
    #[serde(rename = "orderType")]
    order_type: String,
    #[serde(rename = "clientOrderID")]
    client_order_id: String,
}

// ── ClobClient ────────────────────────────────────────────────────────────────

pub struct ClobClient {
    base_url: String,
    http: reqwest::Client,
    auth: PolyAuth,
    max_retries: u32,
    order_timeout_secs: u64,
    dry_run: bool,
}

impl ClobClient {
    pub fn new(
        base_url: String,
        auth: PolyAuth,
        max_retries: u32,
        order_timeout_secs: u64,
        dry_run: bool,
    ) -> Self {
        Self {
            base_url,
            http: reqwest::Client::new(),
            auth,
            max_retries,
            order_timeout_secs,
            dry_run,
        }
    }

    /// Submit a limit order. Returns the order ID on success.
    pub async fn submit_order(&self, req: &OrderRequest) -> Result<String> {
        if self.dry_run {
            info!(
                "[DRY RUN] Would submit: {:?} {} @ {} size={}",
                req.side, req.token_id, req.price, req.size
            );
            return Ok(format!("dry-run-{}", req.client_order_id));
        }

        let payload = ClobOrderPayload {
            token_id: req.token_id.clone(),
            price: req.price.to_string(),
            size: req.size.to_string(),
            side: format!("{:?}", req.side).to_uppercase(),
            order_type: "LIMIT".to_string(),
            client_order_id: req.client_order_id.clone(),
        };

        let body = serde_json::to_string(&payload)?;
        let path = "/order";
        let headers = self.auth.l2_headers("POST", path, &body)?;

        let url = format!("{}{path}", self.base_url);
        let mut backoff = 1u64;

        for attempt in 1..=self.max_retries {
            debug!("Order attempt {attempt}/{}", self.max_retries);
            let mut rb = self
                .http
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body.clone());

            for (k, v) in &headers {
                rb = rb.header(k.as_str(), v.as_str());
            }

            let resp = rb
                .timeout(Duration::from_secs(self.order_timeout_secs))
                .send()
                .await;

            match resp {
                Ok(r) if r.status().is_success() => {
                    let order: OrderResponse = r
                        .json()
                        .await
                        .context("Failed to parse order response")?;
                    info!(
                        order_id = %order.order_id,
                        status = ?order.status,
                        "Order submitted"
                    );
                    return Ok(order.order_id);
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    warn!("Order failed ({status}): {text}");
                    if status.as_u16() == 400 {
                        bail!("Bad order request: {text}");
                    }
                }
                Err(e) => {
                    warn!("Order request error (attempt {attempt}): {e}");
                }
            }

            if attempt < self.max_retries {
                sleep(Duration::from_secs(backoff)).await;
                backoff *= 2;
            }
        }

        bail!("Order failed after {} attempts", self.max_retries)
    }

    /// Cancel an order by ID.
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        if self.dry_run {
            info!("[DRY RUN] Would cancel order: {order_id}");
            return Ok(());
        }

        let path = format!("/order/{order_id}");
        let headers = self.auth.l2_headers("DELETE", &path, "")?;
        let url = format!("{}{path}", self.base_url);

        let mut rb = self.http.delete(&url);
        for (k, v) in &headers {
            rb = rb.header(k.as_str(), v.as_str());
        }

        let resp = rb
            .timeout(Duration::from_secs(self.order_timeout_secs))
            .send()
            .await
            .context("Cancel request failed")?;

        if resp.status().is_success() {
            info!("Order {order_id} cancelled");
            Ok(())
        } else {
            let text = resp.text().await.unwrap_or_default();
            bail!("Cancel failed: {text}")
        }
    }

    /// Get order status by ID.
    pub async fn get_order(&self, order_id: &str) -> Result<OrderResponse> {
        let path = format!("/order/{order_id}");
        let headers = self.auth.l2_headers("GET", &path, "")?;
        let url = format!("{}{path}", self.base_url);

        let mut rb = self.http.get(&url);
        for (k, v) in &headers {
            rb = rb.header(k.as_str(), v.as_str());
        }

        let resp = rb
            .timeout(Duration::from_secs(self.order_timeout_secs))
            .send()
            .await
            .context("Get order request failed")?;

        resp.json::<OrderResponse>()
            .await
            .context("Failed to parse order status")
    }

    /// Submit order with auto-cancel if not filled within timeout.
    pub async fn submit_with_timeout(&self, req: &OrderRequest) -> Result<String> {
        let order_id = self.submit_order(req).await?;

        if self.dry_run {
            return Ok(order_id);
        }

        let timeout = Duration::from_secs(self.order_timeout_secs);
        let cancel_order_id = order_id.clone();
        let client = self.http.clone();
        let base = self.base_url.clone();
        let auth = self.auth.clone();
        let tout_secs = self.order_timeout_secs;

        tokio::spawn(async move {
            sleep(timeout).await;

            // First, check order status — only cancel if still Live (not Filled).
            let get_path = format!("/order/{cancel_order_id}");
            let get_headers = match auth.l2_headers("GET", &get_path, "") {
                Ok(h) => h,
                Err(e) => {
                    warn!("Auto-cancel: failed to build GET headers: {e}");
                    return;
                }
            };
            let get_url = format!("{base}{get_path}");
            let mut get_rb = client.get(&get_url);
            for (k, v) in &get_headers {
                get_rb = get_rb.header(k.as_str(), v.as_str());
            }
            let status_resp = match get_rb.timeout(Duration::from_secs(tout_secs)).send().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Auto-cancel: GET order failed: {e}");
                    return;
                }
            };
            let order_info: OrderResponse = match status_resp.json().await {
                Ok(o) => o,
                Err(e) => {
                    warn!("Auto-cancel: could not parse order status: {e}");
                    return;
                }
            };

            // Only cancel orders that are still live (not already filled or cancelled)
            if order_info.status != OrderStatus::Live {
                return;
            }

            let del_path = format!("/order/{cancel_order_id}");
            let del_headers = match auth.l2_headers("DELETE", &del_path, "") {
                Ok(h) => h,
                Err(e) => {
                    warn!("Auto-cancel: failed to build DELETE headers: {e}");
                    return;
                }
            };
            let del_url = format!("{base}{del_path}");
            let mut del_rb = client.delete(&del_url);
            for (k, v) in &del_headers {
                del_rb = del_rb.header(k.as_str(), v.as_str());
            }
            if let Ok(r) = del_rb.timeout(Duration::from_secs(tout_secs)).send().await {
                if r.status().is_success() {
                    warn!("Auto-cancelled unfilled order: {cancel_order_id}");
                } else {
                    warn!("Auto-cancel DELETE failed ({}): {cancel_order_id}", r.status());
                }
            }
        });

        Ok(order_id)
    }
}

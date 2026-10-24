//! JSON-RPC over HTTP with an endpoint pool.
//!
//! Without rotation every call goes to the first configured endpoint. With rotation, calls go
//! round-robin over all configured endpoints plus the RPC nodes the cluster advertises over
//! gossip (`getClusterNodes`), and a call that hits a rate limit or an unreachable node moves
//! on to the next endpoint. No project-run gateway is involved: the nodes come from the chain.

use anyhow::{anyhow, bail, Result};
use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::RwLock;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_CONCURRENCY: usize = 32;
/// Discovered nodes further than this behind the median slot are left out
const MAX_SLOT_LAG: u64 = 150;

pub struct RpcPool {
    http: reqwest::Client,
    configured: Vec<String>,
    discovered: RwLock<Vec<String>>,
    rotate: AtomicBool,
    cursor: AtomicUsize,
}

/// An error from the node about the request itself (as opposed to the node being unusable)
#[derive(Debug)]
pub struct RpcError {
    pub message: String,
    pub logs: Vec<String>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match program_error(&self.logs) {
            Some(e) => write!(f, "{e}"),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for RpcError {}

/// The program's own message from simulation or preflight logs ("Error Message: ..."), if any
pub fn program_error(logs: &[String]) -> Option<String> {
    logs.iter()
        .find_map(|l| l.find("Error Message: ").map(|i| l[i + 15..].trim_end_matches('.').to_string()))
}

fn logs_of(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// The node could not serve the call; another node might
enum Failure {
    Endpoint(String),
    Request(RpcError),
}

impl RpcPool {
    pub fn new(configured: Vec<String>) -> Result<Self> {
        if configured.is_empty() {
            bail!("No RPC endpoint configured");
        }
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
        Ok(Self {
            http,
            configured,
            discovered: RwLock::new(Vec::new()),
            rotate: AtomicBool::new(false),
            cursor: AtomicUsize::new(0),
        })
    }

    pub fn primary(&self) -> &str {
        &self.configured[0]
    }

    pub fn set_rotation(&self, on: bool) {
        self.rotate.store(on, Ordering::Relaxed);
    }

    pub fn rotation(&self) -> bool {
        self.rotate.load(Ordering::Relaxed)
    }

    /// Endpoints in use: the first configured one, or the whole pool while rotating
    pub fn endpoints(&self) -> Vec<String> {
        if !self.rotation() {
            return vec![self.configured[0].clone()];
        }
        let mut all = self.configured.clone();
        for url in self.discovered.read().unwrap().iter() {
            if !all.contains(url) {
                all.push(url.clone());
            }
        }
        all
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let endpoints = self.endpoints();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        // One pass over the pool; a single endpoint gets a few tries with backoff instead
        let attempts = endpoints.len().max(3);
        let mut last = String::new();
        for attempt in 0..attempts {
            let url = &endpoints[(start + attempt) % endpoints.len()];
            match self.call_once(url, method, &params).await {
                Ok(v) => return Ok(v),
                Err(Failure::Request(e)) => return Err(e.into()),
                Err(Failure::Endpoint(e)) => {
                    last = format!("{}: {e}", short_url(url));
                    if endpoints.len() == 1 {
                        tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
                    }
                }
            }
        }
        Err(anyhow!("{method} failed on every endpoint ({last})"))
    }

    async fn call_once(&self, url: &str, method: &str, params: &serde_json::Value) -> Result<serde_json::Value, Failure> {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self.http.post(url).json(&body).send().await.map_err(|e| Failure::Endpoint(e.to_string()))?;
        let status = resp.status();
        if status.as_u16() == 429 {
            return Err(Failure::Endpoint("rate limited".into()));
        }
        if !status.is_success() {
            return Err(Failure::Endpoint(format!("HTTP {status}")));
        }
        let value: serde_json::Value = resp.json().await.map_err(|e| Failure::Endpoint(e.to_string()))?;
        if let Some(err) = value.get("error") {
            let code = err["code"].as_i64().unwrap_or(0);
            let message = err["message"].as_str().unwrap_or("RPC error").to_string();
            // Node unhealthy / behind, or the method is not served there
            if matches!(code, -32005 | -32601 | -32603) {
                return Err(Failure::Endpoint(message));
            }
            return Err(Failure::Request(RpcError { message, logs: logs_of(&err["data"]["logs"]) }));
        }
        value.get("result").cloned().ok_or_else(|| Failure::Endpoint("response has no result".into()))
    }

    /// Ask the cluster for the RPC nodes its validators advertise, keep the ones that answer and are
    /// close to the tip. Returns (usable, advertised).
    pub async fn discover(&self) -> Result<(usize, usize)> {
        let nodes = self.call("getClusterNodes", serde_json::json!([])).await?;
        let mut seen = HashSet::new();
        let candidates: Vec<String> = nodes
            .as_array()
            .map(|a| a.iter().filter_map(|n| n["rpc"].as_str()).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|addr| !addr.starts_with("0.0.0.0") && !addr.starts_with("127."))
            .map(|addr| format!("http://{addr}"))
            .filter(|url| seen.insert(url.clone()))
            .collect();

        let probe = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build()?;
        let responsive: Vec<(String, u64)> = stream::iter(candidates.iter().cloned())
            .map(|url| {
                let probe = probe.clone();
                async move { probe_node(&probe, &url).await.map(|slot| (url, slot)) }
            })
            .buffer_unordered(PROBE_CONCURRENCY)
            .filter_map(|r| async move { r })
            .collect()
            .await;

        let mut slots: Vec<u64> = responsive.iter().map(|(_, s)| *s).collect();
        slots.sort_unstable();
        let Some(&median) = slots.get(slots.len() / 2) else {
            return Ok((0, candidates.len()));
        };
        let usable: Vec<String> = responsive
            .into_iter()
            .filter(|(_, s)| s + MAX_SLOT_LAG >= median)
            .map(|(url, _)| url)
            .collect();
        let n = usable.len();
        if n > 0 {
            *self.discovered.write().unwrap() = usable;
        }
        Ok((n, candidates.len()))
    }
}

/// The node's slot if it reports healthy and serves getLatestBlockhash
async fn probe_node(http: &reqwest::Client, url: &str) -> Option<u64> {
    let rpc = |method: &str| serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": [] });
    let health: serde_json::Value = http.post(url).json(&rpc("getHealth")).send().await.ok()?.json().await.ok()?;
    if health["result"].as_str() != Some("ok") {
        return None;
    }
    let hash: serde_json::Value = http.post(url).json(&rpc("getLatestBlockhash")).send().await.ok()?.json().await.ok()?;
    hash["result"]["value"]["blockhash"].as_str()?;
    hash["result"]["context"]["slot"].as_u64()
}

pub fn short_url(url: &str) -> &str {
    url.trim_start_matches("https://").trim_start_matches("http://")
}

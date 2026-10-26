//! Reads from X1 and the mint transaction.

use crate::network::Network;
use crate::rpc::RpcPool;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use solana_hash::Hash;
use memo_sdk::{compute_budget, mint, token, TOKEN_2022_PROGRAM_ID};
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use memo_sdk::mint::MEMO_MIN_BYTES as MIN_MEMO_BYTES;
pub use memo_sdk::DECIMALS as MEMO_DECIMALS;

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Everything the dashboard shows about the chain and the wallet
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub supply: u64,
    pub xnt: u64,
    pub memo: u64,
}

pub struct Chain {
    pub network: Network,
    pub rpc: RpcPool,
    /// The wallet's MEMO account is known to exist, so mints leave out the instruction that
    /// creates it (about 7,000 compute units even when there is nothing to create)
    token_account_ready: AtomicBool,
}

impl Chain {
    pub fn new(network: Network, rpc: RpcPool) -> Self {
        Self { network, rpc, token_account_ready: AtomicBool::new(false) }
    }

    fn token_account(&self, owner: &Pubkey) -> Pubkey {
        token::memo_token_account(self.network.sdk(), owner)
    }

    async fn account_data(&self, address: &Pubkey) -> Result<Option<Vec<u8>>> {
        let r = self
            .rpc
            .call("getAccountInfo", serde_json::json!([address.to_string(), { "encoding": "base64", "commitment": "confirmed" }]))
            .await?;
        if r["value"].is_null() {
            return Ok(None);
        }
        let data = r["value"]["data"][0].as_str().ok_or_else(|| anyhow!("Bad account data"))?;
        Ok(Some(base64::engine::general_purpose::STANDARD.decode(data)?))
    }

    pub async fn snapshot(&self, owner: &Pubkey) -> Result<Snapshot> {
        let (supply, xnt, memo) = tokio::try_join!(self.supply(), self.xnt_balance(owner), self.memo_balance(owner))?;
        Ok(Snapshot { supply, xnt, memo })
    }

    pub async fn supply(&self) -> Result<u64> {
        let r = self
            .rpc
            .call("getTokenSupply", serde_json::json!([self.network.sdk().memo_mint().to_string(), { "commitment": "confirmed" }]))
            .await?;
        r["value"]["amount"].as_str().and_then(|a| a.parse().ok()).ok_or_else(|| anyhow!("Bad supply"))
    }

    pub async fn xnt_balance(&self, owner: &Pubkey) -> Result<u64> {
        let r = self.rpc.call("getBalance", serde_json::json!([owner.to_string(), { "commitment": "confirmed" }])).await?;
        r["value"].as_u64().ok_or_else(|| anyhow!("Bad balance"))
    }

    /// Token-2022 account: amount is the u64 at byte 64 (after mint and owner)
    pub async fn memo_balance(&self, owner: &Pubkey) -> Result<u64> {
        Ok(match self.account_data(&self.token_account(owner)).await? {
            Some(d) if d.len() >= 72 => u64::from_le_bytes(d[64..72].try_into()?),
            _ => 0,
        })
    }

    async fn latest_blockhash(&self) -> Result<Hash> {
        let r = self.rpc.call("getLatestBlockhash", serde_json::json!([{ "commitment": "confirmed" }])).await?;
        r["value"]["blockhash"]
            .as_str()
            .and_then(|b| Hash::from_str(b).ok())
            .ok_or_else(|| anyhow!("Bad blockhash"))
    }

    /// memo (index 0), create the MEMO account if `create`, process_mint
    fn mint_instructions(&self, user: &Pubkey, memo: &str, create: bool) -> Result<Vec<Instruction>> {
        Ok(mint::mint_instructions(self.network.sdk(), user, memo, create)?)
    }

    /// Simulate, sign, send and wait for confirmation; returns the signature
    pub async fn mint(&self, payer: &Keypair, memo: &str) -> Result<String> {
        let user = payer.pubkey();
        if !self.token_account_ready.load(Ordering::Relaxed) && self.account_data(&self.token_account(&user)).await?.is_some() {
            self.token_account_ready.store(true, Ordering::Relaxed);
        }
        let create = !self.token_account_ready.load(Ordering::Relaxed);
        let result = self.send(payer, self.mint_instructions(&user, memo, create)?).await;
        // After a mint the account exists; after a failure, check again next time
        self.token_account_ready.store(result.is_ok(), Ordering::Relaxed);
        result
    }

    /// Send XNT (lamports) to another address
    pub async fn transfer_xnt(&self, payer: &Keypair, to: &Pubkey, lamports: u64) -> Result<String> {
        let ix = solana_system_interface::instruction::transfer(&payer.pubkey(), to, lamports);
        self.send(payer, vec![ix]).await
    }

    /// Send MEMO (units) to another wallet, creating its MEMO account if needed
    pub async fn transfer_memo(&self, payer: &Keypair, to: &Pubkey, units: u64) -> Result<String> {
        let owner = payer.pubkey();
        let mint = self.network.sdk().memo_mint();
        let destination = self.token_account(to);
        // Token-2022 TransferChecked = 12: amount (u64), decimals (u8)
        let mut data = vec![12u8];
        data.extend_from_slice(&units.to_le_bytes());
        data.push(MEMO_DECIMALS);
        let transfer = Instruction::new_with_bytes(
            TOKEN_2022_PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new(self.token_account(&owner), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(owner, true),
            ],
        );
        self.send(payer, vec![token::create_memo_token_account(self.network.sdk(), &owner, to), transfer]).await
    }

    /// Simulate to learn the compute units, append the limit plus a small margin (last, so a memo
    /// stays at index 0),
    /// sign, send and wait for confirmation
    async fn send(&self, payer: &Keypair, mut instructions: Vec<Instruction>) -> Result<String> {
        let (blockhash, units) = self.simulate(&payer.pubkey(), &instructions).await?;
        instructions.push(compute_budget::set_compute_unit_limit(compute_budget::limit_for(units)));
        let tx = Transaction::new_signed_with_payer(&instructions, Some(&payer.pubkey()), &[payer], blockhash);
        let signature = tx.signatures[0].to_string();
        self.rpc
            .call(
                "sendTransaction",
                serde_json::json!([b64(&bincode::serialize(&tx)?), { "encoding": "base64", "preflightCommitment": "confirmed" }]),
            )
            .await?;
        self.confirm(&signature).await?;
        Ok(signature)
    }

    /// A blockhash and the compute units a simulation used; the fee follows the requested limit
    async fn simulate(&self, payer: &Pubkey, instructions: &[Instruction]) -> Result<(Hash, u64)> {
        let blockhash = self.latest_blockhash().await?;
        let mut sim = instructions.to_vec();
        sim.push(compute_budget::set_compute_unit_limit(compute_budget::SIMULATION_CU_LIMIT));
        let mut sim_tx = Transaction::new_unsigned(Message::new(&sim, Some(payer)));
        sim_tx.message.recent_blockhash = blockhash;
        let r = self
            .rpc
            .call(
                "simulateTransaction",
                serde_json::json!([b64(&bincode::serialize(&sim_tx)?), {
                    "encoding": "base64", "sigVerify": false, "replaceRecentBlockhash": true, "commitment": "confirmed"
                }]),
            )
            .await?;
        if !r["value"]["err"].is_null() {
            let logs: Vec<String> = r["value"]["logs"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            bail!(crate::rpc::program_error(&logs).unwrap_or_else(|| format!("Simulation failed: {}", r["value"]["err"])));
        }
        let units = r["value"]["unitsConsumed"].as_u64().ok_or_else(|| anyhow!("Simulation returned no compute units"))?;
        Ok((blockhash, units))
    }

    /// Wait up to ~60 s for confirmation
    async fn confirm(&self, signature: &str) -> Result<()> {
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Ok(r) = self.rpc.call("getSignatureStatuses", serde_json::json!([[signature]])).await else {
                continue;
            };
            let status = &r["value"][0];
            if status.is_null() {
                continue;
            }
            if !status["err"].is_null() {
                bail!("Transaction failed: {}", status["err"]);
            }
            if matches!(status["confirmationStatus"].as_str(), Some("confirmed" | "finalized")) {
                return Ok(());
            }
        }
        bail!("Not confirmed after 60 s: {signature}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads and a mint simulation against a live network (nothing is sent).
    /// MEMO_MINER_TEST_WALLET: an address holding a little XNT; MEMO_MINER_TEST_NET: mainnet or testnet
    #[tokio::test]
    #[ignore]
    async fn live_reads_and_simulation() {
        let wallet = Pubkey::from_str(&std::env::var("MEMO_MINER_TEST_WALLET").unwrap()).unwrap();
        let network = match std::env::var("MEMO_MINER_TEST_NET").as_deref() {
            Ok("mainnet") => Network::Mainnet,
            _ => Network::Testnet,
        };
        let chain = Chain::new(network, RpcPool::new(vec![network.default_rpc().into()]).unwrap());
        let s = chain.snapshot(&wallet).await.unwrap();
        println!("{s:?}");
        assert!(s.supply > 0);
        let memo = r#"{"action":"mint","platform":"memo-miner","version":"0.1.0","chain":"X1","ts":0}"#;
        let (_, units) = chain.simulate(&wallet, &chain.mint_instructions(&wallet, memo, true).unwrap()).await.unwrap();
        println!("simulated units: {units}");
        assert!(units > 0);

        chain.rpc.set_rotation(true);
        let (usable, advertised) = chain.rpc.discover().await.unwrap();
        println!("discovered {usable} usable of {advertised}");
        assert!(!chain.rpc.endpoints().is_empty());
        // A read through the pool
        assert!(chain.supply().await.unwrap() > 0);
    }
}

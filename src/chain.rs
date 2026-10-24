//! Reads from X1 and the mint transaction.

use crate::network::{Network, ATA_PROGRAM_ID, MEMO_PROGRAM_ID, TOKEN_2022_PROGRAM_ID};
use crate::rpc::RpcPool;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use sha2::{Digest, Sha256};
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::Message,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const MIN_MEMO_BYTES: usize = 69;
pub const MAX_MEMO_BYTES: usize = 800;
pub const MEMO_DECIMALS: u8 = 6;
/// Headroom over the simulated compute units. Execution can use a few more units than the
/// simulation (chain state moves on in between), and a zero-margin limit then fails with
/// "Computational budget exceeded". Kept small because the fee follows the requested limit.
const CU_LIMIT_MARGIN: u32 = 50;
const COMPUTE_BUDGET_PROGRAM_ID: &str = "ComputeBudget111111111111111111111111111111";
const INSTRUCTIONS_SYSVAR_ID: &str = "Sysvar1nstructions1111111111111111111111111";

fn key(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("valid built-in address")
}

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
        Pubkey::find_program_address(
            &[owner.as_ref(), key(TOKEN_2022_PROGRAM_ID).as_ref(), key(self.network.token_mint()).as_ref()],
            &key(ATA_PROGRAM_ID),
        )
        .0
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
            .call("getTokenSupply", serde_json::json!([self.network.token_mint(), { "commitment": "confirmed" }]))
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

    /// Create `owner`'s MEMO account if it does not exist (associated token account, CreateIdempotent = 1)
    fn create_token_account(&self, payer: &Pubkey, owner: &Pubkey) -> Instruction {
        Instruction::new_with_bytes(
            key(ATA_PROGRAM_ID),
            &[1],
            vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(self.token_account(owner), false),
                AccountMeta::new_readonly(*owner, false),
                AccountMeta::new_readonly(key(self.network.token_mint()), false),
                AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
                AccountMeta::new_readonly(key(TOKEN_2022_PROGRAM_ID), false),
            ],
        )
    }

    /// memo (index 0), create the MEMO account if `create`, process_mint
    fn mint_instructions(&self, user: &Pubkey, memo: &[u8], create: bool) -> Vec<Instruction> {
        let program = key(self.network.mint_program_id());
        let mint = key(self.network.token_mint());
        let token_program = key(TOKEN_2022_PROGRAM_ID);
        let token_account = self.token_account(user);
        let (mint_authority, _) = Pubkey::find_program_address(&[b"mint_authority"], &program);
        let (mint_state, _) = Pubkey::find_program_address(&[b"mint_state"], &program);

        let memo_ix = Instruction::new_with_bytes(key(MEMO_PROGRAM_ID), memo, vec![AccountMeta::new_readonly(*user, true)]);
        let discriminator = Sha256::digest(b"global:process_mint");
        let mint_ix = Instruction::new_with_bytes(
            program,
            &discriminator[..8],
            vec![
                AccountMeta::new(*user, true),
                AccountMeta::new(mint, false),
                AccountMeta::new_readonly(mint_authority, false),
                AccountMeta::new(token_account, false),
                AccountMeta::new_readonly(token_program, false),
                AccountMeta::new_readonly(key(INSTRUCTIONS_SYSVAR_ID), false),
                AccountMeta::new(mint_state, false),
            ],
        );
        if create {
            vec![memo_ix, self.create_token_account(user, user), mint_ix]
        } else {
            vec![memo_ix, mint_ix]
        }
    }

    fn compute_limit(units: u32) -> Instruction {
        // ComputeBudgetInstruction::SetComputeUnitLimit = 2
        let mut data = vec![2u8];
        data.extend_from_slice(&units.to_le_bytes());
        Instruction::new_with_bytes(key(COMPUTE_BUDGET_PROGRAM_ID), &data, vec![])
    }

    /// Simulate, sign, send and wait for confirmation; returns the signature
    pub async fn mint(&self, payer: &Keypair, memo: &[u8]) -> Result<String> {
        if !(MIN_MEMO_BYTES..=MAX_MEMO_BYTES).contains(&memo.len()) {
            bail!("Memo must be {MIN_MEMO_BYTES}-{MAX_MEMO_BYTES} bytes, got {}", memo.len());
        }
        let user = payer.pubkey();
        if !self.token_account_ready.load(Ordering::Relaxed) && self.account_data(&self.token_account(&user)).await?.is_some() {
            self.token_account_ready.store(true, Ordering::Relaxed);
        }
        let create = !self.token_account_ready.load(Ordering::Relaxed);
        let result = self.send(payer, self.mint_instructions(&user, memo, create)).await;
        // After a mint the account exists; after a failure, check again next time
        self.token_account_ready.store(result.is_ok(), Ordering::Relaxed);
        result
    }

    /// Send XNT (lamports) to another address
    pub async fn transfer_xnt(&self, payer: &Keypair, to: &Pubkey, lamports: u64) -> Result<String> {
        let ix = solana_sdk::system_instruction::transfer(&payer.pubkey(), to, lamports);
        self.send(payer, vec![ix]).await
    }

    /// Send MEMO (units) to another wallet, creating its MEMO account if needed
    pub async fn transfer_memo(&self, payer: &Keypair, to: &Pubkey, units: u64) -> Result<String> {
        let owner = payer.pubkey();
        let mint = key(self.network.token_mint());
        let destination = self.token_account(to);
        // Token-2022 TransferChecked = 12: amount (u64), decimals (u8)
        let mut data = vec![12u8];
        data.extend_from_slice(&units.to_le_bytes());
        data.push(MEMO_DECIMALS);
        let transfer = Instruction::new_with_bytes(
            key(TOKEN_2022_PROGRAM_ID),
            &data,
            vec![
                AccountMeta::new(self.token_account(&owner), false),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(owner, true),
            ],
        );
        self.send(payer, vec![self.create_token_account(&owner, to), transfer]).await
    }

    /// Simulate to learn the compute units, append the limit plus a small margin (last, so a memo
    /// stays at index 0),
    /// sign, send and wait for confirmation
    async fn send(&self, payer: &Keypair, mut instructions: Vec<Instruction>) -> Result<String> {
        let (blockhash, units) = self.simulate(&payer.pubkey(), &instructions).await?;
        instructions.push(Self::compute_limit(u32::try_from(units).unwrap_or(u32::MAX).saturating_add(CU_LIMIT_MARGIN)));
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
        sim.push(Self::compute_limit(400_000));
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
        let memo = br#"{"action":"mint","platform":"memo-miner","version":"0.1.0","chain":"X1","ts":0}"#;
        let (_, units) = chain.simulate(&wallet, &chain.mint_instructions(&wallet, memo, true)).await.unwrap();
        println!("simulated units: {units}");
        assert!(units > 0);

        chain.rpc.set_rotation(true);
        let (usable, advertised) = chain.rpc.discover().await.unwrap();
        println!("discovered {usable} usable of {advertised}");
        assert!(chain.rpc.endpoints().len() >= 1);
        // A read through the pool
        assert!(chain.supply().await.unwrap() > 0);
    }
}

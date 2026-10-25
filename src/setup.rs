//! `solana-keygen` from the official Solana CLI (Anza): finding it and creating wallets with it.
//! Release archives ship it next to memo-miner. The miner never generates keys itself; it only
//! reads the keypair file solana-keygen wrote.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const KEYGEN_INSTALL: &str = "cargo install solana-keygen --locked";

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Next to the memo-miner executable, where every release archive ships solana-keygen
fn bundled_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

/// Where Anza's installer puts the active release
fn install_bin_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".local/share/solana/install/active_release/bin"))
}

/// Where `cargo install` puts programs (a solana-keygen built from source)
fn cargo_bin_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".cargo/bin"))
}

/// `solana-keygen`: the one shipped next to memo-miner first, then PATH, Anza's installer and cargo
pub fn find_keygen() -> Option<PathBuf> {
    let name = exe("solana-keygen");
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>());
    bundled_dir()
        .into_iter()
        .chain(on_path)
        .chain([install_bin_dir(), cargo_bin_dir()].into_iter().flatten())
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
}

/// The wallet file name, kept next to the Solana CLI's own keypairs
pub const WALLET_FILE: &str = "memo-miner.json";

/// The miner's wallet: `~/.config/solana/memo-miner.json` on every system, where the Solana CLI
/// keeps its keypairs (`id.json`)
pub fn default_wallet_path() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("No home directory on this system")?.join(".config").join("solana").join(WALLET_FILE))
}

/// If `path` exists, rename it to `<name>-<date>-<time>.json` so nothing is overwritten
fn keep_existing(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("memo-miner");
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let kept = path.with_file_name(format!("{stem}-{stamp}.json"));
    std::fs::rename(path, &kept).with_context(|| format!("Renaming {}", path.display()))?;
    Ok(Some(kept))
}

pub struct NewWallet {
    pub path: PathBuf,
    /// The wallet file that was at `path` before, renamed
    pub backup: Option<PathBuf>,
    pub address: String,
    pub seed_phrase: Vec<String>,
}

/// `solana-keygen new` into `path` (keeping any wallet already there); returns the address and
/// the seed phrase it printed
pub fn create_wallet(keygen: &Path, path: &Path) -> Result<NewWallet> {
    let path = path.to_path_buf();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let backup = keep_existing(&path)?;
    let out = Command::new(keygen)
        .args(["new", "--no-bip39-passphrase", "--outfile"])
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .context("Running solana-keygen")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!("solana-keygen failed: {}{}", text.trim(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let address = lines
        .iter()
        .find_map(|l| l.strip_prefix("pubkey: "))
        .ok_or_else(|| anyhow!("solana-keygen printed no address"))?
        .to_string();
    let seed_phrase: Vec<String> = lines
        .iter()
        .position(|l| l.starts_with("Save this seed phrase"))
        .and_then(|i| lines.get(i + 1))
        .map(|l| l.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    if seed_phrase.len() < 12 {
        bail!("solana-keygen printed no seed phrase; the wallet file is {}", path.display());
    }
    Ok(NewWallet { path, backup, address, seed_phrase })
}

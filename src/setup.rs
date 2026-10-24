//! The official Solana CLI: finding it, installing it, and creating wallets with `solana-keygen`.
//! The miner never generates keys itself; it only reads the keypair file the Solana CLI wrote.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

pub const INSTALL_DOCS: &str = "https://docs.anza.xyz/cli/install";

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Where the official installer puts the active release
fn install_bin_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".local/share/solana/install/active_release/bin"))
}

/// `solana-keygen` from PATH or from the installer's directory
pub fn find_keygen() -> Option<PathBuf> {
    let name = exe("solana-keygen");
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(&name));
    on_path.chain(install_bin_dir().map(|d| d.join(&name))).find(|p| p.is_file())
}

/// What the installer will run, shown to the user before they agree
pub fn install_description() -> &'static str {
    if cfg!(windows) {
        "Download agave-install-init-x86_64-pc-windows-msvc.exe from release.anza.xyz and run it with \"stable\""
    } else {
        "curl --proto '=https' --tlsv1.2 -sSfL https://release.anza.xyz/stable/install | sh"
    }
}

/// Forward a child's output to `lines`, splitting on \n and \r (progress bars), without blocking the UI
fn pipe_lines<R: Read + Send + 'static>(reader: R, lines: Sender<String>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).map(|n| n > 0).unwrap_or(false) {
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(last) = text.split('\r').map(str::trim_end).rfind(|s| !s.is_empty()) {
                let _ = lines.send(last.to_string());
            }
            buf.clear();
        }
    })
}

fn run_streaming(cmd: &mut Command, lines: &Sender<String>) -> Result<()> {
    let _ = lines.send(format!("$ {cmd:?}"));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Starting {cmd:?}"))?;
    let out = pipe_lines(child.stdout.take().unwrap(), lines.clone());
    let err = pipe_lines(child.stderr.take().unwrap(), lines.clone());
    let status = child.wait()?;
    let _ = (out.join(), err.join());
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

/// Run Anza's official installer for the stable release, streaming its output (blocking; run on a thread)
pub fn install_solana_cli(lines: Sender<String>) -> Result<PathBuf> {
    if cfg!(windows) {
        let dir = std::env::temp_dir().join("agave-install-tmp");
        std::fs::create_dir_all(&dir)?;
        let init = dir.join("agave-install-init.exe");
        let url = "https://release.anza.xyz/stable/agave-install-init-x86_64-pc-windows-msvc.exe";
        run_streaming(Command::new("curl").args(["-sSfL", url, "--output"]).arg(&init), &lines)?;
        run_streaming(Command::new(&init).arg("stable"), &lines)?;
    } else {
        run_streaming(Command::new("sh").args(["-c", install_description()]), &lines)?;
    }
    find_keygen().ok_or_else(|| anyhow!("Installed, but solana-keygen was not found. See {INSTALL_DOCS}"))
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

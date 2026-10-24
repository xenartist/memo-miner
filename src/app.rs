//! The terminal app: setup, minting and the wallet, all inside the TUI, by keyboard or mouse.
//! There are no settings: mainnet, RPC rotation on, a mint every 5 s, stop below 0.01 XNT.

use crate::chain::{Chain, Snapshot, MEMO_DECIMALS, MIN_MEMO_BYTES};
use crate::curve::{self, fmt_units, fmt_units_short, reward_at};
use crate::input::{parse_amount, TextInput};
use crate::network::Network;
use crate::rpc::{short_url, RpcPool};
use crate::setup::{self, NewWallet};
use anyhow::Result;
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    symbols,
    widgets::{Block, BorderType, Borders, Clear, LineGauge, List, ListItem, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const REFRESH: Duration = Duration::from_secs(30);
const REDISCOVER: Duration = Duration::from_secs(600);
const LOG_LINES: usize = 500;
const XNT_DECIMALS: u32 = 9;
/// Wait after each confirmed automatic mint
const AUTO_INTERVAL: Duration = Duration::from_secs(5);
/// Automatic minting stops when the wallet holds less XNT than this (0.01 XNT)
const MIN_XNT_LAMPORTS: u64 = 10_000_000;

// Palette: the MEMO logo's yellow and pink on neutral greys
const ACCENT: Color = Color::Rgb(0xff, 0xde, 0x59);
const ACCENT_DEEP: Color = Color::Rgb(0xe6, 0xc2, 0x3a);
const PINK: Color = Color::Rgb(0xff, 0x9b, 0x9b);
/// XNT amounts, so they never read as MEMO
const BLUE: Color = Color::Rgb(0x6c, 0xb4, 0xff);
const TEXT: Color = Color::Rgb(0xec, 0xec, 0xef);
const MUTED: Color = Color::Rgb(0x9a, 0x9a, 0xa3);
const FAINT: Color = Color::Rgb(0x62, 0x62, 0x6b);
const LINE: Color = Color::Rgb(0x38, 0x38, 0x40);
const BUTTON_BG: Color = Color::Rgb(0x26, 0x26, 0x2d);
const KEY_BG: Color = Color::Rgb(0x33, 0x33, 0x3b);
const INPUT_BG: Color = Color::Rgb(0x1d, 0x1d, 0x22);
const INPUT_FOCUS: Color = Color::Rgb(0x29, 0x29, 0x30);
const MODAL_BG: Color = Color::Rgb(0x12, 0x12, 0x15);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The MEMO logo, one cell per two columns: X yellow, P pink
const LOGO: [&str; 6] = ["X..XX", "XXXX.", "X.X.X", "XXXXX", ".XXXX", "...P."];

enum Msg {
    Snapshot(u64, Result<Snapshot, String>),
    MintStarted(u64),
    Minted { gen: u64, auto: bool, result: Result<(String, u64), String> },
    Discovered(u64, Result<(usize, usize), String>),
    AutoStopped(u64, String),
    Sent { what: String, result: Result<String, String> },
    WalletCreated(Result<NewWallet, String>),
    InstallLine(String),
    InstallDone(Result<PathBuf, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mint,
    Wallet,
}

impl Tab {
    const ALL: [Tab; 2] = [Tab::Mint, Tab::Wallet];
    fn name(self) -> &'static str {
        match self {
            Tab::Mint => "Mint",
            Tab::Wallet => "Wallet",
        }
    }
}

/// Everything a click can do
#[derive(Clone, PartialEq)]
enum Action {
    Tab(Tab),
    Mint,
    ToggleAuto,
    ToggleRotation,
    CopyAddress,
    SendXnt,
    SendMemo,
    NewWallet,
    CheckAgain,
    InstallCli,
    Quit,
    FormField(usize),
    ModalOk,
    ModalCancel,
}

#[derive(Clone, Copy, PartialEq)]
enum FormKind {
    SendXnt,
    SendMemo,
}

struct Form {
    kind: FormKind,
    title: String,
    hint: String,
    fields: Vec<(String, TextInput)>,
    focus: usize,
    error: Option<String>,
}

enum Pending {
    SendXnt { to: Pubkey, lamports: u64 },
    SendMemo { to: Pubkey, units: u64 },
    Install,
    CreateWallet,
}

enum Modal {
    Form(Form),
    Confirm { title: String, lines: Vec<String>, ok: String, pending: Pending },
    Info { title: String, lines: Vec<String> },
    Seed(NewWallet),
    Install { lines: Vec<String>, done: Option<Result<PathBuf, String>> },
    Busy(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Good,
    Bad,
    Info,
}

struct LogEntry {
    time: String,
    kind: Kind,
    text: String,
    link: Option<String>,
}

struct Auto {
    stop: Arc<AtomicBool>,
    ok: u32,
    failed: u32,
}

/// An open wallet on a network, with its background tasks
struct Session {
    gen: u64,
    chain: Arc<Chain>,
    payer: Arc<Keypair>,
    snapshot: Option<Snapshot>,
    minting: bool,
    auto: Option<Auto>,
    rotation_stop: Option<Arc<AtomicBool>>,
    refresher_stop: Arc<AtomicBool>,
    pool_size: usize,
    /// Mints since the wallet was opened
    minted: u64,
    ok: u32,
    failed: u32,
}

impl Session {
    fn close(&mut self) {
        self.refresher_stop.store(true, Ordering::Relaxed);
        if let Some(a) = self.auto.take() {
            a.stop.store(true, Ordering::Relaxed);
        }
        if let Some(s) = self.rotation_stop.take() {
            s.store(true, Ordering::Relaxed);
        }
    }
}

pub struct App {
    network: Network,
    /// The only wallet the miner uses: ~/.config/solana/memo-miner.json
    wallet_path: PathBuf,
    /// Opening the wallet file failed; not retried until Check again
    wallet_error: Option<String>,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    session: Option<Session>,
    next_gen: u64,
    tab: Tab,
    modal: Option<Modal>,
    keygen: Option<PathBuf>,
    log: VecDeque<LogEntry>,
    /// Animation frame counter
    tick: usize,
    log_scroll: usize,
    /// Clickable areas from the last frame; while a modal is open only its own count
    hits: Vec<(Rect, Action)>,
    modal_hits_from: usize,
    cursor: Option<Position>,
    clipboard: Option<arboard::Clipboard>,
    quit: bool,
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// The memo for each mint: exactly 69 bytes, the least memo-mint accepts (fewer compute units),
/// with the time in seconds so consecutive mints differ
fn memo_for_mint() -> Vec<u8> {
    let t = chrono::Utc::now().timestamp();
    format!(r#"{{"action":"mint","platform":"memo-miner","chain":"X1","t":{t}}}"#).into_bytes()
}

fn lamports(l: u64) -> String {
    let s = format!("{:.9}", l as f64 / 1e9);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn short_key(k: &str) -> String {
    if k.len() > 12 {
        format!("{}…{}", &k[..6], &k[k.len() - 6..])
    } else {
        k.to_string()
    }
}

/// A path with the home directory shown as ~ and / between parts on every system:
/// ~/.config/solana/memo-miner.json
fn tilde(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(PathBuf::from)) {
        Some(rest) => {
            let parts: Vec<String> = rest.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
            format!("~/{}", parts.join("/"))
        }
        None => path.display().to_string(),
    }
}

/// A rectangle of the given size centred in `area`
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

impl App {
    pub fn new(network: Network, wallet_path: PathBuf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            network,
            wallet_path,
            wallet_error: None,
            tx,
            rx,
            session: None,
            next_gen: 0,
            tab: Tab::Mint,
            modal: None,
            keygen: setup::find_keygen(),
            log: VecDeque::new(),
            tick: 0,
            log_scroll: 0,
            hits: Vec::new(),
            modal_hits_from: 0,
            cursor: None,
            clipboard: arboard::Clipboard::new().ok(),
            quit: false,
        }
    }

    fn push_log(&mut self, kind: Kind, text: impl Into<String>) {
        self.push_entry(kind, text.into(), None);
    }

    fn push_link(&mut self, kind: Kind, text: impl Into<String>, link: String) {
        self.push_entry(kind, text.into(), Some(link));
    }

    fn push_entry(&mut self, kind: Kind, text: String, link: Option<String>) {
        self.log.push_front(LogEntry { time: now(), kind, text, link });
        self.log.truncate(LOG_LINES);
        if self.log_scroll > 0 {
            self.log_scroll += 1;
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        if self.wallet_path.is_file() {
            self.open_session(self.wallet_path.clone());
        }
        while !self.quit {
            self.tick = self.tick.wrapping_add(1);
            // On the welcome screen, open the wallet as soon as the file appears (about once a second)
            if self.session.is_none()
                && self.modal.is_none()
                && self.wallet_error.is_none()
                && self.tick.is_multiple_of(8)
                && self.wallet_path.is_file()
            {
                self.open_session(self.wallet_path.clone());
            }
            terminal.draw(|f| self.draw(f))?;
            while let Ok(msg) = self.rx.try_recv() {
                self.handle(msg);
            }
            if event::poll(Duration::from_millis(120))? {
                match event::read()? {
                    TermEvent::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k.code, k.modifiers),
                    TermEvent::Mouse(m) => match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => self.on_click(m.column, m.row),
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            if m.kind == MouseEventKind::ScrollUp {
                                self.log_scroll = (self.log_scroll + 3).min(self.log.len().saturating_sub(1));
                            } else {
                                self.log_scroll = self.log_scroll.saturating_sub(3);
                            }
                        }
                        _ => {}
                    },
                    TermEvent::Paste(text) => {
                        if let Some(Modal::Form(form)) = &mut self.modal {
                            form.fields[form.focus].1.insert_str(text.trim());
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(s) = &mut self.session {
            s.close();
        }
        Ok(())
    }

    // ---------- session ----------

    fn open_session(&mut self, path: PathBuf) {
        if let Some(mut old) = self.session.take() {
            old.close();
        }
        let payer = match solana_keypair::read_keypair_file(&path) {
            Ok(k) => {
                self.wallet_error = None;
                k
            }
            Err(e) => {
                self.wallet_error = Some(e.to_string());
                self.modal = Some(Modal::Info {
                    title: "Cannot open wallet".into(),
                    lines: vec![format!("{}: {e}", tilde(&path))],
                });
                return;
            }
        };
        let rpc = match RpcPool::new(vec![self.network.default_rpc().to_string()]) {
            Ok(r) => r,
            Err(e) => return self.push_log(Kind::Bad, format!("RPC setup failed: {e}")),
        };
        self.next_gen += 1;
        let gen = self.next_gen;
        let chain = Arc::new(Chain::new(self.network, rpc));
        let payer = Arc::new(payer);
        self.push_log(Kind::Info, format!("Wallet {} on {}", payer.pubkey(), self.network.name()));

        let refresher_stop = Arc::new(AtomicBool::new(false));
        {
            let (chain, owner, tx, stop) = (chain.clone(), payer.pubkey(), self.tx.clone(), refresher_stop.clone());
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let r = chain.snapshot(&owner).await.map_err(|e| e.to_string());
                    if tx.send(Msg::Snapshot(gen, r)).is_err() {
                        break;
                    }
                    tokio::time::sleep(REFRESH).await;
                }
            });
        }
        self.wallet_path = path;
        self.session = Some(Session {
            gen,
            chain,
            payer,
            snapshot: None,
            minting: false,
            auto: None,
            rotation_stop: None,
            refresher_stop,
            pool_size: 0,
            minted: 0,
            ok: 0,
            failed: 0,
        });
        self.start_rotation();
    }

    fn refresh_now(&self) {
        if let Some(s) = &self.session {
            let (chain, owner, tx, gen) = (s.chain.clone(), s.payer.pubkey(), self.tx.clone(), s.gen);
            tokio::spawn(async move {
                let _ = tx.send(Msg::Snapshot(gen, chain.snapshot(&owner).await.map_err(|e| e.to_string())));
            });
        }
    }

    /// The session, if `gen` still refers to it
    fn current(&mut self, gen: u64) -> Option<&mut Session> {
        self.session.as_mut().filter(|s| s.gen == gen)
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Snapshot(gen, Ok(snap)) => {
                if let Some(s) = self.current(gen) {
                    s.snapshot = Some(snap);
                }
            }
            Msg::Snapshot(gen, Err(e)) => {
                if self.current(gen).is_some() {
                    self.push_log(Kind::Bad, format!("Refresh failed: {e}"));
                }
            }
            Msg::MintStarted(gen) => {
                if let Some(s) = self.current(gen) {
                    s.minting = true;
                }
            }
            Msg::Minted { gen, auto, result } => {
                let Some(s) = self.current(gen) else { return };
                s.minting = false;
                let ok = result.is_ok();
                if let Some(a) = s.auto.as_mut().filter(|_| auto) {
                    if ok {
                        a.ok += 1;
                    } else {
                        a.failed += 1;
                    }
                }
                match &result {
                    Ok((_, reward)) => {
                        s.ok += 1;
                        s.minted += reward;
                    }
                    Err(_) => s.failed += 1,
                }
                let network = s.chain.network;
                match result {
                    Ok((sig, reward)) => self.push_link(Kind::Good, format!("Minted {} MEMO", fmt_units(reward)), network.tx_url(&sig)),
                    Err(e) => self.push_log(Kind::Bad, format!("Mint failed: {e}")),
                }
                self.refresh_now();
            }
            Msg::Discovered(gen, r) => {
                let Some(s) = self.current(gen) else { return };
                s.pool_size = s.chain.rpc.endpoints().len();
                match r {
                    Ok((usable, advertised)) => {
                        self.push_log(Kind::Info, format!("RPC pool: {usable} usable of {advertised} advertised nodes"))
                    }
                    Err(e) => self.push_log(Kind::Bad, format!("RPC discovery failed: {e}")),
                }
            }
            Msg::AutoStopped(gen, reason) => {
                if let Some(s) = self.current(gen) {
                    s.auto = None;
                    self.push_log(Kind::Bad, format!("Automatic minting stopped: {reason}"));
                }
            }
            Msg::Sent { what, result } => {
                let network = self.network;
                let (title, lines) = match result {
                    Ok(sig) => {
                        self.push_link(Kind::Good, format!("Sent {what}"), network.tx_url(&sig));
                        (format!("Sent {what}"), vec![network.tx_url(&sig)])
                    }
                    Err(e) => {
                        self.push_log(Kind::Bad, format!("Sending {what} failed: {e}"));
                        (format!("Sending {what} failed"), vec![e])
                    }
                };
                self.modal = Some(Modal::Info { title, lines });
                self.refresh_now();
            }
            Msg::WalletCreated(Ok(w)) => self.modal = Some(Modal::Seed(w)),
            Msg::WalletCreated(Err(e)) => {
                self.modal = Some(Modal::Info { title: "Creating the wallet failed".into(), lines: vec![e] })
            }
            Msg::InstallLine(line) => {
                if let Some(Modal::Install { lines, .. }) = &mut self.modal {
                    lines.push(line);
                }
            }
            Msg::InstallDone(r) => {
                if let Ok(path) = &r {
                    self.keygen = Some(path.clone());
                    self.push_log(Kind::Good, format!("Solana CLI installed: {}", path.display()));
                }
                if let Some(Modal::Install { done, .. }) = &mut self.modal {
                    *done = Some(r);
                }
            }
        }
    }

    // ---------- input ----------

    fn on_click(&mut self, x: u16, y: u16) {
        let from = if self.modal.is_some() { self.modal_hits_from.min(self.hits.len()) } else { 0 };
        let pos = Position { x, y };
        let hit = self.hits[from..].iter().rev().find(|(r, _)| r.contains(pos)).map(|(_, a)| a.clone());
        if let Some(action) = hit {
            self.act(action);
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            return self.modal_key(code, mods);
        }
        let action = match code {
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Quit),
            _ if self.session.is_none() => match code {
                KeyCode::Char('c') | KeyCode::Char('n') => Some(Action::NewWallet),
                KeyCode::Char('r') => Some(Action::CheckAgain),
                KeyCode::Char('s') if self.keygen.is_none() => Some(Action::InstallCli),
                _ => None,
            },
            KeyCode::Char('1') => Some(Action::Tab(Tab::Mint)),
            KeyCode::Char('2') => Some(Action::Tab(Tab::Wallet)),
            KeyCode::Tab | KeyCode::BackTab => {
                Some(Action::Tab(if self.tab == Tab::Mint { Tab::Wallet } else { Tab::Mint }))
            }
            KeyCode::Char('m') => Some(Action::Mint),
            KeyCode::Char('a') => Some(Action::ToggleAuto),
            KeyCode::Char('r') => Some(Action::ToggleRotation),
            KeyCode::PageUp => {
                self.log_scroll = (self.log_scroll + 10).min(self.log.len().saturating_sub(1));
                None
            }
            KeyCode::PageDown => {
                self.log_scroll = self.log_scroll.saturating_sub(10);
                None
            }
            _ => match self.tab {
                Tab::Mint => (code == KeyCode::Enter).then_some(Action::Mint),
                Tab::Wallet => match code {
                    KeyCode::Char('c') => Some(Action::CopyAddress),
                    KeyCode::Char('x') => Some(Action::SendXnt),
                    KeyCode::Char('s') => Some(Action::SendMemo),
                    _ => None,
                },
            },
        };
        if let Some(a) = action {
            self.act(a);
        }
    }

    fn modal_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match self.modal.as_mut().unwrap() {
            Modal::Form(form) => match code {
                KeyCode::Esc => self.modal = None,
                KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % form.fields.len(),
                KeyCode::BackTab | KeyCode::Up => form.focus = (form.focus + form.fields.len() - 1) % form.fields.len(),
                KeyCode::Enter => {
                    if form.focus + 1 < form.fields.len() {
                        form.focus += 1;
                    } else {
                        self.act(Action::ModalOk);
                    }
                }
                _ => {
                    let f = form.focus;
                    form.fields[f].1.handle_key(code, mods);
                    form.error = None;
                }
            },
            Modal::Confirm { .. } => match code {
                KeyCode::Enter | KeyCode::Char('y') => self.act(Action::ModalOk),
                KeyCode::Esc | KeyCode::Char('n') => self.act(Action::ModalCancel),
                _ => {}
            },
            Modal::Info { .. } => {
                if matches!(code, KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ')) {
                    self.modal = None;
                }
            }
            Modal::Seed(_) => {
                if code == KeyCode::Enter {
                    self.act(Action::ModalOk);
                }
            }
            Modal::Install { done, .. } => {
                if done.is_some() && matches!(code, KeyCode::Enter | KeyCode::Esc) {
                    self.act(Action::ModalOk);
                }
            }
            Modal::Busy(_) => {}
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::Tab(t) => self.tab = t,
            Action::Quit => self.quit = true,
            Action::Mint => self.mint_once(),
            Action::ToggleAuto => self.toggle_auto(),
            Action::ToggleRotation => {
                let on = self.session.as_ref().is_none_or(|s| s.rotation_stop.is_none());
                self.set_rotation(on);
            }
            Action::CopyAddress => self.copy_address(),
            Action::SendXnt | Action::SendMemo => self.open_send_form(action == Action::SendXnt),
            Action::NewWallet => {
                if self.keygen.is_none() {
                    return self.ask_install();
                }
                let mut lines = vec![
                    "solana-keygen will create a new wallet file:".to_string(),
                    tilde(&self.wallet_path),
                ];
                if self.wallet_path.exists() {
                    lines.push(String::new());
                    lines.push("The current file there is kept, renamed with the date and time.".into());
                }
                self.modal = Some(Modal::Confirm {
                    title: "Create a new minting wallet".into(),
                    lines,
                    ok: "Create".into(),
                    pending: Pending::CreateWallet,
                });
            }
            Action::InstallCli => self.ask_install(),
            Action::CheckAgain => {
                self.keygen = setup::find_keygen();
                self.wallet_error = None;
                if self.wallet_path.is_file() {
                    self.open_session(self.wallet_path.clone());
                }
            }
            Action::FormField(i) => {
                if let Some(Modal::Form(form)) = &mut self.modal {
                    form.focus = i;
                }
            }
            Action::ModalCancel => {
                if !matches!(self.modal, Some(Modal::Seed(_) | Modal::Busy(_))) {
                    self.modal = None;
                }
            }
            Action::ModalOk => self.modal_ok(),
        }
    }

    fn ask_install(&mut self) {
        self.modal = Some(Modal::Confirm {
            title: "Install the Solana CLI".into(),
            lines: vec![
                "MEMO Miner uses the official Solana CLI (by Anza) to create and keep your wallet,".into(),
                "so it never handles seed phrases itself. It will run the official installer:".into(),
                String::new(),
                setup::install_description().into(),
                String::new(),
                format!("More: {}", setup::INSTALL_DOCS),
            ],
            ok: "Install".into(),
            pending: Pending::Install,
        });
    }

    fn modal_ok(&mut self) {
        match self.modal.take() {
            Some(Modal::Form(form)) => self.submit_form(form),
            Some(Modal::Confirm { pending, .. }) => self.run_pending(pending),
            Some(Modal::Seed(w)) => {
                if let Some(old) = &w.backup {
                    self.push_log(Kind::Info, format!("Previous wallet file kept as {}", tilde(old)));
                }
                self.push_log(Kind::Good, format!("New wallet {} saved to {}", w.address, tilde(&w.path)));
                self.open_session(w.path);
                self.tab = Tab::Wallet;
            }
            // The installer is still running, or work is in progress: stay
            Some(m @ (Modal::Install { done: None, .. } | Modal::Busy(_))) => self.modal = Some(m),
            _ => {}
        }
    }

    fn run_pending(&mut self, pending: Pending) {
        match pending {
            Pending::Install => {
                self.modal = Some(Modal::Install { lines: Vec::new(), done: None });
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let (line_tx, line_rx) = std::sync::mpsc::channel();
                    let forward = {
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            for line in line_rx {
                                let _ = tx.send(Msg::InstallLine(line));
                            }
                        })
                    };
                    let r = setup::install_solana_cli(line_tx).map_err(|e| e.to_string());
                    let _ = forward.join();
                    let _ = tx.send(Msg::InstallDone(r));
                });
            }
            Pending::CreateWallet => {
                let Some(keygen) = self.keygen.clone() else { return self.ask_install() };
                self.modal = Some(Modal::Busy("Creating the wallet with solana-keygen...".into()));
                if let Some(mut s) = self.session.take() {
                    s.close();
                }
                let (tx, path) = (self.tx.clone(), self.wallet_path.clone());
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send(Msg::WalletCreated(setup::create_wallet(&keygen, &path).map_err(|e| e.to_string())));
                });
            }
            Pending::SendXnt { to, lamports: amount } => {
                let Some(s) = &self.session else { return };
                let (chain, payer, tx) = (s.chain.clone(), s.payer.clone(), self.tx.clone());
                let what = format!("{} XNT to {}", lamports(amount), short_key(&to.to_string()));
                self.modal = Some(Modal::Busy(format!("Sending {what}...")));
                tokio::spawn(async move {
                    let result = chain.transfer_xnt(&payer, &to, amount).await.map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Sent { what, result });
                });
            }
            Pending::SendMemo { to, units } => {
                let Some(s) = &self.session else { return };
                let (chain, payer, tx) = (s.chain.clone(), s.payer.clone(), self.tx.clone());
                let what = format!("{} MEMO to {}", fmt_units(units), short_key(&to.to_string()));
                self.modal = Some(Modal::Busy(format!("Sending {what}...")));
                tokio::spawn(async move {
                    let result = chain.transfer_memo(&payer, &to, units).await.map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Sent { what, result });
                });
            }
        }
    }

    fn open_send_form(&mut self, xnt: bool) {
        if self.session.is_none() {
            return;
        }
        let (kind, title, hint) = if xnt {
            (FormKind::SendXnt, "Send XNT", "XNT pays the fees; keep some in the minting wallet.")
        } else {
            (FormKind::SendMemo, "Send MEMO", "Creates the recipient's MEMO account if needed (paid in XNT).")
        };
        self.modal = Some(Modal::Form(Form {
            kind,
            title: title.into(),
            hint: hint.into(),
            fields: vec![("To address".into(), TextInput::default()), ("Amount".into(), TextInput::default())],
            focus: 0,
            error: None,
        }));
    }

    fn submit_form(&mut self, mut form: Form) {
        let values: Vec<String> = form.fields.iter().map(|(_, i)| i.value().trim().to_string()).collect();
        let result: Result<(), String> = match form.kind {
            FormKind::SendXnt | FormKind::SendMemo => {
                let xnt = form.kind == FormKind::SendXnt;
                let decimals = if xnt { XNT_DECIMALS } else { MEMO_DECIMALS as u32 };
                match (Pubkey::from_str(&values[0]), parse_amount(&values[1], decimals)) {
                    (Err(_), _) => Err("Not a valid address".into()),
                    (_, None) | (_, Some(0)) => Err("Not a valid amount".into()),
                    (Ok(to), Some(amount)) => {
                        let (shown, pending) = if xnt {
                            (format!("{} XNT", lamports(amount)), Pending::SendXnt { to, lamports: amount })
                        } else {
                            (format!("{} MEMO", fmt_units(amount)), Pending::SendMemo { to, units: amount })
                        };
                        self.modal = Some(Modal::Confirm {
                            title: "Confirm transfer".into(),
                            lines: vec![
                                format!("Send   {shown}"),
                                format!("To     {to}"),
                                format!("On     X1 {}", self.network.name()),
                                String::new(),
                                "Transfers cannot be undone.".into(),
                            ],
                            ok: "Send".into(),
                            pending,
                        });
                        return;
                    }
                }
            }
        };
        if let Err(e) = result {
            form.error = Some(e);
            self.modal = Some(Modal::Form(form));
        }
    }

    // ---------- minting ----------

    fn mint_once(&mut self) {
        let tx = self.tx.clone();
        let Some(s) = self.session.as_mut().filter(|s| !s.minting && s.auto.is_none()) else { return };
        s.minting = true;
        let (chain, payer, gen) = (s.chain.clone(), s.payer.clone(), s.gen);
        tokio::spawn(async move {
            let result = mint(&chain, &payer).await;
            let _ = tx.send(Msg::Minted { gen, auto: false, result });
        });
    }

    fn toggle_auto(&mut self) {
        let tx = self.tx.clone();
        let Some(s) = self.session.as_mut() else { return };
        if let Some(a) = s.auto.take() {
            a.stop.store(true, Ordering::Relaxed);
            let msg = format!("Automatic minting stopped ({} ok, {} failed)", a.ok, a.failed);
            return self.push_log(Kind::Info, msg);
        }
        if s.minting {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        s.auto = Some(Auto { stop: stop.clone(), ok: 0, failed: 0 });
        let (chain, payer, gen) = (s.chain.clone(), s.payer.clone(), s.gen);
        self.push_log(Kind::Good, format!("Automatic minting started, every {} s", AUTO_INTERVAL.as_secs()));
        tokio::spawn(async move {
            let mut failures_in_row = 0;
            while !stop.load(Ordering::Relaxed) {
                if let Ok(b) = chain.xnt_balance(&payer.pubkey()).await {
                    if b < MIN_XNT_LAMPORTS {
                        let _ = tx.send(Msg::AutoStopped(gen, format!("XNT balance below {}", lamports(MIN_XNT_LAMPORTS))));
                        return;
                    }
                }
                let _ = tx.send(Msg::MintStarted(gen));
                let result = mint(&chain, &payer).await;
                failures_in_row = if result.is_ok() { 0 } else { failures_in_row + 1 };
                if tx.send(Msg::Minted { gen, auto: true, result }).is_err() {
                    return;
                }
                if failures_in_row >= 10 {
                    let _ = tx.send(Msg::AutoStopped(gen, "10 failures in a row".into()));
                    return;
                }
                // Back off after failures; check the stop flag while waiting
                let wait = AUTO_INTERVAL * (1 + failures_in_row.min(5));
                let until = tokio::time::Instant::now() + wait;
                while tokio::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        });
    }

    fn set_rotation(&mut self, on: bool) {
        let Some(s) = self.session.as_ref() else { return };
        if on == s.rotation_stop.is_some() {
            return;
        }
        if on {
            self.start_rotation();
        } else {
            let s = self.session.as_mut().unwrap();
            if let Some(stop) = s.rotation_stop.take() {
                stop.store(true, Ordering::Relaxed);
            }
            s.chain.rpc.set_rotation(false);
            let primary = short_url(s.chain.rpc.primary()).to_string();
            self.push_log(Kind::Info, format!("RPC rotation off, using {primary}"));
        }
    }

    fn start_rotation(&mut self) {
        let tx = self.tx.clone();
        let Some(s) = self.session.as_mut() else { return };
        let stop = Arc::new(AtomicBool::new(false));
        s.rotation_stop = Some(stop.clone());
        s.chain.rpc.set_rotation(true);
        s.pool_size = s.chain.rpc.endpoints().len();
        let (chain, gen) = (s.chain.clone(), s.gen);
        self.push_log(Kind::Info, "RPC rotation on, looking for RPC nodes advertised by the cluster...");
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let r = chain.rpc.discover().await.map_err(|e| e.to_string());
                if stop.load(Ordering::Relaxed) || tx.send(Msg::Discovered(gen, r)).is_err() {
                    return;
                }
                tokio::time::sleep(REDISCOVER).await;
            }
        });
    }

    fn copy_address(&mut self) {
        let Some(address) = self.session.as_ref().map(|s| s.payer.pubkey().to_string()) else { return };
        match self.clipboard.as_mut().map(|c| c.set_text(address.clone())) {
            Some(Ok(())) => self.push_log(Kind::Info, format!("Copied {address}")),
            _ => self.push_log(Kind::Bad, format!("No clipboard here; the address is {address}")),
        }
    }

    // ---------- drawing ----------

    fn hit(&mut self, area: Rect, action: Action) {
        self.hits.push((area, action));
    }

    fn spinner(&self) -> &'static str {
        SPINNER[self.tick % SPINNER.len()]
    }

    /// Pill buttons from `area`'s left edge: an optional keycap, then the label
    fn buttons(&mut self, f: &mut Frame, area: Rect, buttons: &[Button]) {
        let mut x = area.x;
        for b in buttons {
            let key = b.key.map(|k| format!(" {k} ")).unwrap_or_default();
            let label = format!(" {} ", b.label);
            let w = (key.chars().count() + label.chars().count()) as u16;
            if x + w > area.right() {
                break;
            }
            let (key_style, label_style) = if b.primary {
                (
                    Style::new().fg(Color::Black).bg(ACCENT_DEEP).add_modifier(Modifier::BOLD),
                    Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD),
                )
            } else {
                (Style::new().fg(ACCENT).bg(KEY_BG).add_modifier(Modifier::BOLD), Style::new().fg(TEXT).bg(BUTTON_BG))
            };
            let r = Rect { x, y: area.y, width: w, height: 1 };
            f.render_widget(Line::from(vec![Span::styled(key, key_style), Span::styled(label, label_style)]), r);
            self.hit(r, b.action.clone());
            x += w + 2;
        }
    }

    /// A rounded card with a muted title; returns the inside with one column of padding
    fn card(f: &mut Frame, area: Rect, title: &str) -> Rect {
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(LINE))
            .title(Span::styled(format!(" {title} "), Style::new().fg(MUTED)));
        let inner = block.inner(area);
        f.render_widget(block, area);
        Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner }
    }

    fn draw(&mut self, f: &mut Frame) {
        self.hits.clear();
        self.cursor = None;
        let [header, _, body, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let body = Rect { x: body.x + 1, width: body.width.saturating_sub(2), ..body };

        self.draw_header(f, header);
        if self.session.is_none() {
            self.draw_setup(f, body);
        } else {
            match self.tab {
                Tab::Mint => self.draw_mint(f, body),
                Tab::Wallet => self.draw_wallet(f, body),
            }
        }
        self.draw_footer(f, footer);

        self.modal_hits_from = self.hits.len();
        if self.modal.is_some() {
            self.draw_modal(f, f.area());
        }
        if let Some(p) = self.cursor {
            f.set_cursor_position(p);
        }
    }

    fn draw_header(&mut self, f: &mut Frame, area: Rect) {
        let row = Rect { height: 1, y: area.y, ..area };
        let mut left = vec![
            Span::raw(" "),
            Span::styled("▮", Style::new().fg(ACCENT)),
            Span::styled("▮", Style::new().fg(PINK)),
            Span::raw(" "),
            Span::styled("MEMO", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(" MINER", Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
        ];
        if area.width > 100 {
            left.push(Span::styled("   MEMO Engraves Memories Onchain", Style::new().fg(FAINT)));
        }
        f.render_widget(Line::from(left), row);

        // Right side: tabs, then the network chip
        let net_color = if self.network == Network::Mainnet { ACCENT } else { PINK };
        let net = format!(" ● {} ", self.network.name());
        let mut parts: Vec<(String, Style, Option<Action>)> = Vec::new();
        if self.session.is_some() {
            for (i, tab) in Tab::ALL.iter().enumerate() {
                let style = if *tab == self.tab {
                    Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(MUTED).bg(BUTTON_BG)
                };
                parts.push((format!(" {} {} ", i + 1, tab.name()), style, Some(Action::Tab(*tab))));
            }
        }
        parts.push((net, Style::new().fg(net_color).bg(KEY_BG), None));
        let total: u16 = parts.iter().map(|(t, _, _)| t.chars().count() as u16).sum::<u16>() + parts.len() as u16;
        let mut x = row.right().saturating_sub(total + 1);
        for (text, style, action) in parts {
            let w = text.chars().count() as u16;
            let r = Rect { x, y: row.y, width: w, height: 1 };
            f.render_widget(Span::styled(text, style), r);
            if let Some(a) = action {
                self.hit(r, a);
            }
            x += w + 1;
        }
        f.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).fg(LINE),
            Rect { y: area.y + 1, height: 1, ..area },
        );
    }

    fn draw_footer(&mut self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = if self.modal.is_some() {
            &[("Enter", "confirm"), ("Esc", "cancel"), ("Tab", "next field")]
        } else if self.session.is_none() {
            if self.keygen.is_none() {
                &[("s", "install Solana CLI"), ("r", "check again"), ("q", "quit")]
            } else {
                &[("c", "create wallet"), ("r", "check again"), ("q", "quit")]
            }
        } else {
            match self.tab {
                Tab::Mint => &[("m", "mint"), ("a", "auto"), ("r", "rotation"), ("PgUp/PgDn", "log"), ("1/2", "tabs"), ("q", "quit")],
                Tab::Wallet => &[("c", "copy"), ("x", "send XNT"), ("s", "send MEMO"), ("q", "quit")],
            }
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, label) in keys {
            spans.push(Span::styled(format!(" {k} "), Style::new().fg(MUTED).bg(KEY_BG)));
            spans.push(Span::styled(format!(" {label}   "), Style::new().fg(FAINT)));
        }
        f.render_widget(Line::from(spans), area);
        let mouse = "mouse: click ";
        if area.width > 90 {
            f.render_widget(
                Paragraph::new(mouse).fg(FAINT).alignment(Alignment::Right),
                area,
            );
        }
    }

    fn logo(&self) -> Vec<Line<'static>> {
        LOGO.iter()
            .map(|row| {
                Line::from(
                    row.chars()
                        .map(|c| match c {
                            'X' => Span::styled("██", Style::new().fg(ACCENT)),
                            'P' => Span::styled("██", Style::new().fg(PINK)),
                            _ => Span::raw("  "),
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    fn draw_setup(&mut self, f: &mut Frame, area: Rect) {
        let card = centered(area, 86, 19);
        let inner = Self::card(f, card, "Welcome");
        let [_, content, _, buttons] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(8), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        let [logo, _, text] =
            Layout::horizontal([Constraint::Length(12), Constraint::Length(3), Constraint::Min(20)]).areas(content);
        f.render_widget(Paragraph::new(self.logo()), Rect { x: logo.x + 1, ..logo });

        let check = |ok: bool| {
            if ok {
                Span::styled("✓ ", Style::new().fg(ACCENT))
            } else {
                Span::styled("○ ", Style::new().fg(PINK))
            }
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("MEMO", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(" MINER", Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
            ]),
            Line::from("MEMO Engraves Memories Onchain").fg(MUTED),
            Line::from(""),
            Line::from("Mint MEMO on X1 from this terminal. Each mint writes a memo onchain.").fg(TEXT),
            Line::from(""),
        ];
        lines.push(Line::from(vec![
            check(self.keygen.is_some()),
            Span::styled("Solana CLI   ", Style::new().fg(MUTED)),
            match &self.keygen {
                Some(_) => Span::styled("installed", Style::new().fg(TEXT)),
                None => Span::styled("not installed", Style::new().fg(PINK)),
            },
        ]));
        lines.push(Line::from(vec![
            check(false),
            Span::styled("Wallet       ", Style::new().fg(MUTED)),
            Span::styled(
                if self.wallet_error.is_some() {
                    format!("{} cannot be read", tilde(&self.wallet_path))
                } else if self.wallet_path.exists() {
                    "found, opening...".into()
                } else {
                    "not set up yet".into()
                },
                Style::new().fg(TEXT),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("To mint with a keypair you already have, copy it to ", Style::new().fg(MUTED)),
            Span::styled(tilde(&self.wallet_path), Style::new().fg(ACCENT)),
            Span::styled(". It opens automatically.", Style::new().fg(MUTED)),
        ]));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), text);

        let mut list = Vec::new();
        if self.keygen.is_some() {
            list.push(Button::new(Some("c"), "Create minting wallet", Action::NewWallet, true));
        } else {
            list.push(Button::new(Some("s"), "Install Solana CLI", Action::InstallCli, true));
        }
        list.push(Button::new(Some("r"), "Check again", Action::CheckAgain, false));
        list.push(Button::new(Some("q"), "Quit", Action::Quit, false));
        self.buttons(f, Rect { x: text.x, width: inner.right().saturating_sub(text.x), ..buttons }, &list);
    }

    fn draw_mint(&mut self, f: &mut Frame, area: Rect) {
        let s = self.session.as_ref().unwrap();
        let [cards, supply, _, controls, _, log] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .areas(area);
        let [c1, c2, c3] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).spacing(1).areas(cards);
        let big = |v: String, unit: &str| {
            Line::from(vec![
                Span::styled(v, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {unit}"), Style::new().fg(MUTED)),
            ])
        };
        let min = MIN_XNT_LAMPORTS;

        // Balance
        let inner = Self::card(f, c1, "Balance");
        let lines = match &s.snapshot {
            Some(snap) => vec![
                big(fmt_units_short(snap.memo, 2), "MEMO"),
                Line::from(vec![
                    Span::styled(lamports(snap.xnt), Style::new().fg(BLUE)),
                    Span::styled(" XNT", Style::new().fg(MUTED)),
                ]),
                if snap.xnt < min {
                    Line::from("Low XNT for fees").fg(PINK)
                } else {
                    Line::from(short_key(&s.payer.pubkey().to_string())).fg(FAINT)
                },
            ],
            None => vec![Line::from(format!("{} reading", self.spinner())).fg(MUTED)],
        };
        f.render_widget(Paragraph::new(lines), inner);

        // Reward
        let inner = Self::card(f, c2, "Reward per mint");
        let lines = match &s.snapshot {
            Some(snap) => {
                let (_, end, next) = curve::step(snap.supply);
                let mut l = vec![big(fmt_units(reward_at(snap.supply)), "MEMO")];
                if let Some(next) = next {
                    l.push(Line::from(format!("then {} after {}", fmt_units(next), curve::fmt_supply_short(end))).fg(TEXT));
                    l.push(Line::from(format!("{} MEMO to go", fmt_units_short(end.saturating_sub(snap.supply), 0))).fg(FAINT));
                }
                l
            }
            None => vec![Line::from(format!("{} reading", self.spinner())).fg(MUTED)],
        };
        f.render_widget(Paragraph::new(lines), inner);

        // Session
        let inner = Self::card(f, c3, "This session");
        let rpc = if s.chain.rpc.rotation() {
            match s.pool_size.max(1) {
                1 => "RPC: 1 endpoint, finding more...".to_string(),
                n => format!("RPC: {n} endpoints"),
            }
        } else {
            format!("RPC: {}", short_url(s.chain.rpc.primary()))
        };
        f.render_widget(
            Paragraph::new(vec![
                big(format!("+{}", fmt_units_short(s.minted, 2)), "MEMO"),
                Line::from(vec![
                    Span::styled(format!("✓ {}", s.ok), Style::new().fg(ACCENT)),
                    Span::raw("   "),
                    Span::styled(format!("✕ {}", s.failed), Style::new().fg(if s.failed > 0 { PINK } else { FAINT })),
                ]),
                Line::from(rpc).fg(FAINT),
            ]),
            inner,
        );

        // Supply within the current reward step
        let inner = Self::card(f, supply, "Supply");
        if let Some(snap) = &s.snapshot {
            let (start, end, _) = curve::step(snap.supply);
            let ratio = (snap.supply - start) as f64 / (end - start).max(1) as f64;
            let label = format!("{} MEMO", fmt_units_short(snap.supply, 0));
            let range = format!("{} → {}  {:.1}%", curve::fmt_supply_short(start), curve::fmt_supply_short(end), ratio * 100.0);
            let [l, g, r] = Layout::horizontal([
                Constraint::Length(label.len() as u16 + 2),
                Constraint::Min(10),
                Constraint::Length(range.chars().count() as u16 + 2),
            ])
            .areas(inner);
            f.render_widget(Span::styled(label, Style::new().fg(TEXT).add_modifier(Modifier::BOLD)), l);
            f.render_widget(
                LineGauge::default()
                    .ratio(ratio.clamp(0.0, 1.0))
                    .label("")
                    .filled_style(Style::new().fg(ACCENT))
                    .unfilled_style(Style::new().fg(LINE))
                    .line_set(symbols::line::THICK),
                g,
            );
            f.render_widget(Paragraph::new(range).fg(MUTED).alignment(Alignment::Right), r);
        }

        let auto_on = s.auto.is_some();
        let rotation_on = s.rotation_stop.is_some();
        let state = match &s.auto {
            Some(a) => Line::from(vec![
                Span::styled(format!("{} ", self.spinner()), Style::new().fg(ACCENT)),
                Span::styled("Auto minting", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!("   {} ok · {} failed · every {} s", a.ok, a.failed, AUTO_INTERVAL.as_secs()),
                    Style::new().fg(MUTED),
                ),
            ]),
            None if s.minting => Line::from(vec![
                Span::styled(format!("{} ", self.spinner()), Style::new().fg(ACCENT)),
                Span::styled("Minting...", Style::new().fg(ACCENT)),
            ]),
            None => Line::from(vec![Span::styled("○ ", Style::new().fg(FAINT)), Span::styled("Idle", Style::new().fg(MUTED))]),
        };
        f.render_widget(state.alignment(Alignment::Right), Rect { width: controls.width.saturating_sub(1), ..controls });

        self.buttons(
            f,
            Rect { x: controls.x + 1, ..controls },
            &[
                Button::new(Some("m"), "Mint once", Action::Mint, false),
                Button::new(Some("a"), if auto_on { "Stop auto minting" } else { "Start auto minting" }, Action::ToggleAuto, !auto_on),
                Button::new(Some("r"), if rotation_on { "RPC rotation ● on" } else { "RPC rotation ○ off" }, Action::ToggleRotation, false),
            ],
        );
        self.draw_log(f, log);
    }

    fn draw_log(&mut self, f: &mut Frame, area: Rect) {
        let title = if self.log_scroll > 0 { format!("Activity  ↑{}", self.log_scroll) } else { "Activity".into() };
        let inner = Self::card(f, area, &title);
        let items: Vec<ListItem> = self
            .log
            .iter()
            .skip(self.log_scroll)
            .map(|e| {
                let (icon, color) = match e.kind {
                    Kind::Good => ("✓", ACCENT),
                    Kind::Bad => ("✕", PINK),
                    Kind::Info => ("·", MUTED),
                };
                let mut spans = vec![
                    Span::styled(format!("{}  ", e.time), Style::new().fg(FAINT)),
                    Span::styled(format!("{icon} "), Style::new().fg(color)),
                    Span::styled(e.text.clone(), Style::new().fg(if e.kind == Kind::Bad { PINK } else { TEXT })),
                ];
                if let Some(link) = &e.link {
                    spans.push(Span::styled(format!("  {link}"), Style::new().fg(FAINT)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        f.render_widget(List::new(items), inner);
    }

    fn draw_wallet(&mut self, f: &mut Frame, area: Rect) {
        let s = self.session.as_ref().unwrap();
        let [top, balances, _, row1, _, note, log] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .areas(area);
        let label = |t: &str| Span::styled(format!("{t:<10}"), Style::new().fg(MUTED));
        let path = tilde(&self.wallet_path);
        let inner = Self::card(f, top, "Minting wallet");
        f.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    label("Address"),
                    Span::styled(s.payer.pubkey().to_string(), Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![label("File"), Span::styled(path, Style::new().fg(TEXT))]),
                Line::from(vec![label("Network"), Span::styled(format!("X1 {}", self.network.name()), Style::new().fg(TEXT))]),
            ]),
            inner,
        );

        let [b1, b2] = Layout::horizontal([Constraint::Ratio(1, 2); 2]).spacing(1).areas(balances);
        let amount = |v: Option<String>, unit: &str, color: Color| match v {
            Some(v) => Line::from(vec![
                Span::styled(v, Style::new().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {unit}"), Style::new().fg(MUTED)),
            ]),
            None => Line::from("...").fg(MUTED),
        };
        let (xnt, memo) = match &s.snapshot {
            Some(snap) => (Some(lamports(snap.xnt)), Some(fmt_units(snap.memo))),
            None => (None, None),
        };
        let inner = Self::card(f, b1, "MEMO");
        f.render_widget(Paragraph::new(vec![amount(memo, "MEMO", ACCENT), Line::from("minted and received").fg(FAINT)]), inner);
        let inner = Self::card(f, b2, "XNT");
        f.render_widget(Paragraph::new(vec![amount(xnt, "XNT", BLUE), Line::from("pays the fee for each mint").fg(FAINT)]), inner);

        let list = [
            Button::new(Some("c"), "Copy address", Action::CopyAddress, false),
            Button::new(Some("x"), "Send XNT", Action::SendXnt, false),
            Button::new(Some("s"), "Send MEMO", Action::SendMemo, false),
        ];
        self.buttons(f, Rect { x: row1.x + 1, ..row1 }, &list);
        f.render_widget(
            Line::from(vec![
                Span::styled(" ! ", Style::new().fg(PINK)),
                Span::styled(
                    "Anyone with the keypair file or its seed phrase controls this wallet. Keep only what minting needs here.",
                    Style::new().fg(FAINT),
                ),
            ]),
            note,
        );
        self.draw_log(f, log);
    }

    fn draw_modal(&mut self, f: &mut Frame, area: Rect) {
        let modal = self.modal.take().unwrap();
        match &modal {
            Modal::Form(form) => {
                let h = 8 + form.fields.len() as u16 * 2 + u16::from(form.error.is_some());
                let r = centered(area, 82, h);
                let inner = self.modal_frame(f, r, &form.title);
                let mut y = inner.y;
                f.render_widget(Paragraph::new(form.hint.clone()).fg(MUTED), Rect { height: 1, y, ..inner });
                y += 2;
                for (i, (label, input)) in form.fields.iter().enumerate() {
                    let lw = 13u16;
                    let focused = i == form.focus;
                    f.render_widget(
                        Span::styled(label.clone(), Style::new().fg(if focused { TEXT } else { MUTED })),
                        Rect { x: inner.x, y, width: lw.min(inner.width), height: 1 },
                    );
                    let field = Rect { x: inner.x + lw, y, width: inner.width.saturating_sub(lw), height: 1 };
                    let (text, cur) = input.view(field.width.saturating_sub(3) as usize);
                    let (bar, bg) = if focused { (ACCENT, INPUT_FOCUS) } else { (LINE, INPUT_BG) };
                    f.render_widget(
                        Line::from(vec![Span::styled("▎", Style::new().fg(bar).bg(bg)), Span::styled(format!(" {text}"), Style::new().fg(TEXT))])
                            .style(Style::new().bg(bg)),
                        field,
                    );
                    if focused {
                        self.cursor = Some(Position { x: field.x + 2 + cur as u16, y });
                    }
                    self.hit(Rect { x: inner.x, y, width: inner.width, height: 1 }, Action::FormField(i));
                    y += 2;
                }
                if let Some(e) = &form.error {
                    f.render_widget(Line::from(format!("✕ {e}")).fg(PINK), Rect { y, height: 1, ..inner });
                }
                let ok = match form.kind {
                    FormKind::SendXnt | FormKind::SendMemo => "Next",
                };
                self.buttons(
                    f,
                    Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
                    &[Button::new(Some("Enter"), ok, Action::ModalOk, true), Button::new(Some("Esc"), "Cancel", Action::ModalCancel, false)],
                );
            }
            Modal::Confirm { title, lines, ok, .. } => {
                let r = centered(area, 92, lines.len() as u16 + 6);
                let inner = self.modal_frame(f, r, title);
                let text: Vec<Line> = lines.iter().map(|l| Line::from(l.clone()).fg(TEXT)).collect();
                f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
                self.buttons(
                    f,
                    Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
                    &[Button::new(Some("Enter"), ok, Action::ModalOk, true), Button::new(Some("Esc"), "Cancel", Action::ModalCancel, false)],
                );
            }
            Modal::Info { title, lines } => {
                let r = centered(area, 104, lines.len() as u16 + 7);
                let inner = self.modal_frame(f, r, title);
                let text: Vec<Line> = lines.iter().map(|l| Line::from(l.clone()).fg(TEXT)).collect();
                f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
                self.buttons(
                    f,
                    Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
                    &[Button::new(Some("Enter"), "OK", Action::ModalOk, true)],
                );
            }
            Modal::Busy(text) => {
                let r = centered(area, 80, 5);
                let inner = self.modal_frame(f, r, "Working");
                f.render_widget(
                    Line::from(vec![
                        Span::styled(format!("{} ", self.spinner()), Style::new().fg(ACCENT)),
                        Span::styled(text.clone(), Style::new().fg(TEXT)),
                    ]),
                    inner,
                );
            }
            Modal::Seed(w) => {
                let r = centered(area, 86, 20);
                let inner = self.modal_frame(f, r, "Your new minting wallet");
                let label = |t: &str| Span::styled(format!("{t:<9}"), Style::new().fg(MUTED));
                let mut lines = vec![
                    Line::from(vec![label("Address"), Span::styled(w.address.clone(), Style::new().fg(TEXT).add_modifier(Modifier::BOLD))]),
                    Line::from(vec![label("File"), Span::styled(tilde(&w.path), Style::new().fg(TEXT))]),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("! ", Style::new().fg(PINK)),
                        Span::styled("Seed phrase from solana-keygen. Write it down on paper and keep it offline.", Style::new().fg(PINK)),
                    ]),
                    Line::from(""),
                ];
                let rows = w.seed_phrase.len().div_ceil(3);
                for row in 0..rows {
                    let mut spans = vec![Span::raw("  ")];
                    for col in 0..3 {
                        let i = col * rows + row;
                        if let Some(word) = w.seed_phrase.get(i) {
                            spans.push(Span::styled(format!("{:>2} ", i + 1), Style::new().fg(FAINT)));
                            spans.push(Span::styled(format!(" {word:<14}"), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
                            spans.push(Span::raw("   "));
                        }
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
                lines.push(Line::from("It is shown only now and is not saved anywhere else.").fg(FAINT));
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
                self.buttons(
                    f,
                    Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
                    &[Button::new(Some("Enter"), "I have written it down", Action::ModalOk, true)],
                );
            }
            Modal::Install { lines, done } => {
                let r = centered(area, 104, area.height.saturating_sub(4).max(10));
                let inner = self.modal_frame(f, r, "Installing the Solana CLI");
                let log_h = inner.height.saturating_sub(3) as usize;
                let shown: Vec<Line> =
                    lines.iter().skip(lines.len().saturating_sub(log_h)).map(|l| Line::from(l.clone()).fg(MUTED)).collect();
                f.render_widget(Paragraph::new(shown), Rect { height: log_h as u16, ..inner });
                let status = match done {
                    None => Line::from(vec![
                        Span::styled(format!("{} ", self.spinner()), Style::new().fg(ACCENT)),
                        Span::styled("Running the official installer...", Style::new().fg(TEXT)),
                    ]),
                    Some(Ok(p)) => Line::from(format!("✓ Installed: {}", p.display())).fg(ACCENT),
                    Some(Err(e)) => Line::from(format!("✕ Failed: {e}. See {}", setup::INSTALL_DOCS)).fg(PINK),
                };
                f.render_widget(status, Rect { y: inner.bottom().saturating_sub(2), height: 1, ..inner });
                if done.is_some() {
                    self.buttons(
                        f,
                        Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
                        &[Button::new(Some("Enter"), "Close", Action::ModalOk, true)],
                    );
                }
            }
        }
        self.modal = Some(modal);
    }

    /// Clear the area, draw a rounded box with a title, return the inside with padding
    fn modal_frame(&mut self, f: &mut Frame, r: Rect, title: &str) -> Rect {
        f.render_widget(Clear, r);
        let block = Block::new()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(ACCENT))
            .title(Span::styled(format!(" {title} "), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
            .style(Style::new().bg(MODAL_BG));
        let inner = block.inner(r);
        f.render_widget(block, r);
        Rect { x: inner.x + 2, y: inner.y + 1, width: inner.width.saturating_sub(4), height: inner.height.saturating_sub(1) }
    }
}

struct Button {
    key: Option<&'static str>,
    label: String,
    action: Action,
    primary: bool,
}

impl Button {
    fn new(key: Option<&'static str>, label: &str, action: Action, primary: bool) -> Self {
        Self { key, label: label.to_string(), action, primary }
    }
}

/// One mint: the reward is read from the supply just before sending
async fn mint(chain: &Chain, payer: &Keypair) -> Result<(String, u64), String> {
    let supply = chain.supply().await.map_err(|e| e.to_string())?;
    let reward = reward_at(supply);
    if reward == 0 {
        return Err(format!("Minting has ended at the {} MEMO cap", fmt_units(curve::MAX_SUPPLY)));
    }
    let memo = memo_for_mint();
    debug_assert_eq!(memo.len(), MIN_MEMO_BYTES);
    chain.mint(payer, &memo).await.map(|sig| (sig, reward)).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memo_is_the_minimum_length() {
        assert_eq!(memo_for_mint().len(), MIN_MEMO_BYTES);
    }
}

# MEMO Miner

**MEMO Engraves Memories Onchain.** MEMO Miner mints MEMO on the X1 chain from your terminal. Each mint is a transaction that writes a memo onchain and pays you the current MEMO reward.

- Manual minting: one mint per key press.
- Automatic minting: mints in a loop until you stop it.
- RPC rotation: spreads requests over the RPC nodes the X1 cluster advertises, so a single endpoint's rate limit does not stop you.
- Runs on Windows, macOS and Linux, in a terminal.
- Your wallet is created by `solana-keygen` from the official [Solana CLI](https://docs.anza.xyz/cli/install) (Anza), which ships inside every release archive. Nothing else to install. MEMO Miner never generates keys itself.

![Automatic minting on the Mint tab](screenshot/4_mint_auto.png)

## Install

Download the archive for your system from [Releases](https://github.com/xenartist/memo-miner/releases), unpack it and run `memo-miner` (`memo-miner.exe` on Windows) in a terminal. Keep `solana-keygen` in the same folder: MEMO Miner uses it to create your wallet.

| System | Archive |
| --- | --- |
| Windows | `x86_64-pc-windows-msvc.zip` |
| macOS, Apple silicon (M1 and later) | `aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `x86_64-apple-darwin.tar.gz` |
| Linux, x86_64 | `x86_64-unknown-linux-gnu.tar.gz` |
| Linux, ARM64 | `aarch64-unknown-linux-gnu.tar.gz` |

The builds are not signed with an Apple or Microsoft certificate, so the first run needs one extra step.

### macOS: first run

```bash
cd ~/Downloads
tar xzf memo-miner-*-apple-darwin.tar.gz
cd memo-miner-*-apple-darwin
xattr -dr com.apple.quarantine .   # let macOS run memo-miner and solana-keygen, downloaded from the web
./memo-miner
```

If you skipped `xattr` and macOS says "memo-miner" cannot be opened because Apple cannot check it for malicious software:

1. Close the message (do not move the file to the Bin).
2. Open System Settings > Privacy & Security, scroll to Security and click **Open Anyway** next to "memo-miner" was blocked.
3. Run `./memo-miner` again and choose **Open** when asked.

macOS asks the same for `solana-keygen` the first time you create a wallet. Running the `xattr` line above avoids both.

### Windows: first run

1. Right-click the downloaded `.zip`, choose **Properties**, tick **Unblock** and click OK. This keeps Windows from flagging the program later.
2. Right-click the `.zip` again, choose **Extract All**.
3. Open [Windows Terminal](https://aka.ms/terminal) (built into Windows 11) in the extracted folder: right-click inside the folder and choose **Open in Terminal**.
4. Run:

   ```powershell
   .\memo-miner.exe
   ```

If a blue "Windows protected your PC" (SmartScreen) window appears, click **More info**, then **Run anyway**.

Windows Terminal is recommended over the old Command Prompt window: colours, mouse clicks and the layout display correctly there.

### Build from source

With [Rust](https://rustup.rs) installed:

```bash
cargo install --git https://github.com/xenartist/memo-miner
cargo install solana-keygen --locked   # on Linux, first: sudo apt install build-essential pkg-config libudev-dev
```

A build from source finds `solana-keygen` on your PATH, in `~/.cargo/bin` or in an existing Solana CLI install.

## Using it

Run `memo-miner`. Everything happens inside the terminal app, with the keyboard or the mouse. There is nothing to configure.

### First run

1. MEMO Miner finds `solana-keygen` next to itself.
2. **Create minting wallet** runs `solana-keygen new` and shows the new address and seed phrase once.
   Write the seed phrase down and keep it offline.
3. Send a little XNT to the wallet address. XNT pays the fee for each mint.

| Welcome | Create minting wallet | Seed phrase |
| --- | --- | --- |
| ![Welcome screen](screenshot/1_welcome.png) | ![Create minting wallet confirmation](screenshot/2_create_confirm.png) | ![New wallet with its seed phrase](screenshot/3_seed.png) |

The seed phrase in the screenshot belongs to a throwaway wallet made for the screenshot. It has never held funds. Do not use it.

MEMO Miner always mints with one wallet: `~/.config/solana/memo-miner.json`, next to the Solana CLI's own keypairs (`%USERPROFILE%\.config\solana\memo-miner.json` on Windows). It opens automatically every time.

To mint with a keypair you already have, copy it to that path, before starting or while the welcome screen is open (it opens automatically):

```bash
cp ~/.config/solana/your-keypair.json ~/.config/solana/memo-miner.json
```

If `memo-miner.json` exists but cannot be read, **Create minting wallet** renames it with the date and time (`memo-miner-20261024-221500.json`) instead of overwriting it.

### Tabs

| Tab | What you can do | Keys |
| --- | --- | --- |
| 1 Mint | Mint once, start/stop automatic minting, turn RPC rotation on/off, read the activity log | `m`, `a`, `r`, PgUp/PgDn or the mouse wheel |
| 2 Wallet | Copy the address, send XNT, send MEMO | `c`, `x`, `s` |

Click any button or tab, or press its key. Switch tabs with `1`/`2` or Tab. Quit with `q`.

Transfers show a confirmation first.

| Wallet tab | Send MEMO |
| --- | --- |
| ![Wallet tab](screenshot/5_wallet.png) | ![Send MEMO form](screenshot/6_send_memo.png) |

The app captures the mouse. To select text in the terminal, hold Shift (Option in macOS Terminal and iTerm2) while dragging.

### Fixed behaviour

- **Network:** X1 mainnet.
- **RPC rotation:** on. Requests are spread over the public endpoint `rpc.mainnet.x1.xyz` and the RPC nodes listed by `getClusterNodes` that answer and are close to the tip. The node list is refreshed every 10 minutes. `r` turns rotation off for the current run.
  Most advertised nodes are plain HTTP. Signed transactions cannot be altered in transit, but balances shown while rotating come from whichever node answered.
- **Automatic minting:** a mint every 5 seconds after the previous one confirms. It stops on its own when the XNT balance falls below 0.01 XNT, or after 10 failed mints in a row.
- **Memo:** 69 bytes, the minimum memo-mint accepts, which keeps compute units and fees lowest:
  `{"action":"mint","platform":"memo-miner","chain":"X1","t":<unix seconds>}`

## How a mint is built

Each mint is one transaction with these instructions, in order:

1. An SPL Memo instruction at index 0 (69-800 bytes).
2. Only when the wallet has no MEMO account yet: an idempotent creation of its Token-2022 MEMO account.
3. memo-mint `process_mint`.
4. A compute unit limit: the units a simulation used plus 50. The fee follows this limit, so the margin is kept small.

The miner waits for each mint to confirm before sending the next.

## License

MIT. Release archives also include `solana-keygen` from [Agave](https://github.com/anza-xyz/agave), built unmodified from its published source, under the Apache License 2.0 (`LICENSE-solana-keygen`).

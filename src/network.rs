//! X1 networks and the MEMO program addresses on each.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
}

pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const ATA_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
pub const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
        }
    }

    /// The public RPC endpoint every user starts with
    pub fn default_rpc(self) -> &'static str {
        match self {
            Network::Mainnet => "https://rpc.mainnet.x1.xyz",
            Network::Testnet => "https://rpc.testnet.x1.xyz",
        }
    }

    pub fn mint_program_id(self) -> &'static str {
        match self {
            Network::Mainnet => "8iq6zqaEVcfaym2u8t939PAN5jmfPVc6Z333RuxKTTZX",
            Network::Testnet => "A31a17bhgQyRQygeZa1SybytjbCdjMpu6oPr9M3iQWzy",
        }
    }

    pub fn token_mint(self) -> &'static str {
        match self {
            Network::Mainnet => "memoX1sJsBY6od7CfQ58XooRALwnocAZen4L7mW1ick",
            Network::Testnet => "HLCoc7wNDavNMfWWw2Bwd7U7A24cesuhBSNkxZgvZm1",
        }
    }

    pub fn tx_url(self, signature: &str) -> String {
        match self {
            Network::Mainnet => format!("https://explorer.x1.xyz/tx/{signature}"),
            Network::Testnet => format!("https://explorer.testnet.x1.xyz/tx/{signature}"),
        }
    }
}

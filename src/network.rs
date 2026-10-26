//! X1 networks. The MEMO addresses on each come from memo-sdk.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Mainnet,
    Testnet,
}


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

    /// The same network in memo-sdk, which knows the MEMO addresses
    pub fn sdk(self) -> memo_sdk::Network {
        match self {
            Network::Mainnet => memo_sdk::Network::Mainnet,
            Network::Testnet => memo_sdk::Network::Testnet,
        }
    }

    pub fn tx_url(self, signature: &str) -> String {
        match self {
            Network::Mainnet => format!("https://explorer.x1.xyz/tx/{signature}"),
            Network::Testnet => format!("https://explorer.testnet.x1.xyz/tx/{signature}"),
        }
    }
}

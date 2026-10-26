//! The memo-mint emission curve (amounts in units, 1 MEMO = 1,000,000 units), from memo-sdk,
//! and how the miner formats amounts.
//!
//! Up to and including 10M supply every mint pays 1 MEMO; above 10M: 0.9, ..., above 90M: 0.1,
//! above 100M: 0.09, ... down to 1 unit above 9T. Minting stops at the 10T cap.

pub use memo_sdk::mint::{reward_at, MAX_SUPPLY};
pub use memo_sdk::UNITS_PER_MEMO as UNITS_PER_TOKEN;

/// The reward step this supply is in: (start, end], the reward after `end` (None at the last step)
pub fn step(supply: u64) -> (u64, u64, Option<u64>) {
    let s = memo_sdk::mint::step(supply);
    (s.start, s.end, s.next_reward)
}

/// Exact amount from units, trailing zeros trimmed: 900000 -> "0.9"
pub fn fmt_units(units: u64) -> String {
    let (whole, frac) = (units / UNITS_PER_TOKEN, units % UNITS_PER_TOKEN);
    let whole = group(whole);
    if frac == 0 {
        whole
    } else {
        format!("{}.{}", whole, format!("{:06}", frac).trim_end_matches('0'))
    }
}

/// Amount with at most `decimals` decimals: 9082302.685432 -> "9,082,302.68"
pub fn fmt_units_short(units: u64, decimals: u32) -> String {
    let step = UNITS_PER_TOKEN / 10u64.pow(decimals.min(6));
    fmt_units(units / step * step)
}

/// Round supply as a short label: 10M, 200M, 1B, 10T
pub fn fmt_supply_short(units: u64) -> String {
    let tokens = units / UNITS_PER_TOKEN;
    for (div, suffix) in [(1_000_000_000_000u64, "T"), (1_000_000_000, "B"), (1_000_000, "M")] {
        if tokens >= div && tokens.is_multiple_of(div) {
            return format!("{}{}", tokens / div, suffix);
        }
    }
    group(tokens)
}

/// 1234567 -> "1,234,567"
pub fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: u64 = UNITS_PER_TOKEN;

    #[test]
    fn rewards_follow_the_contract() {
        assert_eq!(reward_at(0), M);
        assert_eq!(reward_at(10_000_000 * M), M);
        assert_eq!(reward_at(10_000_000 * M + 1), 900_000);
        assert_eq!(reward_at(20_000_000 * M), 900_000);
        assert_eq!(reward_at(20_000_000 * M + 1), 800_000);
        assert_eq!(reward_at(100_000_000 * M), 100_000);
        assert_eq!(reward_at(100_000_000 * M + 1), 90_000);
        assert_eq!(reward_at(9_000_000_000_000 * M + 1), 1);
        assert_eq!(reward_at(MAX_SUPPLY), 0);
        assert_eq!(reward_at(MAX_SUPPLY - 1), 1);
    }

    #[test]
    fn steps() {
        assert_eq!(step(5 * M), (0, 10_000_000 * M, Some(900_000)));
        assert_eq!(step(10_000_000 * M), (0, 10_000_000 * M, Some(900_000)));
        assert_eq!(step(10_000_000 * M + 1), (10_000_000 * M, 20_000_000 * M, Some(800_000)));
        assert_eq!(step(95_000_000 * M), (90_000_000 * M, 100_000_000 * M, Some(90_000)));
        assert_eq!(step(MAX_SUPPLY - 1).2, None);
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_units(900_000), "0.9");
        assert_eq!(fmt_units(420 * M), "420");
        assert_eq!(fmt_units_short(9_082_302_685_432, 2), "9,082,302.68");
    }
}

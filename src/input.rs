//! A one-line text field: typing, paste, cursor movement.

use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    chars: Vec<char>,
    cursor: usize,
}

impl TextInput {
    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Returns true if the key was used
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match code {
            KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => self.insert_str(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            _ => return false,
        }
        true
    }

    /// The visible part for a field `width` cells wide, and the cursor column inside it
    pub fn view(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let start = self.cursor.saturating_sub(width - 1);
        let text: String = self.chars.iter().skip(start).take(width).collect();
        (text, self.cursor - start)
    }
}

/// "1.5" with 6 decimals -> 1_500_000; None if malformed or too precise
pub fn parse_amount(s: &str, decimals: u32) -> Option<u64> {
    let s = s.trim().replace(',', "");
    let (whole, frac) = s.split_once('.').unwrap_or((&s, ""));
    if (whole.is_empty() && frac.is_empty()) || frac.len() > decimals as usize {
        return None;
    }
    if !whole.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    let frac: u64 = if frac.is_empty() { 0 } else { format!("{frac:0<width$}", width = decimals as usize).parse().ok()? };
    whole.checked_mul(10u64.pow(decimals))?.checked_add(frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(parse_amount("1.5", 6), Some(1_500_000));
        assert_eq!(parse_amount("0.000001", 6), Some(1));
        assert_eq!(parse_amount(".5", 9), Some(500_000_000));
        assert_eq!(parse_amount("1,000", 6), Some(1_000_000_000));
        assert_eq!(parse_amount("1.0000001", 6), None);
        assert_eq!(parse_amount("abc", 6), None);
        assert_eq!(parse_amount("", 6), None);
    }

    #[test]
    fn editing() {
        let mut t = TextInput::default();
        t.insert_str("ab");
        t.handle_key(KeyCode::Left, KeyModifiers::NONE);
        t.insert_str("X");
        assert_eq!(t.value(), "aXb");
        t.handle_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(t.value(), "ab");
        assert_eq!(t.view(10), ("ab".to_string(), 1));
    }
}

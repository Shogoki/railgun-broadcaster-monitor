//! Defence in depth: nothing that looks like a 32-byte secret is ever persisted.
//!
//! Every run of ≥ 64 hex digits (with or without `0x`) is shortened to
//! `0x047c…75cb`, except transaction hashes, which are public and needed.

use std::sync::LazyLock;

use regex::Regex;

static LONG_HEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?P<key>tx_hash[=:]\s?\x22?)?\b(?P<hex>(?:0x)?[0-9a-fA-F]{64,})\b").unwrap());

pub fn shorten_hex(h: &str) -> String {
    let body = h.strip_prefix("0x").unwrap_or(h);
    if body.len() <= 12 {
        return h.to_string();
    }
    format!("0x{}…{}", &body[..4], &body[body.len() - 4..])
}

/// Redact free text. `tx_hash=0x…` / `tx_hash: 0x…` keep their (66-char) value.
pub fn redact_text(s: &str) -> String {
    if !s.bytes().any(|b| b.is_ascii_hexdigit()) {
        return s.to_string();
    }
    LONG_HEX
        .replace_all(s, |c: &regex::Captures<'_>| {
            let hex = &c["hex"];
            // A digits-only run is a decimal integer (e.g. `railgun_txid`), not hex key material.
            if !hex.starts_with("0x") && hex.bytes().all(|b| b.is_ascii_digit()) {
                return c[0].to_string();
            }
            match c.name("key") {
                Some(k) if is_tx_hash(hex) => format!("{}{hex}", k.as_str()),
                Some(k) => format!("{}{}", k.as_str(), shorten_hex(hex)),
                None => shorten_hex(hex),
            }
        })
        .into_owned()
}

/// Redact a structured field value; tx hash fields are kept when well-formed.
pub fn redact_field(key: &str, value: &str) -> String {
    if (key == "tx_hash" || key.ends_with(".tx_hash")) && is_tx_hash(value) {
        return value.to_string();
    }
    redact_text(value)
}

pub fn is_tx_hash(s: &str) -> bool {
    s.len() == 66 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "0x94a9c12e15fbc85b2000277ebdcd37fe1d92ca9a00bb69c4c972939378d5846b";

    #[test]
    fn keeps_tx_hash_shortens_rest() {
        let s = format!("tx_hash={H} fee_blinded_commitment={H}");
        let r = redact_text(&s);
        assert!(r.starts_with(&format!("tx_hash={H}")));
        assert!(r.ends_with("fee_blinded_commitment=0x94a9…846b"));
        assert_eq!(redact_field("tx_hash", H), H);
        assert_eq!(redact_field("key", H), "0x94a9…846b");
        let bare = "efc6ddb59c098a13fb2b618fdae94c1c3a807abc8fb1837c93620c9143ee9e88";
        assert_eq!(redact_text(bare), "0xefc6…9e88");
        assert_eq!(redact_text("amount=106983915816351"), "amount=106983915816351");
        let txid = "7355367226406497579076532373607505435035041364457612435335869681117774542996";
        assert_eq!(redact_field("railgun_txid", txid), txid);
    }
}

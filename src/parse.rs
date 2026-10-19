//! Parser for the broadcaster's `tracing_subscriber::fmt` output.
//!
//! Line shape (ANSI colours stripped first):
//! `<RFC3339 ts> <LEVEL> span{k=v}:span2{k=v}:bare: <message> key=value …`
//!
//! Versioned against `triamazikamno/railgun-broadcaster` @ e5b36fc. Unknown lines are
//! classified as `Other`/`Unparsed` and counted, never fatal.

use std::{borrow::Cow, sync::LazyLock};

use regex::Regex;
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Bump whenever classification or field extraction changes.
pub const PARSER_VERSION: i64 = 1;
pub const PARSER_TARGET: &str = "railgun-broadcaster@e5b36fc";

static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());

pub fn strip_ansi(s: &str) -> Cow<'_, str> {
    if s.contains('\x1b') { ANSI.replace_all(s, "") } else { Cow::Borrowed(s) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub name: String,
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Line {
    /// Unix microseconds.
    pub ts: i64,
    pub level: Level,
    pub spans: Vec<Span>,
    pub message: String,
    pub fields: Vec<(String, String)>,
}

impl Line {
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn has_span(&self, name: &str) -> bool {
        self.spans.iter().any(|s| s.name == name)
    }

    /// Chain id from the innermost span carrying one, else from a `chain_id` field.
    pub fn chain_id(&self) -> Option<u64> {
        self.spans
            .iter()
            .rev()
            .flat_map(|s| s.fields.iter())
            .find(|(k, _)| k == "chain_id")
            .map(|(_, v)| v.as_str())
            .or_else(|| self.field("chain_id"))
            .and_then(|v| v.parse().ok())
    }
}

pub fn parse_ts(s: &str) -> Option<i64> {
    let t = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1000).ok()
}

/// Parse one ANSI-stripped line.
pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim_end_matches(['\r', '\n']);
    let (ts_str, rest) = line.split_once(' ')?;
    let ts = parse_ts(ts_str)?;
    let rest = rest.trim_start();
    let (lvl, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let level = match lvl {
        "TRACE" => Level::Trace,
        "DEBUG" => Level::Debug,
        "INFO" => Level::Info,
        "WARN" => Level::Warn,
        "ERROR" => Level::Error,
        _ => return None,
    };
    let rest = rest.trim_start();
    let (spans, body) = parse_spans(rest);
    let (message, fields) = split_fields(body);
    Some(Line { ts, level, spans, message, fields })
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parse `name{k=v}:name2:` prefixes. A bare (field-less) name is only accepted as a
/// span when it is not the first segment, so messages like `sync_live: …` emitted
/// without a parent span stay intact.
fn parse_spans(s: &str) -> (Vec<Span>, &str) {
    let b = s.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        let mut j = i;
        while j < b.len() && is_ident_byte(b[j]) {
            j += 1;
        }
        if j == start {
            break;
        }
        let name = &s[start..j];
        let mut fields = Vec::new();
        if j < b.len() && b[j] == b'{' {
            let Some(close) = find_matching(s, j) else { break };
            fields = split_fields(&s[j + 1..close]).1;
            if fields.is_empty() {
                // `{…}` content that isn't `k=v` – not a span.
                let (msg, _) = split_fields(&s[j + 1..close]);
                if !msg.is_empty() {
                    break;
                }
            }
            j = close + 1;
        } else if spans.is_empty() {
            break;
        }
        if j < b.len() && b[j] == b':' {
            spans.push(Span { name: name.to_string(), fields });
            j += 1;
            if j < b.len() && b[j] == b' ' {
                return (spans, &s[j + 1..]);
            }
            if j >= b.len() {
                return (spans, "");
            }
            i = j;
        } else {
            break;
        }
    }
    // Not a well-formed span prefix: if we consumed nothing keep the whole string.
    if spans.is_empty() { (Vec::new(), s) } else { (spans, &s[i..]) }
}

/// Index of the bracket closing the one at `open`, respecting quotes and nesting.
fn find_matching(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = open;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Positions where a ` key=` field starts, at bracket depth 0 and outside quotes.
fn field_starts(s: &str, track_brackets: bool) -> Vec<(usize, usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'(' | b'[' if track_brackets => depth += 1,
            b'}' | b')' | b']' if track_brackets => depth = (depth - 1).max(0),
            _ => {}
        }
        if depth == 0 && (i == 0 || b[i - 1] == b' ') && (b[i].is_ascii_alphabetic() || b[i] == b'_') {
            let mut j = i;
            while j < b.len() && (is_ident_byte(b[j]) || b[j] == b'.') {
                j += 1;
            }
            if j < b.len() && b[j] == b'=' && j > i {
                // (field start incl. key, key end, value start)
                out.push((i, j, j + 1));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Split `message key=value key2="v 2"` into message and fields.
pub fn split_fields(s: &str) -> (String, Vec<(String, String)>) {
    let mut starts = field_starts(s, true);
    // An unbalanced bracket in a Display error text would swallow later fields;
    // in that case fall back to ignoring brackets.
    if let Some(&(_, _, v)) = starts.last()
        && find_unbalanced(&s[v..])
    {
        starts = field_starts(s, false);
    }
    let message = match starts.first() {
        Some(&(i, _, _)) => s[..i].trim_end().to_string(),
        None => s.trim_end().to_string(),
    };
    let mut fields = Vec::with_capacity(starts.len());
    for (n, &(k0, k1, v0)) in starts.iter().enumerate() {
        let v1 = starts.get(n + 1).map_or(s.len(), |&(next, _, _)| next);
        let raw = s[v0..v1].trim_end();
        fields.push((s[k0..k1].to_string(), unquote(raw)));
    }
    (message, fields)
}

fn find_unbalanced(s: &str) -> bool {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for c in s.bytes() {
        if in_str {
            if esc {
                esc = false;
            } else if c == b'\\' {
                esc = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth -= 1,
            _ => {}
        }
    }
    depth != 0
}

/// If `s` is a single Rust Debug string literal, return its unescaped content.
pub fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') && find_string_end(s, 0) == Some(s.len() - 1) {
        unescape_debug(&s[1..s.len() - 1])
    } else {
        s.to_string()
    }
}

fn find_string_end(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

pub fn unescape_debug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('u') => {
                let rest: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let hex = rest.trim_start_matches('{');
                if let Some(ch) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// `Ok("…json…")` → `…json…`.
pub fn unwrap_ok_string(v: &str) -> Option<String> {
    let inner = v.strip_prefix("Ok(")?.strip_suffix(')')?;
    if inner.starts_with('"') && find_string_end(inner, 0) == Some(inner.len() - 1) {
        Some(unescape_debug(&inner[1..inner.len() - 1]))
    } else {
        None
    }
}

/// Map a Rust `Debug` error value to a grouping key, e.g.
/// `Poi(ValidateList { list_key: 0x…, source: InvalidSnarkProof })` → `Poi/ValidateList/InvalidSnarkProof`.
pub fn error_path(debug: &str) -> String {
    let mut path = Vec::new();
    let mut cur = debug.trim();
    for _ in 0..6 {
        let b = cur.as_bytes();
        let mut j = 0;
        while j < b.len() && is_ident_byte(b[j]) {
            j += 1;
        }
        if j == 0 || !b[0].is_ascii_uppercase() {
            break;
        }
        let ident = &cur[..j];
        let after = cur[j..].trim_start();
        match after.as_bytes().first() {
            None | Some(b',') => {
                path.push(ident);
                break;
            }
            Some(b'(') => {
                path.push(ident);
                let open = cur.len() - after.len();
                let Some(close) = find_matching(cur, open) else { break };
                cur = cur[open + 1..close].trim();
            }
            Some(b'{') => {
                path.push(ident);
                let open = cur.len() - after.len();
                let Some(close) = find_matching(cur, open) else { break };
                let inner = &cur[open + 1..close];
                match find_source(inner) {
                    Some(src) => cur = src,
                    None => break,
                }
            }
            _ => {
                // Free text such as `POI RPC HTTP 502 Bad Gateway`.
                if path.is_empty() {
                    path.push(ident);
                }
                break;
            }
        }
    }
    if path.is_empty() { "Unknown".into() } else { path.join("/") }
}

/// Value of the `source:` field inside a Debug struct body, at depth 0.
fn find_source(inner: &str) -> Option<&str> {
    let b = inner.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut i = 0;
    let mut src_start = None;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' {
                i += 1;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => depth -= 1,
                b',' if depth == 0 => {
                    if let Some(st) = src_start {
                        return Some(inner[st..i].trim());
                    }
                }
                _ => {}
            }
            if depth == 0
                && src_start.is_none()
                && inner[i..].starts_with("source:")
                && (i == 0 || !is_ident_byte(b[i - 1]))
            {
                src_start = Some(i + "source:".len());
            }
        }
        i += 1;
    }
    src_start.map(|st| inner[st..].trim())
}

// ---------------------------------------------------------------------------
// Classification

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Kind {
    RequestParsed,
    RequestFailed,
    TransactParseFailed,
    PoiFallback,
    ConvertingValue,
    GasEstimated,
    GasTooHigh,
    /// Terminal failure in the tx worker before submission (reason attached).
    TxPrepareFailed,
    TxBroadcasted,
    BroadcastFailed,
    SubmitFailed,
    TxSubmitted,
    RequestWarn,
    FeeNoteSubmitted,
    FeeNoteCompleted,
    FeeNoteFailed,
    FeeNoteWarn,
    FeeNoteInfo,
    OurFees,
    FeePublishError,
    UpdatePricesFailed,
    PriceWarn,
    SyncWarn,
    ServiceStart,
    WalletInfo,
    Startup,
    ChannelFull,
    WakuWarn,
    Maintenance,
    Other,
    Unparsed,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::RequestParsed => "request_parsed",
            Kind::RequestFailed => "request_failed",
            Kind::TransactParseFailed => "transact_parse_failed",
            Kind::PoiFallback => "poi_fallback",
            Kind::ConvertingValue => "converting_value",
            Kind::GasEstimated => "gas_estimated",
            Kind::GasTooHigh => "gas_too_high",
            Kind::TxPrepareFailed => "tx_prepare_failed",
            Kind::TxBroadcasted => "tx_broadcasted",
            Kind::BroadcastFailed => "broadcast_failed",
            Kind::SubmitFailed => "submit_failed",
            Kind::TxSubmitted => "tx_submitted",
            Kind::RequestWarn => "request_warn",
            Kind::FeeNoteSubmitted => "fee_note_submitted",
            Kind::FeeNoteCompleted => "fee_note_completed",
            Kind::FeeNoteFailed => "fee_note_failed",
            Kind::FeeNoteWarn => "fee_note_warn",
            Kind::FeeNoteInfo => "fee_note_info",
            Kind::OurFees => "our_fees",
            Kind::FeePublishError => "fee_publish_error",
            Kind::UpdatePricesFailed => "update_prices_failed",
            Kind::PriceWarn => "price_warn",
            Kind::SyncWarn => "sync_warn",
            Kind::ServiceStart => "service_start",
            Kind::WalletInfo => "wallet_info",
            Kind::Startup => "startup",
            Kind::ChannelFull => "channel_full",
            Kind::WakuWarn => "waku_warn",
            Kind::Maintenance => "maintenance",
            Kind::Other => "other",
            Kind::Unparsed => "unparsed",
        }
    }

    /// High-volume kinds whose raw rows are pruned after `raw_events_days`.
    pub fn is_noisy(self) -> bool {
        matches!(
            self,
            Kind::OurFees
                | Kind::SyncWarn
                | Kind::PriceWarn
                | Kind::ChannelFull
                | Kind::PoiFallback
                | Kind::FeeNoteInfo
                | Kind::Other
                | Kind::Unparsed
                | Kind::WakuWarn
                | Kind::Maintenance
        )
    }
}

/// Reason key for terminal tx-worker failures, by message.
fn tx_prepare_reason(msg: &str) -> Option<&'static str> {
    Some(match msg {
        "no wallets available" => "Tx/NoWalletsAvailable",
        "no query rpc available" => "Tx/NoQueryRpcAvailable",
        "fetch gas price failed" => "Tx/FetchGasPriceFailed",
        "fetch nonce failed" => "Tx/FetchNonceFailed",
        "estimate gas failed" => "Tx/EstimateGasFailed",
        "tx7702 simulation failed" => "Tx/Tx7702SimulationFailed",
        "gas price rejected" => "Tx/GasPriceRejected",
        "tx7702 unsupported on this chain" => "Tx/Tx7702Unsupported",
        "invalid tx7702 request" => "Tx/InvalidTx7702Request",
        _ => return None,
    })
}

pub fn classify(l: &Line) -> (Kind, Option<String>) {
    let m = l.message.as_str();
    let in_tx = l.has_span("tx");
    let maintenance = l.has_span("auto_refill") || l.has_span("utxo_consolidation");
    if maintenance {
        return (Kind::Maintenance, None);
    }
    let kind = match m {
        "parsed transact calldata" => Kind::RequestParsed,
        "failed to handle transact request" => {
            let reason = l.field("error").map(error_path);
            return (Kind::RequestFailed, reason);
        }
        "failed to parse decoded transact request" => {
            let reason = l.field("error").map(|e| format!("Parse/{}", error_path(e)));
            return (Kind::TransactParseFailed, reason);
        }
        "converting value to gas token" => Kind::ConvertingValue,
        "estimated gas" => Kind::GasEstimated,
        _ if m.starts_with("gas cost is too high") => {
            return (Kind::GasTooHigh, Some("Tx/GasCostTooHigh".into()));
        }
        "tx broadcasted" => Kind::TxBroadcasted,
        _ if m.starts_with("failed to broadcast tx") => Kind::BroadcastFailed,
        "submit tx failed" => return (Kind::SubmitFailed, Some("Tx/SubmitFailed".into())),
        "submitted tx" => Kind::TxSubmitted,
        "wrong chain_id" | "cached fees id not found" => Kind::RequestWarn,
        "submitted fee-note single-commitment poi" => Kind::FeeNoteSubmitted,
        "fee-note assurance completed" | "fee-note assurance already complete" => Kind::FeeNoteCompleted,
        "fee-note assurance reached a terminal non-retriable outcome" => {
            let reason = l.field("reason").map(error_path);
            return (Kind::FeeNoteFailed, reason);
        }
        "our fees" => Kind::OurFees,
        "publish fees failed"
        | "serialize fees failed"
        | "sign fees failed"
        | "verify fees signature failed"
        | "fees signature invalid" => Kind::FeePublishError,
        "update prices failed" => Kind::UpdatePricesFailed,
        "failed to get prices" => Kind::PriceWarn,
        _ if m.starts_with("failed to get price for") || m.starts_with("fees: failed to get prices") => Kind::PriceWarn,
        "starting broadcaster service" => Kind::ServiceStart,
        "using EVM wallet" => Kind::WalletInfo,
        "derived address"
        | "spawning workers"
        | "admin server listening"
        | "broadcaster POI artifact cache mode enabled" => Kind::Startup,
        _ if m.contains("failed to send message to sink") => Kind::ChannelFull,
        "get message failed, retrying..." | "parse fees failed" | "decode payload failed" | "unhandled topic" => {
            Kind::WakuWarn
        }
        _ if m.starts_with("Failed to deserialize fees message") => Kind::WakuWarn,
        _ if m.starts_with("broadcaster POI artifact cache") => Kind::PoiFallback,
        _ => {
            if in_tx && let Some(r) = tx_prepare_reason(m) {
                return (Kind::TxPrepareFailed, Some(r.into()));
            }
            if l.has_span("fee_note_assurance") && l.field("tx_hash").is_some() {
                if l.level >= Level::Warn { Kind::FeeNoteWarn } else { Kind::FeeNoteInfo }
            } else if (l.has_span("sync_live")
                || l.has_span("sync_head")
                || m.starts_with("sync_live:")
                || m.starts_with("sync_head:"))
                && l.level >= Level::Warn
            {
                Kind::SyncWarn
            } else {
                Kind::Other
            }
        }
    };
    (kind, None)
}

impl PartialOrd for Level {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some((*self as u8).cmp(&(*other as u8)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARSED: &str = "2026-10-17T22:16:43.988106Z  INFO transact{chain_id=1}: parsed transact calldata fee_token=0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2 fee_amount=106983915816351 railgun_txid=7355367226406497579076532373607505435035041364457612435335869681117774542996 utxo_tree_in=4";

    #[test]
    fn parses_basic_line() {
        let l = parse_line(PARSED).unwrap();
        assert_eq!(l.level, Level::Info);
        assert_eq!(l.spans.len(), 1);
        assert_eq!(l.spans[0].name, "transact");
        assert_eq!(l.chain_id(), Some(1));
        assert_eq!(l.message, "parsed transact calldata");
        assert_eq!(l.field("fee_amount"), Some("106983915816351"));
        assert_eq!(l.field("utxo_tree_in"), Some("4"));
        assert_eq!(classify(&l).0, Kind::RequestParsed);
    }

    #[test]
    fn strips_ansi() {
        let colored = "\x1b[2m2026-10-17T22:16:45.229446Z\x1b[0m \x1b[32m INFO\x1b[0m \x1b[1mtx\x1b[0m\x1b[1m{\x1b[0m\x1b[3mchain_id\x1b[0m\x1b[2m=\x1b[0m1\x1b[1m}\x1b[0m\x1b[2m:\x1b[0m submitted tx \x1b[3mtx_hash\x1b[0m\x1b[2m=\x1b[0m0x94a9c12e15fbc85b2000277ebdcd37fe1d92ca9a00bb69c4c972939378d5846b";
        let l = parse_line(&strip_ansi(colored)).unwrap();
        assert_eq!(l.chain_id(), Some(1));
        assert_eq!(l.message, "submitted tx");
        assert_eq!(l.field("tx_hash"), Some("0x94a9c12e15fbc85b2000277ebdcd37fe1d92ca9a00bb69c4c972939378d5846b"));
    }

    #[test]
    fn quoted_and_spaced_values() {
        let l = parse_line("2026-10-17T22:16:44.686288Z  INFO tx{chain_id=1}: estimated gas gas=1308974 gas_price=58776719 cost=\"0.000076937196976306\" refund=\"0.000106983915816351\"").unwrap();
        assert_eq!(l.field("cost"), Some("0.000076937196976306"));
        assert_eq!(l.field("refund"), Some("0.000106983915816351"));
        let l = parse_line("2026-10-17T22:17:09.731614Z  WARN fee_note_assurance{chain_id=1}: fetch latest block failed for fee-note assurance error=error sending request for url (https://1rpc.io/eth): operation timed out rpc=https://1rpc.io/eth chain_id=1 tx_hash=0xab").unwrap();
        assert_eq!(l.field("error"), Some("error sending request for url (https://1rpc.io/eth): operation timed out"));
        assert_eq!(l.field("rpc"), Some("https://1rpc.io/eth"));
        assert_eq!(l.field("tx_hash"), Some("0xab"));
    }

    #[test]
    fn rejection_without_span() {
        let l = parse_line("2026-10-17T10:02:11.120001Z  WARN failed to handle transact request error=Poi(ValidateList { list_key: 0xefc6ddb59c098a13fb2b618fdae94c1c3a807abc8fb1837c93620c9143ee9e88, source: InvalidSnarkProof })").unwrap();
        assert!(l.spans.is_empty());
        assert_eq!(l.chain_id(), None);
        let (k, reason) = classify(&l);
        assert_eq!(k, Kind::RequestFailed);
        assert_eq!(reason.as_deref(), Some("Poi/ValidateList/InvalidSnarkProof"));
    }

    #[test]
    fn error_paths() {
        assert_eq!(error_path("Enqueue(Closed)"), "Enqueue/Closed");
        assert_eq!(error_path("Decrypt(AesGcm(DecryptFailed))"), "Decrypt/AesGcm/DecryptFailed");
        assert_eq!(error_path("Parse(UnknownFunctionCall { selector: \"0x12345678\" })"), "Parse/UnknownFunctionCall");
        assert_eq!(
            error_path(
                "Poi(ValidateList { list_key: 0x1, source: RpcRequest { source: POI RPC HTTP 502 Bad Gateway } })"
            ),
            "Poi/ValidateList/RpcRequest"
        );
        assert_eq!(
            error_path("Poi(ValidateList { list_key: 0x1, source: SnarkVerify { source: InvalidG1Len { len: 1 } } })"),
            "Poi/ValidateList/SnarkVerify/InvalidG1Len"
        );
        assert_eq!(error_path("RevertedReceipt"), "RevertedReceipt");
        assert_eq!(error_path("something went wrong"), "Unknown");
    }

    #[test]
    fn our_fees_payload() {
        let line = r#"2026-10-17T21:14:56.184940Z DEBUG fees{chain_id=137}: our fees payload=Ok("{\"fees\":{\"0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270\":\"0xe8b3e728c9c0000\",\"0x3c499c542cef5e3811e1192ce70d8cc03d5c3359\":\"0x17f4b\"},\"feeExpiration\":1792271996184,\"feesID\":\"vd3do3cpmhnhysue\",\"railgunAddress\":\"0zk1qyyh\",\"availableWallets\":1,\"version\":\"8.3.0\"}")"#;
        let l = parse_line(line).unwrap();
        assert_eq!(classify(&l).0, Kind::OurFees);
        let json = unwrap_ok_string(l.field("payload").unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["fees"]["0x3c499c542cef5e3811e1192ce70d8cc03d5c3359"], "0x17f4b");
        assert_eq!(v["feesID"], "vd3do3cpmhnhysue");
    }

    #[test]
    fn nested_and_bare_spans() {
        let l = parse_line("2026-10-17T21:00:00.000000Z  WARN service{chain_id=1}:sync_live: failed to fetch logs, retrying... err=rate limited rpc_index=2").unwrap();
        assert_eq!(l.spans.len(), 2);
        assert_eq!(l.spans[1].name, "sync_live");
        assert_eq!(l.chain_id(), Some(1));
        assert_eq!(classify(&l).0, Kind::SyncWarn);
        let l = parse_line("2026-10-17T21:00:00.000000Z  WARN sync_live: failed to fetch logs, retrying...").unwrap();
        assert!(l.spans.is_empty());
        assert_eq!(l.message, "sync_live: failed to fetch logs, retrying...");
        assert_eq!(classify(&l).0, Kind::SyncWarn);
        let l = parse_line("2026-10-17T21:00:00.000000Z  WARN service{chain_id=56}:sync_head{chain_id=56}: failed to fetch latest block err=x rpc_index=0").unwrap();
        assert_eq!(l.chain_id(), Some(56));
        assert_eq!(classify(&l).0, Kind::SyncWarn);
    }

    #[test]
    fn tx_prepare_failures_need_tx_span() {
        let l = parse_line("2026-10-17T21:00:00.000000Z  WARN tx{chain_id=137}: estimate gas failed error=estimate gas failed: server returned an error response: execution reverted rpc=https://polygon.drpc.org").unwrap();
        assert_eq!(classify(&l), (Kind::TxPrepareFailed, Some("Tx/EstimateGasFailed".into())));
        let l = parse_line(
            "2026-10-17T21:00:00.000000Z  WARN service{chain_id=137}:auto_refill: estimate gas failed error=x",
        )
        .unwrap();
        assert_eq!(classify(&l).0, Kind::Maintenance);
    }

    #[test]
    fn garbage_is_none() {
        assert!(parse_line("thread 'main' panicked at src/main.rs").is_none());
        assert!(parse_line("").is_none());
    }
}

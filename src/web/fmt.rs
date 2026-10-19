//! Formatting helpers for the HTML views.

use maud::{Markup, html};
use time::{OffsetDateTime, macros::format_description};

use crate::{config::Config, units};

pub fn ts(us: i64) -> String {
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(us) * 1000)
        .ok()
        .and_then(|t| t.format(&fmt).ok())
        .unwrap_or_else(|| "–".into())
}

pub fn day(us: i64) -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(us) * 1000)
        .ok()
        .and_then(|t| t.format(&fmt).ok())
        .unwrap_or_else(|| "–".into())
}

pub fn ago(us: i64, now: i64) -> String {
    let s = (now - us) / 1_000_000;
    if s < 0 {
        return "just now".into();
    }
    match s {
        0..=89 => format!("{s} s ago"),
        90..=5399 => format!("{} min ago", (s + 30) / 60),
        5400..=172_799 => format!("{} h ago", (s + 1800) / 3600),
        _ => format!("{} d ago", (s + 43_200) / 86_400),
    }
}

/// `<time>` with UTC timestamp in the title and relative text.
pub fn when(us: i64, now: i64) -> Markup {
    html! { time title=(format!("{} UTC", ts(us))) { (ago(us, now)) } }
}

pub fn short(h: &str) -> String {
    if h.len() > 14 { format!("{}…{}", &h[..8], &h[h.len() - 4..]) } else { h.to_string() }
}

pub fn tx_link(cfg: &Config, chain: i64, hash: &str) -> Markup {
    let href = cfg.chain(chain as u64).map(|c| format!("{}{hash}", c.explorer_tx));
    html! {
        @if let Some(href) = href {
            a.mono href=(href) target="_blank" rel="noopener noreferrer" title=(hash) { (short(hash)) }
        } @else {
            span.mono title=(hash) { (short(hash)) }
        }
    }
}

pub fn symbol(cfg: &Config, chain: Option<i64>, token: Option<&str>) -> String {
    match (chain, token) {
        (Some(c), Some(t)) => cfg.token(c as u64, t).map(|t| t.symbol.clone()).unwrap_or_else(|| short(t)),
        (_, Some(t)) => short(t),
        _ => "–".into(),
    }
}

pub fn token_amount(cfg: &Config, chain: Option<i64>, token: Option<&str>, amount: Option<&str>) -> String {
    let Some(a) = amount.and_then(units::parse_uint) else { return "–".into() };
    let dec = chain.zip(token).and_then(|(c, t)| cfg.token(c as u64, t)).map(|t| t.decimals);
    match dec {
        Some(d) => units::fmt_sig(units::to_f64(a, d), 4),
        None => format!("{a} (raw)"),
    }
}

pub fn native(cfg: &Config, chain: i64, v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{} {}", units::fmt_sig(v, 3), native_sym(cfg, chain)),
        None => "–".into(),
    }
}

pub fn native_sym(cfg: &Config, chain: i64) -> String {
    cfg.chain(chain as u64).map(|c| c.native_symbol.clone()).unwrap_or_default()
}

pub fn usd(v: Option<f64>) -> String {
    v.map(units::fmt_usd).unwrap_or_else(|| "–".into())
}

pub fn pct(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => format!(
            "{}{:.1} %",
            if v > 0.0 {
                "+"
            } else if v < 0.0 {
                "−"
            } else {
                ""
            },
            v.abs() * 100.0
        ),
        _ => "–".into(),
    }
}

pub fn mult(v: Option<f64>) -> String {
    match v {
        Some(v) if v.is_finite() => format!("{v:.3}×"),
        _ => "–".into(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Good,
    Warning,
    Critical,
    Unknown,
}

/// Status pill: icon + label, never colour alone.
pub fn pill(s: Status, label: &str) -> Markup {
    let (class, icon) = match s {
        Status::Good => ("good", "✓"),
        Status::Warning => ("warning", "!"),
        Status::Critical => ("critical", "✕"),
        Status::Unknown => ("unknown", "?"),
    };
    html! { span class=(format!("pill {class}")) { span.icon aria-hidden="true" { (icon) } (label) } }
}

/// Signed value with direction styling via text tokens (up good / down bad).
pub fn signed(text: String, v: Option<f64>) -> Markup {
    let class = match v {
        Some(v) if v > 0.0 => "pos",
        Some(v) if v < 0.0 => "neg",
        _ => "",
    };
    html! { span class=(class) { (text) } }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        let now = 10_000_000_000i64;
        assert_eq!(ago(now - 30_000_000, now), "30 s ago");
        assert_eq!(ago(now - 600_000_000, now), "10 min ago");
        assert_eq!(ago(now - 7_200_000_000, now), "2 h ago");
        assert_eq!(ts(0), "1970-01-01 00:00:00");
        assert_eq!(pct(Some(0.55)), "+55.0 %");
        assert_eq!(pct(Some(-0.08)), "−8.0 %");
    }
}

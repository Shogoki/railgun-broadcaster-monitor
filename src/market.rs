//! Market position: all broadcasters' fee offers seen on Waku, ranked per chain/token.
//!
//! Wallet selection (`@railgun-community/waku-broadcaster-client` `BroadcasterSearch`):
//! 1. Offers outside [avg × 0.9, avg × 1.3] of the trusted fee signers' average fee for the
//!    token are dropped; without any trusted fee for the token, wallets show no broadcaster.
//! 2. `findBestBroadcaster` (Railway, Terminal Wallet) takes the cheapest remaining offer,
//!    higher reliability breaking ties. `findRandomBroadcasterForToken` picks at random
//!    among offers ≤ cheapest × 1.05; no wallet we know of uses it.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use bech32::{Bech32m, primitives::decode::CheckedHrpstring};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use ruint::aliases::U256;
use serde::{Deserialize, Serialize};

use crate::{config::Config, units::parse_uint};

// ---------------------------------------------------------------------------
// Wire format (public-broadcaster-protocol, lenient: unknown fields are allowed)

#[derive(Debug, Deserialize)]
struct Envelope {
    data: String,
    signature: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeBody {
    pub fees: HashMap<String, String>,
    /// Unix milliseconds.
    pub fee_expiration: i64,
    #[serde(rename = "feesID", default)]
    pub fees_id: Option<String>,
    pub railgun_address: String,
    #[serde(default)]
    pub available_wallets: Option<i64>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub relay_adapt: Option<String>,
    #[serde(rename = "requiredPOIListKeys", default)]
    pub required_poi_list_keys: Vec<String>,
    #[serde(default)]
    pub reliability: Option<f64>,
    #[serde(default)]
    pub identifier: Option<String>,
}

fn decode_hex(s: &str) -> Result<Vec<u8>> {
    Ok(hex::decode(s.trim_start_matches("0x"))?)
}

/// Ed25519 viewing public key embedded in a 0zk address (bytes 41..73).
pub fn viewing_public_key(addr: &str) -> Result<[u8; 32]> {
    let checked = CheckedHrpstring::new::<Bech32m>(addr).context("bech32m decode")?;
    if checked.hrp().as_str() != "0zk" {
        bail!("unexpected hrp");
    }
    let bytes: Vec<u8> = checked.byte_iter().collect();
    if bytes.len() != 73 || bytes[0] != 1 {
        bail!("unexpected 0zk address layout");
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes[41..73]);
    Ok(k)
}

/// Decode a Waku fee message payload and verify its signature.
pub fn decode_fee_message(payload: &[u8]) -> Result<(FeeBody, bool)> {
    let env: Envelope = serde_json::from_slice(payload).context("envelope")?;
    let data = decode_hex(&env.data)?;
    let sig = decode_hex(&env.signature)?;
    let body: FeeBody = serde_json::from_slice(&data).context("fee body")?;
    let valid = (|| -> Result<bool> {
        let sig: [u8; 64] = sig.as_slice().try_into().context("signature length")?;
        let vk = VerifyingKey::from_bytes(&viewing_public_key(&body.railgun_address)?)?;
        Ok(vk.verify(&data, &Signature::from_bytes(&sig)).is_ok())
    })()
    .unwrap_or(false);
    Ok((body, valid))
}

// ---------------------------------------------------------------------------

const FEE_EXPIRATION_MINIMUM_MS: i64 = 40_000;

#[derive(Debug, Clone)]
pub struct Offer {
    pub railgun_address: String,
    pub fees: HashMap<String, U256>,
    pub fee_expiration_ms: i64,
    pub version: Option<String>,
    pub available_wallets: Option<i64>,
    pub reliability: Option<f64>,
    pub identifier: Option<String>,
    pub relay_adapt: Option<String>,
    pub poi_lists: Vec<String>,
    pub sig_valid: bool,
    pub seen_us: i64,
}

impl Offer {
    pub fn from_body(b: FeeBody, sig_valid: bool, seen_us: i64) -> Self {
        Self {
            fees: b.fees.iter().filter_map(|(k, v)| parse_uint(v).map(|r| (k.to_lowercase(), r))).collect(),
            railgun_address: b.railgun_address,
            fee_expiration_ms: b.fee_expiration,
            version: b.version,
            available_wallets: b.available_wallets,
            reliability: b.reliability,
            identifier: b.identifier,
            relay_adapt: b.relay_adapt,
            poi_lists: b.required_poi_list_keys,
            sig_valid,
            seen_us,
        }
    }

    /// Would a wallet consider this offer right now? (Wallets want ≥ 40 s left to prove and submit.)
    pub fn is_live(&self, now_ms: i64) -> bool {
        self.sig_valid
            && self.fee_expiration_ms >= now_ms + FEE_EXPIRATION_MINIMUM_MS
            && self.available_wallets.unwrap_or(1) > 0
    }
}

#[derive(Debug, Default)]
pub struct WakuStatus {
    pub enabled: bool,
    pub connected_peers: usize,
    pub messages: u64,
    pub invalid: u64,
    pub last_message_us: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct Market {
    pub our_address: String,
    pub pool_factor: f64,
    pub trusted: Vec<String>,
    /// Allowed deviation from the trusted average, in whole percent (as the wallet rounds it).
    pub band_pct: (u64, u64),
    /// chain → broadcaster 0zk address → latest offer
    pub offers: HashMap<u64, HashMap<String, Offer>>,
    pub status: WakuStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub address: String,
    pub identifier: Option<String>,
    pub rate: String,
    pub rank: usize,
    pub is_us: bool,
    pub trusted: bool,
    pub eligibility: Eligibility,
    pub is_pick: bool,
    pub in_pool: bool,
    pub ratio_to_cheapest: f64,
    pub multiplier: Option<f64>,
    pub reliability: Option<f64>,
    pub version: Option<String>,
    pub expires_in_s: i64,
}

/// Whether wallets consider an offer at all (trusted-fee band).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Eligibility {
    Eligible,
    TooCheap,
    TooExpensive,
    /// No trusted signer prices this token: wallets show no broadcaster for it.
    NoTrustedFee,
}

impl Eligibility {
    pub fn label(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::TooCheap => "below band",
            Self::TooExpensive => "above band",
            Self::NoTrustedFee => "no trusted fee",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenView {
    pub chain_id: u64,
    pub token: String,
    pub symbol: String,
    pub decimals: u32,
    pub entries: Vec<Entry>,
    /// Trusted signers' average fee and the band wallets accept around it.
    pub trusted_avg: Option<String>,
    pub band: Option<(String, String)>,
    pub band_multiplier: Option<(f64, f64)>,
    /// Offers wallets consider (inside the band).
    pub n_eligible: usize,
    /// The offer `findBestBroadcaster` returns.
    pub pick: Option<String>,
    pub pick_multiplier: Option<f64>,
    /// Cheapest eligible offer (= `pick` rate) and the random-pool threshold over it.
    pub cheapest: Option<String>,
    pub cheapest_multiplier: Option<f64>,
    pub threshold: Option<String>,
    pub our_rate: Option<String>,
    pub our_multiplier: Option<f64>,
    /// "waku" (seen live) | "log" (our published rate from our own logs only)
    pub our_source: Option<&'static str>,
    /// Rank among eligible offers (overall rank when we're outside the band).
    pub our_rank: Option<usize>,
    pub our_eligibility: Option<Eligibility>,
    /// Would `findBestBroadcaster` return us right now?
    pub picked: Option<bool>,
    pub in_pool: Option<bool>,
    pub n_broadcasters: usize,
    pub pool_size: usize,
    /// Relative change of our rate needed to become the wallet pick (0 when we are).
    pub change_for_pick: Option<f64>,
    /// Relative change of our rate needed to enter the random pool (≤ 0 when already in).
    pub change_for_pool: Option<f64>,
}

impl Market {
    pub fn new(cfg: &Config) -> Self {
        Self {
            our_address: cfg.broadcaster.railgun_address.clone(),
            pool_factor: cfg.waku.pool_factor,
            trusted: cfg.waku.trusted_fee_signers.clone(),
            band_pct: (
                (cfg.waku.trusted_fee_lower * 100.0).round() as u64,
                (cfg.waku.trusted_fee_upper * 100.0).round() as u64,
            ),
            offers: HashMap::new(),
            status: WakuStatus { enabled: cfg.waku.enabled, ..Default::default() },
        }
    }

    /// Insert an offer; an older message (by fee expiration) never replaces a newer one.
    pub fn upsert(&mut self, chain: u64, offer: Offer) {
        let by_addr = self.offers.entry(chain).or_default();
        match by_addr.get(&offer.railgun_address) {
            Some(old) if old.fee_expiration_ms > offer.fee_expiration_ms => {}
            _ => {
                by_addr.insert(offer.railgun_address.clone(), offer);
            }
        }
    }

    pub fn prune(&mut self, now_ms: i64) {
        // Keep expired offers for an hour so "dropped out" broadcasters stay visible briefly.
        for m in self.offers.values_mut() {
            m.retain(|_, o| o.fee_expiration_ms > now_ms - 3_600_000);
        }
    }

    /// Tokens to show for a chain: configured ones first, then others we publish.
    pub fn tokens(&self, cfg: &Config, chain: u64) -> Vec<String> {
        let mut out: Vec<String> =
            cfg.chain(chain).map(|c| c.tokens.iter().map(|t| t.address.clone()).collect()).unwrap_or_default();
        let mut extra = BTreeSet::new();
        if let Some(o) = self.offers.get(&chain).and_then(|m| m.get(&self.our_address)) {
            extra.extend(o.fees.keys().cloned());
        }
        for t in extra {
            if !out.contains(&t) {
                out.push(t);
            }
        }
        out
    }

    pub fn view(
        &self,
        cfg: &Config,
        chain: u64,
        token: &str,
        now_ms: i64,
        our_logged_rate: Option<U256>,
        market_reference: Option<f64>,
    ) -> TokenView {
        let token = token.to_lowercase();
        let (symbol, decimals) = cfg
            .token(chain, &token)
            .map(|t| (t.symbol.clone(), t.decimals))
            .unwrap_or_else(|| (crate::redact::shorten_hex(&token), 18));
        let bonus = cfg.chain(chain).and_then(|c| c.fee_bonus);

        let mut live: Vec<&Offer> = self
            .offers
            .get(&chain)
            .map(|m| m.values().filter(|o| o.is_live(now_ms) && o.fees.contains_key(&token)).collect())
            .unwrap_or_default();
        let rel = |o: &Offer| o.reliability.unwrap_or(0.0);
        // `findBestBroadcaster` order: fee ascending, then higher reliability.
        live.sort_by(|a, b| {
            a.fees[&token]
                .cmp(&b.fees[&token])
                .then(rel(b).total_cmp(&rel(a)))
                .then(a.railgun_address.cmp(&b.railgun_address))
        });

        // Trusted fee band (integer maths as in the wallet).
        let trusted_fees: Vec<U256> =
            live.iter().filter(|o| self.trusted.contains(&o.railgun_address)).map(|o| o.fees[&token]).collect();
        let trusted_avg = (!trusted_fees.is_empty())
            .then(|| trusted_fees.iter().fold(U256::ZERO, |a, b| a + b) / U256::from(trusted_fees.len()));
        let band = trusted_avg.map(|a| {
            let pct = |p: u64| a * U256::from(p) / U256::from(100);
            (a - pct(self.band_pct.0), a + pct(self.band_pct.1))
        });
        let eligibility = |r: U256| match (self.trusted.is_empty(), band) {
            (true, _) => Eligibility::Eligible,
            (false, None) => Eligibility::NoTrustedFee,
            (false, Some((lo, _))) if r < lo => Eligibility::TooCheap,
            (false, Some((_, hi))) if r > hi => Eligibility::TooExpensive,
            _ => Eligibility::Eligible,
        };
        let eligible: Vec<&Offer> =
            live.iter().copied().filter(|o| eligibility(o.fees[&token]) == Eligibility::Eligible).collect();
        let pick = eligible.first().copied();

        let ours_live = live.iter().find(|o| o.railgun_address == self.our_address).copied();
        let (our_rate, our_source) = match (ours_live, our_logged_rate) {
            (Some(o), _) => (Some(o.fees[&token]), Some("waku")),
            (None, Some(r)) => (Some(r), Some("log")),
            _ => (None, None),
        };
        // "× market" reference: our rate without our bonus (≈ the broadcaster's oracle price),
        // else a reference derived from the Chainlink native/USD price.
        let reference = our_rate.zip(bonus).map(|(r, b)| u256_f64(r) / (1.0 + b)).or(market_reference);
        let mult = |r: U256| reference.filter(|x| *x > 0.0).map(|x| u256_f64(r) / x);

        let cheapest = pick.map(|o| o.fees[&token]);
        let threshold_f = cheapest.map(|c| u256_f64(c) * self.pool_factor);
        let in_pool_f = |r: U256| {
            eligibility(r) == Eligibility::Eligible && threshold_f.is_some_and(|t| u256_f64(r) <= t * (1.0 + 1e-12))
        };

        let entries: Vec<Entry> = live
            .iter()
            .map(|o| {
                let r = o.fees[&token];
                Entry {
                    address: o.railgun_address.clone(),
                    identifier: o.identifier.clone(),
                    rate: r.to_string(),
                    rank: 1 + live.iter().filter(|x| x.fees[&token] < r).count(),
                    is_us: o.railgun_address == self.our_address,
                    trusted: self.trusted.contains(&o.railgun_address),
                    eligibility: eligibility(r),
                    is_pick: pick.is_some_and(|p| std::ptr::eq(p, *o)),
                    in_pool: in_pool_f(r),
                    ratio_to_cheapest: cheapest.map_or(1.0, |c| u256_f64(r) / u256_f64(c).max(f64::MIN_POSITIVE)),
                    multiplier: mult(r),
                    reliability: o.reliability,
                    version: o.version.clone(),
                    expires_in_s: (o.fee_expiration_ms - now_ms) / 1000,
                }
            })
            .collect();

        // Our position against everyone else (works when we're only known from our logs).
        let our_eligibility = our_rate.map(eligibility);
        let others: Vec<&Offer> = eligible.iter().copied().filter(|o| o.railgun_address != self.our_address).collect();
        let others_best = others.first().map(|o| o.fees[&token]);
        let our_rank = our_rate.map(|r| {
            let field: &[&Offer] = if our_eligibility == Some(Eligibility::Eligible) { &others } else { &live };
            1 + field.iter().filter(|o| o.railgun_address != self.our_address && o.fees[&token] < r).count()
        });
        let picked = our_rate.map(|r| match ours_live {
            Some(o) => pick.is_some_and(|p| std::ptr::eq(p, o)),
            None => our_eligibility == Some(Eligibility::Eligible) && others_best.is_none_or(|b| r < b),
        });
        let in_pool = our_rate.map(|r| {
            our_eligibility == Some(Eligibility::Eligible)
                && others_best.is_none_or(|b| u256_f64(r) <= u256_f64(b.min(r)) * self.pool_factor * (1.0 + 1e-12))
        });
        // Target rate to become the pick: just under the best other eligible offer, but never
        // outside the band (an undercut below the band floor drops us from wallets entirely).
        let change_for_pick = match (our_rate, picked, our_eligibility) {
            (Some(_), Some(true), _) => Some(0.0),
            (Some(r), _, Some(e)) if e != Eligibility::NoTrustedFee => {
                let mut target = others_best.map_or(r, |b| r.min(b.saturating_sub(U256::from(1))));
                if let Some((lo, hi)) = band {
                    target = target.clamp(lo, hi);
                }
                Some(u256_f64(target) / u256_f64(r) - 1.0)
            }
            _ => None,
        };
        let change_for_pool =
            our_rate.zip(others_best).map(|(r, b)| u256_f64(b) * self.pool_factor / u256_f64(r) - 1.0);

        TokenView {
            chain_id: chain,
            symbol,
            decimals,
            trusted_avg: trusted_avg.map(|a| a.to_string()),
            band: band.map(|(lo, hi)| (lo.to_string(), hi.to_string())),
            band_multiplier: band.and_then(|(lo, hi)| mult(lo).zip(mult(hi))),
            n_eligible: eligible.len(),
            pick: pick.map(|o| o.railgun_address.clone()),
            pick_multiplier: cheapest.and_then(mult),
            cheapest: cheapest.map(|c| c.to_string()),
            cheapest_multiplier: cheapest.and_then(mult),
            threshold: threshold_f.map(|t| format!("{t:.0}")),
            our_rate: our_rate.map(|r| r.to_string()),
            our_multiplier: our_rate.and_then(mult),
            our_source,
            our_rank,
            our_eligibility,
            picked,
            in_pool,
            n_broadcasters: entries.len(),
            pool_size: entries.iter().filter(|e| e.in_pool).count(),
            change_for_pick,
            change_for_pool,
            entries,
            token,
        }
    }
}

pub fn u256_f64(v: U256) -> f64 {
    v.to_string().parse().unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bech32::{
        Hrp,
        primitives::iter::{ByteIterExt, Fe32IterExt},
    };
    use ed25519_dalek::{Signer, SigningKey};

    fn cfg() -> Config {
        let mut c: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        c.normalize();
        c
    }

    pub(crate) fn address_for(vk: &[u8; 32], salt: u8) -> String {
        let mut bytes = vec![1u8];
        bytes.extend([salt; 40]);
        bytes.extend(vk);
        let hrp = Hrp::parse("0zk").unwrap();
        bytes.iter().copied().bytes_to_fes().with_checksum::<Bech32m>(&hrp).chars().collect()
    }

    pub(crate) fn signed_message(sk: &SigningKey, addr: &str, fees: &[(&str, &str)], exp: i64) -> Vec<u8> {
        let fees: serde_json::Map<String, serde_json::Value> =
            fees.iter().map(|(k, v)| ((*k).to_string(), serde_json::Value::String((*v).to_string()))).collect();
        let body = serde_json::json!({
            "fees": fees, "feeExpiration": exp, "feesID": "abc", "railgunAddress": addr,
            "availableWallets": 1, "version": "8.3.0", "relayAdapt": "0xF82d00fC51F730F42A00F85E74895a2849ffF2Dd",
            "requiredPOIListKeys": ["efc6"], "reliability": 0.99, "someFutureField": true
        })
        .to_string();
        let sig = sk.sign(body.as_bytes());
        serde_json::json!({"data": hex::encode(body.as_bytes()), "signature": hex::encode(sig.to_bytes())})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn example_address_decodes() {
        let c = cfg();
        assert!(viewing_public_key(&c.broadcaster.railgun_address).is_ok());
    }

    #[test]
    fn verifies_signatures() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let addr = address_for(&sk.verifying_key().to_bytes(), 3);
        let msg = signed_message(&sk, &addr, &[("0xAA", "0x10")], 5);
        let (body, valid) = decode_fee_message(&msg).unwrap();
        assert!(valid);
        assert_eq!(body.fee_expiration, 5);
        // Tampered data → invalid.
        let other = SigningKey::from_bytes(&[8u8; 32]);
        let forged = signed_message(&other, &addr, &[("0xAA", "0x10")], 5);
        assert!(!decode_fee_message(&forged).unwrap().1);
    }

    fn offer(addr: &str, token: &str, rate: u128, exp: i64, reliability: Option<f64>) -> Offer {
        Offer {
            railgun_address: addr.into(),
            fees: HashMap::from([(token.to_string(), U256::from(rate))]),
            fee_expiration_ms: exp,
            version: None,
            available_wallets: Some(1),
            reliability,
            identifier: None,
            relay_adapt: None,
            poi_lists: vec![],
            sig_valid: true,
            seen_us: 0,
        }
    }

    const TOKEN: &str = "0x0d500b1d8e8ef31e21c99d1db9a6444d3adf1270";
    const E18: u128 = 1_000_000_000_000_000_000;

    #[test]
    fn ranks_and_pool_without_trusted_signers() {
        let c = cfg();
        let mut m = Market::new(&c);
        m.trusted.clear();
        let now = 1_000_000;
        for (a, r) in [("A", 1050), ("B", 1060), ("C", 1200), ("D", 1100)] {
            m.upsert(137, offer(a, TOKEN, E18 * r / 1000, now + 60_000, None));
        }
        // We're not on Waku; our logged rate is 1.048x → cheapest → wallet pick.
        let v = m.view(&c, 137, TOKEN, now, Some(U256::from(E18 * 1048 / 1000)), None);
        assert_eq!(v.n_broadcasters, 4);
        assert_eq!(v.n_eligible, 4);
        assert_eq!(v.our_source, Some("log"));
        assert_eq!(v.our_rank, Some(1));
        assert_eq!(v.picked, Some(true));
        assert_eq!(v.change_for_pick, Some(0.0));
        assert_eq!(v.in_pool, Some(true));
        assert_eq!(v.pool_size, 3); // A 1.05, B 1.06, D 1.10 ≤ 1.05 × 1.05; C 1.20 is out
        // Reference = our rate ÷ (1 + fee_bonus), so we show as 1.048× and the others at face value.
        assert!((v.our_multiplier.unwrap() - 1.048).abs() < 1e-9);
        assert!((v.cheapest_multiplier.unwrap() - 1.05).abs() < 1e-9);

        // Offers with < 40 s left are ignored, like the wallet does.
        let v = m.view(&c, 137, TOKEN, now + 30_000, None, Some(1e18));
        assert_eq!(v.n_broadcasters, 0);

        // Out of pool: our rate 1.2x vs cheapest 1.05x → need ≈ −8 % for the pool, −12.5 % to be the pick.
        let v = m.view(&c, 137, TOKEN, now, Some(U256::from(E18 * 1200 / 1000)), None);
        assert_eq!(v.in_pool, Some(false));
        assert_eq!(v.picked, Some(false));
        assert_eq!(v.our_rank, Some(4));
        assert!((v.change_for_pool.unwrap() - (1.05 * 1.05 / 1.2 - 1.0)).abs() < 1e-9);
        assert!((v.change_for_pick.unwrap() - (1.05 / 1.2 - 1.0)).abs() < 1e-9);
    }

    #[test]
    fn trusted_band_decides_the_pick() {
        let c = cfg();
        let mut m = Market::new(&c);
        m.trusted = vec!["T1".into(), "T2".into()];
        let now = 1_000_000;
        let exp = now + 60_000;
        // Trusted average 1.00 → band 0.90 … 1.30.
        m.upsert(137, offer("T1", TOKEN, E18 * 95 / 100, exp, Some(0.8)));
        m.upsert(137, offer("T2", TOKEN, E18 * 105 / 100, exp, Some(0.8)));
        m.upsert(137, offer("undercut", TOKEN, E18 * 85 / 100, exp, Some(1.0)));
        m.upsert(137, offer("X", TOKEN, E18 * 92 / 100, exp, Some(0.5)));
        m.upsert(137, offer("Y", TOKEN, E18 * 92 / 100, exp, Some(0.9)));
        m.upsert(137, offer("pricey", TOKEN, E18 * 140 / 100, exp, Some(1.0)));

        let v = m.view(&c, 137, TOKEN, now, Some(U256::from(E18)), None);
        assert_eq!(v.trusted_avg.as_deref(), Some(E18.to_string().as_str()));
        assert_eq!(v.band, Some(((E18 * 9 / 10).to_string(), (E18 * 13 / 10).to_string())));
        assert_eq!(v.n_broadcasters, 6);
        assert_eq!(v.n_eligible, 4);
        // Same fee: higher reliability wins.
        assert_eq!(v.pick.as_deref(), Some("Y"));
        let e = |a: &str| v.entries.iter().find(|e| e.address == a).unwrap();
        assert_eq!(e("undercut").eligibility, Eligibility::TooCheap);
        assert_eq!(e("pricey").eligibility, Eligibility::TooExpensive);
        assert!(e("Y").is_pick && !e("X").is_pick && e("T1").trusted);
        assert!(!e("undercut").in_pool);

        // Us at 1.00: eligible, 4th of the eligible field; must go just under 0.92 to be the pick.
        assert_eq!(v.our_eligibility, Some(Eligibility::Eligible));
        assert_eq!(v.picked, Some(false));
        assert_eq!(v.our_rank, Some(4));
        assert!((v.change_for_pick.unwrap() + 0.08).abs() < 1e-9);

        // Above the band: wallets drop us; the hint stays inside the band.
        let v = m.view(&c, 137, TOKEN, now, Some(U256::from(E18 * 135 / 100)), None);
        assert_eq!(v.our_eligibility, Some(Eligibility::TooExpensive));
        assert_eq!(v.picked, Some(false));

        // When the best other offer sits on the band floor, the hint stops at the floor.
        m.upsert(137, offer("floor", TOKEN, E18 * 9 / 10, exp, Some(1.0)));
        let v = m.view(&c, 137, TOKEN, now, Some(U256::from(E18)), None);
        assert!((v.change_for_pick.unwrap() + 0.10).abs() < 1e-9);

        // No trusted signer prices the token: wallets show nobody.
        let v = m.view(&c, 137, "0xother", now, Some(U256::from(E18)), None);
        assert_eq!(v.our_eligibility, Some(Eligibility::NoTrustedFee));
        assert_eq!(v.n_eligible, 0);
        assert_eq!(v.change_for_pick, None);
    }

    #[test]
    fn our_live_offer_is_the_pick() {
        let c = cfg();
        let mut m = Market::new(&c);
        m.trusted = vec!["T1".into()];
        let now = 1_000_000;
        let us = c.broadcaster.railgun_address.clone();
        m.upsert(137, offer("T1", TOKEN, E18, now + 60_000, Some(0.9)));
        m.upsert(137, offer(&us, TOKEN, E18 * 95 / 100, now + 60_000, Some(0.99)));
        let v = m.view(&c, 137, TOKEN, now, None, None);
        assert_eq!(v.our_source, Some("waku"));
        assert_eq!(v.picked, Some(true));
        assert_eq!(v.our_rank, Some(1));
        assert_eq!(v.change_for_pick, Some(0.0));
        assert!(v.entries.iter().any(|e| e.is_us && e.is_pick));
    }
}

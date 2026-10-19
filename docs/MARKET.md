# How wallets pick a broadcaster, and what the Market page shows

A broadcaster only earns fees if a wallet chooses it. The Market page therefore doesn't just rank fees: it reproduces, as closely as is practical, the selection that wallets apply to the signed fee offers that every broadcaster publishes on Waku.

> Verified on 2026-10-19 against [`waku-broadcaster-client`](https://github.com/Railgun-Community/waku-broadcaster-client) `ef8d2b6`, the Railway wallet (`e9398a1`, remote config `railway-config-v3.3.json`), Terminal Wallet (`503d322`) and Rubic's integration. Wallet behaviour can change; treat this page as a model, not a guarantee, and see [Not modelled](#not-modelled).

## The selection, in two steps

**1. Trusted-fee band.** Wallets may be configured with *trusted fee signers*, a list of 0zk addresses (Railway's remote config lists four, Terminal Wallet defaults to one). For each token, the wallet averages the fees those signers currently publish and drops every offer outside

```
[ average × (1 − 10 %),  average × (1 + 30 %) ]
```

If no trusted signer prices a token, the wallet shows **no broadcaster at all** for it. Undercutting the band is therefore not a way to win: an offer below the floor is invisible, exactly like one above the ceiling.

**2. The pick.** Of the remaining offers, `findBestBroadcaster` takes the **cheapest**, with higher reliability breaking exact ties. The wallets we checked (Railway, Terminal Wallet, Rubic) all call it. The client library also offers `findRandomBroadcasterForToken`, which picks at random among offers within 5 % of the cheapest. We found no wallet using it, so `rbm` shows it as a reference ("Random pool") only.

Before either step a wallet discards offers that are expired or expire within 40 seconds (it needs time to prove and submit), have zero available wallets, carry an invalid signature, or come from an unsupported broadcaster version. `rbm` applies all of these except the version check; see [Not modelled](#not-modelled).

## What the page shows

Per chain and token:

| Column | Meaning |
|---|---|
| **Our rate** | Our published rate, as a multiple of the market price (our rate ÷ (1 + `fee_bonus`) is the reference). Marked `(log)` if it is only known from our own logs and not yet seen on Waku. |
| **Wallets** | **Wallet pick**: a wallet would choose us right now. **Eligible, not cheapest**: inside the band but beaten. **Above band** / **Below band**: wallets ignore us. **No trusted fee**: no trusted signer prices the token, so wallets show nobody (normal for a few minutes after `rbm` starts, until their offers arrive). |
| **Rank** | Our place among eligible offers (among all offers if we are outside the band). |
| **Pick** | The rate that wins right now. |
| **Band** | The accepted range around the trusted average. |
| **Picked, 24 h** | Share of the last 24 hours of samples in which a wallet would have chosen us. |
| **Change to be pick** | Relative change of our rate that would make us the pick: just under the best other eligible offer, but never below the band floor. `pick` means we already are; `≈ 0 % (tie)` means we tie on rate and reliability decides. |
| **Random pool** | Whether we would be inside the SDK's 5 % random pool, with its size. For reference. |

Expanding a token lists every live offer with its rate, ratio to the pick, eligibility, reliability, version and time to expiry. Trusted signers are marked.

`rbm waku-probe --chain 137` prints the same table on the command line, handy for cross-checking against another viewer such as [railgun-broadcaster-viewer](https://github.com/suicide/railgun-broadcaster-viewer).

## How the data is collected

- `rbm` joins the Waku network as a **light client** (filter and store protocols, cluster 5, shard 1) using the client from [`railgun-rust`](https://github.com/triamazikamno/railgun-rust). It publishes nothing and accepts no inbound connections.
- It subscribes to `/railgun/v2/0-<chainId>-fees/json` for every configured chain and backfills a short history from a store node on connect.
- Each message is an envelope `{data, signature}`. The Ed25519 signature is verified against the viewing key embedded in the sender's 0zk address (bytes 41..73 of the bech32m payload). Offers with an invalid signature are counted but never used.
- Rates are token base units per 1e18 wei of gas. Every `waku.sample_secs` the current view is stored (one row per chain and token) for the "Picked, 24 h" history; every `waku.fee_sample_secs` every live broadcaster's rate is stored too (pruned after `storage.fee_history_days`) so you can see how others move.

## Configuration

```toml
[waku]
# Offers outside −10 % / +30 % of the trusted signers' average are ignored by wallets.
# trusted_fee_signers = ["0zk1…"]     # default: Railway's list as of 2026-10-19
trusted_fee_lower = 0.10
trusted_fee_upper = 0.30
pool_factor = 1.05                    # only the "Random pool" column
```

An empty `trusted_fee_signers = []` disables the band (a wallet without trusted signers, like Rubic's integration, simply takes the cheapest offer). The default list is a snapshot: if Railway changes its remote config, update it here, otherwise the verdict drifts.

## Not modelled

- **Wallet-side blocklists and allowlists.** Wallets can filter broadcaster addresses; users can block one. `rbm` can't see that.
- **Broadcaster version range.** The client rejects offers whose version is outside its configured range (`8.0.0`–`8.999.0` by default). `rbm` counts them.
- **RelayAdapt matching.** For transactions through RelayAdapt, the client requires the offer's `relayAdapt` address to be one it knows. `rbm` doesn't check it.
- **Several identifiers per address.** A broadcaster address can publish under more than one identifier; `rbm` keeps one (the latest) offer per address.
- **Which wallet.** Different wallets can be configured differently; the model follows the common case above. Users can also pick a broadcaster by hand, so relays can reach you even when you are not the automatic pick.

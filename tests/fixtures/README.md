# Log fixtures

Log lines from a RAILGUN broadcaster running [`triamazikamno/railgun-broadcaster`](https://github.com/triamazikamno/railgun-broadcaster) at `e5b36fc`, with ANSI colours kept because the parser has to cope with them. They drive the unit tests and `tests/acceptance.rs`.

| File | Contents |
|---|---|
| `first-relay-2026-10-17.log` | One complete relay on Ethereum: request parsed, gas estimated, transaction broadcast and submitted. The acceptance test serves a receipt from a mock RPC and checks fee, cost, profit and route. The transaction hash is made up; gas figures and amounts are as logged. |
| `ppoi-rejection.log` | A rejected request (`Poi/ValidateList/InvalidSnarkProof`) that has no chain span. |
| `our-fees-polygon.log` | The broadcaster's per-minute fee publication, plus the RPC warnings that surround it. |

## Rules for fixtures

- **Real formats only.** Don't invent log formats; the point is to catch drift against the real thing. Values that identify an operator may be replaced (see below), the shape of the line may not.
- **Nothing secret.** Never add a line containing a mnemonic, private key, viewing key, `config.yaml` content or a notification topic. Run `rbm replay` on the file and check the database dump the way `no_key_material_in_db` does.
- **Anonymise operator identity** where it isn't needed for the test. The operator's own 0zk address in `our-fees-polygon.log` is replaced by the placeholder from `config.example.toml`, and the relay's transaction hash is made up, so neither points directly at a wallet. (Block number and gas figures are as logged; they are not secret, just not a pointer.)
- **Public chain data is fine** (transaction hashes, token and contract addresses, other broadcasters' fees).

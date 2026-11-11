# Public launch checklist

## Decisions
- [x] **Launch order:** run the DAGKnight **testnet 13** publicly first (DAGKnight on from genesis). Mainnet
  (`aero-mainnet`) keeps `dagknight_activation: never()` until DAGKnight has run cleanly on testnet 13; launching
  mainnet then means either launching without DAGKnight or setting its activation before the genesis is public.
- [x] **Genesis frozen.** The mainnet genesis (`consensus/core/src/config/genesis.rs`) is verified by
  `cargo test -p aero-consensus-core genesis`. Once anyone else runs it, any change to it (message, timestamp,
  bits) forks the chain. Edit it, regenerate with `gen_mainnet_genesis`, and re-run the test *before* publishing.

## Before publishing
- [x] Repo lives at `arkunati-png/rusty-aero`; user-facing upstream links (README, CONTRIBUTING, wasm docs, bridge UI)
  are replaced. What remains is deliberate: `NOTICE`/`LICENSE` credit, code comments citing upstream KIPs, and the
  historical Crescendo/Toccata guides in `docs/`.
- [x] README states the network status (testnet 13 public, mainnet not live) and how to get Aero Core.
- [ ] Merge `dagknight-swap` into `main` and push.
- [ ] CI green (`cargo check --tests --workspace --benches`, tests, clippy, fmt).
- [ ] Tag a release; `deploy.yaml` builds `aero-core` (desktop app), `aerod`, `aero-wallet`, `aero-miner`,
  `stratum-bridge`, `rothschild` for **Windows only** (Linux and macOS were removed until tested). The workflow
  has never run: check the first run. Re-adding Linux means a musl build plus a gnu build of `aero-core` with
  GL/X11/Wayland dev packages; see this file's git history.
- [ ] Windows installer / signed binaries (unsigned `.exe` will trigger SmartScreen warnings).
- [ ] `SECURITY.md` with a vulnerability-reporting contact.
- [ ] Scan git history for committed secrets (keys, seed phrases, wallet files) before making the repo public.
- [ ] Publish a Docker image built from `docker/Dockerfile.aerod`.

## Network infrastructure (see `running-a-seed-node.md`)
- [ ] 2–3 seed nodes on static public IPs with the P2P port open.
- [ ] DNS seeder, then fill `dns_seeders` in `consensus/core/src/config/params.rs`.
- [ ] Public RPC node(s); optionally a wRPC resolver (`rpc/wrpc/client/Resolvers.toml`).
- [ ] Block explorer. Set `AERO_EXPLORER_URL` at build time so the CLI links to it.
- [ ] Faucet (testnet only).

## Mining (see `mining.md`)
- [ ] Release `aero-miner`; document solo and `stratum-bridge` setups.
- [ ] Mine the first blocks yourself so new nodes have something to sync.

## Verification on real machines
- [ ] A fresh node on another machine syncs from the seeds (IBD, then stays synced).
- [ ] A node syncs after pruning has kicked in.
- [ ] Wallet sends and receives a transaction between two machines.

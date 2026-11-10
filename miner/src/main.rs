//! Aero solo miner: fetches block templates from an `aerod` node over gRPC, searches for a RandomX nonce on
//! `--threads` threads, and submits any block that meets the target. The engine lives in the `aero_miner` library,
//! which the Aero Core desktop app also uses for its Mine tab.
//!
//! By default it mines in RandomX *fast* mode, which builds a ~2 GiB dataset once at startup (it needs roughly
//! 2.5 GiB of free RAM). Pass `--light` to use the 256 MiB cache instead on small machines: far slower per hash,
//! but the hashes are identical.

use std::time::Duration;

use aero_addresses::Address;
use aero_core::info;
use aero_miner::{Miner, MinerConfig, default_threads};
use clap::Parser;

#[derive(Parser)]
#[command(name = "aero-miner", about = "Aero RandomX solo miner")]
struct Args {
    /// gRPC address (host:port) of the aerod node to mine for
    #[arg(short = 's', long, default_value = "127.0.0.1:16110")]
    rpcserver: String,

    /// Address that receives the block rewards
    #[arg(short = 'a', long)]
    mining_address: String,

    /// Number of mining threads (default: all logical CPUs)
    #[arg(short = 't', long)]
    threads: Option<usize>,

    /// Use RandomX light mode (256 MiB) instead of fast mode (~2 GiB dataset)
    #[arg(long)]
    light: bool,

    /// How often to fetch a fresh block template, in milliseconds
    #[arg(long, default_value_t = 250)]
    poll_ms: u64,

    /// Keep mining even if the node reports that it is not synced
    #[arg(long)]
    mine_unsynced: bool,
}

#[tokio::main]
async fn main() {
    aero_core::log::init_logger(None, "");
    let args = Args::parse();

    let pay_address = Address::try_from(args.mining_address.as_str()).expect("invalid --mining-address");
    let mut config = MinerConfig::new(args.rpcserver, pay_address);
    config.threads = args.threads.unwrap_or_else(default_threads).max(1);
    config.light = args.light;
    config.poll_ms = args.poll_ms;
    config.mine_unsynced = args.mine_unsynced;

    let miner = Miner::start(config);
    let mut check = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = check.tick() => {
                // The session ends by itself only on a fatal error; otherwise it runs until Ctrl+C.
                if !miner.is_active() {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                miner.stop();
                break;
            }
        }
    }
    // Give the workers a moment to wind down.
    while miner.is_active() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

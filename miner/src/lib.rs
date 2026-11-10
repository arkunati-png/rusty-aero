//! The Aero RandomX mining engine, shared by the `aero-miner` command-line tool and the Aero Core desktop app.
//!
//! [`Miner::start`] spawns a background thread that fetches block templates from an `aerod` node over gRPC, searches
//! for a RandomX nonce on [`MinerConfig::threads`] OS threads, and submits any block that meets the target. Progress is
//! published through [`Miner::stats`], so a UI can poll it without blocking.
//!
//! In RandomX *fast* mode the miner builds a ~2 GiB dataset once (it needs roughly 2.5 GiB of free RAM). *Light* mode
//! uses the 256 MiB cache instead: far slower per hash, but the hashes are identical.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use aero_addresses::Address;
use aero_consensus_core::header::Header;
use aero_core::{info, warn};
use aero_grpc_client::GrpcClient;
use aero_notify::subscription::context::SubscriptionContext;
use aero_pow::{Hasher, State};
use aero_rpc_core::{RpcRawBlock, api::rpc::RpcApi, notify::mode::NotificationMode};
use parking_lot::Mutex;
use rand::Rng;
use tokio::sync::mpsc;

/// Hashes between checks for fresher work. Small, so a new template is picked up within a few hashes.
const HASHES_PER_WORK_CHECK: usize = 4;
/// How often the hash rate shown in [`MinerStats`] is refreshed.
const STATS_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct MinerConfig {
    /// gRPC address (`host:port`) of the `aerod` node to mine for.
    pub rpc_address: String,
    /// Address that receives the block rewards.
    pub pay_address: Address,
    /// Number of mining threads (at least 1).
    pub threads: usize,
    /// Use RandomX light mode (256 MiB) instead of fast mode (~2 GiB dataset).
    pub light: bool,
    /// How often to fetch a fresh block template, in milliseconds.
    pub poll_ms: u64,
    /// Keep mining even if the node reports that it is not synced (only for the first node of a new network).
    pub mine_unsynced: bool,
}

impl MinerConfig {
    pub fn new(rpc_address: String, pay_address: Address) -> Self {
        Self { rpc_address, pay_address, threads: default_threads(), light: false, poll_ms: 250, mine_unsynced: false }
    }
}

/// All logical CPUs, as a default for the command line tool.
pub fn default_threads() -> usize {
    thread::available_parallelism().map_or(1, |n| n.get())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinerPhase {
    /// Not started, or fully stopped.
    Idle,
    /// Building the RandomX cache / dataset.
    Initializing,
    /// Connecting to the node.
    Connecting,
    /// Connected, but the node is not synced, so there is nothing to mine on yet.
    WaitingForSync,
    /// Hashing.
    Mining,
    /// Stop requested; shutting the workers down.
    Stopping,
}

#[derive(Clone, Debug)]
pub struct MinerStats {
    pub phase: MinerPhase,
    /// The last problem worth showing the user (a failed connection, a rejected block...).
    pub message: String,
    /// Hashes per second over the last few seconds.
    pub hashrate: f64,
    pub total_hashes: u64,
    /// Blocks that met the target and were submitted.
    pub blocks_found: u64,
    pub blocks_accepted: u64,
    pub blocks_rejected: u64,
    /// When mining began, for showing the uptime.
    pub started_at: Option<Instant>,
}

impl Default for MinerStats {
    fn default() -> Self {
        Self {
            phase: MinerPhase::Idle,
            message: String::new(),
            hashrate: 0.0,
            total_hashes: 0,
            blocks_found: 0,
            blocks_accepted: 0,
            blocks_rejected: 0,
            started_at: None,
        }
    }
}

/// A running (or finished) mining session. Dropping it asks the session to stop.
pub struct Miner {
    stop: Arc<AtomicBool>,
    stats: Arc<Mutex<MinerStats>>,
}

impl Miner {
    /// Starts mining in the background and returns immediately.
    pub fn start(config: MinerConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(MinerStats { phase: MinerPhase::Initializing, started_at: Some(Instant::now()), ..Default::default() }));

        let (stop_, stats_) = (Arc::clone(&stop), Arc::clone(&stats));
        thread::Builder::new()
            .name("miner".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("failed to start the async runtime");
                runtime.block_on(run(config, Arc::clone(&stop_), Arc::clone(&stats_)));
                let mut stats = stats_.lock();
                stats.phase = MinerPhase::Idle;
                stats.hashrate = 0.0;
            })
            .expect("failed to start the mining thread");

        Self { stop, stats }
    }

    /// Asks the session to stop. This does not block: [`Miner::stats`] reports [`MinerPhase::Idle`] once it has.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut stats = self.stats.lock();
        if stats.phase != MinerPhase::Idle {
            stats.phase = MinerPhase::Stopping;
        }
    }

    pub fn stats(&self) -> MinerStats {
        self.stats.lock().clone()
    }

    /// True until the session has fully stopped.
    pub fn is_active(&self) -> bool {
        self.stats.lock().phase != MinerPhase::Idle
    }
}

impl Drop for Miner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One block template and the PoW state derived from it.
struct Job {
    id: u64,
    block: RpcRawBlock,
    state: State,
}

struct Shared {
    job: Mutex<Option<Arc<Job>>>,
    /// Id of the newest job, so workers can cheaply detect new work without taking the lock.
    latest_job_id: AtomicU64,
    /// Id of the newest job that already has a solution, so we stop hashing it and wait for fresh work instead of
    /// submitting a pile of sibling blocks for the same template (which bloats the DAG, most visibly at low difficulty).
    solved_job_id: AtomicU64,
    stop: AtomicBool,
    hashes: AtomicU64,
}

fn make_job(id: u64, block: RpcRawBlock) -> Result<Job, String> {
    let header: Header = (&block.header).try_into().map_err(|e| format!("invalid block template header: {e:?}"))?;
    Ok(Job { id, state: State::new(&header), block })
}

fn worker(shared: Arc<Shared>, light: bool, found: mpsc::UnboundedSender<(Arc<Job>, u64)>) {
    let mut hasher = if light { Hasher::light() } else { Hasher::fast() };
    let mut rng = rand::thread_rng();

    let mut job: Option<Arc<Job>> = None;
    let mut nonce = 0u64;
    while !shared.stop.load(Ordering::Relaxed) {
        let newest = shared.latest_job_id.load(Ordering::Acquire);
        if job.as_ref().is_none_or(|j| j.id != newest) {
            match shared.job.lock().clone() {
                Some(fresh) => {
                    job = Some(fresh);
                    // A random starting point keeps threads (and restarts) from repeating each other's work.
                    nonce = rng.r#gen();
                }
                None => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            }
        }

        let current = job.as_ref().expect("job was just set");
        if shared.solved_job_id.load(Ordering::Relaxed) == current.id {
            // This template is solved; wait for the next one.
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        for _ in 0..HASHES_PER_WORK_CHECK {
            let (passed, _) = current.state.check_pow_with(&mut hasher, nonce);
            if passed {
                // Only the first solution for a template is submitted; the others are discarded.
                if shared.solved_job_id.swap(current.id, Ordering::AcqRel) != current.id {
                    let _ = found.send((Arc::clone(current), nonce));
                }
                break;
            }
            nonce = nonce.wrapping_add(1);
        }
        shared.hashes.fetch_add(HASHES_PER_WORK_CHECK as u64, Ordering::Relaxed);
    }
}

async fn submit(client: &GrpcClient, job: &Job, nonce: u64, stats: &Mutex<MinerStats>) {
    let mut block = job.block.clone();
    block.header.nonce = nonce;
    stats.lock().blocks_found += 1;
    match client.submit_block(block, false).await {
        Ok(response) if response.report.is_success() => {
            info!("Block found and accepted by the node (nonce {nonce})");
            stats.lock().blocks_accepted += 1;
        }
        Ok(response) => {
            warn!("Block found but rejected by the node: {:?}", response.report);
            let mut stats = stats.lock();
            stats.blocks_rejected += 1;
            stats.message = format!("A block was rejected by the node: {:?}", response.report);
        }
        Err(e) => {
            warn!("Block found but submit_block failed: {e}");
            let mut stats = stats.lock();
            stats.blocks_rejected += 1;
            stats.message = format!("Submitting a block failed: {e}");
        }
    }
}

async fn run(config: MinerConfig, stop: Arc<AtomicBool>, stats: Arc<Mutex<MinerStats>>) {
    let threads = config.threads.max(1);

    // Build the shared RandomX state up front, so the first template isn't wasted waiting on it.
    if config.light {
        info!("Initializing the RandomX cache (256 MiB)...");
        tokio::task::spawn_blocking(|| drop(aero_pow::cache())).await.expect("cache initialization panicked");
    } else {
        info!("Building the RandomX dataset (~2 GiB). This takes a while, then mining is much faster per hash...");
        tokio::task::spawn_blocking(|| drop(aero_pow::dataset())).await.expect("dataset build panicked");
    }
    if stop.load(Ordering::Relaxed) {
        return;
    }

    stats.lock().phase = MinerPhase::Connecting;
    info!("Connecting to grpc://{}", config.rpc_address);
    let client = loop {
        let attempt = GrpcClient::connect_with_args(
            NotificationMode::Direct,
            format!("grpc://{}", config.rpc_address),
            Some(SubscriptionContext::new()),
            true,
            None,
            false,
            Some(500_000),
            Default::default(),
        )
        .await;
        match attempt {
            Ok(client) => break client,
            Err(e) => {
                warn!("Failed to connect to the node's gRPC server: {e}");
                stats.lock().message = format!("Cannot reach the node at {}: {e}", config.rpc_address);
                // Retry, so mining starts by itself once the node is up. Wake often to notice a stop request.
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    };
    info!(
        "Connected. Mining to {} with {threads} thread(s), {} mode",
        config.pay_address,
        if config.light { "light" } else { "fast" }
    );
    {
        let mut stats = stats.lock();
        stats.phase = MinerPhase::Mining;
        stats.message.clear();
    }

    let shared = Arc::new(Shared {
        job: Mutex::new(None),
        latest_job_id: AtomicU64::new(0),
        solved_job_id: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        hashes: AtomicU64::new(0),
    });
    let (found_tx, mut found_rx) = mpsc::unbounded_channel::<(Arc<Job>, u64)>();
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (shared, found_tx, light) = (Arc::clone(&shared), found_tx.clone(), config.light);
            thread::spawn(move || worker(shared, light, found_tx))
        })
        .collect();
    drop(found_tx);

    let mut template_tick = tokio::time::interval(Duration::from_millis(config.poll_ms.max(10)));
    template_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats_tick = tokio::time::interval(STATS_INTERVAL);
    let mut stop_tick = tokio::time::interval(Duration::from_millis(100));
    let (mut last_hashes, mut last_stats) = (0u64, Instant::now());
    let mut next_id = 1u64;

    loop {
        tokio::select! {
            _ = template_tick.tick() => {
                match client.get_block_template(config.pay_address.clone(), vec![]).await {
                    Ok(template) if !template.is_synced && !config.mine_unsynced => {
                        warn!("Node is not synced; waiting (use --mine-unsynced to override)");
                        *shared.job.lock() = None;
                        let mut stats = stats.lock();
                        stats.phase = MinerPhase::WaitingForSync;
                        stats.message = "The node is not synced yet. Mining starts once it has caught up.".into();
                    }
                    Ok(template) => match make_job(next_id, template.block) {
                        Ok(job) => {
                            *shared.job.lock() = Some(Arc::new(job));
                            shared.latest_job_id.store(next_id, Ordering::Release);
                            next_id += 1;
                            let mut stats = stats.lock();
                            stats.phase = MinerPhase::Mining;
                            if stats.message.starts_with("The node is not synced") || stats.message.starts_with("get_block_template") {
                                stats.message.clear();
                            }
                        }
                        Err(e) => warn!("{e}"),
                    },
                    Err(e) => {
                        warn!("get_block_template failed: {e}");
                        stats.lock().message = format!("get_block_template failed: {e}");
                    }
                }
            }
            Some((job, nonce)) = found_rx.recv() => {
                submit(&client, &job, nonce, &stats).await;
            }
            _ = stats_tick.tick() => {
                let hashes = shared.hashes.load(Ordering::Relaxed);
                let secs = last_stats.elapsed().as_secs_f64().max(0.001);
                let rate = (hashes - last_hashes) as f64 / secs;
                info!("Hashrate: {rate:.1} H/s");
                {
                    let mut stats = stats.lock();
                    stats.hashrate = rate;
                    stats.total_hashes = hashes;
                }
                (last_hashes, last_stats) = (hashes, Instant::now());
            }
            _ = stop_tick.tick() => {
                if stop.load(Ordering::Relaxed) {
                    info!("Shutting down the miner...");
                    break;
                }
            }
        }
    }

    shared.stop.store(true, Ordering::Relaxed);
    // Workers notice the flag within a few hashes; joining them off the async threads keeps the runtime responsive.
    let _ = tokio::task::spawn_blocking(move || {
        for handle in workers {
            let _ = handle.join();
        }
    })
    .await;
}

//! Node management: starts `aerod` (unless one is already running), keeps a gRPC connection to it, and publishes a
//! snapshot of its state for the UI. All RPC work happens on a background thread; the UI only reads the snapshot.

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use aero_addresses::Address;
use aero_consensus_core::{
    config::params::Params,
    network::{NetworkId, NetworkType},
    tx::{Transaction, TransactionOutpoint, UtxoEntry},
};
use aero_grpc_client::GrpcClient;
use aero_notify::subscription::context::SubscriptionContext;
use aero_rpc_core::{RpcTransaction, api::rpc::RpcApi, notify::mode::NotificationMode};
use tokio::sync::mpsc;

/// How many confirmations (in DAA score) an ordinary output needs before it counts as available.
pub const CONFIRMATIONS: u64 = 10;

/// Whether mining is allowed while the node says it is not synced. True on every test network: a new one has no
/// recent blocks, so its node never counts as synced until somebody mines the first blocks. Only mainnet waits for sync.
pub fn mines_unsynced(network: NetworkType) -> bool {
    !matches!(network, NetworkType::Mainnet)
}

/// How long to wait for the node before treating it as gone and reconnecting.
const RPC_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub network: NetworkType,
    /// Testnet number (13 is the public Aero testnet); ignored on other networks.
    pub testnet_suffix: u32,
    /// Start `aerod` ourselves. Set to false to attach to a node that is already running.
    pub spawn: bool,
    pub aerod_path: PathBuf,
    pub rpc_address: String,
    /// Folder for the wallet file and (in `node/`) the node's chain data.
    pub data_dir: PathBuf,
    /// `--p2p-port`: the node's P2P listen port, when running several instances on one machine.
    pub p2p_port: Option<u16>,
    /// `--addpeer`: a peer to connect to at startup.
    pub add_peer: Option<String>,
    /// Ask the router to forward the P2P port automatically (UPnP), so other people can connect to this node.
    pub upnp: bool,
}

/// Everything `aerod` prints, overwritten at each start.
fn node_output_path(config: &NodeConfig) -> PathBuf {
    config.data_dir.join("node").join("aerod-output.log")
}

fn read_node_output(config: &NodeConfig) -> String {
    std::fs::read_to_string(node_output_path(config)).unwrap_or_default()
}

/// A plain-language reason why `aerod` stopped, from what it printed on the way out.
fn explain_exit(output: &str) -> Option<String> {
    if output.contains("Don't run on mainnet") {
        return Some("mainnet is not enabled in this version yet. Use the testnet.".into());
    }
    if output.contains("Address already in use") || output.contains("os error 10048") {
        return Some("another program, or another copy of the node, is already using its network port.".into());
    }
    if output.contains("LOCK") && output.contains("lock") && output.contains("IO error") {
        return Some("the node's database is in use by another copy of the node. Close it and try again.".into());
    }
    let last = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("note:") && !l.starts_with("Exiting"))
        .next_back()?;
    Some(last.chars().take(140).collect())
}

/// Where `aerod` writes its log, so the app can read the port forwarding result from it.
fn node_log_dir(config: &NodeConfig) -> PathBuf {
    config.data_dir.join("node").join("logs")
}

/// What the node's automatic port forwarding (UPnP) managed to do, as far as its log says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortForwarding {
    /// Turned off in the app.
    Disabled,
    /// Nothing in the log yet, or the node is still trying.
    Trying,
    /// The router opened the port; other people can reach this node at this public `ip:port`.
    Working(String),
    /// The computer itself has a public address, so no router mapping was needed. Whether others can connect then
    /// depends on the router's firewall letting the port through.
    PublicAddress(String),
    /// The router did not open the port (no UPnP, or the ISP shares one public address between customers).
    Unavailable(String),
}

/// Reads the end of the node's log for the latest port forwarding result.
pub fn port_forwarding_status(config: &NodeConfig) -> PortForwarding {
    use std::io::{Read, Seek, SeekFrom};

    if !config.upnp {
        return PortForwarding::Disabled;
    }
    let Ok(mut file) = std::fs::File::open(node_log_dir(config).join("rusty-aero.log")) else {
        return PortForwarding::Trying;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(256 * 1024)));
    let mut text = String::new();
    // Lossy: the window may start in the middle of a multi-byte character.
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_ok() {
        text = String::from_utf8_lossy(&bytes).into_owned();
    }
    parse_port_forwarding(&text)
}

fn parse_port_forwarding(log: &str) -> PortForwarding {
    // The log keeps earlier runs, so only look at the current one: it starts at the last "Application directory" line.
    let log = log.rfind("Application directory:").map_or(log, |start| &log[start..]);

    // A mapping made through the router is what makes an IPv4 node reachable, so it takes priority.
    for line in log.lines().rev().filter(|l| l.contains("[UPnP] Added port mapping to ")) {
        let rest = line.split("[UPnP] Added port mapping to ").nth(1).unwrap_or("");
        return PortForwarding::Working(rest.rsplit(": ").next().unwrap_or(rest).trim().to_string());
    }

    // No router mapping: the computer may still have a public address of its own (typically IPv6).
    if let Some(line) = log.lines().find(|l| l.contains("Publicly routable local address found: ")) {
        let address = line.split("Publicly routable local address found: ").nth(1).unwrap_or("").trim();
        return PortForwarding::PublicAddress(address.to_string());
    }

    for line in log.lines().rev().filter(|l| l.contains("[UPnP]")) {
        if line.contains("Non-publicly routable") {
            return PortForwarding::Unavailable("your internet provider shares one public address between customers".into());
        }
        if let Some(rest) = line.split("[UPnP] Error adding port mapping: ").nth(1) {
            return PortForwarding::Unavailable(short_reason(rest));
        }
        if line.contains("Attempting to register upnp") {
            return PortForwarding::Trying;
        }
    }
    PortForwarding::Trying
}

/// A short, readable reason for a UPnP failure (the raw error text is long and technical).
fn short_reason(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("did not properly respond") || lower.contains("timed out") || lower.contains("no gateway") {
        "your router did not answer; automatic setup may be switched off on it".into()
    } else {
        error.trim().chars().take(70).collect()
    }
}

#[derive(Clone, Debug)]
pub struct PeerRow {
    pub id: String,
    pub address: String,
    pub outbound: bool,
    pub ping_ms: u64,
    pub user_agent: String,
    pub protocol_version: u32,
    pub connected_secs: u64,
    pub ibd_peer: bool,
}

/// Whether `address` (`ip:port`) belongs to a computer outside the local network: not loopback, not a private
/// (LAN) range, not link-local, and not the carrier-grade NAT range an internet provider uses internally.
pub fn is_outside_address(address: &str) -> bool {
    let Ok(socket) = address.parse::<std::net::SocketAddr>() else {
        return false;
    };
    is_outside_ip(socket.ip())
}

fn is_outside_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let carrier_nat = o[0] == 100 && (o[1] & 0xC0) == 64; // 100.64.0.0/10
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation()
                || carrier_nat
                || o[0] == 0
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_outside_ip(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                let unique_local = first & 0xfe00 == 0xfc00; // fc00::/7
                let link_local = first & 0xffc0 == 0xfe80; // fe80::/10
                !(v6.is_loopback() || v6.is_unspecified() || unique_local || link_local)
            }
        },
    }
}

/// How many peers connected *in* to us from outside our own network. Only these prove that the node is reachable
/// from the internet, which is what makes it a public node.
pub fn outside_inbound_count(peers: &[PeerRow]) -> usize {
    peers.iter().filter(|p| !p.outbound && is_outside_address(&p.address)).count()
}

/// One unspent output belonging to the wallet.
#[derive(Clone, Debug)]
pub struct WalletUtxo {
    pub outpoint: TransactionOutpoint,
    pub entry: UtxoEntry,
    pub address: String,
    pub amount: u64,
    pub daa_score: u64,
    pub is_coinbase: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NodeState {
    /// True once the gRPC connection to the node is established and answering.
    pub connected: bool,
    /// A human-readable status line (starting, connecting, error text...).
    pub status: String,
    pub synced: bool,
    pub utxo_indexed: bool,
    pub server_version: String,
    pub network: String,
    pub block_count: u64,
    pub header_count: u64,
    pub difficulty: f64,
    pub virtual_daa_score: u64,
    /// Median time of the latest blocks, in milliseconds since the Unix epoch.
    pub past_median_time: u64,
    pub mempool_size: u64,
    pub peers: Vec<PeerRow>,
    pub utxos: Vec<WalletUtxo>,
    /// Console output lines, oldest first.
    pub console: Vec<String>,
    /// Outcome of the last send: `Ok(txid)` or `Err(reason)`.
    pub send_result: Option<Result<String, String>>,
}

pub enum Cmd {
    /// A console command line.
    Console(String),
    /// Submit a signed transaction.
    Submit(Box<Transaction>),
}

pub struct Node {
    pub config: NodeConfig,
    pub state: Arc<Mutex<NodeState>>,
    watch: Arc<Mutex<Vec<Address>>>,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    child: Option<Child>,
    /// Set while the user has stopped the node on purpose, so the status line says so instead of reporting a lost connection.
    user_stopped: Arc<AtomicBool>,
}

impl Node {
    pub fn start(config: NodeConfig) -> Self {
        let state = Arc::new(Mutex::new(NodeState { status: "Starting node...".into(), ..Default::default() }));
        let watch: Arc<Mutex<Vec<Address>>> = Arc::default();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let user_stopped = Arc::new(AtomicBool::new(false));
        let child = if config.spawn { spawn_aerod(&config, &state) } else { None };

        {
            let (config, state, watch, user_stopped) = (config.clone(), Arc::clone(&state), Arc::clone(&watch), Arc::clone(&user_stopped));
            std::thread::Builder::new()
                .name("node-rpc".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .enable_all()
                        .build()
                        .expect("failed to start the async runtime");
                    runtime.block_on(run(config, state, watch, user_stopped, cmd_rx));
                })
                .expect("failed to start the RPC thread");
        }

        Self { config, state, watch, cmd_tx, child, user_stopped }
    }

    /// Starts the `aerod` process, optionally connecting to `add_peer` (`ip:port`) first. Does nothing if it is already
    /// running, or if this app is attached to a node it did not start.
    pub fn start_process(&mut self, add_peer: Option<String>, upnp: bool) {
        self.poll_process();
        if self.child.is_some() || !self.config.spawn {
            return;
        }
        self.user_stopped.store(false, Ordering::Relaxed);
        self.config.add_peer = add_peer.filter(|p| !p.trim().is_empty());
        self.config.upnp = upnp;
        self.state.lock().unwrap().status = "Starting node...".into();
        self.child = spawn_aerod(&self.config, &self.state);
    }

    /// Stops the `aerod` process that this app started.
    pub fn stop_process(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        self.user_stopped.store(true, Ordering::Relaxed);
        let _ = child.kill();
        let _ = child.wait();
        let mut s = self.state.lock().unwrap();
        *s = NodeState { status: "The node is stopped. Click \"Start node\" to start it.".into(), ..Default::default() };
    }

    /// True while the `aerod` process started by this app is alive. Notices a process that exited by itself.
    pub fn poll_process(&mut self) -> bool {
        if let Some(child) = &mut self.child
            && let Ok(Some(status)) = child.try_wait()
        {
            self.child = None;
            self.user_stopped.store(true, Ordering::Relaxed);
            let mut s = self.state.lock().unwrap();
            s.connected = false;
            s.status = match explain_exit(&read_node_output(&self.config)) {
                Some(reason) => format!("The node could not run: {reason}"),
                None => format!("The node stopped unexpectedly ({status}). Click \"Start node\" to start it again."),
            };
        }
        self.child.is_some()
    }

    pub fn snapshot(&self) -> NodeState {
        self.state.lock().unwrap().clone()
    }

    /// Tells the node thread which wallet addresses to fetch outputs for.
    pub fn set_watch(&self, addresses: Vec<Address>) {
        *self.watch.lock().unwrap() = addresses;
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn clear_send_result(&self) {
        self.state.lock().unwrap().send_result = None;
    }

    /// Whether this app started (and therefore owns) the node process.
    pub fn owns_process(&self) -> bool {
        self.child.is_some()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_aerod(config: &NodeConfig, state: &Arc<Mutex<NodeState>>) -> Option<Child> {
    let mut command = Command::new(&config.aerod_path);
    command.arg("--utxoindex").arg(format!("--rpclisten={}", config.rpc_address));
    command.arg("--appdir").arg(config.data_dir.join("node"));
    command.arg("--logdir").arg(node_log_dir(config));
    if !config.upnp {
        command.arg("--disable-upnp");
    }
    if let Some(port) = config.p2p_port {
        command.arg(format!("--listen=0.0.0.0:{port}"));
    }
    if let Some(peer) = &config.add_peer {
        command.arg(format!("--addpeer={peer}"));
    }
    match config.network {
        NetworkType::Mainnet => {}
        NetworkType::Testnet => {
            command.arg("--testnet").arg(format!("--netsuffix={}", config.testnet_suffix));
        }
        NetworkType::Devnet => {
            command.arg("--devnet");
        }
        NetworkType::Simnet => {
            command.arg("--simnet");
        }
    }
    // Test networks may have no other miners, so let the node accept locally mined blocks while "unsynced".
    if mines_unsynced(config.network) {
        command.arg("--enable-unsynced-mining");
    }
    // Keep the node's output, so that if it dies right away the app can say why.
    command.stdin(Stdio::null());
    let output = node_output_path(config);
    let _ = std::fs::create_dir_all(output.parent().unwrap_or(&config.data_dir));
    match std::fs::File::create(&output).and_then(|f| f.try_clone().map(|g| (f, g))) {
        Ok((out, err)) => {
            command.stdout(out).stderr(err);
        }
        Err(_) => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    match command.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            state.lock().unwrap().status = format!("Could not start {}: {e}", config.aerod_path.display());
            None
        }
    }
}

async fn run(
    config: NodeConfig,
    state: Arc<Mutex<NodeState>>,
    watch: Arc<Mutex<Vec<Address>>>,
    user_stopped: Arc<AtomicBool>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let subscription_context = SubscriptionContext::new();
    let mut client: Option<GrpcClient> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // The node was stopped on purpose: let go of the old connection. It is set to reconnect by itself, and
                // calls on it would otherwise block until the node comes back, which stops a fresh connection being made.
                if user_stopped.load(Ordering::Relaxed) {
                    if let Some(old) = client.take() {
                        let _ = tokio::time::timeout(Duration::from_secs(2), old.disconnect()).await;
                    }
                    state.lock().unwrap().connected = false;
                    continue;
                }
                if client.is_none() {
                    match tokio::time::timeout(RPC_TIMEOUT, connect(&config.rpc_address, &subscription_context)).await.unwrap_or(None) {
                        Some(c) => client = Some(c),
                        None => {
                            let mut s = state.lock().unwrap();
                            s.connected = false;
                            if !user_stopped.load(Ordering::Relaxed) && (s.status.is_empty() || !s.status.starts_with("Could not start")) {
                                s.status = "Waiting for the node to start...".into();
                            }
                            continue;
                        }
                    }
                }
                if let Some(c) = client.as_ref()
                    && let Err(e) = tokio::time::timeout(RPC_TIMEOUT, refresh(c, &config, &state, &watch))
                        .await
                        .unwrap_or_else(|_| Err("the node did not answer in time".to_string()))
                {
                    let mut s = state.lock().unwrap();
                    s.connected = false;
                    if !user_stopped.load(Ordering::Relaxed) {
                        s.status = format!("Lost connection to the node: {e}");
                    }
                    drop(s);
                    // Close it properly, so the old connection does not keep trying to reconnect in the background.
                    if let Some(old) = client.take() {
                        let _ = tokio::time::timeout(Duration::from_secs(2), old.disconnect()).await;
                    }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match client.as_ref() {
                    Some(c) => handle_command(c, cmd, &state).await,
                    None => {
                        let mut s = state.lock().unwrap();
                        match cmd {
                            Cmd::Console(line) => s.console.push(format!("> {line}\nError: not connected to the node")),
                            Cmd::Submit(_) => s.send_result = Some(Err("Not connected to the node".into())),
                        }
                    }
                }
            }
        }
    }
}

async fn connect(rpc_address: &str, ctx: &SubscriptionContext) -> Option<GrpcClient> {
    GrpcClient::connect_with_args(
        NotificationMode::Direct,
        format!("grpc://{rpc_address}"),
        Some(ctx.clone()),
        true,
        None,
        false,
        Some(500_000),
        Default::default(),
    )
    .await
    .ok()
}

async fn refresh(
    client: &GrpcClient,
    config: &NodeConfig,
    state: &Arc<Mutex<NodeState>>,
    watch: &Arc<Mutex<Vec<Address>>>,
) -> Result<(), String> {
    let info = client.get_info().await.map_err(|e| e.to_string())?;
    let dag = client.get_block_dag_info().await.map_err(|e| e.to_string())?;
    let peers = client.get_connected_peer_info().await.map_err(|e| e.to_string())?;

    let addresses = watch.lock().unwrap().clone();
    let utxos = if addresses.is_empty() || !info.is_utxo_indexed {
        Vec::new()
    } else {
        let entries = client.get_utxos_by_addresses(addresses).await.map_err(|e| e.to_string())?;
        entries
            .into_iter()
            .map(|entry| WalletUtxo {
                outpoint: TransactionOutpoint::from(entry.outpoint),
                address: entry.address.map(|a| a.to_string()).unwrap_or_default(),
                amount: entry.utxo_entry.amount,
                daa_score: entry.utxo_entry.block_daa_score,
                is_coinbase: entry.utxo_entry.is_coinbase,
                entry: UtxoEntry::from(entry.utxo_entry),
            })
            .collect()
    };

    let peer_rows = peers
        .peer_info
        .into_iter()
        .map(|p| PeerRow {
            id: p.id.to_string(),
            address: p.address.to_string(),
            outbound: p.is_outbound,
            ping_ms: p.last_ping_duration,
            user_agent: p.user_agent,
            protocol_version: p.advertised_protocol_version,
            connected_secs: p.time_connected / 1000,
            ibd_peer: p.is_ibd_peer,
        })
        .collect();

    let mut s = state.lock().unwrap();
    s.connected = true;
    s.status = "Connected".into();
    s.synced = info.is_synced;
    s.utxo_indexed = info.is_utxo_indexed;
    s.server_version = info.server_version;
    s.network = dag.network.to_string();
    s.block_count = dag.block_count;
    s.header_count = dag.header_count;
    s.difficulty = dag.difficulty;
    s.virtual_daa_score = dag.virtual_daa_score;
    s.past_median_time = dag.past_median_time;
    s.mempool_size = info.mempool_size;
    s.peers = peer_rows;
    s.utxos = utxos;
    let _ = config;
    Ok(())
}

async fn handle_command(client: &GrpcClient, cmd: Cmd, state: &Arc<Mutex<NodeState>>) {
    match cmd {
        Cmd::Console(line) => {
            let output = run_console_command(client, line.trim()).await;
            state.lock().unwrap().console.push(format!("> {line}\n{output}"));
        }
        Cmd::Submit(tx) => {
            let rpc_tx = RpcTransaction::from(&*tx);
            let result = client.submit_transaction(rpc_tx, false).await.map(|id| id.to_string()).map_err(|e| e.to_string());
            state.lock().unwrap().send_result = Some(result);
        }
    }
}

const CONSOLE_HELP: &str = "Available commands:\n  help\n  getinfo\n  getblockchaininfo\n  getblockcount\n  getdifficulty\n  getconnectioncount\n  getpeerinfo\n  getmempoolinfo\n  getnetworkinfo\n  stop (closes the application's node)";

async fn run_console_command(client: &GrpcClient, line: &str) -> String {
    match line {
        "" => String::new(),
        "help" => CONSOLE_HELP.to_string(),
        "getinfo" => match client.get_info().await {
            Ok(r) => format!("{r:#?}"),
            Err(e) => format!("Error: {e}"),
        },
        "getblockchaininfo" | "getblockdaginfo" => match client.get_block_dag_info().await {
            Ok(r) => format!("{r:#?}"),
            Err(e) => format!("Error: {e}"),
        },
        "getblockcount" => match client.get_block_dag_info().await {
            Ok(r) => r.block_count.to_string(),
            Err(e) => format!("Error: {e}"),
        },
        "getdifficulty" => match client.get_block_dag_info().await {
            Ok(r) => r.difficulty.to_string(),
            Err(e) => format!("Error: {e}"),
        },
        "getconnectioncount" => match client.get_connected_peer_info().await {
            Ok(r) => r.peer_info.len().to_string(),
            Err(e) => format!("Error: {e}"),
        },
        "getpeerinfo" => match client.get_connected_peer_info().await {
            Ok(r) => format!("{:#?}", r.peer_info),
            Err(e) => format!("Error: {e}"),
        },
        "getmempoolinfo" => match client.get_info().await {
            Ok(r) => format!("size: {}", r.mempool_size),
            Err(e) => format!("Error: {e}"),
        },
        "getnetworkinfo" => match client.get_server_info().await {
            Ok(r) => format!("{r:#?}"),
            Err(e) => format!("Error: {e}"),
        },
        other => format!("Command not found: {other}\nType \"help\" for the list of commands."),
    }
}

/// Default network parameters, for confirmation depth and block rate.
pub fn params_for(network: NetworkType, testnet_suffix: u32) -> Params {
    match network {
        NetworkType::Testnet => Params::from(NetworkId::with_suffix(NetworkType::Testnet, testnet_suffix)),
        other => Params::from(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_addresses_outside_the_local_network_count_as_public() {
        for outside in ["1.1.1.1:16711", "8.8.8.8:1", "[2a00:1450::1]:16711"] {
            assert!(is_outside_address(outside), "{outside} should be outside");
        }
        for local in [
            "127.0.0.1:16711",
            "192.168.1.160:16711",
            "10.0.0.5:16711",
            "172.16.4.1:16711",
            "169.254.1.1:16711",
            "100.64.0.1:16711", // carrier-grade NAT
            "[::1]:16711",
            "[fe80::1]:16711",
            "[fd00::1]:16711",
            "[::ffff:192.168.1.5]:16711",
            "not an address",
        ] {
            assert!(!is_outside_address(local), "{local} should not count as outside");
        }
    }

    #[test]
    fn test_port_forwarding_is_read_from_the_node_log() {
        let attempting = "2026-11-10 18:00:00.000+00:00 [INFO ] [UPnP] Attempting to register upnp... (to disable run the node with --disable-upnp)";
        assert_eq!(parse_port_forwarding(""), PortForwarding::Trying);
        assert_eq!(parse_port_forwarding(attempting), PortForwarding::Trying);

        let working = format!("{attempting}\n2026-11-10 18:00:01.000+00:00 [INFO ] [UPnP] Added port mapping to default external port: 203.0.113.7:16711\n");
        assert_eq!(parse_port_forwarding(&working), PortForwarding::Working("203.0.113.7:16711".into()));

        let random = format!("{attempting}\n2026-11-10 18:00:01.000+00:00 [INFO ] [UPnP] Added port mapping to random external port: 203.0.113.7:50123\n");
        assert_eq!(parse_port_forwarding(&random), PortForwarding::Working("203.0.113.7:50123".into()));

        let failed = format!("{attempting}\n2026-11-10 18:00:01.000+00:00 [WARN ] [UPnP] Error adding port mapping: no gateway found\n");
        assert!(matches!(parse_port_forwarding(&failed), PortForwarding::Unavailable(_)));
        assert_eq!(short_reason("IO error: A connection attempt failed because the connected party did not properly respond"), "your router did not answer; automatic setup may be switched off on it");
        assert!(short_reason(&"x".repeat(500)).len() <= 70);

        let cgnat = format!("{attempting}\n2026-11-10 18:00:01.000+00:00 [INFO ] [UPnP] Non-publicly routable external ip from gateway using upnp 100.64.1.2 not added to store\n");
        assert!(matches!(parse_port_forwarding(&cgnat), PortForwarding::Unavailable(_)));

        // A node with its own public (IPv6) address skips the router and says so.
        let ipv6 = "2026-11-10 18:00:00.000+00:00 [INFO ] Application directory: x\n2026-11-10 18:00:00.500+00:00 [INFO ] Publicly routable local address found: 2a0a::1\n";
        assert_eq!(parse_port_forwarding(ipv6), PortForwarding::PublicAddress("2a0a::1".into()));
        // ...even if the router mapping then failed, because the node still advertises that address.
        let ipv6_failed = format!("{ipv6}2026-11-10 18:00:01.000+00:00 [WARN ] [UPnP] Error adding port mapping: no gateway found\n");
        assert_eq!(parse_port_forwarding(&ipv6_failed), PortForwarding::PublicAddress("2a0a::1".into()));
        // Only the current run counts: an old failure before the last start is ignored.
        let two_runs = format!("{failed}\n2026-11-10 19:00:00.000+00:00 [INFO ] Application directory: x\n");
        assert_eq!(parse_port_forwarding(&two_runs), PortForwarding::Trying);

        // The newest result wins, e.g. after a restart on a different router.
        let restarted = format!("{failed}\n{attempting}\n");
        assert_eq!(parse_port_forwarding(&restarted), PortForwarding::Trying);
    }

    #[test]
    fn test_port_forwarding_off_is_reported_without_reading_any_log() {
        let config = NodeConfig {
            network: NetworkType::Simnet,
            testnet_suffix: 13,
            spawn: true,
            aerod_path: PathBuf::new(),
            rpc_address: String::new(),
            data_dir: PathBuf::from("does-not-exist"),
            p2p_port: None,
            add_peer: None,
            upnp: false,
        };
        assert_eq!(port_forwarding_status(&config), PortForwarding::Disabled);
        assert_eq!(port_forwarding_status(&NodeConfig { upnp: true, ..config }), PortForwarding::Trying);
    }

    #[test]
    fn test_a_node_that_exits_right_away_is_explained() {
        let mainnet = "2026-11-10 18:49:07.893+00:00 [ERROR] thread 'main' panicked at aerod\\src\\daemon.rs:287:9: Experimental. Don't run on mainnet\nExiting...\n";
        assert!(explain_exit(mainnet).unwrap().contains("mainnet is not enabled"));
        assert!(explain_exit("[ERROR] bind failed: Address already in use (os error 98)").unwrap().contains("network port"));
        // Anything else: show the last line it printed.
        assert_eq!(explain_exit("starting\nsomething broke\nnote: run with RUST_BACKTRACE\n").unwrap(), "something broke");
        assert_eq!(explain_exit(""), None);
    }

    /// Starts a real `aerod`, stops it, and starts it again, as the Start node / Stop node buttons do.
    /// Run with `AEROD_PATH=<path to aerod> cargo test -p aero-core-gui -- --ignored node_can_be_stopped_and_started_again`.
    #[test]
    #[ignore = "starts a real aerod (set AEROD_PATH)"]
    fn node_can_be_stopped_and_started_again() {
        let Some(aerod_path) = std::env::var_os("AEROD_PATH").map(PathBuf::from) else {
            panic!("set AEROD_PATH to the aerod executable");
        };
        let data_dir = std::env::temp_dir().join(format!("aero-node-restart-{}", std::process::id()));
        let config = NodeConfig {
            network: NetworkType::Simnet,
            testnet_suffix: 13,
            spawn: true,
            aerod_path,
            rpc_address: "127.0.0.1:16510".into(),
            data_dir: data_dir.clone(),
            p2p_port: None,
            add_peer: None,
            // Skip the router search, which is slow and irrelevant here.
            upnp: false,
        };
        let wait_connected = |node: &mut Node, want: bool| {
            for _ in 0..120 {
                node.poll_process();
                if node.snapshot().connected == want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            false
        };

        let mut node = Node::start(config);
        assert!(wait_connected(&mut node, true), "first start: {}", node.snapshot().status);
        node.stop_process();
        assert!(!node.poll_process());
        assert!(wait_connected(&mut node, false));
        assert!(node.snapshot().status.contains("stopped"), "status after Stop: {}", node.snapshot().status);

        node.start_process(None, false);
        assert!(node.poll_process(), "the process should be running after Start node");
        assert!(wait_connected(&mut node, true), "second start: {}", node.snapshot().status);

        node.stop_process();
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

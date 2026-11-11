//! Aero Core: the desktop node and wallet application.
//!
//! The window layout follows Bitcoin Core's Qt GUI: a menu bar (File / Settings / Window / Help), a tab toolbar
//! (Overview / Send / Receive / Transactions / Mine), a status bar with sync progress and the connection count, and the
//! Console, Peers and Information windows. The app starts `aerod` as a child process (or attaches to a running one)
//! and talks to it over gRPC; see `node.rs` and `wallet.rs`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod node;
mod wallet;

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use aero_addresses::{Address, Prefix};
use aero_consensus_core::{network::{NetworkId, NetworkType}, tx::TransactionOutpoint};
use aero_miner::{Miner, MinerConfig, MinerPhase, MinerStats};
use eframe::egui::{self, Color32, RichText};
use node::{CONFIRMATIONS, Cmd, Node, NodeConfig, NodeState, WalletUtxo};
use wallet::{SentTx, Wallet, data_dir, format_aero, parse_aero, required_fee, unix_now};

const APP_NAME: &str = "Aero Core";
/// The Aero logo (a circle with transparent corners), used as the window icon and in the About dialog.
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
/// The teal of the Aero logo, used for selections and highlights.
const ACCENT: Color32 = Color32::from_rgb(0, 150, 140);
const UNIT: &str = "ARO";
/// How long an output we just spent stays excluded from new payments, while the node catches up.
const SPENT_HOLD: Duration = Duration::from_secs(120);

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Send,
    Receive,
    Transactions,
    Mine,
    Public,
}

struct PendingSend {
    to: Address,
    amount: u64,
    fee: u64,
    tx: aero_consensus_core::tx::Transaction,
    /// Outputs consumed by this payment.
    spent: Vec<TransactionOutpoint>,
}

struct TxRow {
    time: u64,
    kind: &'static str,
    label: String,
    amount: i128,
    confirmations: u64,
}

struct App {
    node: Node,
    wallet: Result<Wallet, String>,
    network: NetworkType,
    prefix: Prefix,
    coinbase_maturity: u64,
    bps: u64,
    data_dir: PathBuf,

    tab: Tab,
    show_console: bool,
    show_peers: bool,
    show_info: bool,
    show_receiving_addresses: bool,
    notice: Option<(String, String)>,

    console_input: String,

    pay_to: String,
    pay_label: String,
    pay_amount: String,
    send_error: Option<String>,
    confirm: Option<PendingSend>,
    in_flight: Option<PendingSend>,
    recently_spent: Vec<(TransactionOutpoint, Instant)>,

    receive_label: String,
    tx_search: String,

    logo: Option<egui::TextureHandle>,
    miner: Option<Miner>,
    mine_address: usize,
    mine_threads: usize,
    mine_fast: bool,
    peer_input: String,
    /// Ask the router to open the P2P port automatically (UPnP) the next time the node starts.
    upnp: bool,
    port_forwarding: (Instant, node::PortForwarding),

    /// First run: the recovery phrase must be shown and confirmed before the wallet is used.
    backup_prompt: bool,
    backup_confirmed: bool,
    show_recovery_phrase: bool,
    show_restore: bool,
    restore_input: String,
    restore_error: Option<String>,
    restore_confirmed: bool,
}

impl App {
    fn new(config: NodeConfig, data_dir: PathBuf) -> Self {
        let network = config.network;
        let prefix = Prefix::from(network);
        let params = node::params_for(network, config.testnet_suffix);
        let wallet = Wallet::load_or_create(&data_dir, prefix);
        let peer_input = config.add_peer.clone().unwrap_or_default();
        let node = Node::start(config);
        if let Ok(w) = &wallet {
            node.set_watch(w.addresses().into_iter().map(|(_, a)| a).collect());
        }
        let backup_prompt = wallet.as_ref().is_ok_and(|w| w.recovery_phrase().is_some() && !w.seed_backed_up());
        Self {
            node,
            wallet,
            network,
            prefix,
            coinbase_maturity: params.coinbase_maturity(),
            bps: params.bps().max(1),
            data_dir,
            tab: Tab::Overview,
            show_console: false,
            show_peers: false,
            show_info: false,
            show_receiving_addresses: false,
            notice: None,
            console_input: String::new(),
            pay_to: String::new(),
            pay_label: String::new(),
            pay_amount: String::new(),
            send_error: None,
            confirm: None,
            in_flight: None,
            recently_spent: Vec::new(),
            receive_label: String::new(),
            tx_search: String::new(),
            logo: None,
            miner: None,
            mine_address: 0,
            mine_threads: default_mine_threads(),
            mine_fast: true,
            peer_input,
            upnp: true,
            port_forwarding: (Instant::now(), node::PortForwarding::Trying),
            backup_prompt,
            backup_confirmed: false,
            show_recovery_phrase: false,
            show_restore: false,
            restore_input: String::new(),
            restore_error: None,
            restore_confirmed: false,
        }
    }

    fn is_spendable(&self, u: &WalletUtxo, virtual_daa: u64) -> bool {
        is_spendable_utxo(u, virtual_daa, self.coinbase_maturity)
    }

    /// (available, pending, immature) in sompi.
    fn balances(&self, s: &NodeState) -> (u64, u64, u64) {
        let (mut available, mut pending, mut immature) = (0u64, 0u64, 0u64);
        for u in &s.utxos {
            if self.is_spendable(u, s.virtual_daa_score) {
                available += u.amount;
            } else if u.is_coinbase {
                immature += u.amount;
            } else {
                pending += u.amount;
            }
        }
        (available, pending, immature)
    }

    fn spendable_utxos(&self, s: &NodeState) -> Vec<WalletUtxo> {
        let held: HashSet<TransactionOutpoint> =
            self.recently_spent.iter().filter(|(_, at)| at.elapsed() < SPENT_HOLD).map(|(op, _)| *op).collect();
        s.utxos.iter().filter(|u| self.is_spendable(u, s.virtual_daa_score) && !held.contains(&u.outpoint)).cloned().collect()
    }

    /// Wallet history reconstructed from the node's outputs plus what this wallet has sent.
    fn tx_rows(&self, s: &NodeState) -> Vec<TxRow> {
        let now = unix_now();
        let mut rows: Vec<TxRow> = s
            .utxos
            .iter()
            .map(|u| {
                let confirmations = s.virtual_daa_score.saturating_sub(u.daa_score);
                TxRow {
                    time: now.saturating_sub(confirmations / self.bps),
                    kind: if u.is_coinbase { "Mined" } else { "Received with" },
                    label: u.address.clone(),
                    amount: u.amount as i128,
                    confirmations,
                }
            })
            .collect();
        if let Ok(w) = &self.wallet {
            rows.extend(w.sent().iter().map(|t| TxRow {
                time: t.time,
                kind: "Sent to",
                label: t.to.clone(),
                amount: -((t.amount + t.fee) as i128),
                confirmations: 0,
            }));
        }
        rows.sort_by_key(|r| std::cmp::Reverse(r.time));
        rows
    }

    fn rebuild_watch(&self) {
        if let Ok(w) = &self.wallet {
            self.node.set_watch(w.addresses().into_iter().map(|(_, a)| a).collect());
        }
    }

    // ---------------------------------------------------------------- menu & toolbar & status bar

    fn menu_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    ui.add_enabled(false, egui::Button::new("Create Wallet..."));
                    ui.add_enabled(false, egui::Button::new("Open Wallet"));
                    ui.add_enabled(false, egui::Button::new("Close Wallet..."));
                    ui.separator();
                    if ui.button("Show Recovery Phrase...").clicked() {
                        self.show_recovery_phrase = true;
                        ui.close_menu();
                    }
                    if ui.button("Restore from Recovery Phrase...").clicked() {
                        self.show_restore = true;
                        self.restore_error = None;
                        ui.close_menu();
                    }
                    if ui.button("Backup Wallet...").clicked() {
                        let path = self.wallet.as_ref().map(|w| w.path().display().to_string()).unwrap_or_default();
                        self.notice = Some((
                            "Backup Wallet".into(),
                            format!("To back up your wallet, copy this file to a safe place:\n\n{path}\n\nAnyone with this file can spend your coins."),
                        ));
                        ui.close_menu();
                    }
                    if ui.button("Sign message...").clicked() {
                        self.notice = Some(("Sign message".into(), "Message signing is not available yet.".into()));
                        ui.close_menu();
                    }
                    if ui.button("Verify message...").clicked() {
                        self.notice = Some(("Verify message".into(), "Message verification is not available yet.".into()));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Receiving addresses...").clicked() {
                        self.show_receiving_addresses = true;
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Settings", |ui| {
                    if ui.button("Encrypt Wallet...").clicked() {
                        self.notice = Some((
                            "Encrypt Wallet".into(),
                            "Wallet encryption is not implemented yet. Your wallet file is stored unencrypted, so do not keep significant funds in it.".into(),
                        ));
                        ui.close_menu();
                    }
                    ui.add_enabled(false, egui::Button::new("Change Passphrase..."));
                    ui.separator();
                    if ui.button("Options...").clicked() {
                        self.notice = Some((
                            "Options".into(),
                            format!(
                                "Network: {:?}\nData directory: {}\nNode binary: {}\nRPC address: {}\n\nRestart with --testnet, --devnet or --simnet to change networks.",
                                self.network,
                                self.data_dir.display(),
                                self.node.config.aerod_path.display(),
                                self.node.config.rpc_address
                            ),
                        ));
                        ui.close_menu();
                    }
                });
                ui.menu_button("Window", |ui| {
                    if ui.button("Minimize").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Console").clicked() {
                        self.show_console = true;
                        ui.close_menu();
                    }
                    if ui.button("Peers").clicked() {
                        self.show_peers = true;
                        ui.close_menu();
                    }
                    if ui.button("Information").clicked() {
                        self.show_info = true;
                        ui.close_menu();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Command-line options").clicked() {
                        self.notice = Some((
                            "Command-line options".into(),
                            "--testnet | --devnet | --simnet   choose the network (default: testnet 13; mainnet is not enabled yet)\n--netsuffix <n>   testnet number (default: 13)\n--no-spawn   attach to a node that is already running instead of starting one\n--aerod <path>   path to the aerod executable\n--rpc <host:port>   gRPC address of the node".into(),
                        ));
                        ui.close_menu();
                    }
                    if ui.button(format!("About {APP_NAME}")).clicked() {
                        self.notice = Some((
                            format!("About {APP_NAME}"),
                            format!("{APP_NAME} is the Aero node and wallet.\n\nRandomX proof of work, 250 ms blocks, 100,000,000 {UNIT} hard cap."),
                        ));
                        ui.close_menu();
                    }
                });
            });
        });
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                for (tab, text) in
                    [
                    (Tab::Overview, "Overview"),
                    (Tab::Send, "Send"),
                    (Tab::Receive, "Receive"),
                    (Tab::Transactions, "Transactions"),
                    (Tab::Mine, "Mine"),
                    (Tab::Public, "Go public"),
                ]
                {
                    let selected = self.tab == tab;
                    let label = RichText::new(text).size(15.0);
                    let button = egui::SelectableLabel::new(selected, label);
                    if ui.add_sized([110.0, 30.0], button).clicked() {
                        self.tab = tab;
                    }
                }
            });
            ui.add_space(2.0);
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context, s: &NodeState) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if !s.connected {
                    ui.label(&s.status);
                } else if !s.synced {
                    ui.label("Synchronizing with network...");
                    let fraction =
                        if s.header_count > 0 { (s.block_count as f32 / s.header_count as f32).clamp(0.0, 1.0) } else { 0.0 };
                    ui.add(egui::ProgressBar::new(fraction).desired_width(220.0).show_percentage());
                } else {
                    ui.label(format!("Last received block was generated {} ago.", ago(s.past_median_time / 1000)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(UNIT);
                    ui.separator();
                    let n = s.peers.len();
                    ui.label(format!("{n} active connection{} to the Aero network", if n == 1 { "" } else { "s" }));
                });
            });
        });
    }

    // ---------------------------------------------------------------- pages

    fn page_overview(&mut self, ui: &mut egui::Ui, s: &NodeState) {
        self.node_controls(ui, s);
        ui.add_space(8.0);
        if !s.synced {
            egui::Frame::none().fill(Color32::from_rgb(255, 248, 200)).stroke(egui::Stroke::new(1.0, Color32::from_rgb(200, 190, 120))).inner_margin(8.0).show(ui, |ui| {
                ui.label(RichText::new("Recent transactions may not yet be visible, and therefore your wallet's balance might be incorrect. This information will be correct once your wallet has finished synchronizing with the Aero network, as detailed below.").color(Color32::from_rgb(90, 70, 0)));
                ui.add_space(4.0);
                egui::Grid::new("sync_info").num_columns(2).show(ui, |ui| {
                    ui.label("Number of blocks left");
                    ui.label(if s.connected { s.header_count.saturating_sub(s.block_count).to_string() } else { "Unknown...".into() });
                    ui.end_row();
                    ui.label("Last block time");
                    ui.label(if s.past_median_time > 0 { fmt_datetime(s.past_median_time / 1000) } else { "Unknown".into() });
                    ui.end_row();
                });
            });
            ui.add_space(8.0);
        }

        let (available, pending, immature) = self.balances(s);
        let rows = self.tx_rows(s);
        ui.columns(2, |cols| {
            group_box(&mut cols[0], "Balances", |ui| {
                ui.label(RichText::new("Your current spendable balance").weak());
                ui.add_space(6.0);
                egui::Grid::new("balances").num_columns(2).spacing([24.0, 8.0]).show(ui, |ui| {
                    for (name, value) in [("Available:", available), ("Pending:", pending), ("Immature:", immature)] {
                        ui.label(name);
                        ui.label(RichText::new(format!("{} {UNIT}", format_aero(value))).monospace());
                        ui.end_row();
                    }
                    ui.separator();
                    ui.separator();
                    ui.end_row();
                    ui.label(RichText::new("Total:").strong());
                    ui.label(RichText::new(format!("{} {UNIT}", format_aero(available + pending + immature))).monospace().strong());
                    ui.end_row();
                });
            });
            group_box(&mut cols[1], "Recent transactions", |ui| {
                if rows.is_empty() {
                    ui.label(RichText::new("No recent transactions").weak());
                }
                for row in rows.iter().take(6) {
                    ui.horizontal(|ui| {
                        ui.label(fmt_datetime(row.time));
                        ui.label(row.kind);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(format_signed(row.amount)).monospace().color(amount_color(row.amount)));
                        });
                    });
                }
            });
        });
    }

    fn page_send(&mut self, ui: &mut egui::Ui, s: &NodeState) {
        let spendable = self.spendable_utxos(s);
        let available: u64 = spendable.iter().map(|u| u.amount).sum();

        group_box(ui, "Send Coins", |ui| {
            egui::Grid::new("send_form").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                ui.label("Pay To:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.pay_to)
                        .hint_text(format!("Enter an Aero address (e.g. {})", example_address(self.prefix)))
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("Label:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.pay_label)
                        .hint_text("Enter a label for this address to add it to your address book")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("Amount:");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.pay_amount).desired_width(180.0));
                    ui.label(UNIT);
                });
                ui.end_row();
                ui.label("Transaction Fee:");
                ui.label(
                    RichText::new(format!("{} {UNIT} (estimated, based on the transaction size)", format_aero(required_fee(1, 2))))
                        .weak(),
                );
                ui.end_row();
            });
        });

        if let Some(err) = &self.send_error {
            ui.add_space(6.0);
            ui.colored_label(Color32::from_rgb(180, 30, 30), err);
        }
        if self.in_flight.is_some() {
            ui.add_space(6.0);
            ui.label("Sending...");
        }

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let can_send = self.in_flight.is_none() && s.connected;
            if ui.add_enabled(can_send, egui::Button::new(RichText::new("Send").strong())).clicked() {
                self.send_error = None;
                match self.prepare_send(&spendable) {
                    Ok(pending) => self.confirm = Some(pending),
                    Err(e) => self.send_error = Some(e),
                }
            }
            if ui.button("Clear All").clicked() {
                self.pay_to.clear();
                self.pay_label.clear();
                self.pay_amount.clear();
                self.send_error = None;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} {UNIT}", format_aero(available))).monospace());
                ui.label("Balance:");
            });
        });
    }

    fn prepare_send(&self, spendable: &[WalletUtxo]) -> Result<PendingSend, String> {
        let to =
            Address::try_from(self.pay_to.trim()).map_err(|_| "The recipient address is not valid, please recheck.".to_string())?;
        if to.prefix != self.prefix {
            return Err(format!("This address is for a different network. Expected an address starting with \"{}:\".", self.prefix));
        }
        let amount = parse_aero(&self.pay_amount)?;
        let wallet = self.wallet.as_ref().map_err(|e| e.clone())?;
        let built = wallet.build_payment(spendable, &to, amount)?;
        let spent = built.tx.inputs.iter().map(|i| i.previous_outpoint).collect();
        Ok(PendingSend { to, amount, fee: built.fee, tx: built.tx, spent })
    }

    /// Start / stop the node, the peer to connect to, and how many peers are connected.
    fn node_controls(&mut self, ui: &mut egui::Ui, s: &NodeState) {
        let running = self.node.poll_process();
        // Reading the log is cheap, but there is no need to do it every frame.
        if self.port_forwarding.0.elapsed() > Duration::from_secs(3) {
            self.port_forwarding = (Instant::now(), node::port_forwarding_status(&self.node.config));
        }
        let (inbound, outbound) = (s.peers.iter().filter(|p| !p.outbound).count(), s.peers.iter().filter(|p| p.outbound).count());
        group_box(ui, "Node", |ui| {
            if !self.node.config.spawn {
                ui.label(format!("Attached to a node that is already running at {}.", self.node.config.rpc_address));
            } else {
                ui.horizontal(|ui| {
                    if running {
                        if ui.button("Stop node").clicked() {
                            if let Some(miner) = &self.miner {
                                miner.stop();
                            }
                            self.node.stop_process();
                        }
                    } else if ui.button("Start node").clicked() {
                        self.node.start_process(Some(self.peer_input.trim().to_string()), self.upnp);
                    }
                    ui.add_enabled_ui(!running, |ui| {
                        ui.label("Connect to peer:");
                        ui.add(egui::TextEdit::singleline(&mut self.peer_input).hint_text("ip:port (optional)").desired_width(200.0));
                    });
                });
                ui.add_enabled(
                    !running,
                    egui::Checkbox::new(&mut self.upnp, "Try to become a public node (let other people connect in)"),
                );
                if self.upnp {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!(
                                "You only count as a public node once someone outside your network has connected in. To get there, make sure TCP                                  port {} is forwarded to this computer on your router. The app tries to do it automatically (UPnP), but many                                  routers have UPnP turned off. If the status below says it is not open, forward the port by hand (router                                  settings, Port Forwarding) and allow it through the Windows firewall. Leave this off to run a normal private                                  node: it works fully, others just cannot connect in to you.",
                                p2p_port(&self.node.config)
                            ))
                            .weak(),
                        )
                        .wrap(),
                    );
                }
            }
            ui.add_space(4.0);
            egui::Grid::new("node_status").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                ui.label("Status:");
                ui.label(if s.connected { "Connected".to_string() } else { s.status.clone() });
                ui.end_row();
                ui.label("Active peers:");
                ui.label(RichText::new(format!("{}  ({outbound} outbound, {inbound} inbound)", s.peers.len())).monospace());
                ui.end_row();
                ui.label("Port forwarding:");
                ui.add(egui::Label::new(port_forwarding_text(&self.port_forwarding.1, self.node.config.spawn, p2p_port(&self.node.config))).wrap());
                ui.end_row();
                ui.label("Node type:");
                let outside = node::outside_inbound_count(&s.peers);
                ui.label(if outside > 0 {
                    format!("PUBLIC NODE (verified): {outside} peer{} outside your network connected in", if outside == 1 { "" } else { "s" })
                } else if inbound > 0 {
                    format!("{inbound} incoming connection{}, but only from your own network: not verified as public", if inbound == 1 { "" } else { "s" })
                } else if self.upnp && running {
                    "private for now: nobody outside has connected in yet. Check the port forward".to_string()
                } else {
                    "private node: you connect out to others and nobody connects in. That works fine".to_string()
                });
                ui.end_row();
                ui.label("Sync:");
                ui.label(if !s.connected {
                    "-".to_string()
                } else if s.synced {
                    "Synced".to_string()
                } else {
                    format!("Syncing ({} of {} blocks)", s.block_count, s.header_count)
                });
                ui.end_row();
            });
        });
    }

    /// Explains, with the exact values for this computer, how to let other people connect in.
    fn page_public(&mut self, ui: &mut egui::Ui, s: &NodeState) {
        let port = p2p_port(&self.node.config);
        let local_ip = local_ip_address().unwrap_or_else(|| "(could not detect: run ipconfig and use your IPv4 address)".to_string());
        let outside = node::outside_inbound_count(&s.peers);
        let firewall = if cfg!(windows) {
            format!("New-NetFirewallRule -DisplayName \"Aero P2P\" -Direction Inbound -Protocol TCP -LocalPort {port} -Action Allow")
        } else {
            format!("sudo ufw allow {port}/tcp")
        };

        group_box(ui, "Your status", |ui| {
            if outside > 0 {
                ui.colored_label(
                    Color32::from_rgb(20, 130, 40),
                    format!("PUBLIC NODE (verified): {outside} peer{} outside your network connected in.", if outside == 1 { "" } else { "s" }),
                );
            } else {
                ui.label(
                    "Private node. It works fully; you connect out to others, but nobody connects in to you.                      Follow the steps below to become a public node. You are public once someone outside your network connects in.",
                );
            }
        });
        ui.add_space(8.0);

        group_box(ui, "1. Forward this port on your router", |ui| {
            egui::Grid::new("forward_values").num_columns(3).spacing([16.0, 6.0]).show(ui, |ui| {
                for (label, value) in [
                    ("Protocol:", "TCP".to_string()),
                    ("External port:", port.to_string()),
                    ("Internal port:", port.to_string()),
                    ("Internal IP address (this computer):", local_ip.clone()),
                ] {
                    ui.label(label);
                    ui.label(RichText::new(&value).monospace().strong());
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(value);
                    }
                    ui.end_row();
                }
            });
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    "Open your router's page (often http://192.168.1.1 or http://192.168.0.1; the address is on a sticker on the router) and look                      for Port Forwarding, NAT or Virtual Server. Add a rule with the values above. Also reserve this computer's address in the                      router (DHCP reservation) so it does not change, or the rule will point at the wrong machine later.",
                )
                .wrap(),
            );
        });
        ui.add_space(8.0);

        group_box(ui, "2. Allow it through this computer's firewall", |ui| {
            ui.label(if cfg!(windows) { "Run this in PowerShell as administrator:" } else { "Run this in a terminal:" });
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(&firewall).monospace()).wrap());
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(firewall.clone());
                }
            });
        });
        ui.add_space(8.0);

        group_box(ui, "3. Make sure your internet provider allows it", |ui| {
            ui.add(
                egui::Label::new(
                    "Some providers share one public address between many customers (CGNAT), and then no port forward can work. To check,                      compare the Internet/WAN IP shown on your router's status page with the address a \"what is my IP\" website shows. If                      they differ, or the router's one starts with 100., you are behind CGNAT: run the node on a cloud server instead.",
                )
                .wrap(),
            );
        });
        ui.add_space(8.0);

        group_box(ui, "4. Start the node as public", |ui| {
            ui.add(
                egui::Label::new(
                    "On the Overview tab tick \"Try to become a public node\" and press Start node (restart it if it was already running).                      This tab and the Overview tab turn to PUBLIC NODE (verified) once someone outside your network connects in. That can take                      a few minutes, because other nodes have to learn your address first.",
                )
                .wrap(),
            );
        });
        ui.add_space(8.0);

        group_box(ui, "Privacy: who can see your IP address", |ui| {
            ui.add(
                egui::Label::new(
                    "Every node you connect to sees your IP address, public or not. A public node also shares its address with other nodes so                      they can find it, which means anyone running a node can see and record your home IP address. If you do not want that,                      stay a private node, or run the public node on a cloud server instead of at home.",
                )
                .wrap(),
            );
        });
    }

    fn page_mine(&mut self, ui: &mut egui::Ui, s: &NodeState) {
        let addresses = self.wallet.as_ref().map(|w| w.addresses()).unwrap_or_default();
        let stats = self.miner.as_ref().map(Miner::stats).unwrap_or_default();
        let active = stats.phase != MinerPhase::Idle;
        let max_threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        self.mine_address = self.mine_address.min(addresses.len().saturating_sub(1));
        self.mine_threads = self.mine_threads.clamp(1, max_threads);

        self.node_controls(ui, s);
        ui.add_space(10.0);

        let can_mine = s.connected && (s.synced || node::mines_unsynced(self.network));
        let (mut start, mut stop) = (false, false);
        group_box(ui, "Mining", |ui| {
            ui.label("Mine new blocks with this computer's CPU (RandomX). Block rewards are paid to the address below.");
            ui.add_space(6.0);
            ui.add_enabled_ui(!active, |ui| {
                egui::Grid::new("mine_form").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                    ui.label("Reward address:");
                    let selected = addresses.get(self.mine_address).map_or_else(String::new, |(label, a)| format!("{label}   {a}"));
                    egui::ComboBox::from_id_salt("mine_address").width(520.0).selected_text(selected).show_ui(ui, |ui| {
                        for (i, (label, address)) in addresses.iter().enumerate() {
                            ui.selectable_value(&mut self.mine_address, i, format!("{label}   {address}"));
                        }
                    });
                    ui.end_row();

                    ui.label("Threads:");
                    ui.add(egui::Slider::new(&mut self.mine_threads, 1..=max_threads));
                    ui.end_row();

                    ui.label("Memory:");
                    ui.checkbox(&mut self.mine_fast, "Fast mode (about 9x faster, needs ~2.5 GiB of free RAM)");
                    ui.end_row();

                });
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if active {
                    if ui.add_enabled(stats.phase != MinerPhase::Stopping, egui::Button::new("Stop mining")).clicked() {
                        stop = true;
                    }
                } else if ui.add_enabled(can_mine && !addresses.is_empty(), egui::Button::new("Start mining")).clicked() {
                    start = true;
                }
                if !active && !can_mine {
                    ui.label(RichText::new(if s.connected { "Mining starts once the node has synced." } else { "Start the node first." }).weak());
                }
            });
        });
        ui.add_space(10.0);
        group_box(ui, "Status", |ui| mine_status(ui, &stats));

        if start {
            if let Some((_, address)) = addresses.get(self.mine_address) {
                let mut config = MinerConfig::new(self.node.config.rpc_address.clone(), address.clone());
                config.threads = self.mine_threads;
                config.light = !self.mine_fast;
                // Test networks may have no other miners, so their node is never "synced"; mainnet waits for sync.
                config.mine_unsynced = node::mines_unsynced(self.network);
                self.miner = Some(Miner::start(config));
            }
        }
        if stop {
            if let Some(miner) = &self.miner {
                miner.stop();
            }
        }
    }

    fn page_receive(&mut self, ui: &mut egui::Ui) {
        group_box(ui, "Receive coins", |ui| {
            ui.label("Use this form to request payments. All fields are optional.");
            ui.add_space(6.0);
            egui::Grid::new("receive_form").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                ui.label("Label:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.receive_label)
                        .hint_text("An optional label to associate with the new receiving address")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
            ui.add_space(6.0);
            if ui.button("Create new receiving address").clicked() {
                let label = if self.receive_label.trim().is_empty() {
                    "(no label)".to_string()
                } else {
                    self.receive_label.trim().to_string()
                };
                if let Ok(w) = &mut self.wallet {
                    match w.new_key(&label) {
                        Ok(_) => self.rebuild_watch(),
                        Err(e) => self.notice = Some(("Wallet error".into(), e)),
                    }
                }
                self.receive_label.clear();
            }
        });
        ui.add_space(10.0);
        group_box(ui, "Receiving addresses", |ui| {
            self.address_table(ui);
        });
    }

    fn address_table(&self, ui: &mut egui::Ui) {
        let Ok(wallet) = &self.wallet else {
            ui.colored_label(Color32::from_rgb(180, 30, 30), "The wallet could not be loaded.");
            return;
        };
        egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            egui::Grid::new("addresses").num_columns(3).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label(RichText::new("Label").strong());
                ui.label(RichText::new("Address").strong());
                ui.label("");
                ui.end_row();
                for (label, address) in wallet.addresses().iter().rev() {
                    let text = address.to_string();
                    ui.label(label);
                    ui.label(RichText::new(&text).monospace());
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(text);
                    }
                    ui.end_row();
                }
            });
        });
    }

    fn page_transactions(&mut self, ui: &mut egui::Ui, s: &NodeState) {
        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.add(egui::TextEdit::singleline(&mut self.tx_search).hint_text("Enter address or label to search").desired_width(320.0));
        });
        ui.add_space(6.0);
        let needle = self.tx_search.trim().to_lowercase();
        let rows: Vec<TxRow> =
            self.tx_rows(s).into_iter().filter(|r| needle.is_empty() || r.label.to_lowercase().contains(&needle)).collect();
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("transactions").num_columns(5).striped(true).spacing([18.0, 6.0]).min_col_width(60.0).show(ui, |ui| {
                for head in ["", "Date", "Type", "Label", "Amount"] {
                    ui.label(RichText::new(head).strong());
                }
                ui.end_row();
                for row in &rows {
                    ui.label(status_icon(row.confirmations));
                    ui.label(fmt_datetime(row.time));
                    ui.label(row.kind);
                    ui.label(RichText::new(&row.label).monospace());
                    ui.label(
                        RichText::new(format!("{} {UNIT}", format_signed(row.amount))).monospace().color(amount_color(row.amount)),
                    );
                    ui.end_row();
                }
            });
            if rows.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new("No transactions").weak());
            }
        });
    }

    // ---------------------------------------------------------------- windows

    fn windows(&mut self, ctx: &egui::Context, s: &NodeState) {
        // Peers
        let mut open = self.show_peers;
        egui::Window::new("Peers").open(&mut open).default_size([760.0, 320.0]).show(ctx, |ui| {
            egui::ScrollArea::both().show(ui, |ui| {
                egui::Grid::new("peers").num_columns(7).striped(true).spacing([14.0, 6.0]).show(ui, |ui| {
                    for head in ["Peer", "Address", "Direction", "Ping", "Version", "User Agent", "Connected"] {
                        ui.label(RichText::new(head).strong());
                    }
                    ui.end_row();
                    for p in &s.peers {
                        ui.label(RichText::new(short_id(&p.id)).monospace());
                        ui.label(&p.address);
                        ui.label(if p.outbound { "Outbound" } else { "Inbound" });
                        ui.label(format!("{} ms", p.ping_ms));
                        ui.label(p.protocol_version.to_string());
                        ui.label(&p.user_agent);
                        ui.label(format!("{}s{}", p.connected_secs, if p.ibd_peer { " (sync peer)" } else { "" }));
                        ui.end_row();
                    }
                });
                if s.peers.is_empty() {
                    ui.label(RichText::new("No peers connected").weak());
                }
            });
        });
        self.show_peers = open;

        // Information
        let mut open = self.show_info;
        egui::Window::new("Information").open(&mut open).default_size([460.0, 360.0]).show(ctx, |ui| {
            group_box(ui, "General", |ui| {
                egui::Grid::new("info_general").num_columns(2).spacing([20.0, 6.0]).show(ui, |ui| {
                    for (k, v) in [
                        ("Client version", if s.server_version.is_empty() { "-".to_string() } else { s.server_version.clone() }),
                        ("Network", if s.network.is_empty() { format!("{:?}", self.network) } else { s.network.clone() }),
                        ("Data directory", self.data_dir.display().to_string()),
                        ("Number of connections", s.peers.len().to_string()),
                    ] {
                        ui.label(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            });
            ui.add_space(8.0);
            group_box(ui, "Block chain", |ui| {
                egui::Grid::new("info_chain").num_columns(2).spacing([20.0, 6.0]).show(ui, |ui| {
                    for (k, v) in [
                        ("Number of blocks", s.block_count.to_string()),
                        ("Number of headers", s.header_count.to_string()),
                        ("Last block time", if s.past_median_time > 0 { fmt_datetime(s.past_median_time / 1000) } else { "-".into() }),
                        ("Current difficulty", format!("{:.4}", s.difficulty)),
                        ("DAA score", s.virtual_daa_score.to_string()),
                        ("Mempool transactions", s.mempool_size.to_string()),
                        ("Status", s.status.clone()),
                    ] {
                        ui.label(k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
            });
        });
        self.show_info = open;

        // Console
        let mut open = self.show_console;
        let mut input = std::mem::take(&mut self.console_input);
        let node = &self.node;
        egui::Window::new("Console").open(&mut open).default_size([640.0, 420.0]).show(ctx, |ui| {
            let height = (ui.available_height() - 34.0).max(80.0);
            egui::ScrollArea::vertical().max_height(height).stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                ui.label(
                    RichText::new("Welcome to the Aero Core RPC console.\nType help for an overview of available commands.\n")
                        .monospace()
                        .weak(),
                );
                for entry in &s.console {
                    ui.label(RichText::new(entry).monospace());
                }
            });
            let response = ui.add(
                egui::TextEdit::singleline(&mut input).font(egui::TextStyle::Monospace).hint_text("> ").desired_width(f32::INFINITY),
            );
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let line = input.trim().to_string();
                if !line.is_empty() {
                    node.send(Cmd::Console(line));
                    input.clear();
                }
                response.request_focus();
            }
        });
        self.console_input = input;
        self.show_console = open;

        // Receiving addresses (File menu)
        let mut open = self.show_receiving_addresses;
        egui::Window::new("Receiving addresses").open(&mut open).default_size([640.0, 320.0]).show(ctx, |ui| self.address_table(ui));
        self.show_receiving_addresses = open;

        self.recovery_dialogs(ctx);

        // Generic notice
        if let Some((title, body)) = self.notice.clone() {
            egui::Window::new(&title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
                ctx,
                |ui| {
                    ui.set_max_width(480.0);
                    if title.starts_with("About") {
                        if self.logo.is_none() {
                            self.logo = load_logo(ctx);
                        }
                        if let Some(logo) = &self.logo {
                            ui.vertical_centered(|ui| ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(96.0, 96.0))));
                            ui.add_space(6.0);
                        }
                    }
                    ui.label(body);
                    ui.add_space(8.0);
                    if ui.button("OK").clicked() {
                        self.notice = None;
                    }
                },
            );
        }

        self.send_dialogs(ctx, s);
    }

    /// The first-run backup prompt, the "show recovery phrase" window, and the restore window.
    fn recovery_dialogs(&mut self, ctx: &egui::Context) {
        let phrase = self.wallet.as_ref().ok().and_then(|w| w.recovery_phrase().map(str::to_string));

        // First run: the phrase cannot be skipped. Without it, losing this computer means losing the coins.
        if self.backup_prompt {
            if let Some(phrase) = &phrase {
                let mut done = false;
                egui::Window::new("Write down your recovery phrase")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.set_max_width(560.0);
                        ui.label(
                            "These words are the only way to recover your wallet if this computer is lost or broken. \
                             Write them down, in order, on paper and keep them somewhere safe and private.",
                        );
                        ui.add_space(8.0);
                        phrase_grid(ui, phrase);
                        ui.add_space(8.0);
                        ui.colored_label(
                            Color32::from_rgb(180, 30, 30),
                            "Anyone who has these words can take all of your coins. Never share them or type them into a website.",
                        );
                        ui.add_space(8.0);
                        ui.checkbox(&mut self.backup_confirmed, "I have written down my recovery phrase");
                        ui.add_space(4.0);
                        if ui.add_enabled(self.backup_confirmed, egui::Button::new("Continue")).clicked() {
                            done = true;
                        }
                    });
                if done {
                    if let Ok(w) = &mut self.wallet {
                        w.mark_seed_backed_up();
                    }
                    self.backup_prompt = false;
                    self.backup_confirmed = false;
                }
            } else {
                self.backup_prompt = false;
            }
        }

        if self.show_recovery_phrase {
            let mut open = true;
            egui::Window::new("Recovery phrase").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_max_width(560.0);
                match &phrase {
                    Some(phrase) => {
                        ui.colored_label(
                            Color32::from_rgb(180, 30, 30),
                            "Anyone who sees these words can take all of your coins. Make sure nobody is watching your screen.",
                        );
                        ui.add_space(8.0);
                        phrase_grid(ui, phrase);
                    }
                    None => {
                        ui.label(
                            "This wallet was created before recovery phrases existed, so it has none and cannot be recovered from words. \
                             To get one, restore or create a new wallet and send your coins to it.",
                        );
                    }
                }
            });
            self.show_recovery_phrase = open;
        }

        if self.show_restore {
            let mut open = true;
            let mut restore = false;
            egui::Window::new("Restore from recovery phrase").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.label("Type the 12 or 24 words of your recovery phrase, separated by spaces.");
                ui.add_space(6.0);
                ui.add(egui::TextEdit::multiline(&mut self.restore_input).desired_rows(3).desired_width(f32::INFINITY).hint_text("word1 word2 word3 ..."));
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "This replaces the current wallet on this network. Your current wallet file is kept as a backup next to it. \
                         The first 20 addresses are restored; coins on later addresses will appear once you create addresses up to them.",
                    )
                    .weak(),
                );
                ui.add_space(4.0);
                ui.checkbox(&mut self.restore_confirmed, "I understand this replaces the current wallet");
                if let Some(e) = &self.restore_error {
                    ui.add_space(4.0);
                    ui.colored_label(Color32::from_rgb(180, 30, 30), e);
                }
                ui.add_space(6.0);
                if ui.add_enabled(self.restore_confirmed && !self.restore_input.trim().is_empty(), egui::Button::new("Restore")).clicked() {
                    restore = true;
                }
            });
            if restore {
                match Wallet::restore(&self.data_dir, self.prefix, &self.restore_input) {
                    Ok(wallet) => {
                        self.wallet = Ok(wallet);
                        self.rebuild_watch();
                        self.mine_address = 0;
                        self.restore_input.clear();
                        self.restore_error = None;
                        self.restore_confirmed = false;
                        open = false;
                        self.notice = Some(("Wallet restored".into(), "Your wallet was restored from the recovery phrase. It will show your balance once the node has synced.".into()));
                    }
                    Err(e) => self.restore_error = Some(e),
                }
            }
            if !open {
                self.restore_input.clear();
                self.restore_error = None;
                self.restore_confirmed = false;
            }
            self.show_restore = open;
        }
    }

    fn send_dialogs(&mut self, ctx: &egui::Context, s: &NodeState) {
        // Confirmation
        if let Some(pending) = &self.confirm {
            let (to, amount, fee) = (pending.to.to_string(), pending.amount, pending.fee);
            let mut decision = None;
            egui::Window::new("Confirm send coins")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Are you sure you want to send?");
                    ui.add_space(6.0);
                    egui::Grid::new("confirm_grid").num_columns(2).spacing([20.0, 6.0]).show(ui, |ui| {
                        ui.label("Pay To:");
                        ui.label(RichText::new(&to).monospace());
                        ui.end_row();
                        ui.label("Amount:");
                        ui.label(RichText::new(format!("{} {UNIT}", format_aero(amount))).monospace());
                        ui.end_row();
                        ui.label("Transaction fee:");
                        ui.label(RichText::new(format!("{} {UNIT}", format_aero(fee))).monospace());
                        ui.end_row();
                        ui.label(RichText::new("Total Amount:").strong());
                        ui.label(RichText::new(format!("{} {UNIT}", format_aero(amount + fee))).monospace().strong());
                        ui.end_row();
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            decision = Some(false);
                        }
                        if ui.button(RichText::new("Send").strong()).clicked() {
                            decision = Some(true);
                        }
                    });
                });
            match decision {
                Some(true) => {
                    if let Some(pending) = self.confirm.take() {
                        self.node.clear_send_result();
                        self.node.send(Cmd::Submit(Box::new(pending.tx.clone())));
                        self.in_flight = Some(pending);
                    }
                }
                Some(false) => self.confirm = None,
                None => {}
            }
        }

        // Result of a submitted payment
        if self.in_flight.is_some()
            && let Some(result) = &s.send_result
        {
            let pending = self.in_flight.take().expect("checked above");
            match result {
                Ok(txid) => {
                    let now = Instant::now();
                    self.recently_spent.extend(pending.spent.iter().map(|op| (*op, now)));
                    if let Ok(w) = &mut self.wallet {
                        w.record_sent(SentTx {
                            txid: txid.clone(),
                            time: unix_now(),
                            amount: pending.amount,
                            fee: pending.fee,
                            to: pending.to.to_string(),
                        });
                    }
                    self.pay_to.clear();
                    self.pay_label.clear();
                    self.pay_amount.clear();
                    self.notice = Some((
                        "Transaction sent".into(),
                        format!("The transaction was submitted to the network.\n\nTransaction ID:\n{txid}"),
                    ));
                }
                Err(e) => self.notice = Some(("Send failed".into(), e.clone())),
            }
            self.node.clear_send_result();
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.node.poll_process();
        let state = self.node.snapshot();
        self.recently_spent.retain(|(_, at)| at.elapsed() < SPENT_HOLD);

        self.menu_bar(ctx);
        self.toolbar(ctx);
        self.status_bar(ctx, &state);
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Err(e) = &self.wallet {
                ui.colored_label(Color32::from_rgb(180, 30, 30), format!("Wallet error: {e}"));
                return;
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Overview => self.page_overview(ui, &state),
                Tab::Send => self.page_send(ui, &state),
                Tab::Receive => self.page_receive(ui),
                Tab::Transactions => self.page_transactions(ui, &state),
                Tab::Mine => self.page_mine(ui, &state),
                Tab::Public => self.page_public(ui, &state),
            });
        });
        self.windows(ctx, &state);

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

// -------------------------------------------------------------------------------------------------- helpers

/// An output is available once it has enough confirmations (DAA score); coinbase outputs need twice the maturity.
/// A network with no coinbase maturity (the test network) has no waiting at all.
fn is_spendable_utxo(u: &WalletUtxo, virtual_daa: u64, coinbase_maturity: u64) -> bool {
    if coinbase_maturity == 0 {
        return true;
    }
    let needed = if u.is_coinbase { coinbase_maturity * 2 } else { CONFIRMATIONS };
    u.daa_score + needed < virtual_daa
}

/// A GUI-subsystem executable has no console of its own; attach to the launching one so command-line output shows.
#[cfg(windows)]
fn attach_parent_console() {
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    // SAFETY: plain Win32 call with no pointers; failure (no parent console) is harmless.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_parent_console() {}

/// Headless check of the node connection and wallet, for scripts and CI: prints a summary and the wallet balances,
/// and optionally makes one payment. Returns the process exit code.
fn selftest(config: NodeConfig, send: Option<(String, String)>) -> i32 {
    let prefix = Prefix::from(config.network);
    let params = node::params_for(config.network, config.testnet_suffix);
    let mut wallet = match Wallet::load_or_create(&config.data_dir, prefix) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("wallet error: {e}");
            return 1;
        }
    };
    let node = Node::start(config);
    node.set_watch(wallet.addresses().into_iter().map(|(_, a)| a).collect());

    let deadline = Instant::now() + Duration::from_secs(90);
    let mut state = node.snapshot();
    while !state.connected {
        if Instant::now() > deadline {
            eprintln!("timed out waiting for the node: {}", state.status);
            return 1;
        }
        std::thread::sleep(Duration::from_millis(500));
        state = node.snapshot();
    }
    // One more refresh so the wallet outputs are loaded.
    std::thread::sleep(Duration::from_millis(2500));
    state = node.snapshot();

    let (mut available, mut pending, mut immature) = (0u64, 0u64, 0u64);
    let mut spendable = Vec::new();
    for u in &state.utxos {
        if is_spendable_utxo(u, state.virtual_daa_score, params.coinbase_maturity()) {
            available += u.amount;
            spendable.push(u.clone());
        } else if u.is_coinbase {
            immature += u.amount;
        } else {
            pending += u.amount;
        }
    }
    println!("connected: yes");
    println!("network: {}", state.network);
    println!("synced: {}", state.synced);
    println!("blocks: {}  headers: {}  daa score: {}", state.block_count, state.header_count, state.virtual_daa_score);
    println!("peers: {}", state.peers.len());
    println!("wallet outputs: {}", state.utxos.len());
    println!("available: {} {UNIT}", format_aero(available));
    println!("pending:   {} {UNIT}", format_aero(pending));
    println!("immature:  {} {UNIT}", format_aero(immature));

    let Some((to, amount)) = send else { return 0 };
    let Ok(to) = Address::try_from(to.as_str()) else {
        eprintln!("invalid recipient address");
        return 1;
    };
    let amount = match parse_aero(&amount) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let built = match wallet.build_payment(&spendable, &to, amount) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("could not build the payment: {e}");
            return 1;
        }
    };
    println!(
        "built payment: amount {} {UNIT}, fee {} {UNIT}, inputs {}",
        format_aero(amount),
        format_aero(built.fee),
        built.tx.inputs.len()
    );
    node.clear_send_result();
    node.send(Cmd::Submit(Box::new(built.tx)));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match node.snapshot().send_result {
            Some(Ok(txid)) => {
                println!("sent: {txid}");
                wallet.record_sent(SentTx { txid, time: unix_now(), amount, fee: built.fee, to: to.to_string() });
                return 0;
            }
            Some(Err(e)) => {
                eprintln!("send failed: {e}");
                return 1;
            }
            None if Instant::now() > deadline => {
                eprintln!("timed out waiting for the node to accept the transaction");
                return 1;
            }
            None => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Half the CPUs (at least one), so the computer stays usable while mining.
/// The port the node listens on for other nodes: the one given with `--p2p-port`, else the network's default.
fn p2p_port(config: &node::NodeConfig) -> u16 {
    config.p2p_port.unwrap_or_else(|| match config.network {
        NetworkType::Testnet => NetworkId::with_suffix(NetworkType::Testnet, config.testnet_suffix),
        other => NetworkId::new(other),
    }
    .default_p2p_port())
}

fn port_forwarding_text(status: &node::PortForwarding, spawned: bool, port: u16) -> String {
    use node::PortForwarding::*;
    if !spawned {
        return "managed by the node you attached to".into();
    }
    match status {
        Disabled => "off (you can still connect out, but others cannot connect to you)".into(),
        Trying => "trying...".into(),
        Working(address) => format!("on: others can connect to you at {address}"),
        PublicAddress(address) if address.contains(':') => format!(
            "your computer has a public IPv6 address ({address}). Only IPv6 peers can reach you; forward the port on your router so IPv4 peers can too"
        ),
        PublicAddress(address) => format!("your computer has a public address ({address})"),
        Unavailable(reason) => format!(
            "NOT OPEN: {reason}. Others cannot connect to you yet. Turn on UPnP in your router, or forward TCP port {port} to this computer by hand"
        ),
    }
}

fn default_mine_threads() -> usize {
    (std::thread::available_parallelism().map_or(2, |n| n.get()) / 2).max(1)
}

fn mine_status(ui: &mut egui::Ui, stats: &MinerStats) {
    let phase = match stats.phase {
        MinerPhase::Idle => "Not mining",
        MinerPhase::Initializing => "Preparing RandomX (building the dataset takes a while in fast mode)...",
        MinerPhase::Connecting => "Connecting to the node...",
        MinerPhase::WaitingForSync => "Waiting for the node to sync...",
        MinerPhase::Mining => "Mining",
        MinerPhase::Stopping => "Stopping...",
    };
    egui::Grid::new("mine_status").num_columns(2).spacing([24.0, 8.0]).show(ui, |ui| {
        ui.label("Status:");
        ui.label(RichText::new(phase).strong());
        ui.end_row();
        ui.label("Hash rate:");
        ui.label(RichText::new(format!("{:.1} H/s", stats.hashrate)).monospace());
        ui.end_row();
        ui.label("Hashes:");
        ui.label(RichText::new(stats.total_hashes.to_string()).monospace());
        ui.end_row();
        ui.label("Blocks found:");
        ui.label(RichText::new(format!("{} ({} accepted, {} rejected)", stats.blocks_found, stats.blocks_accepted, stats.blocks_rejected)).monospace());
        ui.end_row();
        ui.label("Running for:");
        let secs = stats.started_at.filter(|_| stats.phase != MinerPhase::Idle).map_or(0, |t| t.elapsed().as_secs());
        ui.label(RichText::new(format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)).monospace());
        ui.end_row();
    });
    if !stats.message.is_empty() {
        ui.add_space(6.0);
        ui.colored_label(Color32::from_rgb(180, 90, 0), &stats.message);
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Mined rewards show as Immature on the Overview tab until they have matured.").weak());
}

/// The recovery phrase as a numbered grid, four words per row.
fn phrase_grid(ui: &mut egui::Ui, phrase: &str) {
    egui::Frame::none().fill(Color32::WHITE).stroke(egui::Stroke::new(1.0, Color32::from_rgb(168, 168, 168))).inner_margin(10.0).show(ui, |ui| {
        egui::Grid::new("recovery_phrase_grid").num_columns(4).spacing([28.0, 8.0]).show(ui, |ui| {
            for (i, word) in phrase.split_whitespace().enumerate() {
                ui.label(RichText::new(format!("{:>2}. {word}", i + 1)).monospace().size(15.0));
                if (i + 1) % 4 == 0 {
                    ui.end_row();
                }
            }
        });
    });
}

/// This computer's address on the local network (what a router port forward must point at). Connecting a UDP socket
/// only picks a route; no packet is sent.
fn local_ip_address() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

fn group_box(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none().stroke(egui::Stroke::new(1.0, Color32::from_rgb(190, 190, 190))).rounding(3.0).inner_margin(10.0).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong());
            ui.add_space(6.0);
            add_contents(ui);
        },
    );
}

fn amount_color(amount: i128) -> Color32 {
    if amount < 0 { Color32::from_rgb(190, 40, 40) } else { Color32::from_rgb(20, 20, 20) }
}

fn format_signed(amount: i128) -> String {
    let sign = if amount < 0 { "-" } else { "" };
    format!("{sign}{}", format_aero(amount.unsigned_abs() as u64))
}

fn status_icon(confirmations: u64) -> &'static str {
    match confirmations {
        0 => "o",
        1..=9 => "-",
        _ => "+",
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn example_address(prefix: Prefix) -> String {
    format!("{prefix}:qq...")
}

/// "3 minutes", "2 hours"...
fn ago(unix_secs: u64) -> String {
    if unix_secs == 0 {
        return "an unknown time".into();
    }
    let secs = unix_now().saturating_sub(unix_secs);
    match secs {
        0..=59 => format!("{secs} second{}", if secs == 1 { "" } else { "s" }),
        60..=3599 => format!("{} minutes", secs / 60),
        3600..=86_399 => format!("{} hours", secs / 3600),
        _ => format!("{} days", secs / 86_400),
    }
}

/// `YYYY-MM-DD HH:MM` in UTC (civil-from-days algorithm; avoids a date-time dependency).
fn fmt_datetime(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// A look close to Qt's default "Fusion" light style, which is what Bitcoin Core uses on most platforms.
/// Decodes the embedded logo into a texture. `None` if it cannot be decoded, in which case the dialog just has no logo.
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).ok()?;
    let image = egui::ColorImage::from_rgba_unmultiplied([icon.width as usize, icon.height as usize], &icon.rgba);
    Some(ctx.load_texture("aero-logo", image, egui::TextureOptions::LINEAR))
}

fn apply_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::light();
    let v = &mut style.visuals;
    let window = Color32::from_rgb(239, 239, 239);
    v.panel_fill = window;
    v.window_fill = window;
    v.extreme_bg_color = Color32::WHITE;
    v.faint_bg_color = Color32::from_rgb(246, 246, 246);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
    let border = egui::Stroke::new(1.0, Color32::from_rgb(168, 168, 168));
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(228, 228, 228);
    v.widgets.inactive.bg_fill = Color32::from_rgb(228, 228, 228);
    v.widgets.inactive.bg_stroke = border;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(208, 236, 233);
    v.widgets.hovered.bg_fill = Color32::from_rgb(208, 236, 233);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(170, 220, 215);
    v.widgets.active.bg_fill = Color32::from_rgb(170, 220, 215);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.rounding = egui::Rounding::same(2.0);
    }
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 4.0);
    style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(13.5));
    style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(13.5));
    style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(13.0));
    ctx.set_style(style);
}

fn main() -> eframe::Result<()> {
    // Mainnet is not enabled in this version, so the default is the public testnet (13).
    let mut network = NetworkType::Testnet;
    let mut testnet_suffix = 13;
    let mut spawn = true;
    let mut aerod_path: Option<PathBuf> = None;
    let mut rpc_address: Option<String> = None;
    let (mut print_address, mut run_selftest) = (false, false);
    let (mut send_to, mut send_amount): (Option<String>, Option<String>) = (None, None);
    let (mut initial_tab, mut initial_window): (Option<String>, Option<String>) = (None, None);
    let mut datadir: Option<PathBuf> = None;
    let (mut p2p_port, mut add_peer): (Option<u16>, Option<String>) = (None, None);
    let mut window_pos: Option<(f32, f32)> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--testnet" => network = NetworkType::Testnet,
            "--mainnet" => network = NetworkType::Mainnet,
            "--netsuffix" => testnet_suffix = args.next().and_then(|v| v.parse().ok()).unwrap_or(testnet_suffix),
            "--devnet" => network = NetworkType::Devnet,
            "--simnet" => network = NetworkType::Simnet,
            "--no-spawn" => spawn = false,
            "--aerod" => aerod_path = args.next().map(PathBuf::from),
            "--rpc" => rpc_address = args.next(),
            "--print-address" => print_address = true,
            "--selftest" => run_selftest = true,
            "--send-to" => send_to = args.next(),
            "--amount" => send_amount = args.next(),
            "--page" => initial_tab = args.next(),
            "--datadir" => datadir = args.next().map(PathBuf::from),
            "--p2p-port" => p2p_port = args.next().and_then(|p| p.parse().ok()),
            "--addpeer" => add_peer = args.next(),
            "--window-pos" => {
                window_pos = args
                    .next()
                    .and_then(|p| p.split_once(',').and_then(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?))))
            }
            "--open" => initial_window = args.next(),
            "--help" | "-h" => {
                attach_parent_console();
                println!(
                    "{APP_NAME}\n  --testnet | --devnet | --simnet\n  --no-spawn\n  --aerod <path>\n  --rpc <host:port>\n  --print-address   print the wallet's receiving address and exit\n  --selftest [--send-to <addr> --amount <ARO>]   headless connection and wallet check"
                );
                return Ok(());
            }
            other => eprintln!("ignoring unknown argument: {other}"),
        }
    }

    let aerod_path = aerod_path.unwrap_or_else(|| {
        let exe = if cfg!(windows) { "aerod.exe" } else { "aerod" };
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(exe))).unwrap_or_else(|| PathBuf::from(exe))
    });
    let rpc_address = rpc_address.unwrap_or_else(|| format!("127.0.0.1:{}", network.default_rpc_port()));
    let data_dir = datadir.unwrap_or_else(wallet::data_dir);
    let config = NodeConfig { network, testnet_suffix, spawn, aerod_path, rpc_address, data_dir: data_dir.clone(), p2p_port, add_peer, upnp: true };

    if print_address {
        attach_parent_console();
        match Wallet::load_or_create(&data_dir, Prefix::from(network)) {
            Ok(w) => w.addresses().iter().for_each(|(label, a)| println!("{a}  {label}")),
            Err(e) => eprintln!("wallet error: {e}"),
        }
        return Ok(());
    }
    if run_selftest {
        attach_parent_console();
        std::process::exit(selftest(config, send_to.zip(send_amount)));
    }

    let title = match network {
        NetworkType::Mainnet => format!("{APP_NAME} - Wallet"),
        other => format!("{APP_NAME} - Wallet [{}]", format!("{other:?}").to_lowercase()),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(eframe::icon_data::from_png_bytes(ICON_PNG).expect("the embedded icon is a valid PNG"))
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title(title)
            .with_position(window_pos.map(|(x, y)| egui::pos2(x, y)).unwrap_or(egui::pos2(40.0, 40.0))),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            apply_theme(&cc.egui_ctx);
            let data_dir = config.data_dir.clone();
            let mut app = App::new(config, data_dir);
            match initial_tab.as_deref() {
                Some("send") => app.tab = Tab::Send,
                Some("receive") => app.tab = Tab::Receive,
                Some("transactions") => app.tab = Tab::Transactions,
                Some("mine") => app.tab = Tab::Mine,
                _ => {}
            }
            match initial_window.as_deref() {
                Some("console") => app.show_console = true,
                Some("peers") => app.show_peers = true,
                Some("info") => app.show_info = true,
                Some("addresses") => app.show_receiving_addresses = true,
                _ => {}
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datetime_formatting() {
        assert_eq!(fmt_datetime(0), "1970-01-01 00:00");
        assert_eq!(fmt_datetime(1_794_182_400), "2026-11-09 00:00");
        assert_eq!(fmt_datetime(951_782_400 + 3600 * 13 + 60 * 7), "2000-02-29 13:07");
    }

    #[test]
    fn test_signed_amount_formatting() {
        assert_eq!(format_signed(150_000_000), "1.50000000");
        assert_eq!(format_signed(-150_000_000), "-1.50000000");
        assert_eq!(format_signed(0), "0.00000000");
    }
}

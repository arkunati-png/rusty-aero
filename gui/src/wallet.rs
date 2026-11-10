//! A simple file-backed wallet: a list of Schnorr keys, receiving addresses derived from them, and transaction
//! building/signing over the outputs the node reports for those addresses.
//!
//! New wallets are created from a 12-word BIP39 recovery phrase: every key is derived from it with BIP32 at
//! `m/44'/777777'/0'/0/<index>`, so the phrase alone restores the wallet. Older wallets (random keys with no phrase)
//! still load, but cannot be recovered from words.
//!
//! PRE-RELEASE: the recovery phrase and keys are stored in `wallet-<network>.json` UNENCRYPTED. Do not keep real funds
//! in this wallet until encryption is implemented (see the "Encrypt Wallet" menu entry).

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use std::str::FromStr;

use aero_addresses::{Address, Prefix, Version};
use aero_bip32::{DerivationPath, ExtendedPrivateKey, Language, Mnemonic, SecretKey, WordCount};
use aero_consensus_core::{
    constants::{SOMPI_PER_AERO, TX_VERSION},
    sign::{Signed, sign_with_multiple_v2},
    subnets::SUBNETWORK_ID_NATIVE,
    tx::{ComputeCommit, MutableTransaction, Transaction, TransactionInput, TransactionOutput},
};
use aero_txscript::pay_to_address_script;
use itertools::Itertools;
use secp256k1::Keypair;
use serde::{Deserialize, Serialize};

use crate::node::WalletUtxo;

/// Minimum standard relay fee is 100 sompi/gram; 110 leaves a margin (same as the `rothschild` tool).
const FEE_RATE: u64 = 110;
/// Mempool minimum relay fee uses post-Toccata cofactors immediately, so normalize accordingly.
const NORMALIZED_TRANSIENT_BYTE_FACTOR: u64 = 2;
/// Outputs below this are not created: change smaller than this is added to the fee instead.
const DUST: u64 = 1_000;
/// BIP44 coin type for Aero's key derivation. Deliberately not Kaspa's (111111), so a Kaspa recovery phrase and an
/// Aero wallet never share keys.
const COIN_TYPE: u32 = 777_777;
/// How many addresses a restore derives up front (there is no chain scan to find used ones).
const RESTORE_ADDRESSES: u32 = 20;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KeyEntry {
    pub secret_hex: String,
    pub label: String,
    pub created: u64,
    /// Position in the recovery phrase's derivation path; `None` for a random key that the phrase cannot recreate.
    #[serde(default)]
    pub index: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SentTx {
    pub txid: String,
    pub time: u64,
    pub amount: u64,
    pub fee: u64,
    pub to: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct WalletFile {
    keys: Vec<KeyEntry>,
    sent: Vec<SentTx>,
    /// The BIP39 recovery phrase, for wallets created with one.
    #[serde(default)]
    mnemonic: Option<String>,
    /// The next derivation index for a new receiving address.
    #[serde(default)]
    next_index: u32,
    /// Whether the user confirmed that they wrote the phrase down.
    #[serde(default)]
    seed_backed_up: bool,
}

pub struct Wallet {
    path: PathBuf,
    prefix: Prefix,
    file: WalletFile,
}

pub struct BuiltTx {
    pub tx: Transaction,
    pub fee: u64,
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `%APPDATA%\Aero` on Windows, `~/.aero` elsewhere.
pub fn data_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Aero");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".aero")
}

impl Wallet {
    /// Loads the wallet from `dir/wallet.json`, creating it (with one receiving address) if it does not exist.
    pub fn load_or_create(dir: &Path, prefix: Prefix) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(format!("wallet-{}.json", prefix));
        let file = if path.exists() {
            let text = fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            serde_json::from_str(&text).map_err(|e| format!("wallet file is corrupt: {e}"))?
        } else {
            WalletFile::default()
        };
        let mut wallet = Self { path, prefix, file };
        if wallet.file.keys.is_empty() {
            let mnemonic = Mnemonic::random(WordCount::Words12, Language::English)
                .map_err(|e| format!("cannot create a recovery phrase: {e}"))?;
            wallet.file.mnemonic = Some(mnemonic.phrase().to_string());
            wallet.new_key("(no label)")?;
        }
        Ok(wallet)
    }

    /// Restores a wallet from a recovery phrase, replacing the current wallet file for this network. The old file is
    /// kept next to it as `*.backup-<time>`, so no key is ever destroyed.
    pub fn restore(dir: &Path, prefix: Prefix, phrase: &str) -> Result<Self, String> {
        let phrase = phrase.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>().join(" ");
        let mnemonic = Mnemonic::new(&phrase, Language::English)
            .map_err(|_| "That is not a valid recovery phrase. Check the words, their spelling and their order.".to_string())?;

        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = dir.join(format!("wallet-{}.json", prefix));
        if path.exists() {
            let backup = dir.join(format!("wallet-{}.json.backup-{}", prefix, unix_now()));
            fs::rename(&path, &backup).map_err(|e| format!("cannot back up the current wallet to {}: {e}", backup.display()))?;
        }

        let mut wallet = Self {
            path,
            prefix,
            file: WalletFile { mnemonic: Some(mnemonic.phrase().to_string()), seed_backed_up: true, ..Default::default() },
        };
        for _ in 0..RESTORE_ADDRESSES {
            wallet.new_key("(restored)")?;
        }
        Ok(wallet)
    }

    /// The recovery phrase, if this wallet has one.
    pub fn recovery_phrase(&self) -> Option<&str> {
        self.file.mnemonic.as_deref()
    }

    /// True once the user confirmed writing the phrase down (always true for restored wallets).
    pub fn seed_backed_up(&self) -> bool {
        self.file.seed_backed_up
    }

    pub fn mark_seed_backed_up(&mut self) {
        self.file.seed_backed_up = true;
        let _ = self.save();
    }

    fn save(&self) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&self.file).map_err(|e| e.to_string())?;
        fs::write(&self.path, text).map_err(|e| format!("cannot write {}: {e}", self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a new key, saves the wallet, and returns its address.
    pub fn new_key(&mut self, label: &str) -> Result<Address, String> {
        let (keypair, index) = match &self.file.mnemonic {
            Some(phrase) => {
                let index = self.file.next_index;
                let keypair = Keypair::from_seckey_slice(secp256k1::SECP256K1, &derive_secret(phrase, index)?)
                    .map_err(|e| format!("derived an invalid key: {e}"))?;
                self.file.next_index += 1;
                (keypair, Some(index))
            }
            // An older wallet without a recovery phrase keeps getting random keys.
            None => (Keypair::new(secp256k1::SECP256K1, &mut rand::thread_rng()), None),
        };
        let entry = KeyEntry { secret_hex: hex_encode(&keypair.secret_bytes()), label: label.to_string(), created: unix_now(), index };
        self.file.keys.push(entry);
        self.save()?;
        Ok(self.address_of(&keypair))
    }

    fn address_of(&self, keypair: &Keypair) -> Address {
        let (xonly, _) = keypair.x_only_public_key();
        Address::new(self.prefix, Version::PubKey, &xonly.serialize())
    }

    fn keypairs(&self) -> Vec<Keypair> {
        self.file
            .keys
            .iter()
            .filter_map(|k| hex_decode(&k.secret_hex))
            .filter_map(|bytes| Keypair::from_seckey_slice(secp256k1::SECP256K1, &bytes).ok())
            .collect()
    }

    /// All receiving addresses with their labels, oldest first.
    pub fn addresses(&self) -> Vec<(String, Address)> {
        self.file
            .keys
            .iter()
            .filter_map(|k| {
                let bytes = hex_decode(&k.secret_hex)?;
                let keypair = Keypair::from_seckey_slice(secp256k1::SECP256K1, &bytes).ok()?;
                Some((k.label.clone(), self.address_of(&keypair)))
            })
            .collect()
    }

    pub fn sent(&self) -> &[SentTx] {
        &self.file.sent
    }

    pub fn record_sent(&mut self, sent: SentTx) {
        self.file.sent.push(sent);
        let _ = self.save();
    }

    /// Builds and signs a payment of `amount` sompi to `to`, spending from `spendable` (largest first) and
    /// returning change to the wallet's first address. The fee is computed from the estimated mass.
    pub fn build_payment(&self, spendable: &[WalletUtxo], to: &Address, amount: u64) -> Result<BuiltTx, String> {
        if amount == 0 {
            return Err("The amount to pay must be larger than 0.".into());
        }
        let change_address = self.addresses().into_iter().next().map(|(_, a)| a).ok_or("The wallet has no addresses.")?;

        let mut candidates: Vec<&WalletUtxo> = spendable.iter().collect();
        candidates.sort_by_key(|u| std::cmp::Reverse(u.amount));

        let mut selected: Vec<&WalletUtxo> = Vec::new();
        let mut total = 0u64;
        let mut fee = 0u64;
        for utxo in candidates {
            selected.push(utxo);
            total += utxo.amount;
            fee = required_fee(selected.len(), 2);
            if total >= amount + fee {
                break;
            }
        }
        if selected.is_empty() || total < amount + fee {
            return Err("The amount exceeds your balance (including the transaction fee).".into());
        }

        let mut change = total - amount - fee;
        let mut outputs = vec![TransactionOutput { value: amount, script_public_key: pay_to_address_script(to), covenant: None }];
        if change >= DUST {
            outputs.push(TransactionOutput {
                value: change,
                script_public_key: pay_to_address_script(&change_address),
                covenant: None,
            });
        } else {
            // Not worth an output: it simply becomes extra fee.
            fee += change;
            change = 0;
        }
        let _ = change;

        let inputs = selected
            .iter()
            .map(|u| TransactionInput {
                previous_outpoint: u.outpoint,
                signature_script: vec![],
                sequence: 0,
                compute_commit: ComputeCommit::SigopCount(1.into()),
            })
            .collect_vec();
        let unsigned = Transaction::new_non_finalized(TX_VERSION, inputs, outputs, 0, SUBNETWORK_ID_NATIVE, 0, vec![]);
        let entries = selected.iter().map(|u| u.entry.clone()).collect_vec();
        let mutable = MutableTransaction::with_entries(unsigned, entries);

        let secrets: Vec<[u8; 32]> = self.keypairs().iter().map(|k| k.secret_bytes()).collect();
        match sign_with_multiple_v2(mutable, &secrets) {
            Signed::Fully(signed) => Ok(BuiltTx { tx: signed.tx, fee }),
            Signed::Partially(_) => Err("Could not sign all inputs with the wallet's keys.".into()),
        }
    }
}

/// Fee for a standard transaction with `num_inputs` inputs and `num_outputs` outputs (single-signature).
pub fn required_fee(num_inputs: usize, num_outputs: u64) -> u64 {
    let serialized_bytes = 94 // version, counts, locktime, subnetwork id, gas, payload hash and length
        + 118 * num_inputs as u64 // std input: outpoint + signature script (single signature) + sequence
        + 53 * num_outputs; // std output: value + script public key version/length + max std script public key
    let compute_mass = serialized_bytes + 1000 * num_inputs as u64 + 370 * num_outputs;
    FEE_RATE * compute_mass.max(serialized_bytes * NORMALIZED_TRANSIENT_BYTE_FACTOR)
}

/// Formats sompi as a decimal ARO amount with 8 decimals.
pub fn format_aero(sompi: u64) -> String {
    format!("{}.{:08}", sompi / SOMPI_PER_AERO, sompi % SOMPI_PER_AERO)
}

/// Parses a decimal ARO amount ("1", "1.5", "0.00000001") into sompi.
pub fn parse_aero(text: &str) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Enter an amount.".into());
    }
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 8 {
        return Err("An amount can have at most 8 decimal places.".into());
    }
    let digits_ok = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if !digits_ok(whole) || !digits_ok(fraction) || (whole.is_empty() && fraction.is_empty()) {
        return Err("Invalid amount.".into());
    }
    let whole: u64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| "Invalid amount.".to_string())? };
    let fraction: u64 =
        if fraction.is_empty() { 0 } else { format!("{fraction:0<8}").parse().map_err(|_| "Invalid amount.".to_string())? };
    whole.checked_mul(SOMPI_PER_AERO).and_then(|w| w.checked_add(fraction)).ok_or_else(|| "The amount is too large.".to_string())
}

/// The private key at `m/44'/COIN_TYPE'/0'/0/index` for a BIP39 phrase.
fn derive_secret(phrase: &str, index: u32) -> Result<[u8; 32], String> {
    let mnemonic = Mnemonic::new(phrase, Language::English).map_err(|e| format!("invalid recovery phrase: {e}"))?;
    let seed = mnemonic.to_seed("");
    let path =
        DerivationPath::from_str(&format!("m/44'/{COIN_TYPE}'/0'/0/{index}")).map_err(|e| format!("bad derivation path: {e}"))?;
    let key = ExtendedPrivateKey::<SecretKey>::new(seed.as_bytes())
        .and_then(|root| root.derive_path(&path))
        .map_err(|e| format!("key derivation failed: {e}"))?;
    Ok(key.to_bytes().into())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_amount_formatting_and_parsing_round_trip() {
        for sompi in [0u64, 1, 99_999_999, 100_000_000, 123_456_789, 10_000_000_000_000_000] {
            assert_eq!(parse_aero(&format_aero(sompi)).unwrap(), sompi);
        }
        assert_eq!(parse_aero("1.5").unwrap(), 150_000_000);
        assert_eq!(parse_aero(".5").unwrap(), 50_000_000);
        assert_eq!(parse_aero("2").unwrap(), 200_000_000);
        assert!(parse_aero("1.123456789").is_err());
        assert!(parse_aero("abc").is_err());
        assert!(parse_aero("").is_err());
        assert!(parse_aero("-1").is_err());
    }

    #[test]
    fn test_hex_round_trip() {
        let bytes = [0u8, 1, 127, 128, 255];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn test_fee_grows_with_inputs() {
        assert!(required_fee(2, 2) > required_fee(1, 2));
        assert!(required_fee(1, 2) > 0);
    }

    #[test]
    fn test_wallet_creates_addresses_with_the_network_prefix() {
        let dir = std::env::temp_dir().join(format!("aero-gui-test-{}", unix_now()));
        let mut wallet = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        let second = wallet.new_key("savings").unwrap();
        assert!(second.to_string().starts_with("aerodev:"));
        assert_eq!(wallet.addresses().len(), 2);
        // Reloading finds the same keys.
        let reloaded = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        assert_eq!(reloaded.addresses().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("aero-gui-{name}-{}-{}", std::process::id(), unix_now()))
    }

    const TEST_PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn test_new_wallets_have_a_12_word_phrase_that_restores_them() {
        let dir = temp_dir("seed");
        let mut wallet = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        wallet.new_key("second").unwrap();
        let phrase = wallet.recovery_phrase().expect("new wallets have a phrase").to_string();
        assert_eq!(phrase.split_whitespace().count(), 12);
        assert!(!wallet.seed_backed_up());
        let originals = wallet.addresses();

        // The same phrase, in another folder, recreates the same addresses in the same order.
        let other = temp_dir("seed-restore");
        let mut restored = Wallet::restore(&other, Prefix::Devnet, &phrase).unwrap();
        assert!(restored.seed_backed_up());
        let restored_addresses = restored.addresses();
        assert_eq!(restored_addresses.len(), RESTORE_ADDRESSES as usize);
        for (original, again) in originals.iter().zip(&restored_addresses) {
            assert_eq!(original.1, again.1);
        }
        // New addresses carry on after the restored ones, never reusing an index.
        let next = restored.new_key("next").unwrap();
        assert!(restored_addresses.iter().all(|(_, a)| *a != next));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&other);
    }

    #[test]
    fn test_known_phrase_gives_a_stable_address() {
        // The standard BIP39 test phrase. If the address changes, existing users' funds would move: do not "fix" it.
        let dir = temp_dir("known");
        let wallet = Wallet::restore(&dir, Prefix::Mainnet, TEST_PHRASE).unwrap();
        let first = wallet.addresses()[0].1.to_string();
        let again = Wallet::restore(&temp_dir("known2"), Prefix::Mainnet, &format!("  {}  ", TEST_PHRASE.to_uppercase())).unwrap();
        assert_eq!(first, again.addresses()[0].1.to_string(), "extra spaces and capitals must not matter");
        assert_eq!(first, "aero:qphedg2n2njepsgru2824xrktukxhrsqtpr7j49z7svjj2ns84ngwcaegtpjj");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_invalid_phrases_are_rejected_and_do_not_touch_the_wallet() {
        let dir = temp_dir("invalid");
        let wallet = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        let before = wallet.addresses();
        // Bad checksum, unknown word, wrong length, empty.
        for bad in [
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon",
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon zzzzzz",
            "abandon abandon abandon",
            "",
        ] {
            assert!(Wallet::restore(&dir, Prefix::Devnet, bad).is_err(), "accepted: {bad:?}");
        }
        let reloaded = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        assert_eq!(before, reloaded.addresses());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_restore_keeps_a_backup_of_the_old_wallet() {
        let dir = temp_dir("backup");
        let old = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        let old_addresses = old.addresses();
        let restored = Wallet::restore(&dir, Prefix::Devnet, TEST_PHRASE).unwrap();
        assert_ne!(old_addresses[0].1, restored.addresses()[0].1);
        let backups = fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".backup-")).count();
        assert_eq!(backups, 1, "the replaced wallet file must be kept");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_older_wallet_files_without_a_phrase_still_load() {
        let dir = temp_dir("legacy");
        fs::create_dir_all(&dir).unwrap();
        let secret = "11".repeat(32);
        fs::write(
            dir.join(format!("wallet-{}.json", Prefix::Devnet)),
            format!(r#"{{"keys":[{{"secret_hex":"{secret}","label":"old","created":1}}],"sent":[]}}"#),
        )
        .unwrap();
        let mut wallet = Wallet::load_or_create(&dir, Prefix::Devnet).unwrap();
        assert!(wallet.recovery_phrase().is_none());
        assert_eq!(wallet.addresses().len(), 1);
        wallet.new_key("more").unwrap();
        assert_eq!(wallet.addresses().len(), 2);
        assert!(wallet.recovery_phrase().is_none(), "an old wallet must not silently gain a phrase it cannot be recovered from");
        let _ = fs::remove_dir_all(&dir);
    }
}

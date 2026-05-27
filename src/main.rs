//! Central owner-side monitor for the Era 2FA EN signer network.
//!
//! Unlike the `en-2fa` sidecar (which each operator runs against their own EN), this is a single
//! watcher that the 2FA *owner* runs. It reads everything it needs from L1 on-chain, so it does
//! NOT need access to any operator's External Node / Postgres. It answers the two questions the
//! owner cares about:
//!
//!   1. "Do all the signer accounts have funds to operate?"  -> per-signer L1 ETH balance.
//!   2. "Are all nodes working?"  -> per-signer multisig membership + on-chain liveness
//!      (is the signer still sending approvals?) + whether the network as a whole keeps
//!      executing batches (i.e. threshold keeps being met).
//!
//! When a condition trips it pushes an alert to Slack and/or Telegram. Alerts are de-duplicated:
//! a condition fires once when it starts, re-fires on a cooldown while it persists, and sends a
//! "resolved" message when it clears.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use dotenvy::dotenv;
use ethers::prelude::*;
use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

// Minimal slice of the ExecutionMultisigValidator ABI that the monitor needs.
abigen!(
    ValidatorContract,
    r#"[
        function threshold() view returns (uint256)
        function executionMultisigMember(address signer) view returns (bool)
    ]"#
);

// Standard ZKsync diamond (GettersFacet) batch counters, used to watch execution progress.
abigen!(
    DiamondGetters,
    r#"[
        function getTotalBatchesCommitted() view returns (uint256)
        function getTotalBatchesExecuted() view returns (uint256)
    ]"#
);

#[derive(Parser, Debug)]
#[command(
    name = "en-2fa-monitor",
    about = "Central owner-side monitor for the Era 2FA EN signer network (on-chain + Slack/Telegram alerts)"
)]
struct Args {
    /// L1 JSON-RPC URL (must match the network: sepolia or mainnet).
    #[arg(long, env = "ETH_RPC_URL")]
    eth_rpc_url: String,

    /// ExecutionMultisigValidator contract address.
    #[arg(long, env = "VALIDATOR_ADDRESS")]
    validator_address: String,

    /// Chain (diamond proxy) address, used to read execution progress.
    #[arg(long, env = "CHAIN_ADDRESS")]
    chain_address: String,

    /// Comma-separated list of signer addresses to watch (the set the owner funds/registers).
    /// Each entry may optionally carry a human-readable label as `label=0xADDRESS`
    /// (e.g. `alice=0xabc...,bob=0xdef...`); the label is shown in alerts so you can tell
    /// which operator is affected. Bare `0xADDRESS` entries (no label) are also accepted.
    #[arg(long, env = "SIGNERS", value_delimiter = ',')]
    signers: Vec<String>,

    /// Human-readable label for this network, used in alert text (e.g. "era-mainnet").
    #[arg(long, env = "NETWORK_NAME", default_value = "era-2fa")]
    network_name: String,

    /// Alert when a signer's L1 balance drops below this many ETH.
    #[arg(long, env = "MIN_BALANCE_ETH", default_value = "0.05")]
    min_balance_eth: f64,

    /// Alert when (committed - executed) batches reaches at least this value AND execution has
    /// not advanced for `--stall-secs`. Catches a network that has stopped reaching threshold.
    #[arg(long, env = "EXEC_LAG_ALERT", default_value_t = 1)]
    exec_lag_alert: u64,

    /// How long execution may stall (no executed-batch progress) while there is a lag, and how
    /// long a signer's nonce may stay static while there is work, before alerting. Seconds.
    #[arg(long, env = "STALL_SECS", default_value_t = 3600)]
    stall_secs: u64,

    /// Seconds between monitor cycles.
    #[arg(long, env = "POLL_INTERVAL_SECS", default_value_t = 60)]
    poll_interval_secs: u64,

    /// Re-send a still-active alert at most once per this many seconds.
    #[arg(long, env = "ALERT_COOLDOWN_SECS", default_value_t = 3600)]
    alert_cooldown_secs: u64,

    /// Send an "all healthy" heartbeat summary every this many seconds (0 = disabled).
    #[arg(long, env = "HEARTBEAT_SECS", default_value_t = 0)]
    heartbeat_secs: u64,

    /// Slack incoming-webhook URL (optional).
    #[arg(long, env = "SLACK_WEBHOOK_URL")]
    slack_webhook_url: Option<String>,

    /// Telegram bot token (optional; requires --telegram-chat-id).
    #[arg(long, env = "TELEGRAM_BOT_TOKEN")]
    telegram_bot_token: Option<String>,

    /// Telegram chat id to post to (optional; requires --telegram-bot-token).
    #[arg(long, env = "TELEGRAM_CHAT_ID")]
    telegram_chat_id: Option<String>,
}

fn parse_address(s: &str) -> Result<Address> {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).context("bad hex address")?;
    if bytes.len() != 20 {
        return Err(anyhow!("address must be 20 bytes"));
    }
    Ok(Address::from_slice(&bytes))
}

/// Posts alert text to whichever channels are configured.
struct Notifier {
    http: reqwest::Client,
    slack_webhook_url: Option<String>,
    telegram_bot_token: Option<String>,
    telegram_chat_id: Option<String>,
}

impl Notifier {
    async fn send(&self, text: &str) {
        // Always log; the chat post is best-effort.
        info!(alert = %text, "dispatching alert");

        if let Some(url) = &self.slack_webhook_url {
            // `link_names` makes Slack parse `@handle`/`#channel` in the text into real mentions
            // (so a signer label like `@alice` actually pings them). `<@U123>` ID mentions and
            // `<!here>`/`<!channel>` always work regardless.
            let body = serde_json::json!({ "text": text, "link_names": true });
            if let Err(e) = self.http.post(url).json(&body).send().await {
                warn!("failed to post Slack alert: {e}");
            }
        }

        if let (Some(token), Some(chat_id)) = (&self.telegram_bot_token, &self.telegram_chat_id) {
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            let body = serde_json::json!({ "chat_id": chat_id, "text": text });
            if let Err(e) = self.http.post(&url).json(&body).send().await {
                warn!("failed to post Telegram alert: {e}");
            }
        }
    }
}

struct AlertEntry {
    active: bool,
    last_notified: Instant,
}

/// Tracks per-condition alert state so we fire on transition, remind on a cooldown, and
/// announce recovery — instead of spamming the channel every cycle.
struct AlertManager {
    notifier: Notifier,
    cooldown: Duration,
    network: String,
    states: HashMap<String, AlertEntry>,
}

impl AlertManager {
    /// Evaluate one condition. `key` must be stable per logical alert (e.g. "low_balance:0xabc..").
    async fn evaluate(&mut self, key: &str, firing: bool, fire_msg: &str, resolve_msg: &str) {
        let now = Instant::now();
        let entry = self.states.entry(key.to_string()).or_insert(AlertEntry {
            active: false,
            last_notified: now,
        });

        if firing {
            if !entry.active {
                entry.active = true;
                entry.last_notified = now;
                self.notifier
                    .send(&format!("🔴 [{}] {}", self.network, fire_msg))
                    .await;
            } else if now.duration_since(entry.last_notified) >= self.cooldown {
                entry.last_notified = now;
                self.notifier
                    .send(&format!("🔴 [{}] (still firing) {}", self.network, fire_msg))
                    .await;
            }
        } else if entry.active {
            entry.active = false;
            self.notifier
                .send(&format!("✅ [{}] RESOLVED: {}", self.network, resolve_msg))
                .await;
        }
    }
}

/// Human-friendly identifier for a signer in alert text: `label (0xADDRESS)` if a label was
/// configured, otherwise just the address.
fn signer_name(addr: &Address, labels: &HashMap<Address, String>) -> String {
    match labels.get(addr) {
        Some(label) => format!("{label} ({addr:?})"),
        None => format!("{addr:?}"),
    }
}

/// Per-signer state we carry across cycles to detect liveness (nonce movement over time).
struct SignerActivity {
    last_nonce: U256,
    nonce_changed_at: Instant,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::default().add_directive(LevelFilter::INFO.into())),
        )
        .init();

    let args = Args::parse();

    if args.signers.is_empty() {
        return Err(anyhow!("no signers configured; set --signers / SIGNERS"));
    }
    // Each entry is either `0xADDRESS` or `label=0xADDRESS`. The label (if any) is shown in
    // alerts so the owner can tell which operator is affected without decoding a hex address.
    let mut signers: Vec<Address> = Vec::with_capacity(args.signers.len());
    let mut labels: HashMap<Address, String> = HashMap::new();
    for entry in &args.signers {
        let (label, addr_str) = match entry.split_once('=') {
            Some((label, addr)) => (Some(label.trim().to_string()), addr.trim()),
            None => (None, entry.trim()),
        };
        let addr = parse_address(addr_str).with_context(|| format!("bad signer address {addr_str}"))?;
        signers.push(addr);
        if let Some(label) = label {
            if !label.is_empty() {
                labels.insert(addr, label);
            }
        }
    }

    if args.slack_webhook_url.is_none()
        && (args.telegram_bot_token.is_none() || args.telegram_chat_id.is_none())
    {
        warn!(
            "no alert channel configured (no Slack webhook, no complete Telegram config); \
             alerts will only be logged"
        );
    }

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .context("Failed to build HTTP client")?;
    let rpc_url = url::Url::parse(&args.eth_rpc_url).context("Bad ETH_RPC_URL")?;
    let http = ethers::providers::Http::new_with_client(rpc_url, http_client.clone());
    let provider = Arc::new(Provider::new(http).interval(Duration::from_millis(200)));

    let validator_addr = parse_address(&args.validator_address).context("Bad VALIDATOR_ADDRESS")?;
    let chain_addr = parse_address(&args.chain_address).context("Bad CHAIN_ADDRESS")?;
    let validator = ValidatorContract::new(validator_addr, provider.clone());
    let diamond = DiamondGetters::new(chain_addr, provider.clone());

    let min_balance_wei = ethers::utils::parse_ether(args.min_balance_eth)
        .context("Bad --min-balance-eth")?;

    let notifier = Notifier {
        http: http_client,
        slack_webhook_url: args.slack_webhook_url.clone(),
        telegram_bot_token: args.telegram_bot_token.clone(),
        telegram_chat_id: args.telegram_chat_id.clone(),
    };
    let mut alerts = AlertManager {
        notifier,
        cooldown: Duration::from_secs(args.alert_cooldown_secs),
        network: args.network_name.clone(),
        states: HashMap::new(),
    };

    info!(
        network = %args.network_name,
        signers = signers.len(),
        validator = %validator_addr,
        chain = %chain_addr,
        "2FA monitor starting"
    );

    let stall = Duration::from_secs(args.stall_secs);
    let mut activity: HashMap<Address, SignerActivity> = HashMap::new();
    let mut last_executed: Option<U256> = None;
    let mut executed_changed_at = Instant::now();
    let mut last_committed: Option<U256> = None;
    let mut committed_changed_at = Instant::now();
    let mut last_heartbeat = Instant::now();

    loop {
        if let Err(e) = run_cycle(
            &provider,
            &validator,
            &diamond,
            &signers,
            &labels,
            min_balance_wei,
            args.min_balance_eth,
            args.exec_lag_alert,
            stall,
            &mut alerts,
            &mut activity,
            &mut last_executed,
            &mut executed_changed_at,
            &mut last_committed,
            &mut committed_changed_at,
        )
        .await
        {
            warn!("monitor cycle failed: {e:#}");
            alerts
                .evaluate(
                    "monitor_rpc",
                    true,
                    &format!("monitor cannot read L1: {e}"),
                    "monitor can read L1 again",
                )
                .await;
        } else {
            alerts
                .evaluate("monitor_rpc", false, "", "monitor can read L1 again")
                .await;
        }

        if args.heartbeat_secs > 0
            && last_heartbeat.elapsed() >= Duration::from_secs(args.heartbeat_secs)
        {
            last_heartbeat = Instant::now();
            let active = alerts.states.values().filter(|e| e.active).count();
            let msg = if active == 0 {
                format!("💚 [{}] heartbeat: all {} signers healthy", args.network_name, signers.len())
            } else {
                format!("⚠️ [{}] heartbeat: {} active alert(s)", args.network_name, active)
            };
            alerts.notifier.send(&msg).await;
        }

        sleep(Duration::from_secs(args.poll_interval_secs)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_cycle(
    provider: &Arc<Provider<Http>>,
    validator: &ValidatorContract<Provider<Http>>,
    diamond: &DiamondGetters<Provider<Http>>,
    signers: &[Address],
    labels: &HashMap<Address, String>,
    min_balance_wei: U256,
    min_balance_eth: f64,
    exec_lag_alert: u64,
    stall: Duration,
    alerts: &mut AlertManager,
    activity: &mut HashMap<Address, SignerActivity>,
    last_executed: &mut Option<U256>,
    executed_changed_at: &mut Instant,
    last_committed: &mut Option<U256>,
    committed_changed_at: &mut Instant,
) -> Result<()> {
    // --- Threshold + how many signers are actually usable (funded AND a member) ---
    let threshold = validator
        .threshold()
        .call()
        .await
        .context("read threshold")?;

    let mut usable = 0u64;

    for &signer in signers {
        let balance = provider
            .get_balance(signer, None)
            .await
            .with_context(|| format!("get_balance {signer:?}"))?;
        let is_member = validator
            .execution_multisig_member(signer)
            .call()
            .await
            .with_context(|| format!("executionMultisigMember {signer:?}"))?;
        let nonce = provider
            .get_transaction_count(signer, None)
            .await
            .with_context(|| format!("get_transaction_count {signer:?}"))?;

        let balance_ok = balance >= min_balance_wei;
        if balance_ok && is_member {
            usable += 1;
        }

        // Human-friendly identifier for alert text; the alert key stays keyed on the raw address.
        let name = signer_name(&signer, labels);

        // Funds alert.
        alerts
            .evaluate(
                &format!("low_balance:{signer:?}"),
                !balance_ok,
                &format!(
                    "signer {name} low balance: {} ETH (< {min_balance_eth} ETH)",
                    ethers::utils::format_ether(balance)
                ),
                &format!(
                    "signer {name} balance restored: {} ETH",
                    ethers::utils::format_ether(balance)
                ),
            )
            .await;

        // Membership alert.
        alerts
            .evaluate(
                &format!("not_member:{signer:?}"),
                !is_member,
                &format!("signer {name} is NOT a registered multisig member"),
                &format!("signer {name} is a registered multisig member again"),
            )
            .await;

        // Track nonce movement for liveness.
        let now = Instant::now();
        let act = activity.entry(signer).or_insert(SignerActivity {
            last_nonce: nonce,
            nonce_changed_at: now,
        });
        if nonce != act.last_nonce {
            act.last_nonce = nonce;
            act.nonce_changed_at = now;
        }
    }

    // --- Network execution progress ---
    let committed = diamond
        .get_total_batches_committed()
        .call()
        .await
        .context("getTotalBatchesCommitted")?;
    let executed = diamond
        .get_total_batches_executed()
        .call()
        .await
        .context("getTotalBatchesExecuted")?;
    let lag = committed.saturating_sub(executed);

    let now = Instant::now();
    if Some(executed) != *last_executed {
        *last_executed = Some(executed);
        *executed_changed_at = now;
    }
    if Some(committed) != *last_committed {
        *last_committed = Some(committed);
        *committed_changed_at = now;
    }
    let exec_stalled =
        lag >= U256::from(exec_lag_alert) && now.duration_since(*executed_changed_at) >= stall;

    // The chain is actively producing work to sign if it has committed a new batch within the
    // last `stall` window. Every healthy signer is expected to approve every batch, so a signer
    // silent across this window while the chain keeps committing is treated as down — even if the
    // network still executes via the other signers (early warning before threshold is at risk).
    let chain_producing = now.duration_since(*committed_changed_at) <= stall;

    alerts
        .evaluate(
            "exec_stalled",
            exec_stalled,
            &format!(
                "execution stalled: {lag} batches committed-but-not-executed and no progress for \
                 over {}s (committed={committed}, executed={executed}). Threshold likely not being met.",
                stall.as_secs()
            ),
            "execution is progressing again",
        )
        .await;

    // --- Per-signer liveness: who is silent while the chain keeps committing batches? ---
    if chain_producing {
        for &signer in signers {
            if let Some(act) = activity.get(&signer) {
                let idle = now.duration_since(act.nonce_changed_at) >= stall;
                let name = signer_name(&signer, labels);
                alerts
                    .evaluate(
                        &format!("inactive:{signer:?}"),
                        idle,
                        &format!(
                            "signer {name} has sent no L1 tx for over {}s while the chain keeps \
                             committing batches — node may be down (every healthy signer should \
                             approve every batch)",
                            stall.as_secs()
                        ),
                        &format!("signer {name} is active again"),
                    )
                    .await;
            }
        }
    } else {
        // Chain isn't committing new batches: nothing to sign, so clear any inactivity alerts.
        for &signer in signers {
            alerts
                .evaluate(
                    &format!("inactive:{signer:?}"),
                    false,
                    "",
                    &format!("signer {} is active again", signer_name(&signer, labels)),
                )
                .await;
        }
    }

    // --- Can the network still reach threshold at all? ---
    let below_threshold = U256::from(usable) < threshold;
    alerts
        .evaluate(
            "usable_below_threshold",
            below_threshold,
            &format!(
                "only {usable} signers are funded AND registered, but threshold is {threshold} — \
                 the network may be unable to execute batches"
            ),
            &format!("enough usable signers again ({usable} >= threshold {threshold})"),
        )
        .await;

    info!(
        %threshold,
        usable,
        %committed,
        %executed,
        %lag,
        "monitor cycle complete"
    );

    Ok(())
}

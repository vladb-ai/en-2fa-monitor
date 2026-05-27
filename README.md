# en-2fa-monitor

Central owner-side monitor for the Era 2FA EN signer network.

Unlike the `en-2fa` sidecar (which each operator runs against their own External Node), this is a
single watcher that the 2FA **owner** runs. It reads everything from L1 on-chain, so it needs **no
access to any operator's EN/Postgres** — only an L1 RPC and the list of signer addresses you
fund/register. It answers the two operational questions:

- **Do accounts have funds?** — per-signer L1 ETH balance vs `--min-balance-eth`.
- **Are all nodes working?** — per-signer multisig membership (`executionMultisigMember`),
  per-signer on-chain liveness (nonce stops moving while batches await execution), and whether the
  network as a whole keeps executing batches (`getTotalBatchesCommitted/Executed` on the diamond,
  vs `threshold`).

Conditions are pushed to Slack and/or Telegram. Each alert fires once when it starts, re-fires on a
cooldown while it persists, and sends a "resolved" message when it clears.

## Run

```shell
cargo run -- \
  --eth-rpc-url $ETH_RPC_URL \
  --validator-address 0xdC26B08F0335b68721F64001C38b05D0BC9B539d \
  --chain-address 0x32400084C286CF3E17e7B677ea9583e60a000324 \
  --signers 0xaaaa...,0xbbbb...,0xcccc... \
  --network-name era-mainnet \
  --min-balance-eth 0.05 \
  --slack-webhook-url https://hooks.slack.com/services/XXX/YYY/ZZZ
```

All flags can be set via env vars instead (see `.env.example`); copy it to `.env` and the binary
loads it automatically. Telegram is configured with `--telegram-bot-token` + `--telegram-chat-id`.
If no channel is configured, alerts are only logged.

### Naming signers / pinging people on Slack

Each `--signers` entry may carry a label as `label=0xADDRESS`, shown in every alert about that
signer so you can tell which operator is affected:

```
--signers '@alice=0xaaaa...,@bob=0xbbbb...,carol <@U0C4R0L>=0xcccc...'
```

Make the label a **Slack mention** to actively ping that person when their signer alerts (low
balance, de-registered, or gone silent):

- `@alice` — pings via Slack's `link_names` (enabled automatically on the webhook post).
- `<@U012ABC>` — a Slack **user ID**, which always renders as a mention. Find it in Slack: profile
  → ⋯ → *Copy member ID*. More reliable than `@handle` if display names are ambiguous.

Bare `0xADDRESS` entries (no label) still work and just show the address. Network-wide alerts
(execution stalled, below threshold) aren't signer-specific; to ping a group/channel on those,
include `<!here>` or `<!subteam^ID>` in a future global-mention option — ask if you want that wired.

## Run with Docker Compose

The bundled `docker-compose.yml` runs the monitor as a single always-restart service, in the same
style as the per-network 2FA EN setups. It reads secrets/RPC from a `.env` file next to the compose
file:

```bash
cp .env.example .env
# edit .env: set ETH_RPC_URL, SIGNERS, and SLACK_WEBHOOK_URL (and/or TELEGRAM_*)
docker compose up -d
docker compose logs -f
```

The compose file pins the recommended mainnet values (validator/chain addresses, balance and stall
thresholds). By default it builds the image from this repo; to use the published image instead,
comment out `build: .` and uncomment the `image:` line.

### Adding it to an existing per-network EN compose

If you already run a per-network `docker-compose.yml` (external-node + postgres + en-2fa), drop this
service alongside the others. It needs no DB/EN access — only the L1 RPC:

```yaml
  en-2fa-monitor:
    image: ghcr.io/vladb-ai/en-2fa-monitor:93a4c40
    restart: always
    environment:
      ETH_RPC_URL: $ETH_RPC_URL
      VALIDATOR_ADDRESS: "0xdC26B08F0335b68721F64001C38b05D0BC9B539d"
      CHAIN_ADDRESS: "0x32400084C286CF3E17e7B677ea9583e60a000324"
      SIGNERS: $SIGNERS                       # comma-separated signer addresses
      NETWORK_NAME: "era-mainnet"
      MIN_BALANCE_ETH: "0.1"
      EXEC_LAG_ALERT: "1"
      STALL_SECS: "10800"
      POLL_INTERVAL_SECS: "120"
      ALERT_COOLDOWN_SECS: "3600"
      HEARTBEAT_SECS: "86400"
      SLACK_WEBHOOK_URL: $SLACK_WEBHOOK_URL
```

### Image tags

Images are pinned to immutable tags only — there is no `latest`. A GitHub Actions workflow
publishes `ghcr.io/vladb-ai/en-2fa-monitor:<short-commit-sha>` on every push (e.g. `:4e93a9b`), and
`:<tag>` on `v*` releases. To upgrade, bump the tag in your compose to the build you want; find
available tags under the repo's **Packages**, or use the short SHA of the commit you want to run.

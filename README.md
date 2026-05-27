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

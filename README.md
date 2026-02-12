# marmot-interop-lab-rust

> [!WARNING]
> Alpha software. This project was largely vibe-coded and likely contains privacy and security flaws. Do not use it for sensitive or production workloads.

Phased plan for a Rust-based Marmot interop harness.

## OpenClaw Setup Guide

Use Marmot as an [OpenClaw](https://openclaw.dev) channel plugin so your AI agent can send and receive messages over Nostr MLS groups.

### Prerequisites

- **OpenClaw** installed and running (`openclaw onboard`)
- **Rust toolchain** (to build `marmotd`)
- **A Nostr keypair** in hex format (secret key + public key)

### 1. Clone and build marmotd

```bash
git clone https://github.com/justinmoon/openclaw-marmot
cd openclaw-marmot/marmotd
cargo build --release
# binary at target/release/marmotd
```

### 2. Create a state directory and identity file

```bash
mkdir -p ~/.openclaw/.marmot-state
```

Create `~/.openclaw/.marmot-state/identity.json`:

```json
{
  "secret_key_hex": "<your-hex-secret-key>",
  "public_key_hex": "<your-hex-public-key>"
}
```

```bash
chmod 600 ~/.openclaw/.marmot-state/identity.json
```

> **⚠️ Important:** You must include **both** `secret_key_hex` and `public_key_hex`. Omitting the public key causes a silent sidecar crash.

If you skip this step entirely, `marmotd` will generate a random identity on first run.

### 3. Configure OpenClaw

Add the plugin path and channel config to `~/.openclaw/openclaw.json`:

```json
{
  "plugins": {
    "load": {
      "paths": ["<path-to>/openclaw-marmot/openclaw/extensions/marmot"]
    }
  },
  "channels": {
    "marmot": {
      "relays": ["wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net"],
      "sidecarCmd": "<path-to>/openclaw-marmot/marmotd/target/release/marmotd",
      "stateDir": "~/.openclaw/.marmot-state",
      "autoAcceptWelcomes": true,
      "groupPolicy": "open",
      "groupAllowFrom": ["<hex-pubkey-of-allowed-sender>"]
    }
  }
}
```

Replace `<path-to>` with the absolute path to your clone and `<hex-pubkey-of-allowed-sender>` with the Nostr public key(s) you want to accept messages from.

### 4. Restart OpenClaw gateway

```bash
openclaw gateway restart
```

### 5. Verify

```bash
openclaw status
```

You should see: `Marmot | ON | OK | configured`

### 6. Connect from a client

Use [Pika](https://pika.team) or another Marmot-compatible client to create a group and invite the bot's pubkey. With `autoAcceptWelcomes: true`, the bot joins automatically and starts responding.

### Gotchas

- **`identity.json` needs both fields** — omitting `public_key_hex` causes a silent sidecar crash with no useful error.
- **Relay loading** — the sidecar starts with only the first relay; the rest are added via `setRelays` after startup.
- **`groupPolicy: "allowlist"`** requires explicit group IDs in the `groups` config. Use `"open"` with `groupAllowFrom` if you just want sender-level filtering.
- **Duplicate sidecars** — multiple rapid gateway restarts can spawn duplicate sidecar processes fighting over the SQLite state. Kill extras manually if this happens.
- **Plugin ID warning** — the plugin manifest uses id `"marmot"` but the config entry hints `"openclaw-marmot"`. This warning is harmless.

---

## Phase Tests

- Phase 1: `PLAN.md` (Rust <-> Rust over local Docker relay)
- Phase 2: `OPENCLAW-INTEGRATION-PLAN.md` (Rust harness <-> deterministic Rust bot process)
- Phase 3: `OPENCLAW-CHANNEL-DESIGN.md` + `rust_harness daemon` (JSONL sidecar integration surface)
- Phase 4: Local OpenClaw gateway E2E: Rust harness <-> OpenClaw `marmot` channel (Rust sidecar spawned by OpenClaw)

### Run Phase 1

```sh
./scripts/phase1.sh
```

Defaults:
- Relay URL: random free localhost port (discovered via `docker compose port`)
- State dir: `.state/` (reset each run by the script)

### Run Phase 2

```sh
./scripts/phase2.sh
```

### Run Phase 3 (Daemon JSONL Smoke)

```sh
./scripts/phase3.sh
```

### Run Phase 4 (OpenClaw Marmot Plugin E2E)

This uses the pinned OpenClaw checkout under `./openclaw/`, runs a local relay on a random port,
starts OpenClaw gateway with the `marmot` plugin enabled, then runs a strict Rust harness invite+reply
scenario against the plugin's pubkey.

```sh
./scripts/phase4_openclaw_marmot.sh
```

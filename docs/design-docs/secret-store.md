# Secret Store

Encrypted credential storage with two secret categories: system secrets (internal, never exposed) and tool secrets (passed to worker subprocesses as env vars).

**Hard dependency:** Environment sanitization (sandbox-hardening.md, Phase 2) must ship before or alongside this. Without `--clearenv` in sandbox wrapping, system secrets and other env vars leak to workers. The master key itself is protected by the OS credential store (see "Master Key Storage" below) and is never in the process environment.

## Current State

All secrets currently live in config.toml as plaintext:

**config.toml** — the vast majority of users (all hosted, most self-hosted) set up API keys through the dashboard's provider UI. The dashboard sends the key to `PUT /api/providers`, which writes the literal value directly into config.toml (`anthropic_key = "sk-ant-abc123..."`). The `env:` prefix (`anthropic_key = "env:ANTHROPIC_API_KEY"`) exists as a mechanism but is only used in the initial boot script template and by a small number of self-hosted users who configure env vars manually. In practice, nearly every instance has plaintext API keys in config.toml on the persistent volume.

This file is accessible via `GET /api/config/raw` in the dashboard and via `cat /data/config.toml` through the shell tool when sandbox is off. Users have leaked keys by screensharing their config page.

**Environment variables** on a live hosted instance are all non-sensitive infrastructure vars:

```
# Fly metadata (non-sensitive)
FLY_APP_NAME=sb-0759a0a6
FLY_REGION=iad
FLY_VM_MEMORY_MB=8192
FLY_IMAGE_REF=registry.fly.io/spacebot-image:v0.2.1

# Spacebot config (non-sensitive)
SPACEBOT_DEPLOYMENT=hosted
SPACEBOT_DIR=/data

# Standard process vars
HOME=/root
PATH=/data/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
PWD=/data/agents/spacedrive-discord/workspace

# Browser config
CHROME_FLAGS=--no-sandbox --disable-dev-shm-usage --disable-gpu
```

API keys are NOT in the environment today — they're in config.toml. The secret store keeps it that way for system secrets and only exposes tool secrets as env vars.

**The existing secret store** (`src/secrets/store.rs`) implements AES-256-GCM encrypted key-value storage on redb with a `DecryptedSecret` wrapper that redacts in Debug/Display. It exists, is tested, but has zero callers in production.

## Problems

1. **Config is toxic to display.** The dashboard shows config.toml which contains literal API keys for nearly every user. Users have leaked keys by opening their config in screenshares or screenshots.

2. **Workers can read all secrets.** The config file is on disk at a known path (`/data/config.toml`). With sandbox off, `cat /data/config.toml` via the shell tool dumps every key. With sandbox on, the file is read-only but still readable.

3. **Agents can't safely inspect their own config.** The file tool blocks `/data/config.toml` (outside workspace), but workers have shell/exec which bypass that trivially. If keys were not in the config, agents could freely read it — useful for self-diagnosis ("what model am I configured to use?", "which messaging adapters are enabled?").

4. **No separation between internal and external secrets.** LLM API keys (needed only by the Rust process internally) and tool credentials (needed by CLI tools workers invoke) are stored and handled identically. There's no reason a worker subprocess should ever see `ANTHROPIC_API_KEY`.

5. **Prompt injection risk.** A malicious message in a Discord channel could attempt to convince the agent to read and output the config file. The leak detection hook catches known API key patterns, but if the key format isn't in the pattern list, it goes through.

## Design

### Two Secret Categories

The category controls **subprocess exposure**, not internal access. All secrets in the store are readable by Rust code via `SecretsStore::get()` regardless of category. The category answers one question: **should this value be injected as an env var into worker subprocesses?**

**System secrets** — not exposed to subprocesses. Rust code reads them from the store for internal use (LLM clients, messaging adapters, webhook integrations). Workers never see these as env vars.

Examples:

- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`
- `DISCORD_BOT_TOKEN`
- `TELEGRAM_BOT_TOKEN`
- Slack tokens, webhook signing secrets

**Tool secrets** — exposed to subprocesses as env vars. Rust code can also read them from the store if needed (e.g., a `GITHUB_TOKEN` used by both a Rust webhook integration and by `gh` CLI in workers). `wrap()` injects these via `--setenv` (bubblewrap) or `Command::env()` (passthrough).

Examples:

- `GH_TOKEN` / `GITHUB_TOKEN`
- `NPM_TOKEN`
- `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`
- `DOCKER_TOKEN`
- Any user-configured credential their agent needs at the shell level

Tool is a superset of system in terms of access — tool secrets are readable by Rust code AND visible to workers. System secrets are readable by Rust code only. If a credential is needed by both Rust internals and CLI tools, make it a tool secret. There's no "both" category because tool already implies both.

### Category Assignment

The dashboard's secrets panel exposes the category when adding or editing a secret. Known keys are auto-categorized:

| Pattern                                   | Category       | Rationale                                                 |
| ----------------------------------------- | -------------- | --------------------------------------------------------- |
| `*_API_KEY` matching known LLM providers  | System         | Only LlmManager needs these                               |
| `DISCORD_BOT_TOKEN`, `TELEGRAM_BOT_TOKEN` | System         | Only MessagingManager needs these                         |
| `SLACK_*_TOKEN`, `SLACK_SIGNING_SECRET`   | System         | Only Slack adapter needs these                            |
| `GH_TOKEN`, `GITHUB_TOKEN`                | Tool           | `gh` CLI expects this                                     |
| `NPM_TOKEN`                               | Tool           | `npm` expects this                                        |
| `AWS_*`                                   | Tool           | AWS CLI expects these                                     |
| Everything else                           | Tool (default) | User-configured credentials are most likely for CLI tools |

Users can override the auto-categorization. If someone wants `GH_TOKEN` as a system secret (e.g., used only by a webhook integration, not by workers), they can set it.

### Master Key Storage

The master encryption key is stored in the **OS credential store** — macOS Keychain or Linux kernel keyring. It never exists as an environment variable or a file readable by workers. This is the only approach that definitively protects the key from LLM-driven workers regardless of sandbox state.

#### Why Not an Env Var or File?

Both are readable by workers when sandbox is off:

- **Env var:** `std::env::remove_var()` removes the variable from libc's environ list, but on Linux `/proc/self/environ` is an immutable kernel snapshot from exec time. A worker running `cat /proc/<pid>/environ | strings | grep MASTER` retrieves the key even after removal. On macOS, `ps eww <pid>` can show environment variables.
- **File on disk:** Without sandbox, the worker runs as the same user and can `cat /data/.master_key` or any other path. File permissions don't help — same user, same access.

With sandbox on, both can be protected (bubblewrap's `--unshare-pid` hides `/proc`, and the key file can be excluded from bind mounts). But the secret store should be secure regardless of sandbox state — sandbox protects the workspace, the OS credential store protects the master key. Independent layers.

#### macOS: Keychain

- Store via `SecItemAdd` / retrieve via `SecItemCopyMatching` using the Security framework.
- Access is controlled by the calling binary's code signature and ACL. A `bash` subprocess spawned by ShellTool is a different binary — Keychain will not grant access without explicit user authorization.
- Even `security find-generic-password` from a worker fails because the Keychain item's access list only includes the Spacebot binary.
- The `security-framework` crate provides safe Rust bindings.

**Keychain item:**
```
Service:  "sh.spacebot.master-key"
Account:  "<instance_id>" (or "default" for self-hosted single-instance)
Data:     <32 random bytes>
Access:   Spacebot binary only (kSecAttrAccessibleAfterFirstUnlock)
```

#### Linux: Kernel Keyring (`keyctl`)

- Store via `add_key("user", "spacebot_master", key_bytes, session_keyring)` — the key lives in kernel memory, not on any filesystem.
- Scoped to a **session keyring**. Spacebot creates a new session keyring on startup via `keyctl_join_session_keyring()`. Workers are spawned with a fresh empty session keyring via `pre_exec` — they cannot access the parent's keyring.
- No file to `cat`, no env var to read, no `/proc` exposure. The key is only accessible via the `keyctl` syscall with the correct keyring ID, which workers don't have.
- Works without sandbox, without root. The kernel enforces access control at the syscall level.

**Worker isolation (pre_exec):**
```rust
// Before exec'ing the worker subprocess:
unsafe {
    command.pre_exec(|| {
        // Give the child a new empty session keyring.
        // It cannot access the parent's session keyring.
        libc::syscall(libc::SYS_keyctl, 0x01 /* KEYCTL_JOIN_SESSION_KEYRING */, std::ptr::null::<libc::c_char>());
        Ok(())
    });
}
```

This is additive to `--clearenv` — env sanitization strips env vars, the session keyring swap strips keyring access. Both run regardless of sandbox state.

#### Hosted Deployment

The platform generates a per-instance master key on provisioning and stores it in the platform database (tied to the user's account). On instance startup, the platform writes the key to a **tmpfs file** that Spacebot reads once, then the platform deletes it. Spacebot stores the key in the Linux kernel keyring immediately and never persists it to the volume.

Flow:
1. Platform provisions instance → generates 32-byte random key → stores in platform DB (`instances.master_key`, encrypted at rest).
2. Platform starts Fly machine → writes key to `/run/spacebot/master_key` (tmpfs, 0600, root-only).
3. Spacebot startup → reads `/run/spacebot/master_key` → stores in kernel keyring → deletes the tmpfs file.
4. Key is now only in kernel memory. Volume compromise doesn't expose it. `/run` is tmpfs, wiped on restart.
5. On next restart, the platform re-injects the key via the same tmpfs mechanism.

Properties:
- The key persists across restarts and rollouts — the platform always re-injects it from its database.
- The key is tied to the user's platform account, not to the volume. If the volume is compromised without platform access, the `secrets.redb` file is useless.
- The key is manageable in the dashboard (the platform can rotate it, the user can view/reset it through their account settings).
- The platform database becomes a store of master keys, but it already stores auth tokens and Stripe credentials — same protection requirements.

#### Self-Hosted Deployment

Self-hosted users opt into the secret store through the dashboard:

1. User clicks "Enable Secret Manager" in the embedded dashboard.
2. Spacebot generates a 32-byte random master key.
3. The key is stored directly in the OS credential store (Keychain on macOS, kernel keyring on Linux).
4. On macOS, the key persists in Keychain across restarts — no user action needed.
5. On Linux, the kernel keyring is cleared on reboot. Spacebot writes a **key file** at `{data_dir}/.master_key` (0600) as durable backup. On startup, it reads the file, loads into the kernel keyring, and operates from the keyring thereafter. The file is only readable by the Spacebot user, and with sandbox on, the file path is excluded from bind mounts so workers can't access it. Without sandbox, the file is theoretically readable — but the worker would need to know the exact path and the LLM would need to be instructed to look for it. This is a known trade-off for unsandboxed Linux; the recommendation is to enable sandbox.
6. Docker users: the key file lives on the persistent volume. `docker inspect` doesn't expose it (it's not an env var). A volume mount compromise exposes both `secrets.redb` and `.master_key` — but that's equivalent to the current state where `config.toml` has plaintext keys on the same volume.

**Startup flow (all deployments):**

1. Check OS credential store for master key (Keychain / kernel keyring).
2. If not found, check `{data_dir}/.master_key` file → load into OS credential store.
3. If not found, check tmpfs injection path `/run/spacebot/master_key` (hosted) → load into OS credential store → delete tmpfs file.
4. If still not found, secret store is unavailable. Config resolution falls back to `env:` and literal values. Dashboard shows onboarding prompt.
5. Derive AES-256-GCM cipher key from master key via Argon2id (one-time ~100ms at startup).
6. Open `SecretsStore` (redb), decrypt all secrets.
7. **System secrets:** passed directly to Rust components (`LlmManager`, `MessagingManager`, etc.). Never set as env vars.
8. **Tool secrets:** held in a `HashMap<String, String>` for `Sandbox::wrap()` injection via `--setenv`.

#### Key Derivation

Argon2id rather than the current SHA-256 in `build_cipher()`. For hosted instances where the platform generates a random high-entropy key, SHA-256 would be fine — but self-hosted users may use a passphrase (future: dashboard "set your own key" option), and SHA-256 of a passphrase is trivially brutable. Argon2id handles both cases correctly (memory-hard, resistant to GPU/ASIC attacks) and the cost is a one-time ~100ms at startup. No reason to ship the weak path and upgrade later.

#### Upgrade Path

The secret store activation is **user-initiated**, not automatic:

1. **New version ships.** Nothing changes for anyone by default. No env vars to set, no files to create.
2. **Hosted users:** On the next platform rollout, the platform generates master keys for all existing instances and injects them via the tmpfs mechanism. On first boot with the new image, auto-migration runs: literal keys in config.toml → secret store, config.toml rewritten with `secret:` references. No user action required.
3. **Self-hosted users:** Dashboard shows an onboarding prompt: "Enable Secret Manager." User clicks it → key generated → stored in OS credential store. Dashboard then shows detected env vars that look like secrets and offers to migrate them. Until the user opts in, everything works as before.

On startup, Spacebot scans the environment for variables matching known secret patterns (anything with `TOKEN`, `KEY`, `SECRET`, `PASSWORD` in the name). If found and no master key is configured and `passthrough_env` doesn't list them, a prominent warning is logged: "Detected secrets in environment variables. Enable the secret manager to protect these credentials." The warning is informational — nothing breaks, nothing is stripped.

### Config Resolution Prefixes

Config values support three resolution modes via prefix:

```toml
# Literal — plaintext value inline (current default, what migration replaces)
anthropic_key = "sk-ant-abc123..."

# env: — read from system environment variable at resolve time
anthropic_key = "env:ANTHROPIC_API_KEY"

# secret: — read from encrypted secret store (decrypted at boot, held in memory)
anthropic_key = "secret:ANTHROPIC_API_KEY"
```

The `secret:` prefix is a resolution directive: "this value lives in the encrypted `SecretsStore`, look it up by this name, return the decrypted value." The config doesn't know or care whether the secret is categorized as system or tool — it just gets the resolved string. The category is metadata on the secret in the store and only matters at `wrap()` time when deciding what to inject into worker subprocesses.

**Secret names use UPPER_SNAKE_CASE matching env var convention.** The secret name in the store IS the env var name — `GH_TOKEN`, not `gh_token`. For tool secrets, `wrap()` iterates the store and does `--setenv {name} {value}` with no translation. For system secrets, the name is just an identifier (they're never env vars), but using the same convention keeps everything consistent. Skills that say "set `GH_TOKEN`" map directly to a secret named `GH_TOKEN` in the dashboard.

### Config Key Migration

All provider keys and sensitive tokens move from config.toml to the secret store. Config.toml changes from:

```toml
[llm]
anthropic_key = "sk-ant-abc123..."

[messaging.discord]
token = "env:DISCORD_BOT_TOKEN"
```

To:

```toml
[llm]
anthropic_key = "secret:ANTHROPIC_API_KEY"

[messaging.discord]
token = "secret:DISCORD_BOT_TOKEN"
```

The `resolve_env_value()` function (`config.rs:2974`) is extended to handle the `secret:` prefix:

```rust
fn resolve_secret_or_env(value: &str, secrets: &SecretsStore) -> Option<String> {
    if let Some(alias) = value.strip_prefix("secret:") {
        secrets.get(alias).ok().map(|s| s.expose().to_string())
    } else if let Some(var_name) = value.strip_prefix("env:") {
        std::env::var(var_name).ok()
    } else {
        Some(value.to_string()) // literal
    }
}
```

The resolved values are consumed by Rust code (provider constructors, adapter init). They are never set as env vars. The `secret:` prefix is the config-level reference; the category (system vs tool) determines runtime behavior.

**Migration path:**

1. On startup, if `SPACEBOT_MASTER_KEY` is present and config.toml contains literal key values (not `env:` or `secret:` prefixed), auto-migrate: encrypt each literal value into the secret store under a deterministic UPPER_SNAKE_CASE name (e.g., `anthropic_key` → `secret:ANTHROPIC_API_KEY`), with auto-detected category.
2. Rewrite config.toml in place to replace literal values with `secret:` references.
3. Log every migration step. If migration fails for any key, leave the original value in config.toml and warn.
4. For `env:` prefixed values, leave them as-is. They're already not storing the secret in the config. Users who want to migrate `env:` values to the secret store can do so explicitly via the dashboard.
5. The `env:` prefix continues to work for users who prefer env-var-based key management.
6. **Hosted migration:** The platform sets `SPACEBOT_MASTER_KEY` on existing instances before the image update that introduces the secret store. On first boot with the new image, migration runs automatically. No user action required.
7. **Self-hosted migration:** Users who set `SPACEBOT_MASTER_KEY` get automatic migration. Users who don't keep the existing behavior (literal/env values in config.toml, no secret store).

### Dashboard Changes

- **Provider setup** writes `secret:` references by default. The "API Key" field in the provider UI is a password input that sends the value to the API, which stores it in the secret store (as a system secret) and writes `secret:provider_name` to config.toml.
- **Raw config view** (`GET /api/config/raw`) is safe to display since config.toml only contains aliases.
- **Secrets panel** — list all secrets with name, category (system/tool), and masked value. Add/remove/rotate. Category is editable. Never displays plaintext values (shows masked `***` with a copy button that copies from a short-lived in-memory decryption).
- **Secret store status** — indicator showing whether the store is unlocked (master key present) or locked (missing). For hosted instances, master key is always present (platform-managed).

### Env Sanitization Integration

The sandbox `wrap()` function (see sandbox-hardening.md, Section 2) handles the env var injection:

1. `--clearenv` strips everything from the subprocess.
2. Re-add safe vars: `PATH` (with tools/bin), `HOME`, `USER`, `LANG`, `TERM`.
3. Re-add **tool secrets only** — iterate the secret store's tool category, `--setenv` each into the subprocess.
4. System secrets are **never** injected. `SPACEBOT_MASTER_KEY` is **never** injected.

The `Sandbox` struct needs access to the tool secrets. Options:

- `Sandbox` holds an `Arc<ArcSwap<HashMap<String, String>>>` of tool env vars, updated when secrets change.
- The `SecretsStore` exposes a `tool_env_vars() -> HashMap<String, String>` method, and `Sandbox` holds an `Arc<SecretsStore>`.

Either way, `wrap()` reads the current tool secrets on each call and injects them. This is cheap — the set changes rarely (only when the user adds/removes secrets via the dashboard).

### Worker Secret Awareness

Workers get the **names** of available tool secrets injected into their system prompt — never the values. This tells the worker what credentials are available without it having to run `printenv` to discover them:

```
Available tool secrets (set as environment variables in your shell):
  GH_TOKEN, NPM_TOKEN, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY

Commands that use these credentials will work automatically (e.g., gh commands
use GH_TOKEN). Do not echo, print, or log secret values.
```

This is assembled at worker construction time from the tool secret names in the store. The list updates when secrets change (workers spawned after a secret is added/removed get the updated list).

**Why this matters:**
- Without it, the worker has to guess or run `printenv` to find out what's available. That wastes a turn and the `printenv` output contains the actual values in the worker's own context (even though the scrubber would catch them before they reach the channel).
- With it, the worker knows `GH_TOKEN` is available and can use `gh` commands immediately. No discovery step, no secret values in context.
- Skills that say "requires `GH_TOKEN`" align directly — the worker sees the name in its prompt and knows the credential is present.
- The "do not print secret values" instruction is a soft guardrail. The real guardrail is the output scrubber. But telling the LLM not to do it reduces the frequency, which means less scrubbing and less noise in logs.

### Output Scrubbing (Tool Secret Redaction)

Workers need tool secrets in their subprocess environment to run CLI tools. But there's no reason the secret _values_ should propagate back up to channels or branches. A worker running `gh pr create` needs `GH_TOKEN` in its env, but the channel receiving the worker's result doesn't need to see the token value if it leaks into stdout.

**Mechanism:** Every string that flows from a worker back toward a channel or branch is checked against the current set of tool secret values (exact substring match). Any match is replaced with `[REDACTED:<secret_name>]`.

```
Worker stdout: "Authenticated as user X. Token: ghp_abc123def456..."
Scrubber: checks against all tool_env_vars() values
Match found: "ghp_abc123def456..." == tool secret "GH_TOKEN"
Redacted:    "Authenticated as user X. Token: [REDACTED:GH_TOKEN]"
```

**Where it runs:**

| Output path              | Scrubbing point                                                    |
| ------------------------ | ------------------------------------------------------------------ |
| Worker result text       | Before injection into channel/branch history                       |
| `set_status` tool output | Before adding to the status block (channels read this every turn)  |
| OpenCode SSE events      | Before forwarding to the worker event handler                      |
| Branch conclusions       | Before injection into channel history (branches can spawn workers) |

**Why exact match, not regex:** Leak detection (SpacebotHook) already does regex pattern matching for known key formats (`sk-ant-*`, `ghp_*`, etc.). The scrubber is complementary — it catches secrets that don't have recognizable patterns. A random 64-char string the user stored as a tool secret has no regex pattern, but the scrubber knows its exact value and catches it. The two layers work together:

- **Leak detection (regex):** catches known formats even if the secret isn't in the store (e.g., a key the user typed inline). Reactive — kills the agent after detection.
- **Output scrubbing (exact match):** catches any stored tool secret regardless of format. Proactive — redacts before the value reaches the channel. The channel sees `[REDACTED:GH_TOKEN]` and knows the secret was used, but never sees the value.

**Cost:** Comparing every worker output string against every tool secret value. With typically <20 secrets and output in the KB range, this is a substring search over a small set — negligible. The secret values are already in memory (the tool env var cache). No decryption on each check.

**Implementation:** A `scrub_secrets(text: &str, tool_secrets: &HashMap<String, String>) -> String` function that iterates the map and replaces all occurrences. Called in the worker result path, status update path, and OpenCode event forwarding path. The function lives alongside the existing leak detection code — either in `src/hooks/spacebot.rs` or extracted into a shared `src/secrets/scrub.rs`.

### Protection Layers (Summary)

| Layer                                                  | What It Protects Against                                                       |
| ------------------------------------------------------ | ------------------------------------------------------------------------------ |
| Secret store encryption (AES-256-GCM)                  | Disk access to secrets.redb (stolen volume, backup leak)                       |
| Master key in OS credential store (Keychain / kernel keyring) | Worker access to the encryption key — OS enforces access control at the binary/keyring level, independent of sandbox state |
| `secret:` aliases in config.toml                       | Config file exposure (screenshare, `cat`, dashboard display)                   |
| System/tool category separation                        | Workers seeing LLM API keys, messaging tokens, or other internal credentials   |
| `DecryptedSecret` wrapper                              | Accidental logging of secret values in tracing output                          |
| Env sanitization (`--clearenv` + selective `--setenv`) | Workers only get tool secrets, never system secrets or internal vars |
| Worker session keyring isolation (`pre_exec`)          | Workers accessing parent's kernel keyring on Linux (additive to `--clearenv`)  |
| Worker secret name injection (prompt) | Workers know what credentials are available without running `printenv` — no secret values in LLM context |
| Output scrubbing (exact match) | Tool secret values propagating from worker output back to channels/branches |
| Leak detection (SpacebotHook, regex)                   | Last-resort safety net — known key format patterns in any tool output          |

### What This Doesn't Solve

- **Workers can see tool secrets in their own process.** A worker running `printenv GH_TOKEN` gets the value in its subprocess. This is by design — the worker needs it to run `gh` commands. But the value is scrubbed from the worker's output before it reaches the channel.
- **Side channels within the workspace.** A worker could write a tool secret to a file, then a subsequent worker reads it. The sandbox limits where files can be written, but within the workspace it's unrestricted. Output scrubbing catches the value if it appears in any worker's output, but not if it stays in a file.
- **Encoding/obfuscation.** A worker could base64-encode a secret value before outputting it. The exact-match scrubber wouldn't catch the encoded form. Leak detection's regex patterns also wouldn't match. This is a theoretical attack by a deliberately adversarial LLM, not an accidental leak.

The key properties: **system secrets never leave Rust memory.** Tool secrets reach worker subprocesses (necessary) but are scrubbed from all output flowing back to channels (the LLM context). A channel never sees a secret value — it sees `[REDACTED:GH_TOKEN]` at most.

## Files Changed

| File                                  | Change                                                                                                                                                              |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/secrets/store.rs`                | Add secret category (system/tool); add `tool_env_vars()` method; add `tool_secret_names()` for prompt injection; replace SHA-256 key derivation with Argon2id       |
| `src/secrets/scrub.rs`                | New: `scrub_secrets()` — exact-match redaction of tool secret values in output strings                                                                              |
| `src/config.rs`                       | Extend `resolve_env_value()` to handle `secret:` prefix; wire `SecretsStore` into config loading; migration logic for existing literal/env keys                     |
| `src/sandbox.rs`                      | `wrap()` injects tool secrets via `--setenv`; holds reference to secret store or tool env var cache                                                                 |
| `src/agent/worker.rs`                 | Scrub worker result text through `scrub_secrets()` before injecting into channel/branch history; inject tool secret names into worker system prompt                  |
| `src/tools/set_status.rs`             | Scrub status text through `scrub_secrets()` before updating status block                                                                                            |
| `src/opencode/worker.rs`              | Scrub OpenCode SSE output events through `scrub_secrets()` before forwarding                                                                                        |
| `src/api/secrets.rs`                  | New: secret CRUD for dashboard with category field, secret store status endpoint                                                                                    |
| `src/api/server.rs`                   | Add secret management routes                                                                                                                                        |
| `src/secrets/keystore.rs`             | New: OS credential store abstraction — `KeyStore` trait with `MacOSKeyStore` (Security framework / Keychain) and `LinuxKeyStore` (kernel keyring / keyctl) backends  |
| `src/main.rs`                         | Load master key from OS credential store (or file fallback → keyring), derive cipher key, initialize `SecretsStore`, decrypt system secrets for provider init, run migration if needed |
| `src/agent/worker.rs`                 | Add `pre_exec` hook to spawn workers with a fresh empty session keyring (Linux) |
| `spacebot-platform/api/src/fly.rs`    | Generate per-instance master key on provisioning, store in platform DB, inject via tmpfs at `/run/spacebot/master_key` in `machine_config()`                        |
| `spacebot-platform/api/src/db.rs`     | Add `master_key` column to instances table (encrypted at rest)                                                                                                      |
| `spacebot-platform/api/src/routes.rs` | Add master key rotation endpoint for dashboard                                                                                                                      |

## Phase Plan

**Hard dependency on sandbox-hardening.md Phase 2 (env sanitization).** Without `--clearenv`, the master key and system secrets are exposed via the process environment.

### Phase 1: Core Integration

1. Implement `KeyStore` abstraction with macOS Keychain and Linux kernel keyring backends (`src/secrets/keystore.rs`).
2. Startup: load master key from OS credential store (file fallback on Linux → load into keyring), derive cipher key via Argon2id, initialize `SecretsStore`.
3. Add `pre_exec` hook to worker spawning: children get a fresh empty session keyring (Linux).
4. Platform: generate per-instance master key on provisioning, store in platform DB, inject via tmpfs at `/run/spacebot/master_key`.
5. Extend `resolve_env_value()` to handle `secret:` prefix.
6. System secrets: pass decrypted values directly to `LlmManager`, `MessagingManager`, etc. during init. Never set as env vars.
7. Tool secrets: expose via `tool_env_vars()` for `Sandbox::wrap()` injection.
8. Inject tool secret names (not values) into worker system prompts at construction time.
9. Add secret store status endpoint (locked/unlocked).
10. Self-hosted onboarding: dashboard "Enable Secret Manager" button generates key → stores in OS credential store → writes file backup (Linux).

### Phase 2: Output Scrubbing

1. Implement `scrub_secrets()` — exact substring match against all tool secret values, replace with `[REDACTED:<name>]`.
2. Wire into worker result path (before channel/branch history injection).
3. Wire into `set_status` (before status block update).
4. Wire into OpenCode SSE event forwarding.
5. Wire into branch conclusion path (before channel history injection).
6. Verify: worker running `echo $GH_TOKEN` produces `[REDACTED:GH_TOKEN]` in the channel's view of the result.

### Phase 3: Migration

1. **Hosted:** Platform generates master keys for all existing instances, stores in platform DB. On next image rollout, keys are injected via tmpfs. Auto-migration runs on first boot: literal keys in config.toml → secret store, config.toml rewritten with `secret:` references. No user action required.
2. **Self-hosted:** Dashboard shows onboarding prompt with detected secret-looking env vars. User-initiated — nothing changes until user clicks "Enable Secret Manager."
3. Startup env scan: detect vars matching `*TOKEN*`, `*KEY*`, `*SECRET*`, `*PASSWORD*` patterns. If found and no master key configured, log a prominent warning nudging the user to enable the secret manager.
4. Verify: config.toml contains no plaintext keys; `GET /api/config/raw` is safe to display.

### Phase 4: Dashboard

1. Secrets panel: list secrets with name, category (system/tool), masked value. Add/remove/rotate.
2. Provider setup UI writes `secret:` references and stores as system secrets.
3. Secret store status indicator (locked/unlocked).
4. Hosted: master key rotation via platform API.

## Open Questions

1. **Secret rotation and hot-reload.** When a user rotates a key (e.g., regenerates their Anthropic API key), the workflow is: update via dashboard → store encrypts new value → config references unchanged. But the LLM manager holds the old key in memory. Do we need a reload hook that re-reads system secrets from the store? Tool secrets are re-read on each `wrap()` call so they update automatically.
2. **Migration rollback.** After migrating keys from config.toml to the secret store, if the secret store becomes corrupted or the master key is lost, is there a recovery path? Should we keep a one-time encrypted backup of the pre-migration config?
3. **Platform master key storage.** The platform database will store per-instance master keys. What's the encryption/protection model for the platform database itself? Should the platform encrypt master keys at rest with its own key?
4. **Category override UX.** How prominent should the system/tool toggle be in the dashboard? Auto-categorization handles the common cases, but users need to understand the distinction to make informed overrides.
5. ~~**Self-hosted tool secrets without master key.**~~ **Resolved — see `passthrough_env` below.**
6. **Linux key file exposure without sandbox.** On Linux, the `.master_key` file fallback is readable by workers when sandbox is off (same user, same filesystem). The kernel keyring protects the in-memory key, but the durable file is a weaker link. Mitigation: run Spacebot as a separate user from workers (requires process isolation work), or accept this as a known limitation and recommend sandbox. The keyring itself is always protected regardless.
7. **Keychain ACL on unsigned dev builds.** During development, the Spacebot binary may not be code-signed. macOS Keychain ACLs based on code signature won't work for unsigned binaries — need to handle this gracefully (fall back to file-based storage in dev mode, or use a less restrictive Keychain access policy).

### Env Passthrough for Self-Hosted (No Master Key)

Without `SPACEBOT_MASTER_KEY`, the secret store is unavailable. But env sanitization (`--clearenv`) still runs — it has to, or the master key protection is opt-in and useless. This means self-hosted users who set `GH_TOKEN` in their Docker compose lose it silently after env sanitization ships. That's a breaking change.

**Fix:** A configurable passthrough list in the sandbox config:

```toml
[agents.sandbox]
mode = "enabled"
passthrough_env = ["GH_TOKEN", "GITHUB_TOKEN", "NPM_TOKEN"]
```

`wrap()` builds the subprocess environment from three sources, checked in order:

1. **Safe vars** — always passed: `PATH` (with tools/bin), `HOME`, `USER`, `LANG`, `TERM`.
2. **Tool secrets from the store** — if the secret store is available (master key set), all tool-category secrets are injected via `--setenv`.
3. **`passthrough_env` list** — for each name in the list, if the var exists in the parent process environment, pass it through to the subprocess. This is the escape hatch for self-hosted users without a master key.

When the secret store is available, `passthrough_env` is redundant — everything should be in the store. The config field still works (it's additive), but the dashboard can show a hint: "You have passthrough env vars configured. Consider moving these to the secret store for encrypted storage."

On hosted instances, `passthrough_env` is empty by default and has no effect — the platform manages all secrets via the store.

**Why not just skip `--clearenv` when there's no master key?** Because `--clearenv` protects more than just the master key — it prevents system secrets, internal vars (`SPACEBOT_*`), and any other env vars from leaking to workers. The master key is protected by the OS credential store regardless of `--clearenv`, but env sanitization is still necessary for everything else. The passthrough list is explicit — the user declares exactly which vars they want forwarded. Everything else is stripped.

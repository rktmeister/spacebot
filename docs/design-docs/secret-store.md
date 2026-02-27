# Secret Store

Encrypted credential storage, opaque secret references for agents, and config key migration.

**Hard dependency:** Environment sanitization (sandbox-hardening.md, Phase 2) must ship before or alongside this. Without `--clearenv` in sandbox wrapping, the master key is readable from `/proc/self/environ` and the entire encryption model is meaningless. See "Master Key — Critical Dependency" below.

## Current State

All secrets currently live in config.toml as plaintext:

**config.toml** — the vast majority of users (all hosted, most self-hosted) set up API keys through the dashboard's provider UI. The dashboard sends the key to `PUT /api/providers`, which writes the literal value directly into config.toml (`anthropic_key = "sk-ant-abc123..."`). The `env:` prefix (`anthropic_key = "env:ANTHROPIC_API_KEY"`) exists as a mechanism but is only used in the initial boot script template and by a small number of self-hosted users who configure env vars manually. In practice, nearly every instance has plaintext API keys in config.toml on the persistent volume.

This file is accessible via `GET /api/config/raw` in the dashboard and via `cat /data/config.toml` through the shell tool when sandbox is off. Users have leaked keys by screensharing their config page.

**Environment variables** — the parent spacebot process inherits env vars from the container (Fly machine env, Docker env). Sandbox does NOT call `env_clear()` (`sandbox.rs` bubblewrap wrapping uses `--setenv PATH` but does not use `--clearenv`). Workers inherit the full environment. Even where keys are configured via `env:` prefix, the resolved values are in the process environment and accessible via `printenv`. Leak detection in SpacebotHook catches known key patterns in tool output and kills the agent, but that's reactive — the key was already in the tool output buffer.

**The existing secret store** (`src/secrets/store.rs`) implements AES-256-GCM encrypted key-value storage on redb with a `DecryptedSecret` wrapper that redacts in Debug/Display. It exists, is tested, but has zero callers in production. It's a roadmap item.

## Problems

1. **Config is toxic to display.** The dashboard shows config.toml which contains literal API keys for nearly every user. Users have leaked keys by opening their config in screenshares or screenshots.

2. **Workers can read all secrets.** The config file is on disk at a known path (`/data/config.toml`). With sandbox off, `cat /data/config.toml` via the shell tool dumps every key. With sandbox on, the file is read-only but still readable. Shell/exec tools also inherit the full parent environment, so `printenv` exposes any env-based keys.

3. **Agents can't safely inspect their own config.** The file tool blocks `/data/config.toml` (outside workspace), but workers have shell/exec which bypass that trivially. If keys were not in the config, agents could freely read it — which is useful for self-diagnosis ("what model am I configured to use?", "which messaging adapters are enabled?").

4. **No secure storage for user secrets.** If a user asks the bot to store a GitHub token or deploy key, there's nowhere safe to put it. The agent could write it to a file in workspace, but that's plaintext on disk and readable by anyone with file access.

5. **Prompt injection risk.** A malicious message in a Discord channel could attempt to convince the agent to read and output the config file. The leak detection hook catches known API key patterns, but if the key format isn't in the pattern list, it goes through.

## Design

### Master Key

The master encryption key is a `SPACEBOT_MASTER_KEY` environment variable, controlled externally and never accessible to agents.

**Hosted:** The platform generates a per-instance master key on provisioning and stores it in the platform database (tied to the user's account). The platform injects it as a Fly machine env var alongside `SPACEBOT_DIR` and `SPACEBOT_DEPLOYMENT` (`fly.rs:422-427` — same mechanism, just one more env var). This means:

- The key persists across instance restarts, rollouts, and machine recreations — the platform always re-injects it.
- The key is tied to the user's platform account, not to the volume. If the volume is compromised without platform access, the secrets.redb file is useless.
- The key is manageable in the dashboard (the platform can rotate it, the user can view/reset it through their account settings on the dashboard, which talks to the platform API).
- The platform database becomes a store of master keys, but it already stores auth tokens and Stripe credentials — it's already a high-value target with the same protection requirements.

**Self-hosted:** Users set `SPACEBOT_MASTER_KEY` in their Docker compose, systemd unit, or shell environment. Same as how they'd set any other secret env var. If not set, the secret store is unavailable and config falls back to existing `env:`/literal resolution (backward compatible).

**Startup flow:**

1. Spacebot reads `SPACEBOT_MASTER_KEY` from the environment.
2. Derives the AES-256-GCM cipher key via `build_cipher()` (SHA-256 hash of the raw key bytes).
3. **Clears the env var from the process environment** (`std::env::remove_var("SPACEBOT_MASTER_KEY")`) before initializing any agents or spawning any subprocesses.
4. Holds only the derived cipher key in memory for the lifetime of the process.
5. If the env var is absent, the `SecretsStore` is initialized in a disabled/read-only mode. Config resolution falls back to `env:` and literal values. A warning is logged. The dashboard shows a prompt to configure the master key.

#### Critical Dependency: `remove_var` Is Not Sufficient

`std::env::remove_var` removes the variable from libc's environ list, but on Linux `/proc/self/environ` is a kernel snapshot of the initial process environment taken at exec time. It is **immutable for the lifetime of the process.** A worker running `cat /proc/self/environ | strings | grep MASTER` retrieves the key even after `remove_var`.

This means:

- **Sandbox off:** The master key is trivially readable via `/proc/self/environ` or `/proc/1/environ` (the spacebot process is typically PID 1 in containers). `remove_var` provides zero protection.
- **Sandbox on (current):** Bubblewrap mounts a fresh `/proc` (`--proc /proc`) which gives the sandboxed process its own PID namespace, so `/proc/self/environ` shows the subprocess's environment, not the parent's. But today bubblewrap does NOT use `--clearenv`, so the parent's env vars (including the master key if still present at spawn time) are inherited into the sandbox. The master key would still be visible via `printenv` inside the sandbox.
- **Sandbox on with env sanitization:** Bubblewrap uses `--clearenv` and only passes through safe vars (PATH, HOME, LANG). The subprocess cannot see the master key via `printenv`, `env`, or `/proc/self/environ` (fresh procfs + clean env). This is the only configuration that actually protects the master key.

**Therefore: env sanitization (sandbox-hardening.md, Phase 2) is a hard prerequisite.** The secret store must not ship before sandbox + `--clearenv` is enforced, otherwise the master key is exposed and the entire encryption model is theater.

On macOS, `/proc` doesn't exist so the `/proc/self/environ` attack doesn't apply, but `ps eww <pid>` can show environment variables of running processes. Env sanitization via `Command::env_clear()` is still required.

**Dashboard interaction:** The dashboard doesn't interact with the master key directly for day-to-day operations. It sends secret values (API keys, user tokens) to the spacebot API, which encrypts them using the in-memory cipher key. The master key itself is only managed at the platform level (hosted) or by the user's deployment config (self-hosted). The dashboard can show whether the secret store is unlocked (master key present) or locked (missing), and for hosted instances, it can trigger the platform to rotate the master key.

Future improvement: upgrade key derivation from SHA-256 (current `build_cipher()`) to Argon2id for stronger protection if the master key is a human-chosen passphrase rather than random bytes.

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
anthropic_key = "secret:anthropic_api_key"

[messaging.discord]
token = "secret:discord_bot_token"
```

The `resolve_env_value()` function (`config.rs:2974`) is extended to handle the `secret:` prefix:

```rust
fn resolve_secret_or_env(value: &str, secrets: &SecretsStore, master_key: &[u8]) -> Option<String> {
    if let Some(alias) = value.strip_prefix("secret:") {
        secrets.get(alias, master_key).ok().map(|s| s.expose().to_string())
    } else if let Some(var_name) = value.strip_prefix("env:") {
        std::env::var(var_name).ok()
    } else {
        Some(value.to_string())
    }
}
```

**Migration path:**

1. On startup, if `SPACEBOT_MASTER_KEY` is present and config.toml contains literal key values (not `env:` or `secret:` prefixed), auto-migrate: encrypt each literal value into the secret store under a deterministic alias (e.g., `anthropic_key` → `secret:llm.anthropic_key`).
2. Rewrite config.toml in place to replace literal values with `secret:` references.
3. Log every migration step. If migration fails for any key, leave the original value in config.toml and warn.
4. For `env:` prefixed values, leave them as-is. They're already not storing the secret in the config. Users who want to migrate `env:` values to the secret store can do so explicitly via the dashboard.
5. The `env:` prefix continues to work for users who prefer env-var-based key management. The dashboard's provider setup UI writes `secret:` references by default when the secret store is available.
6. **Hosted migration:** The platform sets `SPACEBOT_MASTER_KEY` on existing instances before the image update that introduces the secret store. On first boot with the new image, migration runs automatically. No user action required.
7. **Self-hosted migration:** Users who set `SPACEBOT_MASTER_KEY` get automatic migration. Users who don't keep the existing behavior (literal/env values in config.toml, no secret store).

### Opaque Secret References for Agents

Agents interact with secrets via aliases, never seeing plaintext values.

**System secrets** (API keys, messaging tokens) are resolved internally by the spacebot process when constructing LLM clients, messaging adapters, etc. The LLM never sees these — they're consumed programmatically. No change in behavior here, just a change in where the value is stored.

**User secrets** (GitHub tokens, deploy keys, credentials the user asks the bot to store) are managed via two new tools:

**`secret_save`** — stores a secret under a named alias.

```
secret_save(name: "github_token", value: "ghp_abc123...")
-> "Secret 'github_token' stored. Use ${{secrets.github_token}} to reference it in commands."
```

The tool accepts the plaintext value (the LLM already has it at this point — the user provided it), encrypts it, and returns only the alias. From this point on, the LLM works with the alias.

**`secret_inject`** — makes a secret available as an environment variable in a subsequent shell/exec command without the LLM seeing the value.

```
secret_inject(names: ["github_token"], command: "gh auth login --with-token <<< $GITHUB_TOKEN")
-> shell output (without the token value in the output)
```

Implementation: the system looks up the secret, decrypts it, and injects it as an env var into the subprocess. The LLM sees the command template and the output, but never the secret value itself. The tool is a thin wrapper around `shell`/`exec` that adds secret env vars to the subprocess.

**`secret_list`** — lists stored secret aliases (names only, no values).

**`secret_delete`** — removes a stored secret by alias.

There is no `secret_get` tool. The LLM cannot retrieve the plaintext value of a stored secret. It can only use it via `secret_inject`.

### Dashboard Changes

- **Provider setup** writes `secret:` references by default. The "API Key" field in the provider UI is a password input that sends the value to the API, which stores it in the secret store and writes `secret:provider_name` to config.toml.
- **Raw config view** (`GET /api/config/raw`) is safe to display since config.toml only contains aliases.
- **Secret management panel** — list aliases, add/remove secrets, rotate values. Never displays plaintext values (only shows masked `***` with a copy button that copies from a short-lived in-memory decryption).
- **Master password prompt** — shown on first use and after restart until the master password is provided.

### Protection Layers (Summary)

| Layer | What It Protects Against |
|-------|--------------------------|
| Secret store encryption (AES-256-GCM) | Disk access to secrets.redb (stolen volume, backup leak) |
| Master key via env var (cleared on startup) | Unauthorized decryption — key is external to the instance (platform DB or user's deployment config), never on the volume, never accessible to agents |
| `secret:` aliases in config.toml | Config file exposure (screenshare, `cat`, dashboard display) |
| `DecryptedSecret` wrapper | Accidental logging of secret values in tracing output |
| Env sanitization (`env_clear()`) | Workers running `printenv` / `env` to discover keys |
| No `secret_get` tool | LLM prompt injection attempting to read stored secrets |
| `secret_inject` tool | Controlled injection without LLM observing the value |
| Leak detection (SpacebotHook) | Last-resort safety net if a secret leaks into tool output |

### What This Doesn't Solve

- **The LLM sees the secret when the user first provides it.** If a user says "store this token: ghp_abc123", the LLM has the plaintext in its context window. `secret_save` encrypts it for future use, but the current conversation context already contains it. Compaction will eventually summarize it away, but there's a window.
- **`secret_inject` is bypassable by a creative LLM.** A worker could construct a command like `echo $GITHUB_TOKEN` and the value would appear in tool output. Leak detection catches known patterns, but custom secrets without recognizable prefixes could slip through.
- **Side channels.** A worker could write a secret to a file, then read it in a subsequent command. The sandbox (when on) limits where files can be written, but within the workspace it's unrestricted.

These are inherent limits of running untrusted code with access to secrets. The design minimizes the attack surface and makes accidental exposure much harder, but doesn't claim to be a perfect isolation boundary against a determined adversary with tool access.

## Files Changed

| File | Change |
|------|--------|
| `src/secrets/store.rs` | Add master key derivation upgrade path (Argon2id); add per-agent namespacing for user secrets vs system secrets |
| `src/config.rs` | Extend `resolve_env_value()` to handle `secret:` prefix; wire `SecretsStore` into config loading; migration logic for existing literal/env keys |
| `src/tools/secret_save.rs` | New tool: encrypt and store a secret by alias |
| `src/tools/secret_inject.rs` | New tool: run shell/exec with secret env vars injected |
| `src/tools/secret_list.rs` | New tool: list secret aliases |
| `src/tools/secret_delete.rs` | New tool: delete a stored secret |
| `src/tools.rs` | Register secret tools for workers |
| `src/api/secrets.rs` | New: secret CRUD for dashboard, secret store status endpoint |
| `src/api/server.rs` | Add secret management routes |
| `src/main.rs` | Read `SPACEBOT_MASTER_KEY` from env, derive cipher key, clear env var, initialize `SecretsStore`, run migration if needed, wire into config loading and `AgentDeps` |
| `spacebot-platform/api/src/fly.rs` | Generate per-instance master key on provisioning, store in platform DB, inject as `SPACEBOT_MASTER_KEY` env var in `machine_config()` |
| `spacebot-platform/api/src/db.rs` | Add `master_key` column to instances table (encrypted at rest) |
| `spacebot-platform/api/src/routes.rs` | Add master key rotation endpoint for dashboard |

## Phase Plan

**Hard dependency on sandbox-hardening.md Phase 2 (env sanitization).** Without `--clearenv`, the master key is exposed via `/proc/self/environ`.

### Phase 1: Core Integration

1. Platform: generate per-instance master key on provisioning, store in platform DB, inject as `SPACEBOT_MASTER_KEY` in Fly machine env.
2. Startup: read `SPACEBOT_MASTER_KEY`, derive cipher key, `remove_var`, initialize `SecretsStore`.
3. Extend `resolve_env_value()` to handle `secret:` prefix.
4. Update dashboard provider UI to write `secret:` references when secret store is available.
5. Add secret store status endpoint (locked/unlocked).

### Phase 2: Migration

1. Auto-migrate existing literal keys in config.toml to secret store on startup.
2. Rewrite config.toml with `secret:` references.
3. Platform: set `SPACEBOT_MASTER_KEY` on existing instances before image update.
4. Verify: config.toml contains no plaintext keys; `GET /api/config/raw` is safe to display.

### Phase 3: Agent Tools

1. Add `secret_save`, `secret_inject`, `secret_list`, `secret_delete` tools.
2. Register tools for workers.
3. Verify: `secret_inject` injects env vars without LLM seeing values; `cat /proc/self/environ` inside a sandboxed worker does not contain `SPACEBOT_MASTER_KEY`.

### Phase 4: Dashboard

1. Secret management panel (list aliases, add/remove, rotate).
2. Master key status indicator (locked/unlocked).
3. Hosted: master key rotation via platform API.

## Open Questions

1. **Secret rotation.** When a user rotates a key (e.g., regenerates their Anthropic API key), the workflow is: update the secret in the dashboard, the alias stays the same, all config references continue working. But do we need to handle the case where the old key is still cached in memory by the LLM manager?
2. **OpenCode leak detection.** OpenCode worker output is not scanned by SpacebotHook. Should we add post-processing on OpenCode SSE events to scan for leaked secrets?
3. **Migration rollback.** After migrating keys from config.toml to the secret store, if the secret store becomes corrupted or the master key is lost, is there a recovery path? Should we keep a one-time encrypted backup of the pre-migration config?
4. **Platform master key storage.** The platform database will store per-instance master keys. What's the encryption/protection model for the platform database itself? Should the platform encrypt master keys at rest with its own key?

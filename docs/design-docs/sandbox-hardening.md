# Sandbox Hardening

Hosted enforcement, dynamic sandbox mode, hot-reload fix, capability manager, and policy enforcement for shell and OpenCode workers.

## 0. Hosted Sandbox Enforcement (Missing)

### Problem

Sandbox enforcement on hosted deployments was never implemented. The original sandbox design doc (`docs/design-docs/sandbox.md:127`) states: "When `SPACEBOT_DEPLOYMENT=hosted`, the platform boot script forces `mode = "enabled"` regardless of user config." This enforcement does not exist anywhere in the codebase.

A hosted instance was confirmed running completely unsandboxed — a worker successfully wrote files to `/etc/spacebot_test` and `/tmp/spacebot_test` as root with exit code 0.

### Evidence

Searched every location where enforcement could exist:

1. **`docker-entrypoint.sh`** generates `config.toml` from env vars on first boot. It never writes a `[agents.sandbox]` section. Agents get `SandboxConfig::default()` (`mode: Enabled`) initially, but this is not enforced after first boot.

2. **`config.rs:1116`** — `AgentConfig::resolve()` does `self.sandbox.clone().unwrap_or_default()`. The default is `mode: Enabled`, but if the user has set `mode = "disabled"` in the TOML (via the UI or direct edit), that value is used as-is. No hosted override.

3. **`api/config.rs:372`** — The `update_agent_config` handler writes whatever sandbox config the UI sends. No check for `SPACEBOT_DEPLOYMENT=hosted`. A user can disable sandbox mode via the UI at any time.

4. **`SPACEBOT_DEPLOYMENT` env var** is set to `"hosted"` by the platform (`spacebot-platform/api/src/fly.rs:424`) but the spacebot binary only checks it for API bind address override (`config.rs:2374`) and agent limits (`api/agents.rs:15`). No sandbox-related code reads it.

5. **`Sandbox::new()` (`sandbox.rs:79`)** — If `config.mode == SandboxMode::Disabled`, backend detection is skipped entirely and `SandboxBackend::None` is used. No hosted override here either.

### Root Cause

The enforcement was specified in the design but never implemented. There is no code path that forces sandbox mode to `Enabled` on hosted deployments. If a user disables the sandbox via the UI (which writes `mode = "disabled"` to `config.toml`), it stays disabled permanently.

### Fix

Enforcement at three points:

**1. Config resolution (`config.rs`, `AgentConfig::resolve()`).**
Add a helper `enforce_hosted_sandbox()` in `sandbox.rs` that checks `SPACEBOT_DEPLOYMENT=hosted` and forces `mode = Enabled` if it's `Disabled`. Call it during resolve so every config load (startup, hot-reload, file watcher) applies the override.

**2. API update handler (`api/config.rs`, `update_agent_config()`).**
Before writing the sandbox section, check if this is a hosted deployment. If the request tries to set `mode = "disabled"`, return `403 Forbidden` with a message explaining sandbox cannot be disabled on hosted deployments.

**3. UI.**
When `SPACEBOT_DEPLOYMENT=hosted` (exposed via an existing status/health API endpoint), the sandbox mode dropdown should be disabled with a tooltip explaining it's always enforced on hosted instances.

### Helpers

```rust
// sandbox.rs

pub fn is_hosted_deployment() -> bool {
    std::env::var("SPACEBOT_DEPLOYMENT")
        .ok()
        .is_some_and(|v| v.eq_ignore_ascii_case("hosted"))
}

pub fn enforce_hosted_sandbox(config: &mut SandboxConfig) {
    if is_hosted_deployment() && config.mode == SandboxMode::Disabled {
        tracing::warn!(
            "sandbox mode forced to enabled — \
             sandbox cannot be disabled on hosted deployments"
        );
        config.mode = SandboxMode::Enabled;
    }
}
```

### Files Changed

| File | Change |
|------|--------|
| `src/sandbox.rs` | Add `is_hosted_deployment()` and `enforce_hosted_sandbox()` |
| `src/config.rs` | Call `enforce_hosted_sandbox()` in `AgentConfig::resolve()` after sandbox config is resolved |
| `src/api/config.rs` | Reject sandbox disable on hosted in `update_agent_config()` |

---

## 1. Dynamic Sandbox Mode (Hot-Reload Fix)

### Problem

Disabling the sandbox on an agent via the UI doesn't work. The setting visually reverts to "enabled" and the actual sandbox enforcement doesn't change. The config file on disk is written correctly, but the in-memory state is never updated.

### Root Cause

Three failures in the reload path:

1. **`reload_config()` skips sandbox.** `config.rs:5012-5014` has an explicit comment: "sandbox config is not hot-reloaded here because the Sandbox instance is constructed once at startup and shared via Arc. Changing sandbox settings requires an agent restart." Every other config field gets `.store(Arc::new(...))` in `reload_config()`, but `self.sandbox` is skipped.

2. **API returns stale data.** `get_agent_config()` reads from `rc.sandbox.load()` (`api/config.rs:232`), which still holds the startup value. The UI receives this stale response and resets the toggle.

3. **`Sandbox` struct stores mode as a plain field.** The `Sandbox` instance (`sandbox.rs:60-68`) captures `mode: SandboxMode` at construction. Even if the `RuntimeConfig.sandbox` ArcSwap were updated, the `Arc<Sandbox>` in `AgentDeps` would still enforce the old mode in `wrap()`.

### Sequence Diagram

```
UI: PUT /agents/config {sandbox: {mode: "disabled"}}
  -> api/config.rs writes mode="disabled" to config.toml      (correct)
  -> api/config.rs calls rc.reload_config()                    (skips sandbox)
  -> api/config.rs calls get_agent_config()                    (reads stale ArcSwap)
  -> returns {sandbox: {mode: "enabled"}}                      (wrong)
UI: displays "enabled"                                         (reverted)

~2s later:
  file watcher detects config.toml change
  -> calls reload_config() for all agents                      (skips sandbox again)
  -> all agents log "runtime config reloaded"                  (sandbox unchanged)
```

### Fix

#### Change 1: Update `RuntimeConfig.sandbox` in `reload_config()` (config.rs ~line 5011)

Add the sandbox store alongside the other fields. Remove the skip comment.

```rust
self.warmup.store(Arc::new(resolved.warmup));
self.sandbox.store(Arc::new(resolved.sandbox.clone()));

mcp_manager.reconcile(&old_mcp, &new_mcp).await;
```

This fixes the API response so `get_agent_config()` returns the correct value after a config change.

#### Change 2: Wrap `RuntimeConfig.sandbox` in `Arc` (config.rs:4920)

Change the field from `ArcSwap<SandboxConfig>` to `Arc<ArcSwap<SandboxConfig>>` so it can be shared with the `Sandbox` struct:

```rust
// Before
pub sandbox: ArcSwap<SandboxConfig>,

// After
pub sandbox: Arc<ArcSwap<SandboxConfig>>,
```

Update `RuntimeConfig::new()` accordingly:

```rust
// Before
sandbox: ArcSwap::from_pointee(agent_config.sandbox.clone()),

// After
sandbox: Arc::new(ArcSwap::from_pointee(agent_config.sandbox.clone())),
```

All existing `.load()` and `.store()` calls work through `Arc`'s `Deref` with no changes.

#### Change 3: Make `Sandbox` read mode dynamically (sandbox.rs)

Replace the `mode: SandboxMode` field with `config: Arc<ArcSwap<SandboxConfig>>`. Always detect the backend at startup (even when mode is initially Disabled), so we know what's available if the user later enables it.

```rust
pub struct Sandbox {
    config: Arc<ArcSwap<SandboxConfig>>,
    workspace: PathBuf,
    data_dir: PathBuf,
    tools_bin: PathBuf,
    backend: SandboxBackend,
}
```

Change `Sandbox::new()` signature to accept the shared ArcSwap:

```rust
pub async fn new(
    config: Arc<ArcSwap<SandboxConfig>>,
    workspace: PathBuf,
    instance_dir: &Path,
    data_dir: PathBuf,
) -> Self
```

Backend detection always runs. The initial mode only affects the startup log message.

In `wrap()`, read the current mode dynamically:

```rust
pub fn wrap(&self, program: &str, args: &[&str], working_dir: &Path) -> Command {
    let config = self.config.load();
    // ...
    if config.mode == SandboxMode::Disabled {
        return self.wrap_passthrough(program, args, working_dir, &path_env);
    }
    match self.backend {
        SandboxBackend::Bubblewrap { proc_supported } => { ... }
        SandboxBackend::SandboxExec => { ... }
        SandboxBackend::None => self.wrap_passthrough(program, args, working_dir, &path_env),
    }
}
```

The `writable_paths` field is removed from the struct. Paths are read from the ArcSwap config and canonicalized in `wrap()`. This is a cheap syscall and commands aren't spawned at rates where it matters.

#### Change 4: Pass the shared ArcSwap to `Sandbox::new()` at both construction sites

**main.rs ~line 1358:**

```rust
let sandbox = std::sync::Arc::new(
    spacebot::sandbox::Sandbox::new(
        runtime_config.sandbox.clone(),  // Arc<ArcSwap<SandboxConfig>>
        agent_config.workspace.clone(),
        &config.instance_dir,
        agent_config.data_dir.clone(),
    )
    .await,
);
```

**api/agents.rs ~line 665:** Same change.

### What Doesn't Change

- `AgentDeps.sandbox` stays as `Arc<Sandbox>` (lib.rs:214). The `Sandbox` itself now reads mode dynamically, so the `Arc<Sandbox>` reference doesn't need to be swapped.
- `ShellTool` and `ExecTool` continue holding `Arc<Sandbox>` and calling `.wrap()`. No changes needed.
- The bubblewrap and sandbox-exec wrapping logic is unchanged. Only the dispatch in `wrap()` reads the dynamic mode.

### Files Changed

| File | Change |
|------|--------|
| `src/config.rs` | `RuntimeConfig.sandbox` type to `Arc<ArcSwap<SandboxConfig>>`; `RuntimeConfig::new()` wraps in `Arc::new()`; `reload_config()` adds `self.sandbox.store()` and removes skip comment |
| `src/sandbox.rs` | `Sandbox.mode` field replaced with `config: Arc<ArcSwap<SandboxConfig>>`; `writable_paths` removed from struct, read dynamically; `Sandbox::new()` signature change; `wrap()` reads mode from ArcSwap; backend detection always runs |
| `src/main.rs` | Pass `runtime_config.sandbox.clone()` to `Sandbox::new()` |
| `src/api/agents.rs` | Same `Sandbox::new()` signature update |

---

## 2. Environment Sanitization

### Problem

Sandbox does NOT call `env_clear()`. Bubblewrap wrapping uses `--setenv PATH` but does not use `--clearenv`. Workers inherit the full parent environment. A worker can run `printenv ANTHROPIC_API_KEY` and get the raw key. Even `remove_var` on startup doesn't help because Linux `/proc/self/environ` is an immutable kernel snapshot from exec time.

MCP processes already do this correctly (`mcp.rs:309` calls `env_clear()`).

### Design

Sandbox `wrap()` must call `env_clear()` (or the bwrap equivalent `--clearenv`) and explicitly pass through only:

- `PATH` (with tools/bin prepended, as today)
- `HOME`, `USER`, `LANG`, `TERM` (basic process operation)
- `TMPDIR` (if needed)

For the passthrough (no sandbox) case: same env sanitization should apply in the shell/exec tools directly via `Command::env_clear()` before `Command::env()` for the allowed vars.

This is also a **hard prerequisite for the secret store** — see `docs/design-docs/secret-store.md`. Without `--clearenv`, the master key is readable from `/proc/self/environ` and the entire encryption model is meaningless.

### Files Changed

| File | Change |
|------|--------|
| `src/sandbox.rs` | Add `--clearenv` to bubblewrap wrapping; add `env_clear()` to sandbox-exec and passthrough modes; re-add only safe vars |
| `src/tools/shell.rs` | Env sanitization for passthrough (no sandbox) mode |
| `src/tools/exec.rs` | Same env sanitization |

---

## 3. Capability Manager

### Problem

Tool availability is implicit and fragile:

- Some binaries come from the container image (`curl`, `gh`, `bubblewrap`), some don't (`git`).
- Hosted users lose ad-hoc `apt-get` installs during rollouts because root filesystem changes aren't durable.
- Agents attempt package-manager installs from shell tools, creating inconsistent behavior across environments.
- The UI has no visibility into what tooling is available or missing.

### Findings From Hosted Incident

On a hosted instance (`SPACEBOT_DEPLOYMENT=hosted`):

- `git` was installed via `apt-get` and landed at `/usr/bin/git` (non-durable root filesystem).
- `git` was not installed in `/data/tools/bin` (durable persistent volume).
- Builtin worker shell/exec commands are sandbox-wrapped when a backend is available, but fall back to unsandboxed execution when backend detection fails.
- OpenCode workers auto-allow all permission prompts (`worker.rs:429-432`) including bash commands. They follow the same execution paths with no package-manager policy guard.

On hosted rollouts, machine image updates preserve `/data` but not ad-hoc root filesystem installs. Any binary at `/usr/bin` from `apt-get` disappears on update/recreate.

### Goals

1. Make tool availability explicit and inspectable.
2. Persist runtime-installed binaries across hosted rollouts.
3. Enforce a single install path (`/data/tools/bin`) instead of ad-hoc system package installs.
4. Provide an API surface the UI can render as a capabilities panel.
5. Keep implementation incremental.

### Non-Goals

- Full package manager replacement.
- Arbitrary third-party plugin execution model in v1.
- Supporting every OS package format in v1.

### Design

#### Durable Binary Location

`{instance_dir}/tools/bin` is the persistent binary directory. It already exists:

- Hosted boot flow creates it (`mkdir -p "$SPACEBOT_DIR/tools/bin"`).
- `Sandbox` already prepends `tools/bin` to `PATH` for worker subprocesses (`sandbox.rs:138-149`).
- `/data` survives hosted machine image rollouts.

This becomes the only supported runtime install target.

#### Capability States

Each capability reports one of:

| State | Meaning |
|-------|---------|
| `available` | Usable now |
| `installable` | Known capability, can be installed |
| `installing` | Active install in progress |
| `error` | Install attempted, failed |

Optional metadata per capability:

- `version` -- reported version string
- `path` -- resolved binary path
- `source` -- `system` (found in PATH outside tools/bin) or `managed` (in tools/bin)
- `last_error` -- last install failure message

#### Capability Registry (v1)

Static in-code registry with pinned artifacts and checksums:

```rust
pub struct CapabilitySpec {
    pub name: &'static str,
    pub version: &'static str,
    /// Download URL template. `{arch}` and `{os}` are substituted at runtime.
    pub url: &'static str,
    pub sha256: &'static str,
    pub binary_name: &'static str,
    /// How to extract the binary from the downloaded artifact.
    pub extract: ExtractMethod,
}

pub enum ExtractMethod {
    /// Binary is the download itself (no archive).
    None,
    /// tar.gz archive, binary at the given path inside the archive.
    TarGz { inner_path: &'static str },
    /// zip archive, binary at the given path inside the archive.
    Zip { inner_path: &'static str },
}
```

Initial entry: `git` (Linux x86_64). Future entries: `gh`, `jq`, `ripgrep`, browser runtime.

#### Install Flow

1. Acquire per-capability install lock (prevents races from concurrent install requests).
2. Download artifact to temp path under `{instance_dir}/tools/bin/.tmp/`.
3. Verify SHA-256 checksum (required, fails install if mismatch).
4. Extract if needed (tar/zip).
5. Set executable bits (`chmod +x`).
6. Atomic rename into `{instance_dir}/tools/bin/{binary_name}`.
7. Run `{binary_name} --version` probe to verify.
8. Publish updated capability status.

On failure: preserve previous working binary if one exists. Report error state with message.

#### Module Structure

New module at `src/capabilities.rs`:

```rust
pub struct CapabilityManager {
    tools_bin: PathBuf,
    specs: Vec<CapabilitySpec>,
    states: ArcSwap<Vec<CapabilityStatus>>,
    install_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

pub struct CapabilityStatus {
    pub name: String,
    pub state: CapabilityState,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub source: Option<CapabilitySource>,
    pub last_error: Option<String>,
}

pub enum CapabilityState {
    Available,
    Installable,
    Installing,
    Error,
}

pub enum CapabilitySource {
    System,
    Managed,
}
```

At startup, the manager probes for each registered capability:

1. Check `{tools_bin}/{binary_name}` -- if found, state is `Available`, source is `Managed`.
2. Check system PATH via `which {binary_name}` -- if found, state is `Available`, source is `System`.
3. Otherwise, state is `Installable`.

#### API Endpoints

**`GET /api/capabilities`**

Returns all known capabilities and their runtime status.

```json
{
  "capabilities": [
    {
      "name": "git",
      "state": "available",
      "version": "2.46.0",
      "path": "/data/tools/bin/git",
      "source": "managed",
      "last_error": null
    },
    {
      "name": "gh",
      "state": "installable",
      "version": null,
      "path": null,
      "source": null,
      "last_error": null
    }
  ]
}
```

**`POST /api/capabilities/{name}/install`**

Triggers installation for an installable capability.

| Response | Meaning |
|----------|---------|
| `202 Accepted` | Install started |
| `409 Conflict` | Install already in progress |
| `404 Not Found` | Unknown capability |
| `400 Bad Request` | Capability already available |

Returns a `CapabilityStatus` in the response body reflecting the new state (`installing` or `error` if it completed synchronously).

#### UI Surface

Add a Capabilities panel (likely under the existing Settings or a new Tools section):

- List capabilities with state badges (green/available, yellow/installable, red/error, spinner/installing).
- Show source indicator (`system` vs `managed`).
- Show version and path.
- Show last error with retry button.
- Install button for `installable` capabilities.

#### LLM Process Awareness

Channels and workers should know what capabilities are available so they don't waste turns attempting to use missing tools or trying to install them via package managers.

**Channels** get a short capability summary injected into their status block -- just the list of available tool names. This lets the channel answer questions like "can you use git?" without branching, and lets it inform the user or route to a worker appropriately. The channel doesn't need paths, versions, or install details.

```
Available tools: git, gh, ripgrep
Unavailable tools: jq (installable)
```

**Workers** get full capability details in their system prompt -- name, version, path, and source. Workers are the processes that actually invoke these binaries, so they need to know exact paths (especially when a tool is in `tools/bin` vs a system location) and version constraints. This also prevents workers from attempting to install missing tools via shell commands since the prompt explicitly states what's available and that package-manager installs are blocked.

The capability list is read from `CapabilityManager.states` (the `ArcSwap<Vec<CapabilityStatus>>`). Channels read a formatted summary via a helper on `CapabilityManager`. Workers receive the full `Vec<CapabilityStatus>` serialized into their system prompt context.

#### Wiring

`CapabilityManager` is created during startup alongside `Sandbox`:

- Stored in `ApiState` for API handlers.
- Also stored in `AgentDeps` (or accessible via a shared `Arc`) so channel status injection and worker prompt assembly can read capability state.
- Probes run once at startup, results cached in `ArcSwap<Vec<CapabilityStatus>>`.
- Install requests update the state atomically. Channels and workers pick up the new state on their next turn via the ArcSwap.

### Files Changed

| File | Change |
|------|--------|
| `src/capabilities.rs` | New module: `CapabilityManager`, `CapabilitySpec`, `CapabilityStatus`, registry, probe, install flow |
| `src/lib.rs` | Add `pub mod capabilities` |
| `src/api/capabilities.rs` | New: `get_capabilities`, `install_capability` handlers |
| `src/api/server.rs` | Add capability routes |
| `src/api/mod.rs` | Add `mod capabilities` |
| `src/main.rs` | Create `CapabilityManager` at startup, store in `ApiState` |

---

## 4. Shell Package-Manager Guard

### Problem

Agents can run `apt-get install`, `apk add`, etc. via the shell tool, installing binaries to non-durable root filesystem locations. On hosted instances, these installs disappear on rollout. Even on self-hosted, ad-hoc package installs create unreproducible environments.

### Design

Add a pre-execution check in `ShellTool::call()` that rejects commands containing package-manager invocations. This runs before sandbox wrapping -- it's a policy check, not a security boundary.

#### Blocked Tokens

Commands are rejected if they contain any of these as standalone command tokens (not as substrings of paths or arguments):

```
apt, apt-get, dpkg, apk, yum, dnf, pacman, brew, snap, pip, pip3, npm install -g, gem install
```

The check splits the command on shell operators (`|`, `&&`, `||`, `;`, newline) and checks if any segment starts with a blocked token.

#### Error Message

```
Package manager commands are not allowed. Binaries installed via apt/apk/etc
are not durable across hosted rollouts and create inconsistent environments.

Use the capabilities API to install supported tools, or request the tool be
added to the capability registry.
```

#### Self-Hosted Override

The guard is **always active on hosted** (`SPACEBOT_DEPLOYMENT=hosted`). On self-hosted instances, it can be disabled via config:

```toml
[agents.sandbox]
mode = "enabled"
allow_package_managers = false  # default: false
```

Setting `allow_package_managers = true` on a self-hosted instance disables the guard. The field is ignored when `SPACEBOT_DEPLOYMENT=hosted`.

#### Implementation

Add a `check_package_manager()` function in `tools/shell.rs`, called at the top of `ShellTool::call()` before `sandbox.wrap()`. Returns a descriptive `ShellError` if blocked.

This is deliberately simpler than the old `check_command()` that was removed during the sandbox implementation. The old checks were security-boundary string filtering (180+ lines). This is a single policy guard (~30 lines) that catches the most common footgun. It's not trying to be exhaustive -- the sandbox handles security.

### Files Changed

| File | Change |
|------|--------|
| `src/tools/shell.rs` | Add `check_package_manager()`, call before `wrap()` |
| `src/sandbox.rs` | Add `allow_package_managers: bool` to `SandboxConfig` (default false) |
| `src/config.rs` | Parse `allow_package_managers` in sandbox config |

---

## 5. OpenCode Permission Guard

### Problem

OpenCode workers auto-allow all permission prompts (`opencode/worker.rs:429-432`). When OpenCode asks permission to run a bash command, the worker replies `PermissionReply::Once` unconditionally. This means OpenCode workers can run `apt-get install` and any other command without the same policy checks that apply to builtin workers.

### Design

Before auto-allowing a permission request, inspect the patterns to check for package-manager commands. If a blocked pattern is detected, reply with `PermissionReply::Deny` (or equivalent) and log a warning.

#### Implementation

In `opencode/worker.rs`, in the `SseEvent::PermissionAsked` handler (line 403), before the auto-reply:

```rust
SseEvent::PermissionAsked(permission) => {
    // ... existing logging ...

    // Check if this is a bash permission with package-manager patterns
    if is_blocked_permission(&permission) {
        tracing::warn!(
            worker_id = %self.id,
            patterns = ?permission.patterns,
            "blocked package-manager command in OpenCode worker"
        );
        let guard = server.lock().await;
        let _ = guard.reply_permission(&permission.id, PermissionReply::Deny).await;
        return EventAction::Continue;
    }

    // Auto-allow
    let guard = server.lock().await;
    guard.reply_permission(&permission.id, PermissionReply::Once).await;
    // ...
}
```

The `is_blocked_permission()` function checks if the permission type is bash-related and if any pattern matches the same package-manager tokens used by the shell guard.

#### Shared Policy

Extract the package-manager token list into a shared constant or function (e.g., `src/policy.rs` or inline in `src/sandbox.rs`) so both the shell tool guard and the OpenCode permission guard use the same blocked list.

### Files Changed

| File | Change |
|------|--------|
| `src/opencode/worker.rs` | Add package-manager check before auto-allow in `PermissionAsked` handler |
| `src/policy.rs` (new) or `src/sandbox.rs` | Shared `is_package_manager_command()` function |

---

## Tool Protection Audit

### File Tool Workspace Guard

Workers get `ShellTool`, `FileTool`, and `ExecTool` in the same toolbox. The file tool's `resolve_path()` workspace guard (`file.rs:26-75`) is security theater when sandbox is off:

- Worker wants to read `/data/config.toml`
- `FileTool.resolve_path()` rejects it — "outside workspace"
- Worker uses `ShellTool` with `cat /data/config.toml` — works fine, no sandbox enforcement
- Or `ExecTool` with `cat` — same thing

The file tool's check only prevents the LLM from using that specific tool for out-of-workspace access. It doesn't prevent anything because shell and exec are right there in the same toolbox with no equivalent restriction when sandbox is off.

When sandbox **is** on, the file tool's check is also redundant in the other direction — bwrap/sandbox-exec already makes everything outside the workspace read-only at the kernel level. The file tool runs in-process (not a subprocess), so the sandbox doesn't wrap it, but the workspace guard duplicates what the sandbox already enforces for writes. For reads, the sandbox allows reading everywhere (read-only mounts), and the file tool is actually **more restrictive** than the sandbox by blocking reads outside workspace too.

The file tool workspace guard has exactly one scenario where it provides unique value: **sandbox is on, and you want to prevent the LLM from reading files outside the workspace via the file tool** (since the sandbox allows reads everywhere). That's a defense-in-depth argument, not a security boundary. It's worth keeping for that reason, but it should not be confused with actual containment.

### Protection Matrix

Current state of protection across all tool paths, with sandbox disabled:

| Tool | Workspace Guard | Sandbox Enforcement | Env Inherited | Leak Detection | Net Protection |
|------|----------------|--------------------|----|-----|----|
| `file` (read/write/list) | Yes — `resolve_path()` blocks outside workspace | No (in-process, not a subprocess) | N/A | Yes (tool output scanned) | Workspace guard only — bypassable via shell/exec in the same toolbox |
| `shell` | Working dir validation only | Sandbox wraps subprocess — but disabled | Full parent env | Yes (args + output scanned) | Leak detection only |
| `exec` | Working dir validation only | Sandbox wraps subprocess — but disabled | Full parent env | Yes (args + output scanned) | Leak detection + dangerous env var blocklist |
| `send_file` | **None** — any absolute path | No (in-process read) | N/A | Yes (output scanned) | Leak detection only |
| `browser` | N/A | N/A | N/A | Yes (output scanned) | SSRF protection (blocks metadata endpoints, private IPs) |
| OpenCode workers | Workspace-scoped by OpenCode | Not sandboxed | Full parent env via OpenCode subprocess | No (OpenCode output not scanned by SpacebotHook) | Auto-allow on all permissions |

### Key Observations

- The file tool's workspace guard is the only tool-level path restriction, but it's trivially bypassed via shell/exec which are in the same toolbox. It gives a false sense of containment.
- With sandbox off, the only real protection across all tools is leak detection (reactive, pattern-based, kills the agent after the fact).
- `send_file` has no workspace validation at all — can exfiltrate any readable file as a message attachment. This is an independent bug regardless of sandbox state.
- OpenCode workers bypass both sandbox and leak detection. They inherit the full environment and auto-allow all permission prompts.
- The file tool guard's only real value is as read-containment when sandbox is on (preventing LLM from reading sensitive files outside workspace via the file tool specifically, since bwrap mounts everything read-only but still readable).

---

## Phase Plan

### Phase 0: Hosted Sandbox Enforcement (Critical)

Fix the security gap on hosted instances. Changes to `sandbox.rs`, `config.rs`, `api/config.rs`.

1. Add `is_hosted_deployment()` and `enforce_hosted_sandbox()` helpers in `sandbox.rs`.
2. Call `enforce_hosted_sandbox()` in `AgentConfig::resolve()`.
3. Reject sandbox disable in `update_agent_config()` on hosted.
4. Verify: on a hosted instance, confirm sandbox mode cannot be set to `disabled` via API; confirm resolve always returns `Enabled`.

### Phase 1: Dynamic Sandbox Mode (Hot-Reload Fix)

Fix the user-facing bug where toggling sandbox mode via UI doesn't take effect. Changes to `config.rs`, `sandbox.rs`, `main.rs`, `api/agents.rs`.

1. Wrap `RuntimeConfig.sandbox` in `Arc`.
2. Add `self.sandbox.store()` to `reload_config()`.
3. Refactor `Sandbox` to read mode from `Arc<ArcSwap<SandboxConfig>>`.
4. Update both `Sandbox::new()` call sites.
5. Verify: change sandbox mode via API, confirm `GET /agents/config` returns the new value, confirm `wrap()` uses the new mode.

### Phase 2: Environment Sanitization

Prevent secret leakage through environment variable inheritance.

1. Add `--clearenv` to bubblewrap wrapping, re-add only PATH and safe vars.
2. Add `env_clear()` to sandbox-exec and passthrough wrapping modes.
3. Add `env_clear()` to shell/exec tools for the no-sandbox case.
4. Verify: worker running `printenv` shows only PATH/HOME/LANG, not API keys.

### Phase 3: Policy Enforcement

Guard against non-durable package-manager installs.

1. Add shell package-manager guard in `tools/shell.rs`.
2. Add OpenCode permission guard in `opencode/worker.rs`.
3. Extract shared blocked-token list.
4. Add `allow_package_managers` config option.
5. Verify: `apt-get install git` via shell tool returns policy error; OpenCode bash permission for `apt-get` is denied.

### Phase 4: Capability Manager

Make tool availability explicit and persistent.

1. Add `capabilities.rs` module with static registry and probe logic.
2. Add capability API endpoints.
3. Wire into startup and `ApiState`.
4. Add UI capabilities panel.
5. Add `git` as first managed capability with pinned artifact.
6. Verify: `GET /api/capabilities` returns correct state; `POST /api/capabilities/git/install` downloads to tools/bin and survives simulated rollout.

### Phase 5: Additional Capabilities

Expand the registry.

1. Add `gh`, `jq`, `ripgrep` specs.
2. Add per-capability health probes (periodic version check).
3. Add telemetry for install attempts and failures.

### Phase 6: Browser Capability

Replace the slim/full image split.

1. Model browser runtime as a capability with its own install lifecycle.
2. Install browser bundle to persistent data path.
3. Add browser-specific readiness checks.
4. Evaluate deprecating slim/full split.

---

## Open Questions

1. **Artifact hosting.** Where do we host pinned, checksummed binary artifacts? GitHub releases? S3? A dedicated CDN?
2. **Self-hosted install opt-out.** Should self-hosted instances be able to disable runtime installs entirely?
3. **Version cleanup.** What's the quota/cleanup policy for old capability versions in tools/bin?
4. **Admin gating.** Should capability installs require admin role on hosted instances?
5. **OpenCode deny semantics.** Does `PermissionReply::Deny` exist in the OpenCode protocol, or do we need `PermissionReply::Once` with a modified command that prints the policy error? Need to check the OpenCode permission reply spec.
6. **Dynamic writable_paths.** The hot-reload fix reads `writable_paths` from the ArcSwap on every `wrap()` call, canonicalizing each time. If an agent has many writable paths and spawns commands at high frequency, this could be optimized with a change-detection cache. Likely not a concern in practice.

---

## Future: True Sandboxing (VM Isolation via stereOS)

Everything above operates at the namespace level — bubblewrap restricts mounts, `--clearenv` strips the environment, policy guards block specific commands. These are necessary fixes but they share a fundamental limitation: the agent and the host share a kernel. A compromised or misconfigured bubblewrap invocation can be escaped. `/proc` attacks, kernel exploits, and symlink races all exist within the same kernel boundary.

stereOS (see `docs/design-docs/stereos-integration.md`) offers a stronger primitive: **run worker processes inside a purpose-built VM with a separate kernel**. This section captures how that maps to the sandbox architecture as a future upgrade path.

### Per-Agent VM, Not Per-Worker

The right granularity is one VM per agent, not one per worker:

- **Startup cost.** stereOS boots in ~2-3 seconds. Fine once at agent boot; unacceptable per fire-and-forget worker. Workers spawn constantly, agents don't.
- **Shared workspace.** All workers for an agent already share the same workspace and data directory. One VM matches the existing `Arc<Sandbox>` isolation boundary.
- **Resource overhead.** One VM per agent (~128-256MB RAM) is manageable. One per worker would balloon memory with concurrent workers.

The VM boots when the agent starts and stays up for the agent's lifetime. Workers spawn and die inside it. This directly parallels how the current `Sandbox` struct is constructed once at agent startup and shared via `Arc` across all workers.

### What Changes

Worker tool execution (shell, file, exec) currently calls `sandbox.wrap()` which prepends bubblewrap arguments to a `Command`. With VM isolation, these tools would instead dispatch commands over a vsock RPC layer to the agent's VM:

```
Current:  ShellTool → sandbox.wrap() → bwrap ... -- sh -c "command" (same kernel)
Future:   ShellTool → vm_rpc.exec()  → agentd → sh -c "command"   (guest kernel)
```

The `Sandbox` trait boundary stays the same — `wrap()` produces a `Command`. The VM backend would produce a `Command` that speaks vsock instead of forking a local subprocess. Tools don't need to know which backend they're using.

stereOS's `agentd` daemon already handles session management inside the VM. Worker commands would be dispatched as agentd sessions, with stdout/stderr streamed back over vsock.

### Security Model Upgrade

stereOS adds layers that bubblewrap cannot provide:

| Layer | bubblewrap (current) | stereOS VM |
|-------|---------------------|------------|
| **Kernel isolation** | Shared kernel (namespace-level) | Separate kernel (VM-level) |
| **PATH restriction** | Sandbox prepends `tools/bin` | Restricted shell with curated PATH, Nix tooling excluded |
| **Privilege escalation** | Relies on namespace user mapping | Explicit sudo denial (`agent ALL=(ALL:ALL) !ALL`), no wheel group |
| **Kernel hardening** | Host kernel settings apply | ptrace blocked, kernel pointers hidden, dmesg restricted, core dumps disabled |
| **Secret injection** | Env vars (cleared by `--clearenv`) | Written to tmpfs at `/run/stereos/secrets/` with root-only permissions (0700), never on disk |
| **User isolation** | UID mapping in namespace | Immutable users, no passwords, SSH keys injected ephemerally over vsock |

The secret injection model is particularly relevant. Today, the secret store design (see `docs/design-docs/secret-store.md`) relies on `--clearenv` to prevent workers from reading the master key via `/proc/self/environ`. With stereOS, the master key never enters the VM at all — it stays on the host. Secrets are injected individually into the guest's tmpfs by `stereosd`. The agent process inside the VM reads from `/run/stereos/secrets/` and the master key exposure problem disappears entirely.

### Network Isolation

bubblewrap's `--unshare-net` exists but breaks most useful worker tasks (git clone, API calls, web browsing). It's all-or-nothing.

VM-level networking is controllable with more granularity. The host can configure the VM's virtual NIC to allow outbound connections to specific hosts/ports while blocking everything else. This enables scenarios like "workers can reach github.com and the OpenAI API but nothing else" — impossible with bubblewrap without a userspace proxy.

### Blockers

This is not ready to implement. Key gaps from the stereOS integration research:

1. **Fly/Firecracker format mismatch.** Fly Machines use Firecracker, which expects ext4 rootfs + kernel binary. stereOS produces raw EFI, QCOW2, and kernel artifacts (bzImage + initrd). Firecracker doesn't use initrd the same way. Either stereOS needs a `formats/firecracker.nix` output, or we run QEMU/KVM on Fly (non-standard).

2. **Architecture.** stereOS is aarch64-linux only. Fly Machines are predominantly x86_64. Cross-compilation in Nix is straightforward but untested for stereOS.

3. **Control plane protocol.** `stereosd` speaks a custom vsock protocol. `spacebot-platform` would need a Rust client, or stereOS would need an HTTP API layer. The protocol isn't documented publicly yet.

4. **Workspace persistence.** stereOS VMs are ephemeral by design. Spacebot needs persistent storage (SQLite, LanceDB, workspace files). Requires virtio-fs mounts to persistent volumes, which stereOS supports but the Fly integration path would need to map to Fly volumes.

### Relationship to Current Work

The phases above (0-6) are prerequisites, not alternatives:

- **Phases 0-2** (enforcement, hot-reload, env sanitization) fix correctness bugs that matter regardless of backend. Even with VM isolation, the host process still needs `--clearenv` for any non-VM code paths (MCP processes, in-process tools).
- **Phase 3** (policy guards) applies inside the VM too. The shell package-manager guard prevents non-durable installs whether the worker runs in bubblewrap or a VM.
- **Phase 4-6** (capability manager) become more important with VMs. The VM image needs to know what binaries to include. The capability registry is the source of truth for what goes into the mixtape.

bubblewrap remains the default sandbox backend for all deployments. VM isolation would be an opt-in upgrade for the hosted platform where multi-tenant security justifies the resource overhead. Self-hosted users who want maximum isolation could run a `spacebot-mixtape` directly (NixOS image, no Docker) as an alternative deployment path.


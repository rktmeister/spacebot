# Named Messaging Adapters via Bindings

## Context

Messaging adapters are currently singleton per platform per instance (one Discord bot token, one Telegram bot token, one Slack app pair). Bindings route traffic by platform + location filters (guild/workspace/chat/channel), but they do not select which credential instance handles that traffic.

This blocks multi-bot scenarios on a single instance, such as:

- Two Telegram bots routing to different agents
- Two Discord bots with different server memberships and identities
- A default shared bot plus one specialized bot for a specific agent

The existing binding abstraction is still the right place to express routing. The missing piece is adapter instance selection.

## Proposal

Add support for multiple named adapter instances per platform, while keeping the existing nameless token fields as the default instance for backward compatibility.

- Existing config shape remains valid
- Named instances are optional and additive
- Bindings gain an optional `adapter` field
- Binding resolution chooses both agent and adapter instance

This keeps the simple path simple (paste one token) and unlocks advanced routing when needed.

## Goals

- Support multiple credential instances per messaging platform
- Keep old configs and API payloads working unchanged
- Keep bindings as the single routing abstraction
- Preserve first-match binding behavior
- Avoid introducing a separate per-agent messaging config system

## Non-Goals

- No backward-compat shim for legacy behavior beyond config/API compatibility
- No cross-platform global adapter namespace
- No automatic migration that rewrites user config files

## Config Shape

### Telegram example

```toml
[messaging.telegram]
enabled = true
token = "env:TELEGRAM_BOT_TOKEN" # default instance (legacy path)

[[messaging.telegram.instances]]
name = "support"
enabled = true
token = "env:TELEGRAM_BOT_TOKEN_SUPPORT"

[[messaging.telegram.instances]]
name = "sales"
enabled = true
token = "env:TELEGRAM_BOT_TOKEN_SALES"

[[bindings]]
agent_id = "main"
channel = "telegram"
chat_id = "-100111111111"
# adapter omitted => default instance

[[bindings]]
agent_id = "support-agent"
channel = "telegram"
chat_id = "-100222222222"
adapter = "support"
```

### Binding semantics

- `adapter` is platform-scoped (same name can exist under Telegram and Discord)
- `adapter` omitted means default instance for that platform
- If no matching binding exists, message still routes to default agent (current behavior)

## Data Model Changes

### `Binding`

Add:

- `adapter: Option<String>`

### Messaging config structs

For each platform with token-based auth (Discord, Telegram, Slack, Twitch):

- Keep existing singleton fields (`token`, `bot_token`, `app_token`, etc.)
- Add optional `instances: Vec<...InstanceConfig>` with:
  - `name: String`
  - `enabled: bool`
  - platform credential fields
  - optional platform-specific extras if needed later

## Runtime Model

## Adapter identity

Each running adapter gets a stable runtime key:

- Default instance: `<platform>` (example: `telegram`)
- Named instance: `<platform>:<name>` (example: `telegram:support`)

`MessagingManager` stores adapters by runtime key instead of platform name alone.

## Inbound routing

When an adapter emits an inbound message:

- Keep `message.source = <platform>` for existing platform semantics
- Add `adapter` metadata (or equivalent field) carrying runtime adapter key

Binding resolution matches on:

1. platform (`channel`)
2. adapter selector (`binding.adapter` vs inbound adapter identity)
3. existing platform filters (`guild_id`, `workspace_id`, `chat_id`, `channel_ids`, DMs)

Then first-match wins as before.

## Outbound routing

`respond`, `send_status`, and `fetch_history` use the adapter identity captured on inbound so replies stay on the same bot instance.

For proactive sends (`broadcast` and tools), the caller can target runtime adapter key explicitly when required.

## Permissions Model

Permission maps currently aggregate per platform. They become per adapter instance:

- Build permission set by filtering bindings on `(platform, adapter)`
- Default adapter uses bindings where `adapter` is absent or explicitly set to default selector
- Named adapter uses bindings with matching `adapter`

This allows independent scope per token instance without changing binding filter fields.

## API Changes

## Bindings API

`POST/PUT/DELETE /bindings` payloads add optional:

- `adapter?: string`

Old payloads remain valid.

## Messaging API

Status/toggle/disconnect move from platform-only targeting to adapter instance targeting:

- Platform + optional adapter name
- Platform-only continues to refer to default instance for compatibility

Response payloads should include adapter instance identity so UI can render multiple cards per platform.

## UI Changes

- Keep current quick setup flow for default token
- Add “Add another token” flow that requires a name
- Settings display becomes list of adapter instances per platform
- Binding editor adds optional adapter selector (default preselected)

The common single-token path remains one step.

## Validation Rules

- No duplicate instance names within a platform
- Binding `adapter` must reference an existing configured instance for that platform
- Reserved names: reject empty names and `default`
- Runtime key collisions are impossible under platform-scoped names but still validated

Config load should fail fast with clear messages when these constraints are violated.

## Backward Compatibility

- Existing `[messaging.<platform>]` blocks continue to create the default adapter
- Existing bindings with no `adapter` continue to work unchanged
- Existing API clients can omit `adapter`
- Existing docs/examples remain valid; new docs add advanced multi-instance examples

No migration required for existing users.

## Failure Modes and Handling

- Binding references missing adapter: reject config/API mutation
- Named adapter disabled/disconnected: bindings remain but produce clear routing/health errors
- Duplicate adapter name: reject config load and UI mutation
- Default adapter missing while bindings rely on default: validation error

## Ordered Implementation Phases

### Phase 1: Config and binding model

1. Add `adapter` to binding structs and TOML parsing
2. Add per-platform `instances` config parsing
3. Add validation rules (names, existence, duplicates)
4. Update docs for config reference

### Phase 2: Runtime adapter identity

1. Refactor `MessagingManager` keying from platform name to runtime adapter key
2. Instantiate default + named adapters per platform
3. Attach adapter identity to inbound messages
4. Route outbound operations via inbound adapter identity

### Phase 3: Binding resolution and permissions

1. Extend binding match logic with adapter selector
2. Build per-adapter permission sets from filtered bindings
3. Ensure hot-reload updates per-adapter permissions correctly

### Phase 4: API and UI

1. Extend bindings API payloads with optional `adapter`
2. Extend messaging status/toggle/disconnect APIs for adapter instances
3. Update dashboard settings and binding editor for named instances
4. Preserve platform-only behavior for default adapter paths

### Phase 5: Test coverage and rollout docs

1. Add config parsing/validation tests for named instances
2. Add routing tests for adapter-specific bindings
3. Add API tests for backward compatible payloads
4. Add setup docs for multi-bot per platform scenarios

## Open Questions

- Should adapter identity be surfaced as a first-class field on `InboundMessage` instead of metadata?
- For Slack, should named instances support independent app-level settings beyond tokens in this phase?
- Should proactive broadcast endpoints require explicit adapter for platforms with multiple configured instances, or keep default fallback?

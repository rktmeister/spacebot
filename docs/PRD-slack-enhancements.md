# PRD: Slack Connector Enhancements

**Status:** Draft  
**Author:** sookochoff  
**Repo:** spacedriveapp/spacebot  
**Branch target:** main  

---

## Background

The Slack connector in `src/messaging/slack.rs` is functional but uses a narrow slice of what the Slack platform and the existing `slack-morphism` library offer. For a company running Spacebot as a team agent, the current adapter covers basic conversation but leaves significant productivity and workflow value on the table.

This document enumerates the gaps, scores them by value and effort, and proposes a phased delivery plan.

---

## Current State Audit

### What the connector already does

| Feature | Status |
|---|---|
| Receives plain text messages via Socket Mode | ✅ |
| Receives file attachments | ✅ |
| Sends plain text replies (single channel or thread) | ✅ |
| Message splitting at 4,000 chars | ✅ |
| Emoji reactions (add) | ✅ |
| Streaming via message edit (`chat.update`) | ✅ |
| File upload (v2 flow: get URL → upload → complete) | ✅ |
| DM broadcasting via `conversations.open` | ✅ |
| History backfill (`conversations.history` + `conversations.replies`) | ✅ |
| User display name resolution on message receipt | ✅ |
| Permission filtering (workspace, channel, DM allowlist) | ✅ |
| Health check (`api.test`) | ✅ |

### What the connector explicitly skips

| Feature | Notes in code |
|---|---|
| Typing indicator | Comment: `// no-op, Slack has no native typing indicator API` — **incorrect**, `assistant.threads.setStatus` is the modern equivalent |
| Message subtypes (edits, deletes) | Silently dropped |
| `app_mention` events | Not wired in `handle_push_event` — agent won't respond to `@bot` in channels where it isn't already present |
| Slash commands | `command_callback` in slack-morphism is available but unused |
| Block Kit (rich messages) | `SlackBlock` et al. exist in the library, never used in outbound |
| Interactive components (button clicks, select menus) | `SlackInteractionEvent` / `SlackInteractionBlockActionsEvent` available, unused |
| Ephemeral messages | `chat_post_ephemeral` in library, unused |
| Scheduled messages | `chat_schedule_message` in library, unused |
| `app_home` tab | `SlackAppHomeOpenedEvent` in library, unused |
| Thread awareness in history | Thread replies fetched separately from channel history — not unified |
| Reaction-received events | `SlackReactionAddedEvent` available, unused |
| Unfurl / link previews | `chat_unfurl` in library, unused |
| User presence | `users_get_presence` in library, unused |
| Message pinning by agent | `pins_add` in library, unused |
| User groups (`@here`, `@channel`, group handles) | `usergroups_list` in library, unused |
| `SlackApiAssistantThreadsSetStatus` | Available — correct typing-indicator mechanism, unused |

---

## Opportunity Analysis

The gaps above are not all equal. Evaluated from the perspective of **a company team using Slack day-to-day with a Spacebot agent:**

### High value, low effort

1. **`app_mention` support** — Users in a channel naturally @mention the bot. Currently those events are ignored. One-line fix in the push event handler.

2. **Typing indicator via `assistant.threads.setStatus`** — The agent goes dark while thinking. On Discord the bot shows a typing indicator; on Slack it's a dead interface. The Slack Assistants API provides exactly this. Low code change; meaningful UX.

3. **Block Kit for structured outbound messages** — A new `OutboundResponse::Blocks` variant (or `RichMessage`) lets the agent send formatted cards, section headers, dividers, and inline code. Slack's renderer makes these significantly more readable than walls of markdown. Library support is full; gap is only in the `OutboundResponse` enum and adapter glue.

4. **Ephemeral messages** — Agent can whisper a confirmation or warning to only the requesting user. Useful for admin-type responses in shared channels.

### High value, medium effort

5. **Slash commands** — `/ask`, `/summarize`, `/task` etc. Slack's command UX is familiar to every team user. The `command_callback` is available in `SlackSocketModeListenerCallbacks` and just needs wiring up + a config schema for command→agent routing.

6. **Interactive components (buttons/select menus)** — The agent posts a decision card; a user clicks "Approve" or "Reject". The `SlackInteractionBlockActionsEvent` comes back through `interaction_callback`. Needs: (a) `OutboundResponse` variant to express buttons, (b) inbound `MessageContent` variant for interaction events, (c) interaction callback wired in the socket mode listener.

7. **Scheduled messages** — The agent can post to a channel at a specific future time (`chat.schedule_message`). This is a natural output for cron-triggered workflows, e.g. "post standup prompt at 9am Monday". Low Rust effort; zero Spacebot architecture change needed — it's purely an outbound response variant.

### Medium value, medium effort

8. **Message edit/delete awareness** — Currently silently dropped. If a user corrects a message that the agent already acted on, the agent should be aware. Needs a new `MessageContent` variant and some channel-level state to correlate `message_changed` subtype events back to prior turns.

9. **Reaction-received events** — User reacts with ✅ or ❌ to acknowledge or reject a proposal. Arriving as `SlackReactionAddedEvent`. Useful for lightweight approvals without typing.

10. **`app_home` tab** — The agent's home tab in the Slack sidebar can surface a custom view (memory summary, recent tasks, status). Entirely cosmetic from the agent's perspective but gives it a professional presence.

### Lower priority / out of scope for now

- User presence (`users.getPresence`) — niche
- Link unfurling — requires Events API subscription changes
- Message pinning — useful but not urgent
- User groups resolution — useful for `@channel` type broadcasts but edge case

---

## Proposed Phases

### Phase 1: Foundational UX (Recommended first PR)

**Scope:** Four targeted changes that require no new architecture.

| Item | Change required |
|---|---|
| **`app_mention` events** | Add `AppMention` arm to `handle_push_event` match |
| **Typing indicator** | Implement `send_status()` using `SlackApiAssistantThreadsSetStatusRequest` |
| **Ephemeral messages** | Add `OutboundResponse::Ephemeral { text, user_id }` variant; handle in Slack adapter with `chat_post_ephemeral` |
| **Reaction removal** | Add `OutboundResponse::RemoveReaction(String)` for completeness (currently only add) |

**Effort:** Small — 1–2 days of focused Rust work  
**Risk:** Low — isolated changes, no new dependencies  
**PRs:** Likely one PR with four commits

---

### Phase 2: Block Kit + Interactive Components

**Scope:** Rich outbound messages and inbound interaction events.

#### 2a — Block Kit outbound

Add a new `OutboundResponse` variant:

```rust
RichMessage {
    /// Plain text fallback (always required — used by non-Slack adapters and notifications).
    text: String,
    /// Block Kit blocks. Slack-only; other adapters fall back to `text`.
    blocks: Vec<SlackBlock>,
}
```

- Slack adapter: build `SlackMessageContent` with `blocks`
- Discord adapter: falls back to `text`
- Webhook/Telegram: falls back to `text`

This is platform-agnostic at the type level. The LLM would request a structured response via a new `reply_with_blocks` tool or through a structured tool output schema.

#### 2b — Interactive components inbound

Add a new `MessageContent` variant:

```rust
Interaction {
    /// action_id of the block element that was acted on.
    action_id: String,
    /// block_id for context.
    block_id: Option<String>,
    /// Selected value(s), if applicable (button value or select menu option).
    value: Option<String>,
    /// Human-readable label of the selected option.
    label: Option<String>,
    /// The original message ts so the agent can correlate back.
    message_ts: Option<String>,
}
```

Wire `interaction_callback` in the socket mode setup to receive `SlackInteractionBlockActionsEvent` and convert to an `InboundMessage` with this content.

**Effort:** Medium — 3–4 days  
**Risk:** Medium — touches `OutboundResponse` and `MessageContent` enums (shared types), all adapters need an audit pass to ensure the new variants are handled (even if as no-ops)  
**PRs:** 2a and 2b can be separate PRs

---

### Phase 3: Slash Commands

**Scope:** Allow users to invoke the agent via `/command` in any channel.

Config extension:

```toml
[messaging.slack.commands]
"/ask" = { agent_id = "main", description = "Ask the agent a question" }
"/task" = { agent_id = "main", description = "Kick off a background task" }
```

Implementation:
- Wire `command_callback` in socket mode listener setup
- Parse `SlackCommandEvent` into an `InboundMessage` (using the command text as content)
- Route via the existing binding resolution
- Respond to the command's `response_url` or via `chat.post_message` to the channel

Slash commands have a 3-second acknowledge requirement from Slack. The adapter must acknowledge immediately (200 OK with empty body or with a deferral message) and post the real reply asynchronously.

**Effort:** Medium — 2–3 days  
**Risk:** Medium — requires config schema extension and a deferred response pattern  
**PRs:** Single focused PR

---

### Phase 4: Scheduled Messages

**Scope:** Let the agent post messages at a future time.

New `OutboundResponse` variant:

```rust
ScheduledMessage {
    text: String,
    /// Unix timestamp when the message should be delivered.
    post_at: i64,
    /// Optional Block Kit content. Falls back to `text` on non-Slack adapters.
    blocks: Option<Vec<SlackBlock>>,
}
```

This maps cleanly to `chat_schedule_message` in slack-morphism and requires no new infrastructure. Practically, it makes cron workflows more polished: instead of sending a message immediately on job completion, the agent can time-shift delivery to normal working hours.

**Effort:** Low — 1 day  
**Risk:** Low — self-contained outbound variant  
**PRs:** Single small PR, could be bundled with Phase 1

---

## What Does NOT Need to Change

- **Socket Mode** — already the right transport for a self-hosted bot. No switch to Events API needed.
- **slack-morphism version** — 2.17 already has full Block Kit and interaction model support. No dependency bump needed.
- **Agent architecture** — Channels, branches, workers, cortex are all unaffected. This is entirely connector-layer work.
- **Existing permissions model** — `SlackPermissions` is sufficient for all phases.

---

## Recommended Delivery Order

| Phase | Deliverable | Effort | Impact |
|---|---|---|---|
| 1 | `app_mention`, typing indicator, ephemeral messages | Small | High |
| 2a | Block Kit outbound (`RichMessage`) | Medium | High |
| 4 | Scheduled messages | Small | Medium |
| 2b | Interactive components inbound | Medium | High |
| 3 | Slash commands | Medium | Medium |

Phases 1 and 4 are natural first PRs — they're self-contained, low risk, and address the most visible UX gaps (agent ignores @mentions, no typing indicator, no rich formatting). Phase 2 is where the real workflow power is.

---

## Open Questions

1. **Tool schema for Block Kit**: Should the LLM specify blocks explicitly (raw Block Kit JSON in tool args), or should there be a higher-level tool interface (e.g., `reply_with_sections([{header, body}])`) that the adapter renders into blocks? The latter is safer and more portable but requires maintaining a thin DSL.

2. **Interaction routing**: When a button click arrives, should it be routed to the same channel that originally sent the message with the buttons, or treated as a new conversation turn? The former is simpler; the latter is more correct for multi-step flows.

3. **Slash command permissions**: Should slash commands go through the same binding/channel filter as regular messages, or have their own allowlist? Consider that `/ask` from an unbound workspace should probably be rejected.

4. **Typing indicator scope**: `assistant.threads.setStatus` works in Slack Assistant threads (AI-mode threads). For regular channel messages, the canonical signal is `typing.setStatus` from the RTM API, which Socket Mode doesn't expose. We may need to accept that typing indicators only work in Slack Assistant thread contexts and document this clearly.

---

## Files Affected (by phase)

**Phase 1:**
- `src/messaging/slack.rs` — `handle_push_event`, new `send_status` impl, new response arms
- `src/lib.rs` — add `Ephemeral` to `OutboundResponse`, `RemoveReaction` optional
- Other adapters — handle new variants as no-ops

**Phase 2:**
- `src/messaging/slack.rs` — interaction callback, `RichMessage` response arm
- `src/lib.rs` — `RichMessage` to `OutboundResponse`, `Interaction` to `MessageContent`
- `src/messaging/discord.rs` — handle `RichMessage` (fallback to text)
- `src/messaging/telegram.rs` — handle `RichMessage` (fallback to text)
- `src/messaging/webhook.rs` — handle `RichMessage` (passthrough or fallback)

**Phase 3:**
- `src/config.rs` — `SlackCommandConfig` struct, extend `SlackConfig`
- `src/messaging/slack.rs` — command callback, deferred response pattern
- `src/lib.rs` — no change (command maps to existing `InboundMessage`)

**Phase 4:**
- `src/messaging/slack.rs` — `ScheduledMessage` response arm
- `src/lib.rs` — add `ScheduledMessage` to `OutboundResponse`
- Other adapters — no-op or error for the new variant

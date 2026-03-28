---
summary: Provider-agnostic calendar subsystem for Spacebot with a local SQLite mirror, dashboard calendar views, agent tools, and optional read-only ICS export. V1 ships with a CalDAV provider implementation.
read_when:
  - Planning Spacebot calendar support
  - Adding calendar sync or calendar UI
  - Defining event create/update/delete behavior
  - Evaluating current .ics invite handling
  - Deciding whether external calendar apps should subscribe read-only via ICS
---

# Calendar Integration

Add a first-class calendar subsystem to Spacebot backed by a remote calendar provider. The remote provider is the source of truth. Spacebot keeps a local SQLite mirror for fast queries, dashboard rendering, and agent tool access.

V1 ships with a **CalDAV provider implementation**. ForwardEmail is the first concrete target, but the subsystem should be shaped so that other CalDAV providers, and later non-CalDAV providers, can fit without redesigning the core calendar model.

This replaces the current pattern where calendar knowledge is inferred indirectly from email threads, memories, or ad hoc `.ics` parsing.

## Decision Summary

These decisions are locked for V1:

- **Source of truth:** configured remote calendar provider
- **V1 provider implementation:** CalDAV
- **Local storage:** SQLite mirror/cache inside the per-agent database
- **Initial scope:** one selected calendar
- **Future scope:** multiple calendars per account/source
- **Sync behavior:** startup + periodic background sync + manual sync
- **Dashboard:** month + week views, event detail drawer, modal editing
- **Writes:** create/update/delete from day one
- **Write safety:** propose then apply; destructive actions require explicit confirmation
- **Recurring events:** read support explicitly honors `RRULE`, `EXDATE`, and `RECURRENCE-ID`; whole-series edit/delete only in V1
- **Email `.ics` role:** secondary ingest/reconciliation path, not the primary calendar backend
- **Reminders/briefings:** continue to use cron; calendar does not replace cron
- **Optional external sharing mode:** read-only ICS export feed generated from the local mirror
- **ICS export freshness:** export only confirmed mirrored remote state, never optimistic local drafts

## Why This Exists

Today Spacebot can read narrow calendar invite information from email attachments, but it does not have a real calendar model:

- there is no calendar source of truth in config or storage
- there is no event API or dashboard calendar view
- there are no agent tools for querying or mutating calendar state
- `.ics` parsing is deliberately limited to invite summarization

As a result, the bot has to reason from memories, email history, or parsed attachment fragments instead of querying a durable event store.

The goal of this feature is to make calendar state explicit, queryable, editable, and optionally subscribable from external apps.

## Goals

1. Let Spacebot read calendar state from a real remote calendar provider.
2. Show that calendar in the dashboard in month and week views.
3. Give the agent a reliable event store for querying availability and upcoming events.
4. Support event create/update/delete against the remote source of truth.
5. Reconcile inbound `.ics` invites into the local calendar model when possible.
6. Keep the local UI and agent tools fast by reading from SQLite, not live provider APIs on every request.
7. Preserve operational safety around destructive changes and conflicting remote edits.
8. Optionally expose a read-only ICS feed so calendar apps that do not support the provider directly can subscribe.

## Non-Goals

1. Replacing cron as the reminder or briefing mechanism.
2. Building attendee RSVP workflows in V1.
3. Supporting single-occurrence recurring edits or series splitting in V1.
4. Building a second bot-only private calendar in V1.
5. Treating email invite parsing as a full iCalendar engine.
6. Making ICS export the primary write path. ICS export is read-only distribution.

## Current State Audit

### Email and `.ics` today

Current email support is IMAP/SMTP only. There is no calendar provider configuration block and no calendar subsystem.

The current email adapter does handle `.ics` invite attachments in a narrow but real way:

- detects `text/calendar`, `application/ics`, or `.ics` filenames
- reads raw attachment bytes from the parsed MIME message
- unfolds iCalendar lines
- extracts `VEVENT` fields
- appends a human-readable invite summary into the email body context
- stores parsed event metadata on the inbound message

The current parser handles:

- `SUMMARY`
- `LOCATION`
- `UID`
- `RRULE`
- `ORGANIZER`
- `DTSTART`
- `DTEND`
- all-day detection
- basic timezone normalization when `TZID` maps cleanly to a known timezone

The current parser does **not** provide full calendar semantics:

- no remote calendar sync
- no attendee model
- no `METHOD:CANCEL` handling
- no `SEQUENCE` or version reconciliation
- no `RECURRENCE-ID` / `EXDATE` exception model
- no robust `VTIMEZONE` interpretation beyond simple `TZID` mapping
- no recurrence expansion for calendar views

It is also still **attachment-oriented** today:

- calendar parsing only runs for MIME parts treated as attachments
- inline `text/calendar` parts without attachment disposition or filename are easy to miss
- the current tests only cover the attachment case

This is acceptable for invite summarization, but it is not sufficient as the calendar backend.

## Architecture

### High-Level Model

```
Calendar provider (V1: CalDAV)
  -> discovery or stored calendar selection
  -> initial sync
  -> periodic incremental sync
  -> local SQLite mirror
  -> API + agent tools + dashboard read from SQLite
  -> create/update/delete go through a propose/apply flow
  -> successful remote writes update the local mirror
  -> optional ICS export is generated from the local mirror
```

### Separation of Concerns

The calendar subsystem should be a new domain. It should not live under `messaging/`.

- `messaging/` remains platform transport for inbound/outbound chat channels
- `calendar/` owns provider integration, event storage, sync state, and recurrence expansion
- `calendar/providers/` owns provider-specific implementations
- `calendar/providers/caldav.rs` is the first implementation
- `api/calendar.rs` exposes HTTP routes
- `tools/calendar_*` exposes agent tools
- `interface/src/routes/AgentCalendar.tsx` provides the dashboard page

This keeps calendar logic aligned with `tasks` and `cron`, not with chat adapters.

### Provider Model

The domain model should be provider-agnostic.

- storage is provider-neutral
- API responses are provider-neutral
- tools are provider-neutral
- provider-specific logic stays behind an interface

Suggested trait shape:

```rust
trait CalendarProvider {
    fn kind(&self) -> &'static str;
    async fn discover_calendars(&self) -> Result<Vec<DiscoveredCalendar>>;
    async fn initial_sync(&self, calendar: &SelectedCalendar) -> Result<SyncBatch>;
    async fn incremental_sync(&self, calendar: &SelectedCalendar, checkpoint: &SyncCheckpoint) -> Result<SyncBatch>;
    async fn create_event(&self, calendar: &SelectedCalendar, event: &OutboundEventDraft) -> Result<RemoteWriteResult>;
    async fn update_event(&self, calendar: &SelectedCalendar, event: &OutboundEventDraft) -> Result<RemoteWriteResult>;
    async fn delete_event(&self, calendar: &SelectedCalendar, remote: &RemoteEventRef) -> Result<RemoteWriteResult>;
}
```

V1 implements:

- `kind = "caldav"`

Future providers can reuse the same domain model without forcing CalDAV-specific assumptions into every layer.

## Source of Truth and Local Mirror

### Remote Source of Truth

The configured remote calendar provider is authoritative.

This means:

- remote event identifiers are preserved locally
- remote edits win unless the user explicitly resolves a conflict
- local state is a mirror, not an independent scheduler

### Local Mirror

SQLite is the correct local storage choice for V1.

Reasons:

- range queries for month/week views are cheap and predictable
- event lookup by UID, href, or time window is straightforward
- sync state, etags, and tombstones fit relational storage well
- API and tool reads stay fast even when the remote provider is slow or unavailable

The local mirror should be treated as:

- query cache
- dashboard backing store
- tool backing store
- sync checkpoint store
- ICS export source

It should not become a divergent second source of truth.

## Calendar Discovery and Selection

### Setup Flow

The setup flow should be provider-aware.

For V1 CalDAV sources, Spacebot should auto-discover calendars from the account, then persist one selected calendar.

Recommended sequence:

1. User configures a calendar source.
2. Spacebot validates the source and authenticates.
3. Spacebot discovers available calendars.
4. Dashboard or setup flow shows a picker.
5. User selects one calendar.
6. Spacebot stores the selected calendar href/URL and metadata locally.

### Runtime Behavior

After selection:

- runtime uses the stored selected calendar directly
- discovery is not repeated on every request
- manual rediscovery can be added later for reconfiguration
- a direct collection URL override can exist as an escape hatch

This gives low-friction setup without making runtime behavior ambiguous.

## Configuration

Add a dedicated calendar config block that follows the existing Spacebot
`defaults -> agent override` model rather than introducing a new global
top-level subsystem unrelated to other agent-local features.

Example shape:

```toml
[defaults.calendar]
enabled = true
provider_kind = "caldav"
base_url = "https://..."
auth_kind = "basic" # or "oauth2"
username = "secret:CALDAV_USERNAME"
password = "secret:CALDAV_PASSWORD"
sync_interval_secs = 300
read_only = false
ics_export_enabled = false
ics_export_token = "secret:CALENDAR_ICS_EXPORT_TOKEN"

[agents.ops_bot.calendar]
selected_calendar_href = "https://caldav.example.com/dav/calendars/user/bot/"
```

### Config Rules

- the calendar subsystem remains separate from the email adapter even when credentials match
- calendar config should be resolvable through `defaults.calendar` and `agents[].calendar`
- do not introduce a second top-level shared-config pattern unless multi-agent shared calendar sources become a real requirement
- `secret:` is the preferred credential reference mechanism
- the selected calendar href should be persisted after discovery/selection
- `read_only` is a useful operational mode
- ICS export is optional and disabled by default

### Auth Model

The domain model should support multiple auth kinds even though V1 only implements CalDAV.

Supported config concepts:

- `auth_kind = "basic"`
- `auth_kind = "oauth2"`

V1 implementation requirements:

- CalDAV + Basic Auth
- config shapes that do not block CalDAV + OAuth2 later

This avoids locking the subsystem to password-only providers.

## Data Model

### Tables

V1 should use at least these tables:

#### `calendar_sources`

Tracks configured calendar backends.

Fields:

- `id`
- `agent_id`
- `kind`
- `base_url`
- `auth_kind`
- `username`
- `selected_calendar_href`
- `selected_calendar_name`
- `capabilities_json`
- `read_only`
- `ics_export_enabled`
- `last_sync_at`
- `sync_status`
- `last_error`
- `created_at`
- `updated_at`

#### `calendar_calendars`

Stores discovered calendars, even if only one is selected in V1.

Fields:

- `id`
- `source_id`
- `remote_href`
- `display_name`
- `description`
- `color`
- `timezone`
- `selected`
- `created_at`
- `updated_at`

#### `calendar_events`

Primary event mirror.

Fields:

- `id`
- `calendar_id`
- `remote_uid`
- `remote_href`
- `etag`
- `summary`
- `description`
- `location`
- `status`
- `organizer_name`
- `organizer_email`
- `starts_at_utc`
- `ends_at_utc`
- `original_timezone`
- `all_day`
- `is_recurring`
- `recurrence_rule`
- `recurrence_id`
- `sequence`
- `raw_ics`
- `deleted`
- `last_synced_at`
- `created_at`
- `updated_at`

Indexes should cover:

- `calendar_id + starts_at_utc`
- `remote_uid`
- `remote_href`
- `deleted`
- `is_recurring`

#### `calendar_attendees`

V1 stores attendee data even if the UI only lightly surfaces it.

Fields:

- `id`
- `event_id`
- `name`
- `email`
- `role`
- `partstat`
- `rsvp`
- `is_organizer`
- `created_at`
- `updated_at`

#### `calendar_sync_state`

Tracks backend sync checkpoints.

Fields:

- `source_id`
- `calendar_id`
- `sync_token`
- `ctag`
- `last_successful_sync_at`
- `last_attempted_sync_at`
- `last_error`

#### `calendar_change_proposals`

Persists propose/apply drafts so chat and dashboard flows use the same durable model.

Fields:

- `id`
- `agent_id`
- `source_id`
- `calendar_id`
- `event_id`
- `proposal_kind` (`create`, `update`, `reschedule`, `delete`)
- `payload_json`
- `target_remote_href`
- `target_etag`
- `created_by`
- `created_at`
- `expires_at`
- `applied_at`
- `discarded_at`

### Optional: `calendar_occurrences`

If recurrence expansion becomes expensive, add a derived occurrence table for fast month/week rendering.

This is optional in V1. It can be computed in memory first if performance is acceptable.

## Sync Model

### Sync Modes

V1 sync behavior:

- startup sync
- periodic background sync
- manual sync from the dashboard

Recommended default:

- `sync_interval_secs = 300`

### Initial Sync

Initial sync should:

1. fetch the selected calendar metadata
2. fetch all events in the initial sync window
3. store remote href, UID, etag, and raw ICS
4. mark deleted events explicitly if known

Initial sync window options:

- full calendar sync
- bounded historical + future window

For V1, the simplest safe choice is:

- sync all remote events returned by the selected calendar if the volume is manageable

If event volume becomes large later, the system can move to a bounded hot window plus lazy historical fetch.

### Incremental Sync

Use provider-specific incremental sync primitives where possible.

For V1 CalDAV this means:

- sync token if available
- ctag or equivalent collection version markers
- etag checks on resources

If incremental sync fails or the token is invalid:

- log the error
- fall back to a clean resync

### Conflict Handling

On remote write conflicts:

- do not silently overwrite
- fail the apply step
- refresh the local mirror
- show a reviewable diff to the user

Default policy:

- remote edit wins until the user explicitly retries against fresh state

## Write Model

### Propose Then Apply

Calendar writes should follow a two-step model:

1. propose a change
2. apply the change after confirmation or explicit user action

This should exist in both:

- chat/tool-driven flows
- dashboard modal editing flows

### Proposal Persistence

Proposals should be persisted in SQLite, not only held in memory.

Reasons:

- chat and dashboard can share one durable proposal model
- apply can revalidate against fresh remote state
- pending changes survive refreshes and process restarts
- auditing and expiry are easier to implement consistently

Apply should:

- load the persisted proposal
- refresh or validate the target remote etag when applicable
- fail safely if the remote state changed materially
- only update the local mirror after a confirmed successful remote write

### Destructive Changes

Destructive actions require explicit confirmation:

- delete event
- delete recurring series
- overwrite conflicting remote changes

### Non-Destructive Changes

Non-destructive changes still use propose/apply, but the confirmation surface can be lighter:

- title
- description
- location
- start/end
- timezone
- whole-series recurrence fields

## Recurring Events

### V1 Read Support

V1 must support full recurring read behavior well enough to make month/week views trustworthy.

That means:

- recurring series show in the correct visible range
- all-day recurring events render correctly
- timezones are handled consistently
- recurrence expansion explicitly honors:
  - `RRULE`
  - `EXDATE`
  - `RECURRENCE-ID`

### Recurrence Source of Truth

For V1, recurring read correctness comes from provider-backed iCalendar data.

- `raw_ics` is the canonical recurrence payload
- normalized event columns remain useful for indexing and base rendering
- recurrence exceptions should not be inferred from heuristics
- if needed for performance later, `calendar_occurrences` can cache derived visible instances

### V1 Write Limits

V1 recurring edit/delete behavior is intentionally constrained:

- create recurring series
- edit whole series
- delete whole series
- no single-occurrence edits
- no split-from-this-point-forward flows
- no `RECURRENCE-ID` exception authoring UI

This keeps writes correct without shipping half-implemented recurrence semantics.

## Email Invite Reconciliation

### Role of Email `.ics`

Email invite parsing remains valuable, but it becomes a secondary ingest path.

When an inbound email has a parsed invite:

1. if a matching calendar event exists by UID or remote metadata, link it
2. if no match exists, mark it as a candidate or importable event
3. do not blindly create duplicates if provider sync will already bring it in

### Reconciliation Strategy

Preferred match order:

1. `UID`
2. organizer + start time + summary heuristic
3. no match -> candidate or importable

This allows email to enrich the user experience without undermining the remote source of truth.

## Agent Tools

Add dedicated calendar tools for branches and cortex chat.

### V1 Tool Surface

- `calendar_list`
- `calendar_get`
- `calendar_find_free_time`
- `calendar_create`
- `calendar_update`
- `calendar_reschedule`
- `calendar_delete`
- `calendar_sync_status`
- `calendar_sync_now`

### Tool Rules

- tools read from SQLite mirror by default
- mutating tools operate through propose/apply
- mutating tools should carry remote identifiers and etag information through the workflow
- tools should never fall back to memory or email search for authoritative answers when calendar data exists locally

## API Surface

Add calendar API routes under `/api/agents/calendar/...`.

### Suggested Endpoints

- `GET /api/agents/calendar/status?agent_id=...`
- `GET /api/agents/calendar/calendars?agent_id=...`
- `POST /api/agents/calendar/select`
- `GET /api/agents/calendar/events?agent_id=...&from=...&to=...&view=month|week`
- `GET /api/agents/calendar/event?agent_id=...&event_id=...`
- `POST /api/agents/calendar/propose`
- `POST /api/agents/calendar/apply`
- `POST /api/agents/calendar/sync`
- `GET /calendar/ics/{agent_id}/{token}/calendar.ics` when ICS export is enabled

### Response Principles

- expose UTC timestamps and original timezone
- expose remote sync status
- expose attendee list in detail views
- expose a stable event identifier for dashboard interactions
- ICS export should remain read-only and token-gated

## Dashboard UX

### New Agent Tab

Add a new `Calendar` tab to the agent dashboard.

### V1 Views

- month view
- week view
- event detail drawer
- modal event editor
- sync status and manual sync action

### Event Card Fields

- title
- start/end
- timezone
- location
- recurrence marker
- attendee count or organizer

### V1 Editing Fields

Editable in the modal:

- title
- description
- location
- start
- end
- timezone
- all-day
- recurrence rule for simple whole-series recurrence

Attendees should be stored and shown in detail, but attendee editing can remain limited in V1.

### UI Safety

Before apply:

- show a human-readable diff
- show whether the change is destructive
- show whether the target is a recurring series

## Read-Only ICS Export

### Why It Exists

Some calendar apps can subscribe to an ICS URL but cannot directly connect to the underlying provider or cannot act as a general third-party CalDAV client.

ICS export provides:

- read-only distribution of the bot-managed calendar
- compatibility with apps that support subscribed calendar feeds
- a simple way to expose the bot's calendar to external viewers

### What It Is Not

ICS export is **not** the source of truth.

- it does not support writes
- it does not replace provider sync
- it does not replace the local mirror

It is a derived feed generated from the mirrored event store.

### Feed Behavior

Recommended V1 behavior:

- disabled by default
- enabled per source or per agent
- exposed through a high-entropy bearer token in the path
- read-only
- generated from the local mirror
- includes only non-deleted events
- supports a bounded export window if full-history export becomes too large
- includes only confirmed mirrored remote state
- excludes pending proposals and un-applied local drafts
- is served from a route that does not require the normal dashboard Bearer token

### Security Model

ICS subscriptions are easy to redistribute accidentally.

So V1 should treat ICS feeds like semi-secret read-only URLs:

- use an unguessable token
- allow token rotation
- allow full disablement
- never allow writes through the feed
- mount the feed outside the normal protected API nest, or explicitly exempt it from Bearer-auth middleware, so calendar clients can actually subscribe

### App Compatibility

ICS export is the compatibility bridge for apps that can subscribe to a URL but cannot directly use the provider.

Examples:

- Apple Calendar can usually do either direct CalDAV or ICS subscription
- Google Calendar is better handled through a subscribed ICS URL when direct third-party provider support is limited

## Cron Integration

Calendar does not replace cron.

Cron remains responsible for:

- daily agenda briefings
- reminders
- scheduled digests

The calendar subsystem provides better raw event data for those cron jobs to query.

Later integration can include helper flows such as:

- "create weekday 9am agenda briefing from calendar"
- "remind me 15 minutes before events tagged important"

But those are follow-up features, not part of the calendar core.

## Security and Operational Notes

- provider credentials should live in the secret store, not in plain config
- remote writes should respect explicit confirmation rules
- conflict errors should be visible and reviewable, not swallowed
- the dashboard should show stale sync state clearly if the local mirror is behind
- ICS export tokens should live in the secret store and be rotatable

## Implementation Phases

### Phase 1: Data Model and Config

1. Add per-agent calendar config and secrets plumbing using the existing `defaults -> agent override` model.
2. Add SQLite migrations for calendar tables.
3. Add runtime structures for selected calendar metadata and sync state.
4. Add ICS export configuration fields without enabling them by default.

### Phase 2: Provider Interface and First Implementation

1. Define the provider interface.
2. Implement the first provider: CalDAV.
3. Implement discovery for CalDAV sources.
4. Implement selected calendar persistence.
5. Implement initial sync.
6. Implement periodic and manual sync.
7. Implement conflict-aware update path.

### Phase 3: API and Tooling

1. Add calendar API handlers and routes.
2. Add calendar tool implementations.
3. Expose sync status and selected calendar state.
4. Add optional ICS export endpoint.

### Phase 4: Dashboard

1. Add `Calendar` route and tab.
2. Implement month and week views.
3. Add event drawer.
4. Add modal propose/apply editing flow.
5. Add manual sync control.
6. Add ICS export status and copy-link or rotate-token controls if enabled.

### Phase 5: Email Reconciliation

1. Link `.ics` invite parsing to the local event mirror.
2. Broaden invite intake beyond attachment-only parsing so inline `text/calendar` parts are not silently skipped.
3. Deduplicate using UID.
4. Surface unmatched invites as candidate or importable events.

### Phase 6: Hardening

1. Add recurrence expansion tests.
2. Add conflict tests.
3. Add CalDAV fixture and integration coverage.
4. Add provider-agnostic tests at the domain layer where possible.
5. Add UI tests for month/week rendering and edit flow.
6. Add ICS export tests.

## Acceptance Criteria

1. Spacebot can configure a calendar source through a provider-agnostic model.
2. A single selected calendar can be persisted and synced locally.
3. Dashboard month and week views render events from the local mirror.
4. The agent can list, inspect, create, update, reschedule, and delete events using calendar tools.
5. Remote writes use propose/apply and require explicit confirmation for destructive changes.
6. Recurring series display correctly in month and week views.
7. Email invite parsing can reconcile into the local calendar model without creating obvious duplicates.
8. Cron-based daily agenda flows can query the calendar subsystem instead of inferring from email or memory.
9. When enabled, Spacebot can expose a read-only ICS feed derived from the mirrored event store.

For V1, the concrete provider implementation satisfying these criteria is CalDAV.

## Follow-Up Work

Likely V2 or later:

- multi-calendar support
- per-calendar filters and colors in the dashboard
- attendee editing and RSVP workflows
- single-occurrence recurring edits
- recurrence exception support
- free/busy optimization helpers
- richer agenda or reminder automation built on cron
- additional provider implementations
- OAuth2-backed CalDAV support
- per-feed event filtering for ICS export

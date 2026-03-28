---
summary: Worker-safe implementation breakdown for the calendar subsystem. Splits the feature into disjoint slices with owned files, dependencies, verification, and rollback guidance.
read_when:
  - Planning the calendar implementation
  - Splitting calendar work across multiple workers
  - Deciding implementation order for calendar, CalDAV sync, UI, and ICS export
---

# Calendar Work Breakdown

Implementation breakdown for the calendar design in [calendar-caldav-integration.md](./calendar-caldav-integration.md).

The goal of this document is not to restate the design. It exists to:

- break the work into independent slices
- keep write ownership explicit
- reduce merge conflicts in a shared worktree
- make it possible to hand slices to multiple workers without stepping on shared registry files

## Ground Rules

1. The remote provider remains the source of truth.
2. V1 ships with a CalDAV provider implementation.
3. SQLite is the local mirror/cache and proposal store.
4. Shared registry files are owned by one integration slice only.
5. Recurring read support must honor `RRULE`, `EXDATE`, and `RECURRENCE-ID`.
6. ICS export is optional, read-only, and generated only from confirmed mirrored remote state.

## Shared File Lock

These files are high-conflict integration points. Only the integration slice should edit them unless explicitly coordinated:

- `src/lib.rs`
- `src/main.rs`
- `src/api.rs`
- `src/api/state.rs`
- `src/api/server.rs`
- `src/api/agents.rs`
- `src/tools.rs`
- `interface/src/router.tsx`
- `interface/src/components/AgentTabs.tsx`

This is the main rule that keeps parallel workers from clobbering each other.

## Slice Overview

Recommended order:

1. Slice A: config + schema + runtime plumbing
2. Slice B: calendar domain core
3. Slice C: CalDAV provider + sync engine
4. Slice D: proposal persistence + calendar tools
5. Slice E: API leaf handlers
6. Slice F: dashboard UI
7. Slice G: email invite reconciliation
8. Slice H: ICS export
9. Slice I: integration wiring
10. Slice J: hardening, docs, and gates

Parallelizable groups:

- A before everything else
- B can start as soon as A is stable
- C, D, and E can proceed in parallel after B, as long as they stay inside their owned files
- F can proceed once E defines response shapes
- G can proceed once B exists
- H can proceed once B and E exist
- I must run after the leaf slices
- J finishes last

## Slice A

### Goal

Add configuration, secret references, and SQLite schema for the calendar subsystem.

### Owned files

- `src/config/types.rs`
- `src/config/toml_schema.rs`
- `src/config/load.rs`
- `src/config/runtime.rs`
- `migrations/<timestamp>_calendar_core.sql`

### Out of scope

- provider code
- API handlers
- tools
- UI
- integration wiring in `src/main.rs` / `src/api/state.rs` / `src/api/agents.rs`

### Risk level

Medium

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test config
sqlx migrate info --database-url "sqlite::memory:"
```

Expected pass condition:

- config parsing and resolution tests pass
- migration is syntactically valid

### Rollback plan

- revert the migration and config fields together
- do not leave partially introduced config keys behind

## Slice B

### Goal

Create the provider-neutral calendar domain core: types, store, recurrence parsing/expansion, and proposal model interfaces.

### Owned files

- `src/calendar.rs`
- `src/calendar/types.rs`
- `src/calendar/store.rs`
- `src/calendar/recurrence.rs`
- `src/calendar/provider.rs`
- `src/calendar/proposals.rs`

### Out of scope

- concrete CalDAV implementation
- API routes
- tools
- UI
- shared registry wiring

### Risk level

High

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test calendar::types
cargo test calendar::recurrence
cargo test calendar::store
```

Expected pass condition:

- recurrence tests cover `RRULE`, `EXDATE`, and `RECURRENCE-ID`
- store round-trips event rows and proposal rows cleanly

### Rollback plan

- revert the entire `src/calendar*` addition as one unit
- keep schema if it is already merged, but disable all callers

## Slice C

### Goal

Implement the first provider: CalDAV discovery, selected calendar loading, full sync, incremental sync, and remote writes.

### Owned files

- `src/calendar/providers/caldav.rs`
- `src/calendar/providers/caldav_auth.rs`
- `src/calendar/providers/caldav_discovery.rs`
- `src/calendar/providers/caldav_sync.rs`

### Out of scope

- provider-neutral types
- API
- tools
- UI
- shared integration files

### Risk level

High

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test caldav
```

Expected pass condition:

- fixture-backed discovery tests pass
- sync diff tests pass
- remote write request-building tests pass

### Rollback plan

- revert the CalDAV provider files only
- leave the provider interface intact

## Slice D

### Goal

Implement persisted propose/apply behavior and the calendar tool leaf modules.

### Owned files

- `src/tools/calendar_list.rs`
- `src/tools/calendar_get.rs`
- `src/tools/calendar_find_free_time.rs`
- `src/tools/calendar_create.rs`
- `src/tools/calendar_update.rs`
- `src/tools/calendar_reschedule.rs`
- `src/tools/calendar_delete.rs`
- `src/tools/calendar_sync_status.rs`
- `src/tools/calendar_sync_now.rs`

### Out of scope

- `src/tools.rs` registration
- API
- UI

### Risk level

Medium

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test tools::calendar
```

Expected pass condition:

- tool argument validation passes
- proposal creation persists rows
- apply path revalidates remote state before commit

### Rollback plan

- revert the new tool leaf files together
- keep proposal table unused if schema already landed

## Slice E

### Goal

Implement calendar API leaf handlers and response types.

### Owned files

- `src/api/calendar.rs`

### Out of scope

- router registration in `src/api/server.rs`
- frontend routes/components
- provider code

### Risk level

Medium

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test api::calendar
```

Expected pass condition:

- range queries return correct event sets
- proposal/apply handlers return stable response shapes
- sync status endpoint exposes freshness and error state

### Rollback plan

- revert `src/api/calendar.rs`
- leave underlying calendar domain untouched

## Slice F

### Goal

Build the dashboard calendar UI using the API contract from Slice E.

### Owned files

- `interface/src/routes/AgentCalendar.tsx`
- `interface/src/components/calendar/CalendarMonthView.tsx`
- `interface/src/components/calendar/CalendarWeekView.tsx`
- `interface/src/components/calendar/CalendarEventDrawer.tsx`
- `interface/src/components/calendar/CalendarEventModal.tsx`
- `interface/src/components/calendar/CalendarToolbar.tsx`

### Out of scope

- router and tab wiring
- API client shared registry files
- backend behavior

### Risk level

Medium

### Verification commands

```bash
cd /home/dylan/tools/spacebot/interface
bun run build
```

Expected pass condition:

- the interface builds
- month/week view renders with mocked or live API data

### Rollback plan

- revert the calendar UI leaf components and route

## Slice G

### Goal

Add email invite reconciliation so parsed `.ics` invites link into the calendar mirror without becoming the source of truth.

### Owned files

- `src/messaging/email.rs`
- `src/calendar/reconcile.rs`

### Out of scope

- provider sync
- dashboard
- router/tool registration

### Risk level

Medium

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test messaging::email
cargo test calendar::reconcile
```

Expected pass condition:

- strong identifier reconciliation by `UID` works
- inline `text/calendar` parts and attached `.ics` parts are both covered
- heuristic matches are surfaced as candidates, not auto-canonicalized

### Rollback plan

- revert the reconciliation layer and email-side hooks together

## Slice H

### Goal

Add read-only ICS export generated from the local mirror.

### Owned files

- `src/calendar/ics_export.rs`
- `src/api/calendar_ics.rs`

### Out of scope

- router registration and auth middleware exceptions
- dashboard wiring for copy-link UI

### Risk level

Medium

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test calendar::ics_export
cargo test api::calendar_ics
```

Expected pass condition:

- feed generation excludes deleted events
- feed generation excludes pending proposals
- token gating works

### Rollback plan

- revert ICS export files only
- leave the calendar core intact

## Slice I

### Goal

Wire all leaf modules into the application and own every shared registry file.

### Owned files

- `src/lib.rs`
- `src/main.rs`
- `src/api.rs`
- `src/api/state.rs`
- `src/api/server.rs`
- `src/api/agents.rs`
- `src/tools.rs`
- `interface/src/router.tsx`
- `interface/src/components/AgentTabs.tsx`
- `interface/src/api/client.ts`

### Out of scope

- implementing leaf behavior already owned by other slices

### Risk level

High

### Verification commands

```bash
cd /home/dylan/tools/spacebot
cargo test
just check-typegen
cd /home/dylan/tools/spacebot/interface
bun run build
```

Expected pass condition:

- the project compiles end-to-end
- calendar routes, tools, and UI are all reachable

### Rollback plan

- revert wiring-only commits first
- do not revert leaf slices unless wiring rollback cannot restore build health

## Slice J

### Goal

Finish hardening, tests, docs, and project gate evidence.

### Owned files

- `docs/design-docs/calendar-caldav-integration.md`
- `docs/design-docs/calendar-caldav-work-breakdown.md`
- any new calendar docs under `docs/content/docs/`
- test files added across owned slices by agreement

### Out of scope

- changing feature behavior without coordination

### Risk level

Low

### Verification commands

```bash
cd /home/dylan/tools/spacebot
just preflight
just gate-pr
```

Expected pass condition:

- all project gates pass
- docs reflect actual implementation boundaries

### Rollback plan

- revert docs and tests independently if needed

## Worker Assignment Pattern

Recommended parallel assignment under the current 6-worker cap:

- Worker 1: Slice A
- Worker 2: Slice B
- Worker 3: Slice C
- Worker 4: Slices D + E
- Worker 5: Slice F
- Worker 6: Slices G + H
- Integrator: Slice I
- Finisher: Slice J

If staffing is smaller:

- Pair B + C
- Pair D + E
- Pair F + H
- Keep I separate

## Dependency Notes

- Slice A is the prerequisite for all backend slices.
- Slice B is the prerequisite for C, D, E, G, and H.
- Slice E should stabilize response shapes before F merges.
- Slice I should not start until the leaf slices have clear compile targets.
- Slice J should record exact verification commands and outcomes per slice.

## Tracking Checklist

- [ ] Slice A complete
- [ ] Slice B complete
- [ ] Slice C complete
- [ ] Slice D complete
- [ ] Slice E complete
- [ ] Slice F complete
- [ ] Slice G complete
- [ ] Slice H complete
- [ ] Slice I complete
- [ ] Slice J complete

## Residual Risks

- recurrence correctness is still the highest-risk behavior area
- provider auth differences will matter once OAuth2-backed CalDAV is added
- ICS export can leak data if token handling is sloppy
- integration slice will become the choke point if leaf slices are not kept cleanly isolated

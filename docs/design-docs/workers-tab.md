# Workers Tab

A two-column worker run viewer in the agent interface. Left column lists all worker runs for the agent (across all channels), right column shows the selected worker's full transcript and event timeline. Replaces the current "coming soon" placeholder at `/agents/$agentId/workers`.

## Concept

Workers currently persist minimal data: a summary row in `worker_runs` (task, result, status, timestamps) and optional filesystem log files. The summary row is enough for the channel timeline but not for a dedicated workers page. The log files are debug artifacts — unstructured, not queryable, and may contain sensitive tool output.

The workers tab needs two things: a lightweight event timeline for live activity tracking, and the full conversation transcript for post-run inspection. The challenge is persisting the transcript without bloating the database — a single worker can generate 30-50 messages with multi-KB tool outputs.

**Solution:** store the full transcript as a single gzipped JSON blob on the `worker_runs` row at completion. No per-message rows, no normalized tables. A typical 30-message transcript compresses from ~15-50KB raw to ~3-8KB gzipped. The blob is only loaded when someone clicks into a specific worker — list queries never touch it.

Key properties:
- **Agent-scoped** — shows workers across all channels, not per-channel
- **Two-column layout** — list on left, detail on right, URL-driven selection
- **Event timeline** — `worker_events` table for lightweight lifecycle tracking (status, tool start/stop)
- **Compressed transcript** — full `Vec<Message>` serialized as gzipped JSON blob on `worker_runs`, written once at completion
- **Lazy loading** — transcript blob only fetched on detail view, never on list queries
- **Polling-driven** — fast polling (5s) on the page, no SSE plumbing needed initially

## Data Model

### Existing: `worker_runs`

Already exists in `migrations/20260213000003_process_runs.sql`.

```sql
CREATE TABLE worker_runs (
    id TEXT PRIMARY KEY,          -- WorkerId (UUID)
    channel_id TEXT,              -- nullable (standalone workers)
    task TEXT NOT NULL,
    result TEXT,
    status TEXT NOT NULL DEFAULT 'running',
    started_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMP
);
```

### New columns on `worker_runs`

```sql
ALTER TABLE worker_runs ADD COLUMN worker_type TEXT NOT NULL DEFAULT 'builtin';
ALTER TABLE worker_runs ADD COLUMN agent_id TEXT;
ALTER TABLE worker_runs ADD COLUMN transcript BLOB;
```

`transcript` stores the gzipped JSON of the worker's full message history. Written once on completion (success or failure). NULL while the worker is running.

The JSON format is a serialized array of transcript entries:

```json
[
  {
    "role": "user",
    "content": [{"type": "text", "text": "Run the test suite in ~/app"}]
  },
  {
    "role": "assistant",
    "content": [
      {"type": "tool_call", "id": "shell:0", "name": "shell", "args": "{\"command\":\"cd ~/app && pytest\"}"}
    ]
  },
  {
    "role": "user",
    "content": [
      {"type": "tool_result", "call_id": "shell:0", "text": "12 passed, 0 failed"}
    ]
  },
  {
    "role": "assistant",
    "content": [{"type": "text", "text": "All 12 tests passed."}]
  }
]
```

This is a simplified serialization of Rig's `Message` type — flattened to role + content items with discriminated types. Tool result text is truncated to 50KB per result (same cap as `SpacebotHook` already applies to broadcast events) before serialization.

### New: `worker_events`

Append-only event log per worker. Lightweight rows for live timeline tracking.

```sql
CREATE TABLE worker_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    worker_id TEXT NOT NULL,
    event_type TEXT NOT NULL,       -- 'status' | 'tool_started' | 'tool_completed' | 'error'
    data TEXT,                      -- JSON payload, varies by event_type
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (worker_id) REFERENCES worker_runs(id) ON DELETE CASCADE
);

CREATE INDEX idx_worker_events_worker ON worker_events(worker_id, created_at);
```

Event type payloads:

| event_type | data JSON |
|---|---|
| `status` | `{"status": "running pytest, 7/12 suites done"}` |
| `tool_started` | `{"tool_name": "shell", "preview": "pytest tests/"}` |
| `tool_completed` | `{"tool_name": "shell", "duration_ms": 4200}` |
| `error` | `{"message": "context overflow after 3 compaction attempts"}` |

The `data` field is intentionally loose JSON — different event types carry different payloads, and we can extend without migrations.

## Transcript Lifecycle

1. Worker starts — `worker_runs` row created with `transcript = NULL`
2. Worker runs — events flow into `worker_events` for live timeline
3. Worker completes (success or failure) — serialize `Vec<Message>` history:
   a. Convert Rig messages to the simplified transcript format
   b. Truncate tool result text (50KB cap per result, same as broadcast events)
   c. Serialize to JSON
   d. Gzip compress
   e. UPDATE `worker_runs SET transcript = ?` (fire-and-forget)
4. UI loads detail — API decompresses blob, returns structured JSON

For OpenCode workers, the transcript is the status event stream (they don't have a Rig `Vec<Message>`). The `send_status` calls already capture the meaningful state transitions. The detail view shows the event timeline for these.

## Phase 1: Persistence Layer

### 1a. Migration

New file `migrations/YYYYMMDD000001_worker_events.sql`:
- `ALTER TABLE worker_runs ADD COLUMN worker_type`
- `ALTER TABLE worker_runs ADD COLUMN agent_id`
- `ALTER TABLE worker_runs ADD COLUMN transcript`
- `CREATE TABLE worker_events` with index

### 1b. Transcript Serialization

New module `src/conversation/worker_transcript.rs`:

```rust
/// Simplified transcript entry for serialization.
#[derive(Serialize, Deserialize)]
struct TranscriptEntry {
    role: String,
    content: Vec<TranscriptContent>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TranscriptContent {
    Text { text: String },
    ToolCall { id: String, name: String, args: String },
    ToolResult { call_id: String, text: String },
}
```

Functions:
- `serialize_transcript(history: &[rig::message::Message]) -> Vec<u8>` — convert to entries, truncate tool output, JSON serialize, gzip
- `deserialize_transcript(blob: &[u8]) -> Result<Vec<TranscriptEntry>>` — gunzip, JSON parse

Tool result truncation reuses `crate::tools::truncate_output` with the existing `MAX_TOOL_OUTPUT_BYTES` (50KB) constant. Tool call args are truncated to 2KB.

### 1c. `WorkerEventLogger`

New struct in `src/conversation/history.rs`:

```rust
pub struct WorkerEventLogger {
    pool: SqlitePool,
}
```

Fire-and-forget write methods:
- `log_event(worker_id, event_type, data: serde_json::Value)`
- `log_tool_started(worker_id, tool_name, args_preview)`
- `log_tool_completed(worker_id, tool_name, duration_ms)`
- `log_status(worker_id, status)`
- `log_error(worker_id, message)`

Query methods:
- `list_worker_runs(agent_id, limit, offset, status_filter) -> Vec<WorkerRunRow>`
- `get_worker_run(worker_id) -> Option<WorkerRunRow>` (without transcript)
- `get_worker_transcript(worker_id) -> Option<Vec<u8>>` (raw blob)
- `load_worker_events(worker_id, limit) -> Vec<WorkerEventRow>`

### 1d. Event Capture

Wire event persistence into the channel's `handle_event()` in `src/agent/channel.rs`:

- `ProcessEvent::WorkerStarted` — store `agent_id` and `worker_type` on the run row
- `ProcessEvent::WorkerStatus` — log `status` event
- `ProcessEvent::WorkerComplete` — log final `status` event
- `ProcessEvent::ToolStarted` where `ProcessId::Worker(id)` — log `tool_started` event
- `ProcessEvent::ToolCompleted` where `ProcessId::Worker(id)` — log `tool_completed` event

### 1e. Transcript Persistence

In `Worker::run()` (`src/agent/worker.rs`), after the run loop completes (before returning the result):

```rust
// Serialize and persist transcript
let transcript_blob = worker_transcript::serialize_transcript(&history);
self.deps.sqlite_pool.clone(); // fire-and-forget
tokio::spawn(async move {
    sqlx::query("UPDATE worker_runs SET transcript = ? WHERE id = ?")
        .bind(&transcript_blob)
        .bind(worker_id.to_string())
        .execute(&pool)
        .await
        .ok();
});
```

This happens on both success and failure paths — failed workers have transcripts too (often more useful for debugging).

### 1f. Update `ProcessRunLogger::log_worker_started`

Extend signature to accept and persist `worker_type` and `agent_id`.

## Phase 2: API

### 2a. New Module `src/api/workers.rs`

### 2b. Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/api/agents/workers?agent_id=...&limit=50&offset=0&status=...` | List worker runs (no transcript) |
| `GET` | `/api/agents/workers/detail?agent_id=...&worker_id=...` | Run metadata + decompressed transcript |
| `GET` | `/api/agents/workers/events?agent_id=...&worker_id=...&limit=200` | Event timeline for a worker |

### 2c. Response Types

**Worker list item** (no transcript, fast query):
```json
{
  "id": "uuid",
  "task": "run the test suite",
  "status": "done",
  "worker_type": "builtin",
  "channel_id": "discord:123:456",
  "channel_name": "#general",
  "started_at": "2026-02-23T10:30:00Z",
  "completed_at": "2026-02-23T10:31:45Z",
  "event_count": 24,
  "has_transcript": true
}
```

`channel_name` resolved via LEFT JOIN to `channels` table. `has_transcript` is `transcript IS NOT NULL`.

**Worker detail** (includes transcript):
```json
{
  "id": "uuid",
  "task": "run the test suite",
  "result": "12 passed, 0 failed",
  "status": "done",
  "worker_type": "builtin",
  "channel_id": "discord:123:456",
  "channel_name": "#general",
  "started_at": "2026-02-23T10:30:00Z",
  "completed_at": "2026-02-23T10:31:45Z",
  "transcript": [
    {
      "role": "user",
      "content": [{"type": "text", "text": "Run the test suite in ~/app"}]
    },
    {
      "role": "assistant",
      "content": [{"type": "tool_call", "id": "shell:0", "name": "shell", "args": "..."}]
    }
  ]
}
```

`transcript` is null if the worker is still running or if no transcript was persisted.

**Worker event:**
```json
{
  "id": 1,
  "event_type": "tool_completed",
  "data": {"tool_name": "shell", "duration_ms": 4200},
  "created_at": "2026-02-23T10:30:12Z"
}
```

### 2d. Route Registration

Add to `src/api/server.rs`:
```rust
.route("/agents/workers", get(workers::list_workers))
.route("/agents/workers/detail", get(workers::worker_detail))
.route("/agents/workers/events", get(workers::worker_events))
```

Add `mod workers;` to `src/api.rs`.

## Phase 3: Frontend

### 3a. API Client Types

New types in `interface/src/api/client.ts`:

```typescript
interface WorkerRunInfo {
    id: string;
    task: string;
    status: string;
    worker_type: string;
    channel_id: string | null;
    channel_name: string | null;
    started_at: string;
    completed_at: string | null;
    event_count: number;
    has_transcript: boolean;
}

interface TranscriptContent =
    | { type: "text"; text: string }
    | { type: "tool_call"; id: string; name: string; args: string }
    | { type: "tool_result"; call_id: string; text: string };

interface TranscriptEntry {
    role: "user" | "assistant";
    content: TranscriptContent[];
}

interface WorkerDetailResponse {
    id: string;
    task: string;
    result: string | null;
    status: string;
    worker_type: string;
    channel_id: string | null;
    channel_name: string | null;
    started_at: string;
    completed_at: string | null;
    transcript: TranscriptEntry[] | null;
}

interface WorkerEvent {
    id: number;
    event_type: "status" | "tool_started" | "tool_completed" | "error";
    data: Record<string, unknown>;
    created_at: string;
}

interface WorkerListResponse {
    workers: WorkerRunInfo[];
    total: number;
}

interface WorkerEventsResponse {
    events: WorkerEvent[];
}
```

### 3b. `AgentWorkers` Page Component

New file `interface/src/routes/AgentWorkers.tsx`.

**Layout:**
```
┌──────────────────────────────────────────────────────────┐
│ [Search...] [Status: All ▾]              42 workers      │
├─────────────────────┬────────────────────────────────────┤
│ Worker List         │ Worker Detail                      │
│                     │                                    │
│ ▸ run test suite    │ Task: run the test suite           │
│   done · #general   │ Channel: #general · builtin        │
│   2m ago            │ Duration: 1m 45s · done            │
│                     │                                    │
│ ▸ check deployment  │ ┌─ Transcript ─────────────────┐  │
│   running · #ops    │ │                               │  │
│   just now          │ │ User:                         │  │
│                     │ │   Run the test suite in ~/app │  │
│ ▸ update docs       │ │                               │  │
│   failed · #dev     │ │ Assistant:                    │  │
│   5m ago            │ │   ▶ shell: cd ~/app && pytest │  │
│                     │ │   ✓ 12 passed, 0 failed       │  │
│                     │ │                               │  │
│                     │ │ Assistant:                    │  │
│                     │ │   All 12 tests passed.        │  │
│                     │ └───────────────────────────────┘  │
│                     │                                    │
│                     │ ┌─ Event Timeline ──────────────┐  │
│                     │ │ 10:30:00  starting             │  │
│                     │ │ 10:30:01  shell ▸              │  │
│                     │ │ 10:30:05  shell ✓ 4.2s         │  │
│                     │ │ 10:31:45  completed             │  │
│                     │ └───────────────────────────────┘  │
└─────────────────────┴────────────────────────────────────┘
```

**Left column (worker list):**
- Search input (filters task text)
- Status filter pills: All / Running / Done / Failed
- Scrollable list of worker run cards
- Each card: task (truncated), status badge (running=amber, done=green, failed=red), channel name, relative time
- Selected worker highlighted with `bg-app-selected`
- Polling: `refetchInterval: 5_000`

**Right column (worker detail):**
- Empty state when no worker selected: "Select a worker to view details"
- Header: task, channel, duration, status badge, worker type badge

- **Transcript section** (if `has_transcript` / worker completed):
  - Renders the full conversation: user prompts, assistant text, tool calls with args, tool results
  - Tool calls shown as collapsible blocks: tool name + truncated args, expand for full args and result
  - Assistant text rendered with markdown
  - Scrollable, full height available

- **Event timeline** (always available, even while running):
  - Compact chronological list below transcript (or as primary view while running)
  - `status` — text with status string
  - `tool_started` — tool name with indicator
  - `tool_completed` — tool name with duration
  - `error` — red text
  - Polling: `refetchInterval: 3_000` only while worker status is `running`

- **Result section** (if completed): markdown-rendered result text

**URL state:**
- Selected worker ID in search params: `/agents/$agentId/workers?worker=<uuid>`
- Deep links and browser back/forward

### 3c. Router Update

Replace the placeholder in `router.tsx`:

```tsx
import { AgentWorkers } from "@/routes/AgentWorkers";

const agentWorkersRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/agents/$agentId/workers",
    validateSearch: (search: Record<string, unknown>): { worker?: string } => ({
        worker: typeof search.worker === "string" ? search.worker : undefined,
    }),
    component: function AgentWorkersPage() {
        const { agentId } = agentWorkersRoute.useParams();
        return (
            <div className="flex h-full flex-col">
                <AgentHeader agentId={agentId} />
                <div className="flex-1 overflow-hidden">
                    <AgentWorkers agentId={agentId} />
                </div>
            </div>
        );
    },
});
```

## Build Order

```
Phase 1a     Migration                 standalone
Phase 1b     Transcript serialization  standalone, parallel with 1a
Phase 1c     WorkerEventLogger         depends on 1a
Phase 1d     Event capture wiring      depends on 1c
Phase 1e     Transcript persistence    depends on 1a, 1b
Phase 1f     log_worker_started update depends on 1a
Phase 2a-2d  API endpoints             depends on 1b, 1c
Phase 3a     Client types              depends on 2
Phase 3b-3c  UI components             depends on 3a
```

Phase 1b (transcript serialization) and Phase 1c (event logger) are independent and can run in parallel. Phase 3 is entirely frontend.

## File Changes

**New files:**
- `migrations/YYYYMMDD000001_worker_events.sql` — new table + alter existing table
- `src/conversation/worker_transcript.rs` — transcript serialization/deserialization
- `src/api/workers.rs` — list, detail, events handlers
- `interface/src/routes/AgentWorkers.tsx` — two-column workers page

**Modified files:**
- `src/api.rs` — add `mod workers`
- `src/api/server.rs` — register three new routes
- `src/conversation.rs` — add `pub mod worker_transcript`
- `src/conversation/history.rs` — `WorkerEventLogger`, extend `log_worker_started`
- `src/agent/channel.rs` — wire event persistence into `handle_event()`
- `src/agent/worker.rs` — persist transcript blob on completion
- `interface/src/api/client.ts` — new types + api methods
- `interface/src/router.tsx` — replace placeholder, add search param validation

## Notes

- Transcript blob is written once on completion — no incremental updates. While a worker is running, the UI uses the event timeline for live tracking. Once done, the full transcript becomes available.
- Compression ratio is favorable: JSON with repetitive key names and ASCII tool output compresses well with gzip. Measured ~5x compression on real worker logs.
- The transcript serialization truncates tool results at 50KB (reusing the existing `MAX_TOOL_OUTPUT_BYTES` constant) and tool args at 2KB. This caps worst-case uncompressed size at ~1.5MB for a 30-tool-call worker, which compresses to ~200KB. Typical workers are much smaller.
- Old transcripts can be pruned by a future maintenance task (e.g. "delete transcript blobs older than 30 days"). The `worker_runs` summary row survives — only the blob is nulled.
- OpenCode workers don't have a Rig `Vec<Message>` — their detail view shows the event timeline as the primary content. The transcript column stays NULL for these.
- The `worker_events` table serves double duty: live timeline while running, and a lightweight audit trail after completion. The transcript is the deep-dive view.
- Event timeline is append-only. The `data` column uses unstructured JSON to avoid migrations when adding new event fields.
- Tool argument previews in `tool_started` events are truncated to 200 chars before persistence.
- Channel name resolution uses a LEFT JOIN to the `channels` table. Standalone workers (channel_id IS NULL) show no channel label.

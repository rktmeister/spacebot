# Metrics Reference

Comprehensive reference for Spacebot's Prometheus metrics. For quick-start setup, see `docs/metrics.md`.

## Feature Gate

All telemetry code is behind the `metrics` cargo feature flag. Without it, every `#[cfg(feature = "metrics")]` block compiles out to nothing — zero runtime cost.

```bash
cargo build --release --features metrics
```

The `[metrics]` config block is always parsed (so config validation works) but has no effect without the feature.

## Metric Inventory

All metrics are prefixed with `spacebot_`. The registry uses a private `prometheus::Registry` (not the default global one) to avoid conflicts with other libraries.

### Counters

#### `spacebot_llm_requests_total`

| Field | Value |
|-------|-------|
| Type | `IntCounterVec` |
| Labels | `agent_id`, `model`, `tier` |
| Instrumented in | `src/llm/model.rs` — `SpacebotModel::completion()` |
| Description | Total LLM completion requests (one per `completion()` call, including retries and fallbacks). |

**Cardinality:** `agents × models × tiers`. Currently `agent_id` and `tier` are hardcoded to `"unknown"` because `SpacebotModel` doesn't carry process context. Effective cardinality is just the number of distinct model names (typically 5–15). Once agent context is threaded through, expect `agents(1–5) × models(5–15) × tiers(5)` = 25–375 series.

**Known limitation:** Labels `agent_id` and `tier` are always `"unknown"`. The `SpacebotHook` has these values but can't be used here without structural changes.

#### `spacebot_tool_calls_total`

| Field | Value |
|-------|-------|
| Type | `IntCounterVec` |
| Labels | `agent_id`, `tool_name` |
| Instrumented in | `src/hooks/spacebot.rs` — `SpacebotHook::on_tool_result()` |
| Description | Total tool calls executed across all processes. Incremented after each tool call completes (success or failure). |

**Cardinality:** `agents × tools`. With 1–5 agents and ~20 tool names, expect 20–100 series. Tool names are a bounded set defined in `src/tools/`.

#### `spacebot_memory_reads_total`

| Field | Value |
|-------|-------|
| Type | `IntCounter` (no labels) |
| Instrumented in | `src/tools/memory_recall.rs` — `MemoryRecallTool::call()` |
| Description | Total successful memory recall (search) operations. |

**Cardinality:** 1 series.

#### `spacebot_memory_writes_total`

| Field | Value |
|-------|-------|
| Type | `IntCounter` (no labels) |
| Instrumented in | `src/tools/memory_save.rs` — `MemorySaveTool::call()` |
| Description | Total successful memory save operations. |

**Cardinality:** 1 series.

### Histograms

#### `spacebot_llm_request_duration_seconds`

| Field | Value |
|-------|-------|
| Type | `HistogramVec` |
| Labels | `agent_id`, `model`, `tier` |
| Buckets | 0.1, 0.25, 0.5, 1, 2.5, 5, 10 |
| Instrumented in | `src/llm/model.rs` — `SpacebotModel::completion()` |
| Description | End-to-end LLM request duration in seconds. Includes retry loops and fallback chain traversal. |

**Cardinality:** Same as `spacebot_llm_requests_total` (per-bucket overhead is fixed, not per-series).

**Known limitation:** Buckets max out at 10s. LLM requests with retries and fallbacks routinely exceed 10s (15–60s is common). Everything above 10s collapses into the +Inf bucket, losing resolution. A future fix should extend buckets to cover `[..., 15, 30, 60, 120]`.

**What the timer measures:** The timer wraps the entire `completion()` method body, including all retry attempts on the primary model and the full fallback chain. This measures user-perceived latency, not individual provider call latency.

#### `spacebot_tool_call_duration_seconds`

| Field | Value |
|-------|-------|
| Type | `Histogram` (no labels) |
| Buckets | 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30 |
| Instrumented in | `src/hooks/spacebot.rs` — `on_tool_call()` starts timer, `on_tool_result()` observes |
| Description | Tool call execution duration in seconds. |

**Cardinality:** 1 series.

**Implementation note:** Duration is tracked via a `LazyLock<Mutex<HashMap<String, Instant>>>` static keyed by Rig's internal call ID. The timer starts in `on_tool_call` and is consumed in `on_tool_result`. If a tool call starts but the agent terminates before `on_tool_result` fires (e.g. leak detection terminates the agent), the timer entry remains in the map. These orphaned entries are small (String + Instant) and bounded by concurrent tool calls, so this is not a practical concern.

### Gauges

#### `spacebot_active_workers`

| Field | Value |
|-------|-------|
| Type | `IntGaugeVec` |
| Labels | `agent_id` |
| Instrumented in | `src/agent/channel.rs` — `spawn_worker_task()` |
| Description | Currently active workers. Incremented when a worker task is spawned, decremented when it completes (success or failure). Covers both builtin Rig workers and OpenCode workers. |

**Cardinality:** Number of agents (typically 1–5).

#### `spacebot_memory_entry_count`

| Field | Value |
|-------|-------|
| Type | `IntGaugeVec` |
| Labels | `agent_id` |
| Instrumented in | **Not instrumented.** Defined in registry but not wired to any call site. |
| Description | Intended to track total memory entries per agent. |

**Cardinality:** Number of agents (typically 1–5). Currently always 0.

**Status:** Requires periodic store queries or integration into `MemoryStore::save()` / `MemoryStore::delete()` to maintain an accurate count. Not blocked for merge — the metric is registered but idle.

## Total Cardinality

With the current instrumentation (hardcoded `"unknown"` labels on LLM metrics):

| Metric | Series estimate |
|--------|-----------------|
| `llm_requests_total` | ~10 (distinct models) |
| `tool_calls_total` | ~20–100 (agents × tools) |
| `memory_reads_total` | 1 |
| `memory_writes_total` | 1 |
| `llm_request_duration_seconds` | ~10 (distinct models) |
| `tool_call_duration_seconds` | 1 |
| `active_workers` | ~1–5 (agents) |
| `memory_entry_count` | 0 (not instrumented) |
| **Total** | **~45–130** |

This is well within safe operating range for any Prometheus deployment.

## Feature Gate Consistency

Every instrumentation call site uses `#[cfg(feature = "metrics")]` at the statement or block level:

| File | Gate type |
|------|-----------|
| `src/lib.rs` | `#[cfg(feature = "metrics")] pub mod telemetry` |
| `src/main.rs` | `#[cfg(feature = "metrics")] let _metrics_handle = ...` |
| `src/llm/model.rs` | `#[cfg(feature = "metrics")] let start` + `#[cfg(feature = "metrics")] { ... }` |
| `src/hooks/spacebot.rs` | `#[cfg(feature = "metrics")] static TOOL_CALL_TIMERS` + 2 blocks |
| `src/tools/memory_save.rs` | `#[cfg(feature = "metrics")] crate::telemetry::Metrics::global()...` |
| `src/tools/memory_recall.rs` | `#[cfg(feature = "metrics")] crate::telemetry::Metrics::global()...` |
| `src/agent/channel.rs` | `#[cfg(feature = "metrics")] ...` (×2, inc + dec) |
| `Cargo.toml` | `prometheus = { version = "0.13", optional = true }`, `metrics = ["dep:prometheus"]` |

All consistent. No path references `crate::telemetry` without a `cfg` gate.

## Endpoints

| Path | Response |
|------|----------|
| `/metrics` | Prometheus text exposition format (0.0.4) |
| `/health` | `200 OK` (liveness probe) |

The metrics server binds to a configurable address (default `0.0.0.0:9090`), separate from the main API server (`127.0.0.1:19898`).

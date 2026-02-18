# Metrics

Prometheus-compatible metrics endpoint. Opt-in via the `metrics` cargo feature flag.

## Building with Metrics

```bash
cargo build --release --features metrics
```

Without the feature flag, all telemetry code is compiled out. The `[metrics]` config block is parsed regardless but has no effect.

## Configuration

```toml
[metrics]
enabled = true
port = 9090
bind = "0.0.0.0"
```

| Key       | Default     | Description                          |
| --------- | ----------- | ------------------------------------ |
| `enabled` | `false`     | Enable the /metrics HTTP endpoint    |
| `port`    | `9090`      | Port for the metrics server          |
| `bind`    | `"0.0.0.0"` | Address to bind the metrics server  |

The metrics server runs as a separate tokio task alongside the main API server. It shuts down gracefully with the rest of the process.

## Endpoints

| Path       | Description                              |
| ---------- | ---------------------------------------- |
| `/metrics` | Prometheus text exposition format (0.0.4)|
| `/health`  | Returns 200 OK (for liveness probes)     |

## Exposed Metrics

All metrics are prefixed with `spacebot_`.

### Counters

| Metric                         | Labels                    | Description                      |
| ------------------------------ | ------------------------- | -------------------------------- |
| `spacebot_llm_requests_total`  | agent_id, model, tier     | Total LLM completion requests    |
| `spacebot_tool_calls_total`    | agent_id, tool_name       | Total tool calls executed        |
| `spacebot_memory_reads_total`  |                           | Total memory recall operations   |
| `spacebot_memory_writes_total` |                           | Total memory save operations     |

The `tier` label corresponds to the process type making the request: `channel`, `branch`, `worker`, `compactor`, or `cortex`.

### Histograms

| Metric                                    | Labels                | Buckets (seconds)                          |
| ----------------------------------------- | --------------------- | ------------------------------------------ |
| `spacebot_llm_request_duration_seconds`   | agent_id, model, tier | 0.1, 0.25, 0.5, 1, 2.5, 5, 10            |
| `spacebot_tool_call_duration_seconds`     |                       | 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30 |

### Gauges

| Metric                         | Labels   | Description                     |
| ------------------------------ | -------- | ------------------------------- |
| `spacebot_active_workers`      | agent_id | Currently active workers        |
| `spacebot_memory_entry_count`  | agent_id | Total memory entries per agent  |

## Prometheus Scrape Config

```yaml
scrape_configs:
  - job_name: spacebot
    scrape_interval: 15s
    static_configs:
      - targets: ["localhost:9090"]
```

## Docker

Expose the metrics port alongside the API port:

```bash
docker run -d \
  --name spacebot \
  -e ANTHROPIC_API_KEY="sk-ant-..." \
  -v spacebot-data:/data \
  -p 19898:19898 \
  -p 9090:9090 \
  ghcr.io/spacedriveapp/spacebot:slim
```

The Docker image must be built with `--features metrics` for this to work. The default images do not include metrics support.

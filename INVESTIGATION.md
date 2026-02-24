# Memory Usage Investigation Report

Date: 2026-02-24  
Repo: `cli_ob`  
Scope: Deep dive into memory growth beyond the expected 5-minute data window

## Executive Summary

The memory growth is real and reproducible. In `release` mode, resident memory (RSS) continued rising well past 5 minutes and exceeded ~1.1 GB during observation.

This is not caused by a single leak point. The behavior is driven by:

1. Large per-frame allocation churn in the UI thread.
2. Recreating heatmap textures every frame using `load_texture`.
3. Retained historical structures whose per-item size can grow over time.
4. Potential producer/consumer imbalance due to unbounded channels.

## Runtime Evidence

Sampled process RSS every 5 seconds while running `target/release/cli_ob`:

- 09:11:57: `196,576 KB` (~192 MB)
- 09:13:53: `569,120 KB` (~556 MB)
- 09:15:30: `798,512 KB` (~780 MB)
- 09:16:30: `1,010,928 KB` (~987 MB)
- 09:17:00: `1,168,688 KB` (~1.14 GB)

Conclusion from sampling: memory keeps increasing after 5 minutes.

## Root-Cause Analysis (Ranked)

### 1) Per-frame deep clone of large shared state

The UI clones the full shared state every frame (~20 FPS):

- Lock + clone: `src/ui.rs:568-572`
- Deep copies include:
  - trades (`trade_history`) at `src/ui.rs:2980-2985`
  - depth slices (`depth_history`) at `src/ui.rs:2987`
  - fill/kill history at `src/ui.rs:2988-2994`
  - cumulative series at `src/ui.rs:2995-3001`

Impact:

- High transient allocation rate.
- Frequent growth of allocator arenas/high-water RSS.
- Lock held during expensive clone work, which can increase contention and downstream backlog.

### 2) Heatmap texture recreated every frame via `load_texture`

Heatmap path currently does:

- Rebuild image each frame, then
- `ctx.load_texture("heatmap", image, ...)` every frame

Location: `src/ui.rs:2547-2552`.

Important upstream note from egui:

- `load_texture` should be called once per image, not in main GUI loop.
- Reference: `egui-0.33.3/src/context.rs:2217` (warning comment in docs).

Impact:

- Repeated texture allocations/updates.
- Significant memory + GPU upload churn and additional allocator pressure.

### 3) DepthHistory count is bounded, but slice payload size is not bounded

Depth ring buffer is capped:

- `max_slices = 600` (5 min @ 500ms) at `src/models.rs:383`.
- Push logic pops old slices when exceeding cap: `src/models.rs:389-393`.

But each slice stores **all current book levels**:

- Snapshot builder iterates full bids+asks and pushes all levels: `src/models.rs:398-405`.

Impact:

- Even with fixed slice count, memory can keep rising if average levels/slice rise.

### 4) OrderBook level count can grow over session

Depth updates insert new price levels and remove only explicit zero-qty levels:

- `apply_update`: `src/models.rs:114-137`.

No explicit depth pruning policy is present (e.g., keep only ±N ticks or top K per side).

Impact:

- Book maps may grow in cardinality depending on stream behavior.
- Growth propagates into snapshot sizes and clone costs.

### 5) Unbounded websocket channels can accumulate backlog

Channels are unbounded:

- depth channel: `src/main.rs:326`
- trade channel: `src/main.rs:327`

Impact:

- If consumer loop can’t keep up (especially with lock contention from UI clones), queued messages can grow without hard memory ceiling.

### 6) 5-minute expectation does not cover all series

Only some structures are 5-minute constrained:

- Trades: `TradeHistory::new(300_000)` in `src/models.rs:431`.
- Depth slices: ring of 600 at 500ms (5 min).

Micro metrics histories are capped by sample count (`10_000`), not time:

- `FILL_KILL_MAX_SAMPLES = 10_000`: `src/micro.rs:5`
- `CUMULATIVE_MAX_SAMPLES = 10_000`: `src/micro.rs:7`

UI rolling mode is view filtering only:

- `Rolling5m` uses time-domain slicing in chart render path: `src/ui.rs:1998-2018`.

Impact:

- “5-minute view” does not imply “5-minute storage” for all metrics.

## Why Growth Continues After 5 Minutes

The 5-minute cap limits count for selected buffers, but not total bytes:

- Bytes per depth slice can increase.
- Full history is still cloned every frame.
- Texture/image allocations occur every frame.
- Allocator high-water behavior can make RSS appear continuously increasing even when some memory is reusable internally.

## Architectural Conclusion

Observed behavior is best explained by combined retained-growth + allocation churn:

- Retained growth: potentially larger order book + large depth snapshots.
- Churn growth: per-frame deep copies + per-frame texture allocation path.

This explains continued RSS increase beyond the 5-minute mark.

## Evidence References (Code)

- UI state clone per frame: `src/ui.rs:568-572`, `src/ui.rs:2937-3025`
- Heatmap rebuild/load every frame: `src/ui.rs:2547-2552`
- Depth ring retention: `src/models.rs:372-395`
- Depth snapshot all levels: `src/models.rs:398-405`
- Order book update behavior: `src/models.rs:114-137`
- Unbounded channels: `src/main.rs:326-327`
- Micro history caps: `src/micro.rs:5-8`, `src/micro.rs:276-281`, `src/micro.rs:54-59`
- Rolling 5m chart window (view only): `src/ui.rs:1967-2018`

## Final Finding

The memory increase is expected from current implementation patterns and is not limited by the 5-minute retention in a way that guarantees stable RSS. The system needs targeted changes in texture lifecycle, snapshot/cloning strategy, and bounded buffering/pruning to achieve stable long-running memory behavior.

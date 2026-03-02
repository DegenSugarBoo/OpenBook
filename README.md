# cli_ob — Real-Time Crypto Order Book GUI

A high-performance, Rust desktop app for visualizing **Binance Futures** market microstructure in real time. It streams live depth + trade data, renders a Bookmap-style depth heatmap, and includes dockable analytics panes for order flow and execution impact.

<video src="assets/demo.mov" controls></video>

---

## Features

- **Real-time Binance Futures streaming** via WebSocket (`@depth@100ms`, `@aggTrade`)
- **Order book sync engine** with REST snapshot + contiguous diff-depth update handling
- **Bookmap-style depth heatmap** with event-driven history replay (checkpoint + delta model)
- **Live trade tape** with side coloring, min-notional filter, and adjustable row cap
- **Market impact estimator** for configurable notional and buy/sell side
- **Fill:Kill analytics** pane with event chart, cumulative chart, ratio states, and overfill highlighting
- **Dockable multi-pane workspace** (Heatmap, Order Book, Market Impact, Fill:Kill, Trades Tape)
- **Layout profiles** with save/load/save-as/rename/delete and automatic migration from legacy layout formats
- **Symbol picker** with searchable USDT perpetual catalog + live 24h mini-ticker data
- **Adaptive rendering cadence** (higher FPS during interaction, lower FPS when idle)
- **Performance overlay** for frame timing and heatmap rebuild metrics

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- Desktop environment capable of running native `eframe` windows (macOS/Linux/Windows)

### Build & Run

```bash
# Clone the repo
git clone https://github.com/<your-username>/cli_ob.git
cd cli_ob

# Build (release mode recommended)
cargo build --release

# Run
cargo run --release
```

### Dev Commands

```bash
# Compile check
cargo check

# Lint (warnings denied)
cargo clippy -- -D warnings

# Format
cargo fmt

# Tests
cargo test
```

On startup, the app auto-connects to `btcusdt` and opens the default workspace layout.

## Controls

| Area | Interaction |
|-----|--------|
| Header | Enter symbol in picker, then `Connect` |
| Symbol picker | `ArrowUp` / `ArrowDown` navigate, `Enter` select/connect, `Esc` close |
| Layout menu | Toggle pane visibility, save/load layout profiles, reset layout |
| Heatmap | Mouse wheel zoom (price/time), drag pan, double-click reset view |
| Trades tape | Configure row cap and minimum notional filter |
| Market impact | Edit notional and switch Buy/Sell side |

## Architecture

```
src/
├── main.rs       # Entry point + WS orchestration + snapshot sync + reconnect logic
├── models.rs     # Core data models (OrderBook, trade/depth history, shared state, WS/REST types)
├── ui.rs         # egui/eframe UI, pane rendering, heatmap image build, snapshot cloning
├── micro.rs      # Fill:Kill burst logic, rolling KPIs, cumulative series math
└── workspace.rs  # Dock layout tree, pane definitions, profile persistence + migration
```

### Data Flow

```
Binance WS/REST ──► background tokio runtime (std::thread)
                       │
                       ├─ depth updates ─► order book apply + depth event history
                       ├─ agg trades   ─► trade history + micro metrics (Fill:Kill)
                       └─ miniTicker   ─► symbol picker live catalog rows

UI thread (egui) ──► clone_snapshot() ──► pane rendering + heatmap texture updates
```

1. **Connection task (`spawn_ws_task`)** connects to Binance depth/trade streams.
2. **Snapshot sync** fetches REST depth snapshot, bridges buffered WS diffs, then enforces contiguous updates.
3. **State updates** mutate `SharedState` (`OrderBook`, `EventDepthHistory`, `TradeHistory`, `MicroMetrics`) behind `Arc<Mutex<_>>`.
4. **UI frame loop** clones immutable snapshot data and renders panes; heatmap texture rebuilds only when render inputs change.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `eframe` / `egui` | Native GUI framework and rendering |
| `egui_tiles` | Dockable pane layout/workspace management |
| `tokio` | Async runtime for WS/HTTP background tasks |
| `tokio-tungstenite` | WebSocket connectivity |
| `reqwest` | Binance REST API calls (snapshot, exchange info, time, ticker snapshot) |
| `serde` / `serde_json` | JSON deserialization |
| `ordered-float` | Ordered `f64` keys for `BTreeMap` price levels |
| `futures-util` | Stream utilities |
| `dhat` (optional feature) | Heap profiling support |

## Known Issues

- Advanced zoom/pan ergonomics can still be improved for dense books.
- Hover detail and visual ergonomics are still being iterated.
- Memory bloat issues still persist,on active markets memory usage climbs to ~600MB on my device
- Depth updated sometimes lag when a burst order appears,which I really cant help with cause trades data is realtime,while depth data has a 100ms update interval
## License

This project is licensed under the [MIT License](LICENSE).

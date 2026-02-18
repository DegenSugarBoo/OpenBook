# cli_ob — Real-Time Crypto Order Book TUI

A blazing-fast, terminal-based order book viewer for **Binance Futures** built in Rust. Streams live depth data and aggregated trades over WebSocket, renders a full-featured TUI with [ratatui](https://github.com/ratatui-org/ratatui).

![Preview](assets/preview.png)

---

## Features

- **Real-time depth streaming** via Binance Futures WebSocket (`@depth@100ms`)
- **Live trade feed** via `@aggTrade` stream with buy/sell classification
- **Configurable price binning** — aggregate levels by custom bucket width
- **Depth visualization** — cumulative + individual quantity bars for bids & asks
- **Price chart** — 60-second rolling chart with trade bubble markers and volume bars
- **Bid/Ask imbalance** indicator
- **Spread & latency** monitoring with automatic clock offset calibration
- **Snapshot + incremental sync** following the official Binance diff-depth protocol
- **20 FPS rendering** with profiling-grade performance counters

## Getting Started

### Prerequisites

- [Rust](https://rustup.rs/) (1.70+ recommended)
- A terminal emulator with **true-color** support (iTerm2, Alacritty, Kitty, WezTerm, etc.)

### Build & Run

```bash
# Clone the repo
git clone https://github.com/<your-username>/cli_ob.git
cd cli_ob

# Build in release mode (recommended for best perf)
cargo build --release

# Run
cargo run --release
```

On launch you'll be prompted to enter:

1. **Trading symbol** — e.g. `btcusdt`, `ethusdt`, `solusdt` (defaults to `btcusdt`)
2. **Price bin width** — e.g. `1`, `10`, `100` (defaults to `1.0`)

### Controls

| Key | Action |
|-----|--------|
| `q` | Quit |
| `Ctrl+C` | Quit |

## Architecture

```
src/
├── main.rs          # Entry point, WebSocket handling, event loop
├── config.rs        # Interactive user configuration prompt
├── models.rs        # OrderBook, TradeHistory, profiling structs
├── ui.rs            # ratatui rendering — depth bars, price chart, layout
└── terminal_ui.rs   # Terminal setup / teardown helpers
```

### Data Flow

```
Binance WS ──► parse ──► mpsc channel ──► sync engine ──► RwLock<OrderBook>
                                                              │
                                          render loop (50ms) ─┘──► ratatui
```

1. **WebSocket task** receives `@depth` and `@aggTrade` messages, deserializes, and sends through unbounded channels.
2. **Main loop** drains the channels, applies incremental updates to the order book (after snapshot sync), and collects trades.
3. **Renderer** reads the order book and trade history under `RwLock`, aggregates into buckets, and draws the TUI at ~20 FPS.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `tokio-tungstenite` | WebSocket client |
| `ratatui` | Terminal UI framework |
| `crossterm` | Terminal control & input |
| `serde` / `serde_json` | JSON deserialization |
| `reqwest` | REST API calls (snapshot, clock sync) |
| `ordered-float` | `BTreeMap` keys for price levels |
| `futures-util` | Stream combinators |

## Known Issues

- The trades are not properly displayed
- The volume plots are also not good 
- I am looking to go the full gui route,cause I do not know how to improve the terminal ui more

## License

This project is licensed under the [MIT License](LICENSE).

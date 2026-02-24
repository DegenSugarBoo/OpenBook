mod micro;
mod models;
mod ui;
mod workspace;

use models::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const DEFAULT_TICK_SIZE: f64 = 0.1;
const DEFAULT_PRICE_DECIMALS: usize = 1;
const MAX_PRICE_DECIMALS: usize = 8;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("Order Book Viewer"),
        ..Default::default()
    };

    eframe::run_native(
        "Order Book Viewer",
        native_options,
        Box::new(|cc| Ok(Box::new(ui::OrderBookApp::new(cc)))),
    )
}

/// Spawns the background tokio runtime that handles WebSocket streaming.
pub fn spawn_ws_task(
    symbol: String,
    bucket_size: f64,
    shared: Arc<Mutex<SharedState>>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async move {
            ws_loop(symbol, bucket_size, shared, shutdown_flag).await;
        });
    });
}

#[derive(Debug, serde::Deserialize)]
struct RestTicker24hr {
    pub symbol: String,
    #[serde(rename = "lastPrice")]
    pub last_price: String,
    #[serde(rename = "openPrice")]
    pub open_price: String,
    #[serde(rename = "quoteVolume")]
    pub quote_volume: String,
    #[serde(rename = "closeTime", default)]
    pub close_time: u64,
}

/// Fetch full exchange info and filter to USDT perpetual trading symbols.
async fn fetch_catalog(picker_state: &Arc<Mutex<PickerSharedState>>) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = "https://fapi.binance.com/fapi/v1/exchangeInfo";
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let info: ExchangeInfoResponse = resp.json().await.map_err(|e| e.to_string())?;

    let catalog: Vec<SymbolCatalogEntry> = info
        .symbols
        .iter()
        .filter(|s| {
            s.quote_asset == "USDT" && s.contract_type == "PERPETUAL" && s.status == "TRADING"
        })
        .map(|s| SymbolCatalogEntry {
            symbol: s.symbol.clone(),
            base_asset: s.base_asset.clone(),
            quote_asset: s.quote_asset.clone(),
        })
        .collect();

    let mut state = picker_state.lock().unwrap();
    state.catalog = catalog;
    Ok(())
}

/// Optional startup snapshot so picker rows get data before the first WS tick.
async fn fetch_initial_ticker_snapshot(
    picker_state: &Arc<Mutex<PickerSharedState>>,
    valid_symbols: &std::collections::HashSet<String>,
) -> Result<(), String> {
    let client = reqwest::Client::new();
    let url = "https://fapi.binance.com/fapi/v1/ticker/24hr";
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let tickers: Vec<RestTicker24hr> = resp.json().await.map_err(|e| e.to_string())?;

    let mut state = picker_state.lock().unwrap();
    let mut updated = 0_u64;
    for ticker in tickers {
        if !valid_symbols.contains(&ticker.symbol) {
            continue;
        }
        if let (Ok(last_price), Ok(open_24h), Ok(quote_volume_24h)) = (
            ticker.last_price.parse::<f64>(),
            ticker.open_price.parse::<f64>(),
            ticker.quote_volume.parse::<f64>(),
        ) {
            let change_pct_24h = if open_24h > 0.0 {
                ((last_price - open_24h) / open_24h) * 100.0
            } else {
                0.0
            };
            let live = state.live_tickers.entry(ticker.symbol).or_default();
            live.last_price = last_price;
            live.open_24h = open_24h;
            live.change_pct_24h = change_pct_24h;
            live.quote_volume_24h = quote_volume_24h;
            live.event_time_ms = ticker.close_time;
            updated = updated.wrapping_add(1);
        }
    }
    if updated > 0 {
        state.ticker_epoch = state.ticker_epoch.wrapping_add(1);
    }
    Ok(())
}

/// Spawns the background tokio runtime that handles picker ticker streaming.
pub fn spawn_ticker_task(
    picker_state: Arc<Mutex<PickerSharedState>>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create ticker tokio runtime");
        rt.block_on(async move {
            ticker_ws_loop(picker_state, shutdown_flag).await;
        });
    });
}

async fn ticker_ws_loop(
    picker_state: Arc<Mutex<PickerSharedState>>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    if let Err(err) = fetch_catalog(&picker_state).await {
        let mut state = picker_state.lock().unwrap();
        state.status = PickerStatus::Error(format!("Catalog fetch failed: {err}"));
        return;
    }

    let valid_symbols: std::collections::HashSet<String> = {
        let state = picker_state.lock().unwrap();
        state
            .catalog
            .iter()
            .map(|entry| entry.symbol.clone())
            .collect()
    };

    if let Err(err) = fetch_initial_ticker_snapshot(&picker_state, &valid_symbols).await {
        eprintln!("Initial ticker snapshot failed: {err}");
    }

    loop {
        if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        let ws_url = "wss://fstream.binance.com/stream?streams=!miniTicker@arr";
        let ws_result = connect_async(ws_url).await;
        let (ws_stream, _) = match ws_result {
            Ok(stream) => stream,
            Err(err) => {
                {
                    let mut state = picker_state.lock().unwrap();
                    state.status = PickerStatus::Reconnecting;
                }
                eprintln!("Ticker WS connect error: {err}, retrying in 3s...");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        {
            let mut state = picker_state.lock().unwrap();
            state.status = PickerStatus::Live;
        }

        let (_write, mut read) = ws_stream.split();
        while let Some(msg_result) = read.next().await {
            if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }

            match msg_result {
                Ok(Message::Text(text)) => {
                    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(data) = wrapper.get("data") {
                            if let Ok(tickers) =
                                serde_json::from_value::<Vec<WsMiniTicker>>(data.clone())
                            {
                                let mut state = picker_state.lock().unwrap();
                                let mut changed = false;
                                for ticker in tickers {
                                    if !valid_symbols.contains(&ticker.symbol) {
                                        continue;
                                    }
                                    if let (Ok(last_price), Ok(open_24h), Ok(quote_volume_24h)) = (
                                        ticker.close_price.parse::<f64>(),
                                        ticker.open_price.parse::<f64>(),
                                        ticker.quote_volume.parse::<f64>(),
                                    ) {
                                        let change_pct_24h = if open_24h > 0.0 {
                                            ((last_price - open_24h) / open_24h) * 100.0
                                        } else {
                                            0.0
                                        };
                                        let live =
                                            state.live_tickers.entry(ticker.symbol).or_default();
                                        live.last_price = last_price;
                                        live.open_24h = open_24h;
                                        live.change_pct_24h = change_pct_24h;
                                        live.quote_volume_24h = quote_volume_24h;
                                        live.event_time_ms = ticker.event_time;
                                        changed = true;
                                    }
                                }
                                if changed {
                                    state.ticker_epoch = state.ticker_epoch.wrapping_add(1);
                                    state.status = PickerStatus::Live;
                                }
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }

        {
            let mut state = picker_state.lock().unwrap();
            state.status = PickerStatus::Stale;
        }
        eprintln!("Ticker WS disconnected, reconnecting in 3s...");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn apply_depth_update(state: &mut SharedState, update: &WsDepthUpdate) {
    let tick_size = state.tick_size;
    state.order_book.apply_update(update);
    state.depth_epoch = state.depth_epoch.wrapping_add(1);
    state
        .micro_metrics
        .on_depth_epoch_advance(update.event_time, state.depth_epoch, tick_size);
}

fn can_bridge_snapshot(update: &WsDepthUpdate, snapshot_id: u64) -> bool {
    update.first_update_id <= snapshot_id && update.final_update_id >= snapshot_id
}

fn is_contiguous(update: &WsDepthUpdate, last_u: u64) -> bool {
    update.prev_final_update_id == last_u
}

fn derive_price_decimals(tick_size_raw: &str) -> usize {
    let trimmed = tick_size_raw.trim();
    let decimals = trimmed
        .split_once('.')
        .map(|(_, fractional)| fractional.trim_end_matches('0').len())
        .unwrap_or(0);
    decimals.min(MAX_PRICE_DECIMALS)
}

fn parse_tick_size_and_decimals(tick_size_raw: &str) -> Option<(f64, usize)> {
    let tick_size = tick_size_raw.trim().parse::<f64>().ok()?;
    if tick_size <= 0.0 {
        return None;
    }
    Some((tick_size, derive_price_decimals(tick_size_raw)))
}

fn current_time_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn ws_loop(
    symbol: String,
    _bucket_size: f64,
    shared: Arc<Mutex<SharedState>>,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) {
    // Update status
    {
        let mut state = shared.lock().unwrap();
        state.status_msg = format!("Connecting to {}...", symbol.to_uppercase());
    }

    let ws_url = format!(
        "wss://fstream.binance.com/stream?streams={}@depth@100ms/{}@aggTrade",
        symbol, symbol
    );

    let ws_result = connect_async(&ws_url).await;
    let (ws_stream, _) = match ws_result {
        Ok(s) => s,
        Err(e) => {
            let mut state = shared.lock().unwrap();
            state.status_msg = format!("WebSocket error: {}", e);
            return;
        }
    };

    let (_write, mut read) = ws_stream.split();

    {
        let mut state = shared.lock().unwrap();
        state.status_msg = "Connected. Fetching snapshot...".to_string();
    }

    // Buffer WS events while we fetch snapshot
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WsDepthUpdate>(2048);
    let (trade_tx, mut trade_rx) = tokio::sync::mpsc::channel::<Trade>(8192);

    let tx_clone = tx.clone();
    let trade_tx_clone = trade_tx.clone();
    let shutdown_ws = Arc::clone(&shutdown_flag);
    tokio::spawn(async move {
        while let Some(msg_result) = read.next().await {
            if shutdown_ws.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match msg_result {
                Ok(Message::Text(text)) => {
                    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(stream) = wrapper.get("stream").and_then(|s| s.as_str()) {
                            if let Some(data) = wrapper.get("data") {
                                if stream.contains("@depth") {
                                    if let Ok(update) =
                                        serde_json::from_value::<WsDepthUpdate>(data.clone())
                                    {
                                        if tx_clone.send(update).await.is_err() {
                                            break;
                                        }
                                    }
                                } else if stream.contains("@aggTrade") {
                                    if let Ok(agg_trade) =
                                        serde_json::from_value::<WsAggTrade>(data.clone())
                                    {
                                        if let (Ok(price), Ok(qty)) = (
                                            agg_trade.price.parse::<f64>(),
                                            agg_trade.quantity.parse::<f64>(),
                                        ) {
                                            let trade = Trade {
                                                timestamp_ms: agg_trade.trade_time,
                                                price,
                                                quantity: qty,
                                                is_buy: !agg_trade.is_buyer_maker,
                                            };
                                            if trade_tx_clone.send(trade).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(_) => break,
                _ => {}
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Fetch REST snapshot
    let client = reqwest::Client::new();
    let symbol_upper = symbol.to_uppercase();
    let rest_url = format!(
        "https://fapi.binance.com/fapi/v1/depth?symbol={}&limit=1000",
        symbol_upper
    );

    let snapshot: RestDepthResponse = match client.get(&rest_url).send().await {
        Ok(resp) => match resp.json().await {
            Ok(s) => s,
            Err(e) => {
                let mut state = shared.lock().unwrap();
                state.status_msg = format!("Snapshot parse error: {}", e);
                return;
            }
        },
        Err(e) => {
            let mut state = shared.lock().unwrap();
            state.status_msg = format!("Snapshot fetch error: {}", e);
            return;
        }
    };

    let snapshot_update_id = snapshot.last_update_id;

    // Clock offset calibration (measured independently from other requests)
    // + tick size fetch
    let time_url = "https://fapi.binance.com/fapi/v1/time";
    let exchange_info_url = format!(
        "https://fapi.binance.com/fapi/v1/exchangeInfo?symbol={}",
        symbol_upper
    );
    let local_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let time_resp = client.get(time_url).send().await;
    let local_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let clock_offset = match time_resp {
        Ok(resp) => match resp.json::<ServerTimeResponse>().await {
            Ok(server_time) => {
                let local_mid = (local_before + local_after) / 2;
                Some(local_mid - server_time.server_time as i64)
            }
            Err(_) => None,
        },
        Err(_) => None,
    };
    let exchange_info_resp = client.get(&exchange_info_url).send().await;
    let (tick_size, price_decimals) = match exchange_info_resp {
        Ok(resp) => match resp.json::<ExchangeInfoResponse>().await {
            Ok(info) => info
                .symbols
                .iter()
                .find(|s| s.symbol == symbol_upper)
                .or_else(|| info.symbols.first())
                .and_then(|symbol_info| {
                    symbol_info.filters.iter().find_map(|filter| {
                        let is_price_filter = filter.get("filterType").and_then(|v| v.as_str())
                            == Some("PRICE_FILTER");
                        if !is_price_filter {
                            return None;
                        }
                        filter
                            .get("tickSize")
                            .and_then(|v| v.as_str())
                            .and_then(parse_tick_size_and_decimals)
                    })
                })
                .unwrap_or((DEFAULT_TICK_SIZE, DEFAULT_PRICE_DECIMALS)),
            Err(_) => (DEFAULT_TICK_SIZE, DEFAULT_PRICE_DECIMALS),
        },
        Err(_) => (DEFAULT_TICK_SIZE, DEFAULT_PRICE_DECIMALS),
    };

    // Apply snapshot
    {
        let mut state = shared.lock().unwrap();
        state.order_book.apply_snapshot(&snapshot);
        state.order_book.clock_offset = clock_offset.unwrap_or(0);
        if clock_offset.is_none() {
            state.latency_ms = -1;
        }
        state.tick_size = tick_size;
        state.price_decimals = price_decimals;
        state.depth_epoch = 0;
        state.depth_slice_epoch = 0;
        state.trade_epoch = 0;
        state.micro_metrics.reset_fill_kill();
        state.sync_micro_epochs();
        state.connected = false;
        state.status_msg = "Syncing...".to_string();
    }

    // Sync buffered events
    let mut synced = false;
    let mut last_final_update_id: u64 = 0;

    while let Ok(update) = rx.try_recv() {
        if update.final_update_id < snapshot_update_id {
            continue;
        }
        if !synced {
            if can_bridge_snapshot(&update, snapshot_update_id) {
                let mut state = shared.lock().unwrap();
                apply_depth_update(&mut state, &update);
                last_final_update_id = update.final_update_id;
                synced = true;
                state.micro_metrics.reset_fill_kill();
                state.sync_micro_epochs();
            }
            continue;
        }
        if is_contiguous(&update, last_final_update_id) {
            let mut state = shared.lock().unwrap();
            apply_depth_update(&mut state, &update);
            last_final_update_id = update.final_update_id;
        }
    }

    if synced {
        let mut state = shared.lock().unwrap();
        state.connected = true;
        state.status_msg = if clock_offset.is_some() {
            "Live".to_string()
        } else {
            "Live (latency N/A)".to_string()
        };
    }

    // Main update loop
    'main_loop: loop {
        if shutdown_flag.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        // Process depth updates
        while let Ok(update) = rx.try_recv() {
            if update.final_update_id <= last_final_update_id {
                continue;
            }

            if !synced {
                if can_bridge_snapshot(&update, snapshot_update_id) {
                    let mut state = shared.lock().unwrap();
                    apply_depth_update(&mut state, &update);
                    last_final_update_id = update.final_update_id;
                    synced = true;
                    state.micro_metrics.reset_fill_kill();
                    state.sync_micro_epochs();
                    state.connected = true;
                    state.status_msg = if clock_offset.is_some() {
                        "Live".to_string()
                    } else {
                        "Live (latency N/A)".to_string()
                    };
                }
                continue;
            }

            if is_contiguous(&update, last_final_update_id) {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                let event_time = update.event_time as i64;
                let latency = clock_offset
                    .map(|offset| (now_ms - event_time - offset).max(0))
                    .unwrap_or(-1);

                let mut state = shared.lock().unwrap();
                apply_depth_update(&mut state, &update);
                state.latency_ms = latency;
                last_final_update_id = update.final_update_id;
            } else {
                {
                    let mut state = shared.lock().unwrap();
                    state.connected = false;
                    state.status_msg = "Desynced (gap). Reconnecting...".to_string();
                }
                shutdown_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(1)).await;
                break 'main_loop;
            }
        }

        // Process trades
        let mut pending_trades = Vec::new();
        while let Ok(trade) = trade_rx.try_recv() {
            pending_trades.push(trade);
        }
        if !pending_trades.is_empty() {
            let mut state = shared.lock().unwrap();
            let depth_epoch = state.depth_epoch;
            let tick_size = state.tick_size;
            let SharedState {
                order_book,
                trade_history,
                trade_epoch,
                fill_kill_epoch,
                cumulative_epoch,
                micro_metrics,
                ..
            } = &mut *state;
            for trade in pending_trades {
                if synced {
                    micro_metrics.on_trade(&trade, depth_epoch, tick_size, order_book);
                }
                trade_history.add_trade(trade);
            }
            *trade_epoch = trade_epoch.wrapping_add(1);
            *fill_kill_epoch = micro_metrics.fill_kill_epoch;
            *cumulative_epoch = micro_metrics.cumulative_epoch;
        }

        // Sample depth/fill-kill history.
        let now_ms = current_time_ms_u64();
        {
            let mut state = shared.lock().unwrap();
            if synced {
                if now_ms.saturating_sub(state.depth_history.last_sample_ms)
                    >= state.depth_history.sample_interval_ms
                {
                    let slice = build_snapshot_from_book(&state.order_book, now_ms);
                    state.depth_history.push(slice);
                    state.depth_slice_epoch = state.depth_slice_epoch.wrapping_add(1);
                    state.depth_history.last_sample_ms = now_ms;
                }
                let depth_epoch = state.depth_epoch;
                let tick_size = state.tick_size;
                state
                    .micro_metrics
                    .flush_fill_kill_if_needed(now_ms, depth_epoch, tick_size);
                state.micro_metrics.sample_cumulative(now_ms);
                state.sync_micro_epochs();
            }
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::micro::{BurstDirection, FillKillSample, RatioValue};

    fn make_depth_update(
        first_update_id: u64,
        final_update_id: u64,
        prev_final_update_id: u64,
    ) -> WsDepthUpdate {
        WsDepthUpdate {
            event_type: "depthUpdate".to_string(),
            event_time: 0,
            transaction_time: 0,
            symbol: "TESTUSDT".to_string(),
            first_update_id,
            final_update_id,
            prev_final_update_id,
            bids: Vec::new(),
            asks: Vec::new(),
        }
    }

    #[test]
    fn can_bridge_when_final_equals_snapshot() {
        let update = make_depth_update(100, 110, 99);
        assert!(can_bridge_snapshot(&update, 110));
    }

    #[test]
    fn can_bridge_when_final_exceeds_snapshot() {
        let update = make_depth_update(100, 120, 99);
        assert!(can_bridge_snapshot(&update, 110));
    }

    #[test]
    fn cannot_bridge_when_final_below_snapshot() {
        let update = make_depth_update(100, 109, 99);
        assert!(!can_bridge_snapshot(&update, 110));
    }

    #[test]
    fn cannot_bridge_when_first_after_snapshot() {
        let update = make_depth_update(111, 120, 110);
        assert!(!can_bridge_snapshot(&update, 110));
    }

    #[test]
    fn contiguous_only_when_prev_equals_last_u() {
        let update = make_depth_update(0, 0, 200);
        assert!(is_contiguous(&update, 200));
    }

    #[test]
    fn not_contiguous_when_prev_greater_than_last_u() {
        let update = make_depth_update(0, 0, 201);
        assert!(!is_contiguous(&update, 200));
    }

    #[test]
    fn not_contiguous_when_prev_less_than_last_u() {
        let update = make_depth_update(0, 0, 199);
        assert!(!is_contiguous(&update, 200));
    }

    #[test]
    fn derive_decimals_from_tick_size() {
        assert_eq!(derive_price_decimals("0.1"), 1);
        assert_eq!(derive_price_decimals("0.01"), 2);
        assert_eq!(derive_price_decimals("0.01000000"), 2);
        assert_eq!(derive_price_decimals("1.00000000"), 0);
    }

    #[test]
    fn integer_tick_size_has_zero_decimals() {
        assert_eq!(derive_price_decimals("1"), 0);
        assert_eq!(derive_price_decimals("10.00000000"), 0);
    }

    #[test]
    fn invalid_tick_size_uses_fallback_precision() {
        let decimals = parse_tick_size_and_decimals("invalid")
            .map(|(_, decimals)| decimals)
            .unwrap_or(DEFAULT_PRICE_DECIMALS);
        assert_eq!(decimals, DEFAULT_PRICE_DECIMALS);
    }

    #[test]
    fn depth_epoch_increments_when_depth_update_applied() {
        let mut state = SharedState::new();
        state.tick_size = 1.0;
        state
            .order_book
            .bids
            .insert(ordered_float::OrderedFloat(100.0), 1.0);
        state
            .order_book
            .asks
            .insert(ordered_float::OrderedFloat(101.0), 1.0);

        let mut update = make_depth_update(1, 2, 0);
        update.event_time = 10;
        update.bids = vec![["100.0".to_string(), "0.0".to_string()]];

        apply_depth_update(&mut state, &update);

        assert_eq!(state.depth_epoch, 1);
    }

    #[test]
    fn reset_fill_kill_clears_event_and_cumulative_histories() {
        let mut state = SharedState::new();
        let sample = FillKillSample {
            timestamp_ms: 1_000,
            fill_qty: 2.0,
            kill_qty: 1.0,
            pre_resting_walked_qty: 3.0,
            levels_moved: 1,
            ratio: RatioValue::Finite(2.0),
            direction: BurstDirection::Buy,
            signed_log_ratio: Some(0.3),
            overfill: true,
        };

        state
            .micro_metrics
            .fill_kill_history
            .samples
            .push_back(sample.clone());
        state.micro_metrics.on_fill_kill_sample(&sample);
        state.micro_metrics.reset_fill_kill();

        assert!(state.micro_metrics.fill_kill_history.samples.is_empty());
        assert!(state.micro_metrics.cumulative_history.samples.is_empty());
        assert_eq!(state.micro_metrics.cum_event_count, 0);
        assert_eq!(state.micro_metrics.cum_overfill_count, 0);
    }
}

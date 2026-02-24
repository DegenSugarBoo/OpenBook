use crate::micro::{CumulativeSample, FillKillSample, MicroMetrics};
use ordered_float::OrderedFloat;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct RestDepthResponse {
    #[serde(rename = "lastUpdateId")]
    pub last_update_id: u64,
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
}

#[derive(Debug, Deserialize)]
pub struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    pub server_time: u64,
}

#[derive(Debug, Deserialize)]
pub struct ExchangeInfoResponse {
    pub symbols: Vec<ExchangeSymbolInfo>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ExchangeSymbolInfo {
    pub symbol: String,
    pub filters: Vec<serde_json::Value>,
    #[serde(rename = "baseAsset", default)]
    pub base_asset: String,
    #[serde(rename = "quoteAsset", default)]
    pub quote_asset: String,
    #[serde(rename = "contractType", default)]
    pub contract_type: String,
    #[serde(rename = "status", default)]
    pub status: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WsDepthUpdate {
    #[serde(rename = "e")]
    pub event_type: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "T")]
    pub transaction_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "U")]
    pub first_update_id: u64,
    #[serde(rename = "u")]
    pub final_update_id: u64,
    #[serde(rename = "pu")]
    pub prev_final_update_id: u64,
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,
}

#[derive(Clone, Debug)]
pub struct OrderBook {
    pub bids: BTreeMap<OrderedFloat<f64>, f64>,
    pub asks: BTreeMap<OrderedFloat<f64>, f64>,
    pub last_update_id: u64,
    pub last_event_time: u64,
    pub clock_offset: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MarketImpact {
    pub avg_fill_price: f64,
    pub worst_fill_price: f64,
    pub slippage_bps: f64,
    pub slippage_pct: f64,
    pub levels_consumed: usize,
    pub total_qty_filled: f64,
    pub total_notional: f64,
    pub fully_filled: bool,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
            last_event_time: 0,
            clock_offset: 0,
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: &RestDepthResponse) {
        self.bids.clear();
        self.asks.clear();
        for bid in &snapshot.bids {
            if let (Ok(p), Ok(q)) = (bid[0].parse::<f64>(), bid[1].parse::<f64>()) {
                if q > 0.0 {
                    self.bids.insert(OrderedFloat(p), q);
                }
            }
        }
        for ask in &snapshot.asks {
            if let (Ok(p), Ok(q)) = (ask[0].parse::<f64>(), ask[1].parse::<f64>()) {
                if q > 0.0 {
                    self.asks.insert(OrderedFloat(p), q);
                }
            }
        }
        self.last_update_id = snapshot.last_update_id;
    }

    pub fn apply_update(&mut self, update: &WsDepthUpdate) {
        for bid in &update.bids {
            if let (Ok(p), Ok(q)) = (bid[0].parse::<f64>(), bid[1].parse::<f64>()) {
                let key = OrderedFloat(p);
                if q == 0.0 {
                    self.bids.remove(&key);
                } else {
                    self.bids.insert(key, q);
                }
            }
        }
        for ask in &update.asks {
            if let (Ok(p), Ok(q)) = (ask[0].parse::<f64>(), ask[1].parse::<f64>()) {
                let key = OrderedFloat(p);
                if q == 0.0 {
                    self.asks.remove(&key);
                } else {
                    self.asks.insert(key, q);
                }
            }
        }
        self.last_update_id = update.final_update_id;
        self.last_event_time = update.event_time;
    }

    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.iter().next_back().map(|(p, q)| (p.0, *q))
    }

    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.iter().next().map(|(p, q)| (p.0, *q))
    }

    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((bid, _)), Some((ask, _))) => Some(ask - bid),
            _ => None,
        }
    }

    /// Simulate a market order of `notional_usd` and estimate fill/slippage.
    pub fn estimate_market_impact(
        &self,
        notional_usd: f64,
        is_buy: bool,
        mid_price: f64,
    ) -> MarketImpact {
        if notional_usd <= 0.0 {
            return MarketImpact::default();
        }

        let mut total_qty_filled = 0.0;
        let mut total_notional = 0.0;
        let mut worst_fill_price = 0.0;
        let mut levels_consumed = 0usize;
        let mut fully_filled = false;

        let consume_level = |price: f64,
                             qty: f64,
                             total_qty_filled: &mut f64,
                             total_notional: &mut f64,
                             worst_fill_price: &mut f64,
                             levels_consumed: &mut usize,
                             fully_filled: &mut bool| {
            if price <= 0.0 || qty <= 0.0 {
                return;
            }

            let level_notional = price * qty;
            if level_notional <= 0.0 {
                return;
            }

            *levels_consumed += 1;
            *worst_fill_price = price;

            if *total_notional + level_notional >= notional_usd {
                let remaining_notional = notional_usd - *total_notional;
                let partial_qty = remaining_notional / price;
                *total_qty_filled += partial_qty;
                *total_notional += remaining_notional;
                *fully_filled = true;
            } else {
                *total_qty_filled += qty;
                *total_notional += level_notional;
            }
        };

        if is_buy {
            for (&price, &qty) in &self.asks {
                consume_level(
                    price.0,
                    qty,
                    &mut total_qty_filled,
                    &mut total_notional,
                    &mut worst_fill_price,
                    &mut levels_consumed,
                    &mut fully_filled,
                );
                if fully_filled {
                    break;
                }
            }
        } else {
            for (&price, &qty) in self.bids.iter().rev() {
                consume_level(
                    price.0,
                    qty,
                    &mut total_qty_filled,
                    &mut total_notional,
                    &mut worst_fill_price,
                    &mut levels_consumed,
                    &mut fully_filled,
                );
                if fully_filled {
                    break;
                }
            }
        }

        let avg_fill_price = if total_qty_filled > 0.0 {
            total_notional / total_qty_filled
        } else {
            0.0
        };
        let slippage_pct = if avg_fill_price > 0.0 && mid_price > 0.0 {
            ((avg_fill_price - mid_price).abs() / mid_price) * 100.0
        } else {
            0.0
        };

        MarketImpact {
            avg_fill_price,
            worst_fill_price,
            slippage_bps: slippage_pct * 100.0,
            slippage_pct,
            levels_consumed,
            total_qty_filled,
            total_notional,
            fully_filled,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WsAggTrade {
    #[serde(rename = "e")]
    pub event_type: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "a")]
    pub agg_trade_id: u64,
    #[serde(rename = "p")]
    pub price: String,
    #[serde(rename = "q")]
    pub quantity: String,
    #[serde(rename = "f")]
    pub first_trade_id: u64,
    #[serde(rename = "l")]
    pub last_trade_id: u64,
    #[serde(rename = "T")]
    pub trade_time: u64,
    #[serde(rename = "m")]
    pub is_buyer_maker: bool,
}

/// Binance Futures mini-ticker WS payload.
/// Stream: wss://fstream.binance.com/stream?streams=!miniTicker@arr
/// Each message is a JSON array of these objects.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsMiniTicker {
    #[serde(rename = "e")]
    pub event_type: String,
    #[serde(rename = "E")]
    pub event_time: u64,
    #[serde(rename = "s")]
    pub symbol: String,
    #[serde(rename = "c")]
    pub close_price: String,
    #[serde(rename = "o")]
    pub open_price: String,
    #[serde(rename = "h")]
    pub high_price: String,
    #[serde(rename = "l")]
    pub low_price: String,
    #[serde(rename = "v")]
    pub base_volume: String,
    #[serde(rename = "q")]
    pub quote_volume: String,
}

#[derive(Clone, Debug)]
pub struct Trade {
    pub timestamp_ms: u64,
    pub price: f64,
    pub quantity: f64,
    pub is_buy: bool,
}

#[derive(Clone)]
pub struct TradeHistory {
    pub trades: VecDeque<Trade>,
    pub window_ms: u64,
}

impl TradeHistory {
    pub fn new(window_ms: u64) -> Self {
        Self {
            trades: VecDeque::new(),
            window_ms,
        }
    }

    pub fn add_trade(&mut self, trade: Trade) {
        let timestamp = trade.timestamp_ms;
        self.trades.push_back(trade);
        self.cleanup(timestamp);
    }

    fn cleanup(&mut self, current_time_ms: u64) {
        let cutoff = current_time_ms.saturating_sub(self.window_ms);
        while let Some(trade) = self.trades.front() {
            if trade.timestamp_ms < cutoff {
                self.trades.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn rolling_tps(&self, now_exchange_ms: u64, window_ms: u64) -> f64 {
        if self.trades.is_empty() || window_ms == 0 {
            return 0.0;
        }

        let cutoff = now_exchange_ms.saturating_sub(window_ms);
        let trades_in_window = self
            .trades
            .iter()
            .filter(|trade| trade.timestamp_ms >= cutoff)
            .count();

        trades_in_window as f64 / (window_ms as f64 / 1000.0)
    }
}

/// A single snapshot of the order book at a point in time.
#[derive(Clone)]
pub struct DepthSlice {
    pub timestamp_ms: u64,
    pub levels: Vec<(f64, f64)>, // bids first, then asks; each side remains price-sorted
    pub bids_len: usize,
}

/// Ring buffer of historical order book snapshots for heatmap rendering.
pub struct DepthHistory {
    pub slices: VecDeque<DepthSlice>,
    pub max_slices: usize,
    pub sample_interval_ms: u64,
    pub last_sample_ms: u64,
}

impl DepthHistory {
    pub fn new() -> Self {
        Self {
            slices: VecDeque::new(),
            max_slices: 600, // 5 min at 500ms intervals
            sample_interval_ms: 500,
            last_sample_ms: 0,
        }
    }

    pub fn push(&mut self, slice: DepthSlice) {
        self.slices.push_back(slice);
        while self.slices.len() > self.max_slices {
            self.slices.pop_front();
        }
    }
}

/// Build a DepthSlice from the current OrderBook state.
pub fn build_snapshot_from_book(book: &OrderBook, now_ms: u64) -> DepthSlice {
    let mut levels: Vec<(f64, f64)> = Vec::with_capacity(book.bids.len() + book.asks.len());
    for (price, &qty) in &book.bids {
        levels.push((price.0, qty));
    }
    let bids_len = levels.len();
    for (price, &qty) in &book.asks {
        levels.push((price.0, qty));
    }
    DepthSlice {
        timestamp_ms: now_ms,
        levels,
        bids_len,
    }
}

/// Shared state between the async WS task and the GUI thread.
pub struct SharedState {
    pub order_book: OrderBook,
    pub trade_history: TradeHistory,
    pub depth_history: DepthHistory,
    pub depth_epoch: u64,
    pub depth_slice_epoch: u64,
    pub trade_epoch: u64,
    pub fill_kill_epoch: u64,
    pub cumulative_epoch: u64,
    pub micro_metrics: MicroMetrics,
    pub snapshot_trade_epoch: u64,
    pub snapshot_depth_slice_epoch: u64,
    pub snapshot_fill_kill_epoch: u64,
    pub snapshot_cumulative_epoch: u64,
    pub snapshot_trades: Arc<Vec<(u64, f64, f64, bool)>>,
    pub snapshot_depth_slices: Arc<Vec<DepthSlice>>,
    pub snapshot_fill_kill_series: Arc<Vec<FillKillSample>>,
    pub snapshot_cumulative_series: Arc<Vec<CumulativeSample>>,
    pub connected: bool,
    pub status_msg: String,
    pub latency_ms: i64,
    pub tick_size: f64,
    pub price_decimals: usize,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            order_book: OrderBook::new(),
            trade_history: TradeHistory::new(300_000),
            depth_history: DepthHistory::new(),
            depth_epoch: 0,
            depth_slice_epoch: 0,
            trade_epoch: 0,
            fill_kill_epoch: 0,
            cumulative_epoch: 0,
            micro_metrics: MicroMetrics::default(),
            snapshot_trade_epoch: u64::MAX,
            snapshot_depth_slice_epoch: u64::MAX,
            snapshot_fill_kill_epoch: u64::MAX,
            snapshot_cumulative_epoch: u64::MAX,
            snapshot_trades: Arc::new(Vec::new()),
            snapshot_depth_slices: Arc::new(Vec::new()),
            snapshot_fill_kill_series: Arc::new(Vec::new()),
            snapshot_cumulative_series: Arc::new(Vec::new()),
            connected: false,
            status_msg: "Initializing...".to_string(),
            latency_ms: -1,
            tick_size: 0.1,
            price_decimals: 1,
        }
    }

    pub fn sync_micro_epochs(&mut self) {
        self.fill_kill_epoch = self.micro_metrics.fill_kill_epoch;
        self.cumulative_epoch = self.micro_metrics.cumulative_epoch;
    }
}

/// A single symbol entry from the exchange catalog.
#[derive(Clone, Debug)]
pub struct SymbolCatalogEntry {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
}

/// Latest ticker snapshot for one symbol (from !miniTicker@arr).
#[derive(Clone, Debug, Default)]
pub struct LiveTicker {
    pub last_price: f64,
    pub open_24h: f64,
    pub change_pct_24h: f64,
    pub quote_volume_24h: f64,
    pub event_time_ms: u64,
}

/// Status of the picker data feed.
#[derive(Clone, Debug, PartialEq)]
pub enum PickerStatus {
    Loading,
    Live,
    Stale,
    Reconnecting,
    Error(String),
}

/// Shared state for the symbol picker.
pub struct PickerSharedState {
    pub catalog: Vec<SymbolCatalogEntry>,
    pub live_tickers: HashMap<String, LiveTicker>,
    pub ticker_epoch: u64,
    pub status: PickerStatus,
}

impl PickerSharedState {
    pub fn new() -> Self {
        Self {
            catalog: Vec::new(),
            live_tickers: HashMap::new(),
            ticker_epoch: 0,
            status: PickerStatus::Loading,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MarketImpact, OrderBook, Trade, TradeHistory};
    use ordered_float::OrderedFloat;

    fn assert_close(left: f64, right: f64, tol: f64) {
        assert!(
            (left - right).abs() <= tol,
            "left={left}, right={right}, tol={tol}"
        );
    }

    #[test]
    fn estimate_market_impact_buy_with_partial_last_level() {
        let mut book = OrderBook::new();
        book.asks.insert(OrderedFloat(100.0), 1.0);
        book.asks.insert(OrderedFloat(101.0), 2.0);

        let impact = book.estimate_market_impact(150.0, true, 100.0);

        assert!(impact.fully_filled);
        assert_eq!(impact.levels_consumed, 2);
        assert_close(impact.total_notional, 150.0, 1e-9);
        assert_close(impact.total_qty_filled, 1.495049504950495, 1e-12);
        assert_close(impact.avg_fill_price, 100.33112582781457, 1e-10);
        assert_close(impact.worst_fill_price, 101.0, 1e-9);
        assert_close(impact.slippage_bps, 33.11258278145695, 1e-9);
    }

    #[test]
    fn estimate_market_impact_sell_partial_when_book_is_thin() {
        let mut book = OrderBook::new();
        book.bids.insert(OrderedFloat(99.0), 1.0);
        book.bids.insert(OrderedFloat(98.0), 1.0);

        let impact = book.estimate_market_impact(300.0, false, 100.0);

        assert!(!impact.fully_filled);
        assert_eq!(impact.levels_consumed, 2);
        assert_close(impact.total_notional, 197.0, 1e-9);
        assert_close(impact.total_qty_filled, 2.0, 1e-9);
        assert_close(impact.avg_fill_price, 98.5, 1e-9);
        assert_close(impact.worst_fill_price, 98.0, 1e-9);
        assert_close(impact.slippage_pct, 1.5, 1e-9);
    }

    #[test]
    fn estimate_market_impact_zero_notional_returns_default() {
        let book = OrderBook::new();
        let impact = book.estimate_market_impact(0.0, true, 100.0);
        assert_eq!(
            impact,
            MarketImpact {
                avg_fill_price: 0.0,
                worst_fill_price: 0.0,
                slippage_bps: 0.0,
                slippage_pct: 0.0,
                levels_consumed: 0,
                total_qty_filled: 0.0,
                total_notional: 0.0,
                fully_filled: false,
            }
        );
    }

    #[test]
    fn rolling_tps_returns_zero_for_empty_history() {
        let history = TradeHistory::new(300_000);
        assert_eq!(history.rolling_tps(100_000, 10_000), 0.0);
    }

    #[test]
    fn rolling_tps_returns_zero_when_all_trades_are_older_than_window() {
        let mut history = TradeHistory::new(300_000);
        history.trades.push_back(Trade {
            timestamp_ms: 89_000,
            price: 100.0,
            quantity: 1.0,
            is_buy: true,
        });
        history.trades.push_back(Trade {
            timestamp_ms: 89_500,
            price: 100.0,
            quantity: 1.0,
            is_buy: false,
        });

        assert_eq!(history.rolling_tps(100_000, 10_000), 0.0);
    }

    #[test]
    fn rolling_tps_counts_only_trades_inside_window() {
        let mut history = TradeHistory::new(300_000);
        history.trades.push_back(Trade {
            timestamp_ms: 89_999,
            price: 100.0,
            quantity: 1.0,
            is_buy: true,
        });
        history.trades.push_back(Trade {
            timestamp_ms: 90_000,
            price: 101.0,
            quantity: 2.0,
            is_buy: false,
        });
        history.trades.push_back(Trade {
            timestamp_ms: 95_000,
            price: 102.0,
            quantity: 1.0,
            is_buy: true,
        });
        history.trades.push_back(Trade {
            timestamp_ms: 99_500,
            price: 103.0,
            quantity: 3.0,
            is_buy: false,
        });

        let tps = history.rolling_tps(100_000, 10_000);
        assert_close(tps, 0.3, 1e-12);
    }

    #[test]
    fn rolling_tps_includes_trade_at_exact_cutoff() {
        let mut history = TradeHistory::new(300_000);
        history.trades.push_back(Trade {
            timestamp_ms: 90_000,
            price: 100.0,
            quantity: 1.0,
            is_buy: true,
        });

        let tps = history.rolling_tps(100_000, 10_000);
        assert_close(tps, 0.1, 1e-12);
    }

    #[test]
    fn rolling_tps_handles_dense_burst_with_decimal_result() {
        let mut history = TradeHistory::new(300_000);
        for i in 0..37_u64 {
            history.trades.push_back(Trade {
                timestamp_ms: 90_000 + i,
                price: 100.0,
                quantity: 1.0,
                is_buy: i % 2 == 0,
            });
        }

        let tps = history.rolling_tps(100_000, 10_000);
        assert_close(tps, 3.7, 1e-12);
    }
}

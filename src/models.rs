use crate::micro::{CumulativeSample, FillKillSample, MicroMetrics};
use ordered_float::OrderedFloat;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

pub const HISTORY_MAX_AGE_MS: u64 = 180_000;
pub const ORDER_BOOK_MAX_LEVELS_PER_SIDE: usize = 3000;
pub const DEPTH_HISTORY_MAX_BYTES: usize = 160 * 1024 * 1024;
pub const DEPTH_CHECKPOINT_INTERVAL_MS: u64 = 1_000;
pub const DEPTH_DELTA_TO_CHECKPOINT_THRESHOLD: usize = 1_200;

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

    /// Apply update and return only real state changes as deltas (no-ops filtered).
    pub fn apply_update_with_deltas(&mut self, update: &WsDepthUpdate) -> Vec<DepthLevelDelta> {
        let mut deltas = Vec::new();

        for bid in &update.bids {
            if let (Ok(p), Ok(q)) = (bid[0].parse::<f64>(), bid[1].parse::<f64>()) {
                let key = OrderedFloat(p);
                let old_qty = self.bids.get(&key).copied().unwrap_or(0.0);
                if (old_qty - q).abs() < 1e-15 {
                    continue; // no-op
                }
                if q == 0.0 {
                    self.bids.remove(&key);
                } else {
                    self.bids.insert(key, q);
                }
                deltas.push(DepthLevelDelta {
                    side: DepthSide::Bid,
                    price: p,
                    qty: q,
                });
            }
        }
        for ask in &update.asks {
            if let (Ok(p), Ok(q)) = (ask[0].parse::<f64>(), ask[1].parse::<f64>()) {
                let key = OrderedFloat(p);
                let old_qty = self.asks.get(&key).copied().unwrap_or(0.0);
                if (old_qty - q).abs() < 1e-15 {
                    continue; // no-op
                }
                if q == 0.0 {
                    self.asks.remove(&key);
                } else {
                    self.asks.insert(key, q);
                }
                deltas.push(DepthLevelDelta {
                    side: DepthSide::Ask,
                    price: p,
                    qty: q,
                });
            }
        }
        self.last_update_id = update.final_update_id;
        self.last_event_time = update.event_time;
        deltas
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

    /// Prune each side of the book to at most `max_levels` closest to the
    /// best bid/ask. Uses `BTreeMap::split_off` which is O(log N).
    pub fn prune_to_max_levels(&mut self, max_levels: usize) {
        // Bids: keep the highest `max_levels` entries (closest to best bid).
        if self.bids.len() > max_levels {
            let split_idx = self.bids.len() - max_levels;
            if let Some(&key) = self.bids.keys().nth(split_idx) {
                self.bids = self.bids.split_off(&key);
            }
        }
        // Asks: keep the lowest `max_levels` entries (closest to best ask).
        if self.asks.len() > max_levels {
            if let Some(&key) = self.asks.keys().nth(max_levels) {
                self.asks.split_off(&key);
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepthSide {
    Bid,
    Ask,
}

#[derive(Clone, Debug)]
pub struct DepthLevelDelta {
    pub side: DepthSide,
    pub price: f64,
    pub qty: f64, // 0 => delete
}

#[derive(Clone, Debug)]
pub struct DepthDeltaEvent {
    pub timestamp_ms: u64,
    pub update_id: u64,
    pub changes: Vec<DepthLevelDelta>,
}

#[derive(Clone, Debug)]
pub struct DepthCheckpoint {
    pub timestamp_ms: u64,
    pub update_id: u64,
    pub levels: Vec<(f64, f64)>, // bids first, then asks
    pub bids_len: usize,
}

impl DepthCheckpoint {
    fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.levels.len() * std::mem::size_of::<(f64, f64)>()
    }
}

impl DepthDeltaEvent {
    fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.changes.len() * std::mem::size_of::<DepthLevelDelta>()
    }
}

pub struct EventDepthHistory {
    pub checkpoints: VecDeque<Arc<DepthCheckpoint>>,
    pub deltas: VecDeque<Arc<DepthDeltaEvent>>,
    pub max_age_ms: u64,
    pub target_checkpoint_interval_ms: u64,
    pub approx_bytes: usize,
    pub max_bytes: usize,
    pub last_checkpoint_ms: u64,
}

impl EventDepthHistory {
    pub fn new() -> Self {
        Self {
            checkpoints: VecDeque::new(),
            deltas: VecDeque::new(),
            max_age_ms: HISTORY_MAX_AGE_MS,
            target_checkpoint_interval_ms: DEPTH_CHECKPOINT_INTERVAL_MS,
            approx_bytes: 0,
            max_bytes: DEPTH_HISTORY_MAX_BYTES,
            last_checkpoint_ms: 0,
        }
    }

    pub fn reset_from_book(&mut self, book: &OrderBook, timestamp_ms: u64, update_id: u64) {
        self.checkpoints.clear();
        self.deltas.clear();
        self.approx_bytes = 0;
        self.last_checkpoint_ms = 0;
        self.push_checkpoint_from_book(book, timestamp_ms, update_id);
    }

    fn push_checkpoint_from_book(&mut self, book: &OrderBook, timestamp_ms: u64, update_id: u64) {
        let mut levels = Vec::with_capacity(book.bids.len() + book.asks.len());
        for (price, &qty) in &book.bids {
            levels.push((price.0, qty));
        }
        let bids_len = levels.len();
        for (price, &qty) in &book.asks {
            levels.push((price.0, qty));
        }
        let cp = Arc::new(DepthCheckpoint {
            timestamp_ms,
            update_id,
            levels,
            bids_len,
        });
        self.approx_bytes += cp.estimated_bytes();
        self.checkpoints.push_back(cp);
        self.last_checkpoint_ms = timestamp_ms;
    }

    pub fn push_event(
        &mut self,
        timestamp_ms: u64,
        update_id: u64,
        changes: Vec<DepthLevelDelta>,
        book: &OrderBook,
    ) {
        // Clamp timestamp to avoid regression
        let ts = if let Some(last) = self.last_event_timestamp() {
            timestamp_ms.max(last)
        } else {
            timestamp_ms
        };

        // Large delta bursts → promote to checkpoint
        if changes.len() >= DEPTH_DELTA_TO_CHECKPOINT_THRESHOLD {
            self.push_checkpoint_from_book(book, ts, update_id);
            return;
        }

        let delta = Arc::new(DepthDeltaEvent {
            timestamp_ms: ts,
            update_id,
            changes,
        });
        self.approx_bytes += delta.estimated_bytes();
        self.deltas.push_back(delta);

        // Periodic checkpoint
        if ts.saturating_sub(self.last_checkpoint_ms) >= self.target_checkpoint_interval_ms {
            self.push_checkpoint_from_book(book, ts, update_id);
        }
    }

    fn last_event_timestamp(&self) -> Option<u64> {
        let last_cp = self.checkpoints.back().map(|c| c.timestamp_ms);
        let last_delta = self.deltas.back().map(|d| d.timestamp_ms);
        match (last_cp, last_delta) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    pub fn prune(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.max_age_ms);

        // Drop deltas older than cutoff
        while let Some(d) = self.deltas.front() {
            if d.timestamp_ms < cutoff {
                self.approx_bytes = self.approx_bytes.saturating_sub(d.estimated_bytes());
                self.deltas.pop_front();
            } else {
                break;
            }
        }

        // Drop checkpoints older than cutoff, but keep at least one
        while self.checkpoints.len() > 1 {
            if self.checkpoints[0].timestamp_ms < cutoff {
                let cp = self.checkpoints.pop_front().unwrap();
                self.approx_bytes = self.approx_bytes.saturating_sub(cp.estimated_bytes());
            } else {
                break;
            }
        }

        // Memory pressure: drop oldest deltas, then oldest checkpoints (keep >=1)
        while self.approx_bytes > self.max_bytes {
            if let Some(d) = self.deltas.pop_front() {
                self.approx_bytes = self.approx_bytes.saturating_sub(d.estimated_bytes());
            } else if self.checkpoints.len() > 1 {
                let cp = self.checkpoints.pop_front().unwrap();
                self.approx_bytes = self.approx_bytes.saturating_sub(cp.estimated_bytes());
            } else {
                break;
            }
        }
    }

    /// Reconstruct depth columns for the heatmap by replaying deltas over checkpoints.
    /// Returns a Vec of DepthSlice, one per output column, evenly spaced between start_ms..end_ms.
    pub fn materialize_columns(
        &self,
        start_ms: u64,
        end_ms: u64,
        max_columns: usize,
    ) -> Vec<Arc<DepthSlice>> {
        if max_columns == 0 || self.checkpoints.is_empty() || end_ms <= start_ms {
            return Vec::new();
        }

        // Build target timestamps for each column
        let span = end_ms - start_ms;
        let col_count = max_columns.min(span as usize);
        if col_count == 0 {
            return Vec::new();
        }
        let step = span as f64 / col_count as f64;
        let target_timestamps: Vec<u64> = (0..col_count)
            .map(|i| start_ms + (i as f64 * step) as u64)
            .collect();

        // Find the latest checkpoint at or before the first target timestamp
        let first_ts = target_timestamps[0];
        let cp_idx = match self
            .checkpoints
            .binary_search_by_key(&first_ts, |c| c.timestamp_ms)
        {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };

        let base_cp = &self.checkpoints[cp_idx];

        // Build working book from checkpoint
        let mut working_bids: BTreeMap<OrderedFloat<f64>, f64> = BTreeMap::new();
        let mut working_asks: BTreeMap<OrderedFloat<f64>, f64> = BTreeMap::new();
        for &(price, qty) in &base_cp.levels[..base_cp.bids_len] {
            working_bids.insert(OrderedFloat(price), qty);
        }
        for &(price, qty) in &base_cp.levels[base_cp.bids_len..] {
            working_asks.insert(OrderedFloat(price), qty);
        }

        // Collect deltas in the range [base_cp.timestamp_ms .. end_ms]
        let delta_start_idx = match self
            .deltas
            .binary_search_by_key(&base_cp.timestamp_ms, |d| d.timestamp_ms)
        {
            Ok(i) => i,
            Err(i) => i,
        };

        // Also apply any checkpoints between base_cp and first_ts (they may have
        // different state), but the monotonic sweep below handles this naturally
        // since checkpoints are also snapshotted from the same book.

        let mut delta_cursor = delta_start_idx;
        let mut cp_cursor = cp_idx + 1;
        let mut columns = Vec::with_capacity(col_count);

        for &target_ts in &target_timestamps {
            // Apply deltas up to target_ts
            while delta_cursor < self.deltas.len() {
                let d = &self.deltas[delta_cursor];
                if d.timestamp_ms > target_ts {
                    break;
                }
                for change in &d.changes {
                    let key = OrderedFloat(change.price);
                    match change.side {
                        DepthSide::Bid => {
                            if change.qty == 0.0 {
                                working_bids.remove(&key);
                            } else {
                                working_bids.insert(key, change.qty);
                            }
                        }
                        DepthSide::Ask => {
                            if change.qty == 0.0 {
                                working_asks.remove(&key);
                            } else {
                                working_asks.insert(key, change.qty);
                            }
                        }
                    }
                }
                delta_cursor += 1;
            }

            // Also apply any checkpoint that's newer and <= target_ts
            // (this handles the case where a checkpoint was emitted mid-stream)
            while cp_cursor < self.checkpoints.len()
                && self.checkpoints[cp_cursor].timestamp_ms <= target_ts
            {
                let cp = &self.checkpoints[cp_cursor];
                working_bids.clear();
                working_asks.clear();
                for &(price, qty) in &cp.levels[..cp.bids_len] {
                    working_bids.insert(OrderedFloat(price), qty);
                }
                for &(price, qty) in &cp.levels[cp.bids_len..] {
                    working_asks.insert(OrderedFloat(price), qty);
                }
                cp_cursor += 1;
            }

            // Snapshot current state
            let mut levels = Vec::with_capacity(working_bids.len() + working_asks.len());
            for (&price, &qty) in &working_bids {
                levels.push((price.0, qty));
            }
            let bids_len = levels.len();
            for (&price, &qty) in &working_asks {
                levels.push((price.0, qty));
            }

            columns.push(Arc::new(DepthSlice {
                timestamp_ms: target_ts,
                levels,
                bids_len,
            }));
        }

        columns
    }

    pub fn time_range(&self) -> Option<(u64, u64)> {
        let earliest = self
            .checkpoints
            .front()
            .map(|c| c.timestamp_ms)
            .unwrap_or(u64::MAX);
        let latest_cp = self.checkpoints.back().map(|c| c.timestamp_ms).unwrap_or(0);
        let latest_delta = self.deltas.back().map(|d| d.timestamp_ms).unwrap_or(0);
        let latest = latest_cp.max(latest_delta);
        if latest >= earliest {
            Some((earliest, latest))
        } else {
            None
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
    pub received_at_ms: u64,
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
        let now_ms = trade.received_at_ms;
        self.trades.push_back(trade);
        self.prune_now(now_ms);
    }

    pub fn prune_now(&mut self, now_ms: u64) -> usize {
        let cutoff = now_ms.saturating_sub(self.window_ms);
        let initial_len = self.trades.len();
        while let Some(trade) = self.trades.front() {
            if trade.received_at_ms < cutoff {
                self.trades.pop_front();
            } else {
                break;
            }
        }
        let removed = initial_len.saturating_sub(self.trades.len());
        // Reclaim memory when capacity is much larger than usage (e.g. after
        // a burst of high TPS subsides).  We use 2× as the threshold and
        // shrink to 1.5× to leave some headroom.
        if removed > 0 {
            let cap = self.trades.capacity();
            let len = self.trades.len();
            if cap > 1024 && cap > len * 2 {
                self.trades.shrink_to(len * 3 / 2);
            }
        }
        removed
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

/// Shared state between the async WS task and the GUI thread.
pub struct SharedState {
    pub order_book: OrderBook,
    pub trade_history: TradeHistory,
    pub depth_history: EventDepthHistory,
    pub depth_epoch: u64,
    pub depth_history_epoch: u64,
    pub trade_epoch: u64,
    pub fill_kill_epoch: u64,
    pub cumulative_epoch: u64,
    pub micro_metrics: MicroMetrics,
    pub snapshot_trade_epoch: u64,
    pub snapshot_depth_history_epoch: u64,
    pub snapshot_fill_kill_epoch: u64,
    pub snapshot_cumulative_epoch: u64,
    pub snapshot_book_epoch: u64,
    pub snapshot_bids: Arc<Vec<(f64, f64)>>,
    pub snapshot_asks: Arc<Vec<(f64, f64)>>,
    pub snapshot_trades: Arc<Vec<(u64, f64, f64, bool)>>,
    pub snapshot_depth_checkpoints: Arc<Vec<Arc<DepthCheckpoint>>>,
    pub snapshot_depth_deltas: Arc<Vec<Arc<DepthDeltaEvent>>>,
    pub snapshot_depth_slices: Arc<Vec<Arc<DepthSlice>>>,
    pub snapshot_fill_kill_series: Arc<Vec<FillKillSample>>,
    pub snapshot_cumulative_series: Arc<Vec<CumulativeSample>>,
    pub snapshot_depth_slices_epoch: u64,
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
            trade_history: TradeHistory::new(HISTORY_MAX_AGE_MS),
            depth_history: EventDepthHistory::new(),
            depth_epoch: 0,
            depth_history_epoch: 0,
            trade_epoch: 0,
            fill_kill_epoch: 0,
            cumulative_epoch: 0,
            micro_metrics: MicroMetrics::default(),
            snapshot_trade_epoch: u64::MAX,
            snapshot_depth_history_epoch: u64::MAX,
            snapshot_fill_kill_epoch: u64::MAX,
            snapshot_cumulative_epoch: u64::MAX,
            snapshot_book_epoch: u64::MAX,
            snapshot_bids: Arc::new(Vec::new()),
            snapshot_asks: Arc::new(Vec::new()),
            snapshot_trades: Arc::new(Vec::new()),
            snapshot_depth_checkpoints: Arc::new(Vec::new()),
            snapshot_depth_deltas: Arc::new(Vec::new()),
            snapshot_depth_slices: Arc::new(Vec::new()),
            snapshot_fill_kill_series: Arc::new(Vec::new()),
            snapshot_cumulative_series: Arc::new(Vec::new()),
            snapshot_depth_slices_epoch: u64::MAX,
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
#[path = "../tests/unit/models_tests.rs"]
mod tests;

use crate::models::{OrderBook, Trade};
use ordered_float::OrderedFloat;
use std::collections::{BTreeMap, VecDeque};

const FILL_KILL_MAX_SAMPLES: usize = 10_000;
pub const ROLLING_WINDOW_MS: u64 = 180_000;
const CUMULATIVE_MAX_SAMPLES: usize = 10_000;
const CUMULATIVE_CARRY_FORWARD_INTERVAL_MS: u64 = 500;
const BURST_MAX_GAP_MS: u64 = 80;
const BURST_MAX_AGE_MS: u64 = 100;
const RATIO_EPS: f64 = 1e-9;
const SIGNED_LOG_RATIO_EPS: f64 = 1e-2;
const SIGNED_LOG_RATIO_CLAMP: f64 = 3.0;
const OVERFILL_THRESHOLD_MULTIPLIER: f64 = 1.02;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RatioValue {
    Na,
    Finite(f64),
    Infinite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurstDirection {
    Buy,
    Sell,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CumulativeSample {
    pub timestamp_ms: u64,
    pub cum_fill_qty: f64,
    pub cum_kill_qty: f64,
    pub cum_net_qty: f64,
    pub cum_ratio: RatioValue,
}

#[derive(Clone, Debug)]
pub struct CumulativeHistory {
    pub samples: VecDeque<CumulativeSample>,
    pub max_samples: usize,
}

impl Default for CumulativeHistory {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            max_samples: CUMULATIVE_MAX_SAMPLES,
        }
    }
}

impl CumulativeHistory {
    pub fn push(&mut self, sample: CumulativeSample) {
        self.samples.push_back(sample);
        while self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    pub fn trim_window(&mut self, now_ms: u64, window_ms: u64) {
        let cutoff = now_ms.saturating_sub(window_ms);
        while let Some(front) = self.samples.front() {
            if front.timestamp_ms < cutoff {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
    }

    pub fn latest(&self) -> Option<&CumulativeSample> {
        self.samples.back()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FillKillKpis {
    pub cum_fill_qty: f64,
    pub cum_kill_qty: f64,
    pub cum_ratio: RatioValue,
    pub cum_net_qty: f64,
    pub overfill_pct: f64,
    pub cum_overfill_count: u64,
    pub cum_event_count: u64,
}

impl Default for FillKillKpis {
    fn default() -> Self {
        Self {
            cum_fill_qty: 0.0,
            cum_kill_qty: 0.0,
            cum_ratio: RatioValue::Na,
            cum_net_qty: 0.0,
            overfill_pct: 0.0,
            cum_overfill_count: 0,
            cum_event_count: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct BurstState {
    start_timestamp_ms: u64,
    last_trade_timestamp_ms: u64,
    direction: BurstDirection,
    worst_trade_price: f64,
    fill_qty: f64,
    pre_book: OrderBook,
    epoch_id: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FillKillSample {
    pub timestamp_ms: u64,
    pub fill_qty: f64,
    pub kill_qty: f64,
    pub pre_resting_walked_qty: f64,
    pub levels_moved: u64,
    pub ratio: RatioValue,
    pub direction: BurstDirection,
    pub signed_log_ratio: Option<f64>,
    pub overfill: bool,
}

#[derive(Clone, Debug)]
pub struct FillKillHistory {
    pub samples: VecDeque<FillKillSample>,
    pub max_samples: usize,
    active_burst: Option<BurstState>,
}

#[derive(Clone, Copy, Debug, Default)]
struct PrunedFillKillTotals {
    fill_qty: f64,
    kill_qty: f64,
    event_count: u64,
    overfill_count: u64,
    removed_samples: usize,
}

impl Default for FillKillHistory {
    fn default() -> Self {
        Self {
            samples: VecDeque::new(),
            max_samples: FILL_KILL_MAX_SAMPLES,
            active_burst: None,
        }
    }
}

impl FillKillHistory {
    fn reset(&mut self) {
        self.samples.clear();
        self.active_burst = None;
    }

    fn on_trade(
        &mut self,
        trade: &Trade,
        depth_epoch: u64,
        tick_size: f64,
        book: &OrderBook,
    ) -> Option<FillKillSample> {
        let timestamp_ms = trade.timestamp_ms;
        let price = trade.price;
        let qty = trade.quantity;
        if !price.is_finite() || !qty.is_finite() || qty <= 0.0 {
            return None;
        }

        let mut flushed = self.flush_if_needed(timestamp_ms, depth_epoch, tick_size);

        let direction = if trade.is_buy {
            BurstDirection::Buy
        } else {
            BurstDirection::Sell
        };

        let should_start_new = self
            .active_burst
            .as_ref()
            .map(|burst| {
                should_flush_for_trade_boundary(burst, timestamp_ms, direction, depth_epoch)
            })
            .unwrap_or(true);

        if should_start_new {
            if flushed.is_none() {
                flushed = self.flush_open_burst(tick_size);
            }
            self.start_new_burst(
                timestamp_ms,
                price,
                qty,
                direction,
                depth_epoch,
                book.clone(),
            );
            return flushed;
        }

        if let Some(burst) = &mut self.active_burst {
            burst.last_trade_timestamp_ms = timestamp_ms;
            burst.fill_qty += qty;
            match burst.direction {
                BurstDirection::Buy => {
                    if price > burst.worst_trade_price {
                        burst.worst_trade_price = price;
                    }
                }
                BurstDirection::Sell => {
                    if price < burst.worst_trade_price {
                        burst.worst_trade_price = price;
                    }
                }
            }
        }

        flushed
    }

    fn on_depth_epoch_advance(
        &mut self,
        now_ms: u64,
        depth_epoch: u64,
        tick_size: f64,
    ) -> Option<FillKillSample> {
        self.flush_if_needed(now_ms, depth_epoch, tick_size)
    }

    fn flush_if_needed(
        &mut self,
        now_ms: u64,
        depth_epoch: u64,
        tick_size: f64,
    ) -> Option<FillKillSample> {
        let should_flush = self
            .active_burst
            .as_ref()
            .map(|burst| {
                now_ms.saturating_sub(burst.start_timestamp_ms) > BURST_MAX_AGE_MS
                    || burst.epoch_id != depth_epoch
            })
            .unwrap_or(false);

        if should_flush {
            return self.flush_open_burst(tick_size);
        }

        None
    }

    fn flush_open_burst(&mut self, tick_size: f64) -> Option<FillKillSample> {
        let burst = self.active_burst.take()?;
        let sample = build_fill_kill_sample(&burst, tick_size);
        self.push_sample(sample.clone());
        Some(sample)
    }

    fn start_new_burst(
        &mut self,
        timestamp_ms: u64,
        price: f64,
        qty: f64,
        direction: BurstDirection,
        depth_epoch: u64,
        pre_book: OrderBook,
    ) {
        self.active_burst = Some(BurstState {
            start_timestamp_ms: timestamp_ms,
            last_trade_timestamp_ms: timestamp_ms,
            direction,
            worst_trade_price: price,
            fill_qty: qty,
            pre_book,
            epoch_id: depth_epoch,
        });
    }

    fn push_sample(&mut self, sample: FillKillSample) {
        self.samples.push_back(sample);
        while self.samples.len() > self.max_samples {
            self.samples.pop_front();
        }
    }

    fn prune_window(&mut self, now_ms: u64, window_ms: u64) -> PrunedFillKillTotals {
        let cutoff = now_ms.saturating_sub(window_ms);
        let mut pruned = PrunedFillKillTotals::default();
        while let Some(front) = self.samples.front() {
            if front.timestamp_ms < cutoff {
                let removed = self.samples.pop_front().expect("front exists");
                pruned.fill_qty += removed.fill_qty;
                pruned.kill_qty += removed.kill_qty;
                pruned.event_count = pruned.event_count.saturating_add(1);
                if removed.overfill {
                    pruned.overfill_count = pruned.overfill_count.saturating_add(1);
                }
                pruned.removed_samples += 1;
            } else {
                break;
            }
        }
        pruned
    }
}

#[derive(Clone, Debug, Default)]
pub struct MicroMetrics {
    pub fill_kill_history: FillKillHistory,
    pub fill_kill_epoch: u64,
    pub cum_fill_qty: f64,
    pub cum_kill_qty: f64,
    pub cum_overfill_count: u64,
    pub cum_event_count: u64,
    pub cumulative_history: CumulativeHistory,
    pub cumulative_epoch: u64,
    last_cumulative_sample_ms: u64,
}

impl MicroMetrics {
    pub fn on_trade(&mut self, trade: &Trade, depth_epoch: u64, tick_size: f64, book: &OrderBook) {
        if let Some(sample) = self
            .fill_kill_history
            .on_trade(trade, depth_epoch, tick_size, book)
        {
            self.on_fill_kill_sample(&sample);
        }
    }

    pub fn on_depth_epoch_advance(&mut self, now_ms: u64, depth_epoch: u64, tick_size: f64) {
        if let Some(sample) =
            self.fill_kill_history
                .on_depth_epoch_advance(now_ms, depth_epoch, tick_size)
        {
            self.on_fill_kill_sample(&sample);
        }
    }

    pub fn flush_fill_kill_if_needed(&mut self, now_ms: u64, depth_epoch: u64, tick_size: f64) {
        if let Some(sample) = self
            .fill_kill_history
            .flush_if_needed(now_ms, depth_epoch, tick_size)
        {
            self.on_fill_kill_sample(&sample);
        }
    }

    pub fn on_fill_kill_sample(&mut self, sample: &FillKillSample) {
        self.fill_kill_epoch = self.fill_kill_epoch.wrapping_add(1);
        self.cum_fill_qty += sample.fill_qty;
        self.cum_kill_qty += sample.kill_qty;
        self.cum_event_count = self.cum_event_count.saturating_add(1);
        if sample.overfill {
            self.cum_overfill_count = self.cum_overfill_count.saturating_add(1);
        }
        self.append_cumulative_sample(sample.timestamp_ms);
    }

    pub fn sample_cumulative(&mut self, now_ms: u64) {
        if self.cumulative_history.samples.is_empty() {
            return;
        }
        if self.last_cumulative_sample_ms > 0
            && now_ms.saturating_sub(self.last_cumulative_sample_ms)
                < CUMULATIVE_CARRY_FORWARD_INTERVAL_MS
        {
            return;
        }
        if self
            .cumulative_history
            .latest()
            .is_some_and(|sample| sample.timestamp_ms == now_ms)
        {
            return;
        }

        self.append_cumulative_sample(now_ms);
    }

    pub fn prune_rolling_window(&mut self, now_ms: u64) {
        let pruned = self
            .fill_kill_history
            .prune_window(now_ms, ROLLING_WINDOW_MS);
        if pruned.removed_samples > 0 {
            self.cum_fill_qty = normalize_non_negative(self.cum_fill_qty - pruned.fill_qty);
            self.cum_kill_qty = normalize_non_negative(self.cum_kill_qty - pruned.kill_qty);
            self.cum_event_count = self.cum_event_count.saturating_sub(pruned.event_count);
            self.cum_overfill_count = self
                .cum_overfill_count
                .saturating_sub(pruned.overfill_count);
            if self.cum_overfill_count > self.cum_event_count {
                self.cum_overfill_count = self.cum_event_count;
            }
            for sample in &mut self.cumulative_history.samples {
                sample.cum_fill_qty = normalize_non_negative(sample.cum_fill_qty - pruned.fill_qty);
                sample.cum_kill_qty = normalize_non_negative(sample.cum_kill_qty - pruned.kill_qty);
                sample.cum_net_qty = sample.cum_fill_qty - sample.cum_kill_qty;
                sample.cum_ratio =
                    compute_fill_kill_ratio(sample.cum_fill_qty, sample.cum_kill_qty);
            }
            self.fill_kill_epoch = self.fill_kill_epoch.wrapping_add(1);
            self.cumulative_epoch = self.cumulative_epoch.wrapping_add(1);
        }

        let initial_cumulative_len = self.cumulative_history.samples.len();
        self.cumulative_history
            .trim_window(now_ms, ROLLING_WINDOW_MS);
        if self.cumulative_history.samples.len() != initial_cumulative_len {
            self.cumulative_epoch = self.cumulative_epoch.wrapping_add(1);
        }
        self.last_cumulative_sample_ms = self
            .cumulative_history
            .latest()
            .map(|sample| sample.timestamp_ms)
            .unwrap_or(0);
    }

    pub fn kpi_snapshot(&self) -> FillKillKpis {
        let cum_ratio = compute_fill_kill_ratio(self.cum_fill_qty, self.cum_kill_qty);
        let overfill_pct = if self.cum_event_count > 0 {
            (self.cum_overfill_count as f64 / self.cum_event_count as f64) * 100.0
        } else {
            0.0
        };

        FillKillKpis {
            cum_fill_qty: self.cum_fill_qty,
            cum_kill_qty: self.cum_kill_qty,
            cum_ratio,
            cum_net_qty: self.cum_fill_qty - self.cum_kill_qty,
            overfill_pct,
            cum_overfill_count: self.cum_overfill_count,
            cum_event_count: self.cum_event_count,
        }
    }

    pub fn reset_fill_kill(&mut self) {
        self.fill_kill_history.reset();
        self.fill_kill_epoch = self.fill_kill_epoch.wrapping_add(1);
        self.cum_fill_qty = 0.0;
        self.cum_kill_qty = 0.0;
        self.cum_overfill_count = 0;
        self.cum_event_count = 0;
        self.cumulative_history.reset();
        self.cumulative_epoch = self.cumulative_epoch.wrapping_add(1);
        self.last_cumulative_sample_ms = 0;
    }

    fn append_cumulative_sample(&mut self, timestamp_ms: u64) {
        let cumulative = CumulativeSample {
            timestamp_ms,
            cum_fill_qty: self.cum_fill_qty,
            cum_kill_qty: self.cum_kill_qty,
            cum_net_qty: self.cum_fill_qty - self.cum_kill_qty,
            cum_ratio: compute_fill_kill_ratio(self.cum_fill_qty, self.cum_kill_qty),
        };

        if self
            .cumulative_history
            .latest()
            .is_some_and(|sample| sample.timestamp_ms == timestamp_ms)
        {
            if let Some(last) = self.cumulative_history.samples.back_mut() {
                *last = cumulative;
            }
        } else {
            self.cumulative_history.push(cumulative);
        }

        self.cumulative_epoch = self.cumulative_epoch.wrapping_add(1);
        self.last_cumulative_sample_ms = timestamp_ms;
    }
}

fn build_fill_kill_sample(burst: &BurstState, tick_size: f64) -> FillKillSample {
    let fill_qty = sanitize_qty(burst.fill_qty);

    let Some(touch_price) = touch_price(&burst.pre_book, burst.direction) else {
        return build_na_sample(burst.last_trade_timestamp_ms, fill_qty, burst.direction);
    };
    let Some(touch_idx) = price_to_tick_index(touch_price, tick_size) else {
        return build_na_sample(burst.last_trade_timestamp_ms, fill_qty, burst.direction);
    };
    let Some(worst_idx) = price_to_tick_index(burst.worst_trade_price, tick_size) else {
        return build_na_sample(burst.last_trade_timestamp_ms, fill_qty, burst.direction);
    };

    let lower_idx = touch_idx.min(worst_idx);
    let upper_idx = touch_idx.max(worst_idx);
    let levels_moved = touch_idx.abs_diff(worst_idx);

    let pre_resting_walked_qty = sanitize_qty(match burst.direction {
        BurstDirection::Buy => {
            sum_walked_qty(&burst.pre_book.asks, lower_idx, upper_idx, tick_size)
        }
        BurstDirection::Sell => {
            sum_walked_qty(&burst.pre_book.bids, lower_idx, upper_idx, tick_size)
        }
    });

    let kill_qty = (pre_resting_walked_qty - fill_qty).max(0.0);
    let ratio = compute_fill_kill_ratio(fill_qty, kill_qty);
    let signed_log_ratio = compute_signed_log_ratio(fill_qty, kill_qty);
    let overfill = fill_qty > pre_resting_walked_qty * OVERFILL_THRESHOLD_MULTIPLIER;

    FillKillSample {
        timestamp_ms: burst.last_trade_timestamp_ms,
        fill_qty,
        kill_qty,
        pre_resting_walked_qty,
        levels_moved,
        ratio,
        direction: burst.direction,
        signed_log_ratio,
        overfill,
    }
}

fn build_na_sample(timestamp_ms: u64, fill_qty: f64, direction: BurstDirection) -> FillKillSample {
    FillKillSample {
        timestamp_ms,
        fill_qty,
        kill_qty: 0.0,
        pre_resting_walked_qty: 0.0,
        levels_moved: 0,
        ratio: RatioValue::Na,
        direction,
        signed_log_ratio: None,
        overfill: false,
    }
}

fn sanitize_qty(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn normalize_non_negative(value: f64) -> f64 {
    if !value.is_finite() || value <= RATIO_EPS {
        0.0
    } else {
        value
    }
}

fn touch_price(book: &OrderBook, direction: BurstDirection) -> Option<f64> {
    match direction {
        BurstDirection::Buy => book.best_ask().map(|(price, _)| price),
        BurstDirection::Sell => book.best_bid().map(|(price, _)| price),
    }
}

fn sum_walked_qty(
    levels: &BTreeMap<OrderedFloat<f64>, f64>,
    lower_idx: i64,
    upper_idx: i64,
    tick_size: f64,
) -> f64 {
    let mut total = 0.0;
    for (price, qty) in levels {
        if !qty.is_finite() || *qty <= 0.0 {
            continue;
        }
        let Some(level_idx) = price_to_tick_index(price.0, tick_size) else {
            continue;
        };
        if level_idx >= lower_idx && level_idx <= upper_idx {
            total += qty;
        }
    }
    if total.is_finite() {
        total
    } else {
        0.0
    }
}

fn price_to_tick_index(price: f64, tick_size: f64) -> Option<i64> {
    if !price.is_finite() || !tick_size.is_finite() || tick_size <= 0.0 {
        return None;
    }
    let raw = (price / tick_size).round();
    if !raw.is_finite() || raw < i64::MIN as f64 || raw > i64::MAX as f64 {
        return None;
    }
    Some(raw as i64)
}

fn should_flush_for_trade_boundary(
    burst: &BurstState,
    timestamp_ms: u64,
    direction: BurstDirection,
    depth_epoch: u64,
) -> bool {
    if burst.direction != direction || burst.epoch_id != depth_epoch {
        return true;
    }
    if timestamp_ms < burst.last_trade_timestamp_ms {
        return true;
    }
    if timestamp_ms.saturating_sub(burst.last_trade_timestamp_ms) > BURST_MAX_GAP_MS {
        return true;
    }
    timestamp_ms.saturating_sub(burst.start_timestamp_ms) > BURST_MAX_AGE_MS
}

fn compute_fill_kill_ratio(fill_qty: f64, kill_qty: f64) -> RatioValue {
    if !fill_qty.is_finite() || !kill_qty.is_finite() {
        return RatioValue::Na;
    }
    if fill_qty <= RATIO_EPS && kill_qty <= RATIO_EPS {
        return RatioValue::Na;
    }
    if kill_qty <= RATIO_EPS && fill_qty > RATIO_EPS {
        return RatioValue::Infinite;
    }
    RatioValue::Finite(fill_qty / kill_qty)
}

fn compute_signed_log_ratio(fill_qty: f64, kill_qty: f64) -> Option<f64> {
    if !fill_qty.is_finite() || !kill_qty.is_finite() || fill_qty < 0.0 || kill_qty < 0.0 {
        return None;
    }
    if fill_qty <= RATIO_EPS && kill_qty <= RATIO_EPS {
        return None;
    }
    if kill_qty <= RATIO_EPS && fill_qty > RATIO_EPS {
        return None;
    }

    let ratio = ((fill_qty + SIGNED_LOG_RATIO_EPS) / (kill_qty + SIGNED_LOG_RATIO_EPS)).log10();
    if !ratio.is_finite() {
        return None;
    }

    Some(ratio.clamp(-SIGNED_LOG_RATIO_CLAMP, SIGNED_LOG_RATIO_CLAMP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::OrderBook;

    fn assert_close(left: f64, right: f64, tol: f64) {
        assert!(
            (left - right).abs() <= tol,
            "left={left}, right={right}, tol={tol}"
        );
    }

    fn make_book(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> OrderBook {
        let mut book = OrderBook::new();
        for (price, qty) in bids {
            book.bids.insert(OrderedFloat(*price), *qty);
        }
        for (price, qty) in asks {
            book.asks.insert(OrderedFloat(*price), *qty);
        }
        book.last_event_time = 1_000;
        book
    }

    fn make_trade(timestamp_ms: u64, price: f64, quantity: f64, is_buy: bool) -> Trade {
        Trade {
            timestamp_ms,
            received_at_ms: timestamp_ms,
            price,
            quantity,
            is_buy,
        }
    }

    fn make_sample(
        timestamp_ms: u64,
        fill_qty: f64,
        kill_qty: f64,
        overfill: bool,
    ) -> FillKillSample {
        FillKillSample {
            timestamp_ms,
            fill_qty,
            kill_qty,
            pre_resting_walked_qty: fill_qty + kill_qty,
            levels_moved: 1,
            ratio: compute_fill_kill_ratio(fill_qty, kill_qty),
            direction: BurstDirection::Buy,
            signed_log_ratio: compute_signed_log_ratio(fill_qty, kill_qty),
            overfill,
        }
    }

    fn record_sample(metrics: &mut MicroMetrics, sample: FillKillSample) {
        metrics.fill_kill_history.samples.push_back(sample.clone());
        metrics.on_fill_kill_sample(&sample);
    }

    #[test]
    fn burst_flush_on_side_flip() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(100.0, 3.0)], &[(101.0, 3.0)]);

        history.on_trade(&make_trade(1_000, 101.0, 1.0, true), 10, 1.0, &book);
        history.on_trade(&make_trade(1_010, 100.0, 2.0, false), 10, 1.0, &book);

        assert_eq!(history.samples.len(), 1);
        let sample = history.samples.back().expect("sample");
        assert_close(sample.fill_qty, 1.0, 1e-12);
    }

    #[test]
    fn burst_flush_on_gap() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(100.0, 3.0)], &[(101.0, 3.0)]);

        history.on_trade(&make_trade(1_000, 101.0, 1.0, true), 1, 1.0, &book);
        history.on_trade(&make_trade(1_081, 101.0, 1.0, true), 1, 1.0, &book);

        assert_eq!(history.samples.len(), 1);
        let sample = history.samples.back().expect("sample");
        assert_eq!(sample.timestamp_ms, 1_000);
    }

    #[test]
    fn burst_flush_on_epoch_advance() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(100.0, 3.0)], &[(101.0, 3.0)]);

        history.on_trade(&make_trade(1_000, 101.0, 1.0, true), 1, 1.0, &book);
        history.on_depth_epoch_advance(1_010, 2, 1.0);

        assert_eq!(history.samples.len(), 1);
        assert!(history.active_burst.is_none());
    }

    #[test]
    fn levels_moved_and_walked_qty_buy() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(99.0, 5.0)], &[(100.0, 2.0), (101.0, 3.0), (102.0, 4.0)]);

        history.on_trade(&make_trade(1_000, 102.0, 5.0, true), 1, 1.0, &book);
        history.on_depth_epoch_advance(1_001, 2, 1.0);

        let sample = history.samples.back().expect("sample");
        assert_eq!(sample.levels_moved, 2);
        assert_close(sample.pre_resting_walked_qty, 9.0, 1e-12);
        assert_close(sample.fill_qty, 5.0, 1e-12);
        assert_close(sample.kill_qty, 4.0, 1e-12);
        assert_eq!(sample.direction, BurstDirection::Buy);
        let RatioValue::Finite(ratio) = sample.ratio else {
            panic!("expected finite ratio");
        };
        assert_close(ratio, 1.25, 1e-12);
        assert!(sample.signed_log_ratio.is_some());
        assert!(!sample.overfill);
    }

    #[test]
    fn levels_moved_and_walked_qty_sell() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(100.0, 2.0), (99.0, 3.0), (98.0, 4.0)], &[(101.0, 5.0)]);

        history.on_trade(&make_trade(1_000, 98.0, 5.0, false), 1, 1.0, &book);
        history.on_depth_epoch_advance(1_001, 2, 1.0);

        let sample = history.samples.back().expect("sample");
        assert_eq!(sample.levels_moved, 2);
        assert_close(sample.pre_resting_walked_qty, 9.0, 1e-12);
        assert_close(sample.fill_qty, 5.0, 1e-12);
        assert_close(sample.kill_qty, 4.0, 1e-12);
        assert_eq!(sample.direction, BurstDirection::Sell);
        let RatioValue::Finite(ratio) = sample.ratio else {
            panic!("expected finite ratio");
        };
        assert_close(ratio, 1.25, 1e-12);
    }

    #[test]
    fn ratio_infinite_when_kill_zero() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(99.0, 1.0)], &[(100.0, 1.0), (101.0, 1.0)]);

        history.on_trade(&make_trade(1_000, 101.0, 3.0, true), 1, 1.0, &book);
        history.on_depth_epoch_advance(1_001, 2, 1.0);

        let sample = history.samples.back().expect("sample");
        assert_close(sample.kill_qty, 0.0, 1e-12);
        assert_eq!(sample.ratio, RatioValue::Infinite);
        assert!(sample.signed_log_ratio.is_none());
    }

    #[test]
    fn ratio_na_when_no_signal() {
        assert_eq!(compute_fill_kill_ratio(0.0, 0.0), RatioValue::Na);
    }

    #[test]
    fn signed_log_ratio_is_none_for_infinite_and_clamped_for_finite() {
        assert!(compute_signed_log_ratio(1.0, 0.0).is_none());

        let value = compute_signed_log_ratio(1000.0, 0.0001).expect("finite log ratio");
        assert!(value <= 3.0);
        assert!(value > 0.0);

        let negative = compute_signed_log_ratio(0.0, 1000.0).expect("negative log ratio");
        assert!(negative >= -3.0);
        assert!(negative < 0.0);
    }

    #[test]
    fn overfill_flag_when_fill_exceeds_walked_by_threshold() {
        let mut history = FillKillHistory::default();
        let book = make_book(&[(99.0, 1.0)], &[(100.0, 10.0)]);

        history.on_trade(&make_trade(1_000, 100.0, 10.21, true), 1, 1.0, &book);
        history.on_depth_epoch_advance(1_001, 2, 1.0);

        let sample = history.samples.back().expect("sample");
        assert!(sample.overfill);
        assert_eq!(sample.ratio, RatioValue::Infinite);
    }

    #[test]
    fn history_retention_max_samples() {
        let mut history = FillKillHistory::default();
        history.max_samples = 5;
        let book = make_book(&[(99.0, 1.0)], &[(100.0, 1.0)]);

        for i in 0..20 {
            let ts = i * 1_000;
            history.on_trade(&make_trade(ts, 100.0, 1.0, true), i as u64, 1.0, &book);
            history.on_depth_epoch_advance(ts + 1, i as u64 + 1, 1.0);
        }

        assert_eq!(history.samples.len(), 5);
        let first_ts = history.samples.front().map(|s| s.timestamp_ms).unwrap_or(0);
        let last_ts = history.samples.back().map(|s| s.timestamp_ms).unwrap_or(0);
        assert!(first_ts > 0);
        assert!(last_ts > first_ts);
    }

    #[test]
    fn cumulative_accumulates_and_kpi_snapshot_matches() {
        let mut metrics = MicroMetrics::default();
        metrics.on_fill_kill_sample(&make_sample(1_000, 3.0, 1.0, false));
        metrics.on_fill_kill_sample(&make_sample(1_100, 2.0, 0.0, true));

        assert_close(metrics.cum_fill_qty, 5.0, 1e-12);
        assert_close(metrics.cum_kill_qty, 1.0, 1e-12);
        assert_eq!(metrics.cum_event_count, 2);
        assert_eq!(metrics.cum_overfill_count, 1);
        assert_eq!(metrics.cumulative_history.samples.len(), 2);

        let latest = metrics
            .cumulative_history
            .latest()
            .expect("latest cumulative");
        assert_close(latest.cum_net_qty, 4.0, 1e-12);
        let RatioValue::Finite(ratio) = latest.cum_ratio else {
            panic!("expected finite cumulative ratio");
        };
        assert_close(ratio, 5.0, 1e-12);

        let kpis = metrics.kpi_snapshot();
        assert_close(kpis.overfill_pct, 50.0, 1e-12);
        assert_eq!(kpis.cum_event_count, 2);
        assert_eq!(kpis.cum_overfill_count, 1);
    }

    #[test]
    fn cumulative_ratio_infinite_when_no_cumulative_kill() {
        let mut metrics = MicroMetrics::default();
        metrics.on_fill_kill_sample(&make_sample(1_000, 2.0, 0.0, false));

        let kpis = metrics.kpi_snapshot();
        assert_eq!(kpis.cum_ratio, RatioValue::Infinite);
        assert_eq!(
            metrics
                .cumulative_history
                .latest()
                .expect("latest")
                .cum_ratio,
            RatioValue::Infinite
        );
    }

    #[test]
    fn sample_cumulative_adds_carry_forward_points() {
        let mut metrics = MicroMetrics::default();
        metrics.on_fill_kill_sample(&make_sample(1_000, 2.0, 1.0, false));

        metrics.sample_cumulative(1_200);
        assert_eq!(metrics.cumulative_history.samples.len(), 1);

        metrics.sample_cumulative(1_500);
        assert_eq!(metrics.cumulative_history.samples.len(), 2);
        let latest = metrics.cumulative_history.latest().expect("latest");
        assert_eq!(latest.timestamp_ms, 1_500);
        assert_close(latest.cum_fill_qty, 2.0, 1e-12);
        assert_close(latest.cum_kill_qty, 1.0, 1e-12);
    }

    #[test]
    fn reset_fill_kill_clears_event_and_cumulative_state() {
        let mut metrics = MicroMetrics::default();
        let sample = make_sample(1_000, 2.0, 1.0, true);
        metrics.fill_kill_history.samples.push_back(sample.clone());
        metrics.on_fill_kill_sample(&sample);

        metrics.reset_fill_kill();

        assert!(metrics.fill_kill_history.samples.is_empty());
        assert!(metrics.fill_kill_history.active_burst.is_none());
        assert!(metrics.cumulative_history.samples.is_empty());
        assert_close(metrics.cum_fill_qty, 0.0, 1e-12);
        assert_close(metrics.cum_kill_qty, 0.0, 1e-12);
        assert_eq!(metrics.cum_event_count, 0);
        assert_eq!(metrics.cum_overfill_count, 0);
    }

    #[test]
    fn prune_rolling_window_keeps_cutoff_sample_and_updates_kpis() {
        let mut metrics = MicroMetrics::default();
        record_sample(&mut metrics, make_sample(1_000, 3.0, 1.0, false));
        metrics.sample_cumulative(1_500);
        record_sample(&mut metrics, make_sample(2_000, 2.0, 1.0, true));
        metrics.sample_cumulative(2_500);
        record_sample(&mut metrics, make_sample(250_000, 4.0, 2.0, false));
        metrics.sample_cumulative(250_500);

        let fill_epoch_before = metrics.fill_kill_epoch;
        let cumulative_epoch_before = metrics.cumulative_epoch;

        metrics.prune_rolling_window(302_000);

        assert_eq!(metrics.fill_kill_history.samples.len(), 1);
        assert_eq!(
            metrics
                .fill_kill_history
                .samples
                .front()
                .expect("first sample")
                .timestamp_ms,
            250_000
        );
        assert_close(metrics.cum_fill_qty, 4.0, 1e-12);
        assert_close(metrics.cum_kill_qty, 2.0, 1e-12);
        assert_eq!(metrics.cum_event_count, 1);
        assert_eq!(metrics.cum_overfill_count, 0);
        assert!(metrics
            .cumulative_history
            .samples
            .iter()
            .all(|sample| sample.timestamp_ms >= 250_000));
        let latest = metrics.cumulative_history.latest().expect("latest");
        assert_close(latest.cum_fill_qty, 4.0, 1e-12);
        assert_close(latest.cum_kill_qty, 2.0, 1e-12);
        assert!(metrics.fill_kill_epoch > fill_epoch_before);
        assert!(metrics.cumulative_epoch > cumulative_epoch_before);
    }

    #[test]
    fn prune_rolling_window_expires_all_data_and_resets_kpis() {
        let mut metrics = MicroMetrics::default();
        record_sample(&mut metrics, make_sample(1_000, 2.0, 1.0, true));
        metrics.sample_cumulative(1_500);
        record_sample(&mut metrics, make_sample(2_000, 1.0, 1.0, false));
        metrics.sample_cumulative(2_500);

        metrics.prune_rolling_window(700_000);

        assert!(metrics.fill_kill_history.samples.is_empty());
        assert!(metrics.cumulative_history.samples.is_empty());
        assert_close(metrics.cum_fill_qty, 0.0, 1e-12);
        assert_close(metrics.cum_kill_qty, 0.0, 1e-12);
        assert_eq!(metrics.cum_event_count, 0);
        assert_eq!(metrics.cum_overfill_count, 0);
        let kpis = metrics.kpi_snapshot();
        assert_close(kpis.cum_fill_qty, 0.0, 1e-12);
        assert_close(kpis.cum_kill_qty, 0.0, 1e-12);
        assert_eq!(kpis.cum_ratio, RatioValue::Na);
        assert_eq!(kpis.cum_event_count, 0);
        assert_eq!(kpis.cum_overfill_count, 0);
    }

    #[test]
    fn rolling_window_trim_helper_is_non_destructive_when_used_on_clone() {
        let mut canonical = CumulativeHistory::default();
        canonical.push(CumulativeSample {
            timestamp_ms: 0,
            cum_fill_qty: 1.0,
            cum_kill_qty: 0.5,
            cum_net_qty: 0.5,
            cum_ratio: RatioValue::Finite(2.0),
        });
        canonical.push(CumulativeSample {
            timestamp_ms: 100_000,
            cum_fill_qty: 2.0,
            cum_kill_qty: 1.0,
            cum_net_qty: 1.0,
            cum_ratio: RatioValue::Finite(2.0),
        });
        canonical.push(CumulativeSample {
            timestamp_ms: 400_000,
            cum_fill_qty: 3.0,
            cum_kill_qty: 1.5,
            cum_net_qty: 1.5,
            cum_ratio: RatioValue::Finite(2.0),
        });

        let mut rolling = canonical.clone();
        rolling.trim_window(400_000, ROLLING_WINDOW_MS);

        assert_eq!(canonical.samples.len(), 3);
        assert_eq!(rolling.samples.len(), 1);
        assert_eq!(
            rolling.samples.front().expect("first").timestamp_ms,
            400_000
        );
    }
}

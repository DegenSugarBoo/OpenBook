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

fn make_sample(timestamp_ms: u64, fill_qty: f64, kill_qty: f64, overfill: bool) -> FillKillSample {
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

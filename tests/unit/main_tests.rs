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

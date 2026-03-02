use super::{
    DepthLevelDelta, DepthSide, EventDepthHistory, MarketImpact, OrderBook, Trade, TradeHistory,
    HISTORY_MAX_AGE_MS,
};
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
        received_at_ms: 89_000,
        price: 100.0,
        quantity: 1.0,
        is_buy: true,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 89_500,
        received_at_ms: 89_500,
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
        received_at_ms: 89_999,
        price: 100.0,
        quantity: 1.0,
        is_buy: true,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 90_000,
        received_at_ms: 90_000,
        price: 101.0,
        quantity: 2.0,
        is_buy: false,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 95_000,
        received_at_ms: 95_000,
        price: 102.0,
        quantity: 1.0,
        is_buy: true,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 99_500,
        received_at_ms: 99_500,
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
        received_at_ms: 90_000,
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
            received_at_ms: 90_000 + i,
            price: 100.0,
            quantity: 1.0,
            is_buy: i % 2 == 0,
        });
    }

    let tps = history.rolling_tps(100_000, 10_000);
    assert_close(tps, 3.7, 1e-12);
}

#[test]
fn event_depth_history_push_and_prune_by_age() {
    let mut book = OrderBook::new();
    book.bids.insert(OrderedFloat(100.0), 1.0);
    book.asks.insert(OrderedFloat(101.0), 1.0);

    let mut history = EventDepthHistory::new();
    history.reset_from_book(&book, 100_000, 1);

    // Push deltas at various times
    history.push_event(
        200_000,
        2,
        vec![DepthLevelDelta {
            side: DepthSide::Bid,
            price: 99.0,
            qty: 2.0,
        }],
        &book,
    );
    history.push_event(
        350_000,
        3,
        vec![DepthLevelDelta {
            side: DepthSide::Ask,
            price: 102.0,
            qty: 3.0,
        }],
        &book,
    );

    // Prune at 600_000 → cutoff = 420_000. Both deltas are old.
    history.prune(600_000);

    // All deltas are pruned under the 180s retention window.
    assert!(history.deltas.is_empty());
    // At least one checkpoint should remain (never drop the last one)
    assert!(!history.checkpoints.is_empty());
}

#[test]
fn event_depth_history_enforces_memory_cap() {
    let mut book = OrderBook::new();
    for i in 0..500 {
        book.bids.insert(OrderedFloat(100.0 + i as f64), 1000.0);
        book.asks.insert(OrderedFloat(200.0 + i as f64), 1000.0);
    }

    let mut history = EventDepthHistory::new();
    history.max_bytes = 1024; // artificially low
    history.reset_from_book(&book, 1_000, 1);

    // Push many events to exceed budget
    for i in 0..100 {
        history.push_event(
            2_000 + i * 100,
            2 + i,
            vec![DepthLevelDelta {
                side: DepthSide::Bid,
                price: 99.0,
                qty: i as f64,
            }],
            &book,
        );
    }

    history.prune(100_000);

    // After pruning under memory pressure, at least one checkpoint must remain
    assert!(!history.checkpoints.is_empty());
}

#[test]
fn delta_threshold_promotes_checkpoint() {
    let mut book = OrderBook::new();
    book.bids.insert(OrderedFloat(100.0), 1.0);
    book.asks.insert(OrderedFloat(101.0), 1.0);

    let mut history = EventDepthHistory::new();
    history.reset_from_book(&book, 1_000, 1);
    let initial_checkpoints = history.checkpoints.len();

    // Push a delta event with changes >= threshold
    let large_changes: Vec<DepthLevelDelta> = (0..super::DEPTH_DELTA_TO_CHECKPOINT_THRESHOLD)
        .map(|i| DepthLevelDelta {
            side: DepthSide::Bid,
            price: 50.0 + i as f64 * 0.01,
            qty: 1.0,
        })
        .collect();

    history.push_event(2_000, 2, large_changes, &book);

    // Should have added a checkpoint, not a delta
    assert_eq!(history.checkpoints.len(), initial_checkpoints + 1);
    // No new delta should have been added for this event
    assert!(history.deltas.is_empty());
}

#[test]
fn materialize_columns_replays_deltas_correctly() {
    let mut book = OrderBook::new();
    book.bids.insert(OrderedFloat(100.0), 5.0);
    book.asks.insert(OrderedFloat(101.0), 3.0);

    let mut history = EventDepthHistory::new();
    history.reset_from_book(&book, 1_000, 1);

    // Apply a delta that changes bid qty
    book.bids.insert(OrderedFloat(100.0), 10.0);
    history.push_event(
        1_500,
        2,
        vec![DepthLevelDelta {
            side: DepthSide::Bid,
            price: 100.0,
            qty: 10.0,
        }],
        &book,
    );

    let columns = history.materialize_columns(1_000, 2_000, 2);
    assert_eq!(columns.len(), 2);

    // First column at t=1000: bid qty should be 5.0
    let col0 = &columns[0];
    let bid_qty_0: f64 = col0.levels[..col0.bids_len]
        .iter()
        .find(|(p, _)| (*p - 100.0).abs() < 1e-9)
        .map(|(_, q)| *q)
        .unwrap_or(0.0);
    assert_close(bid_qty_0, 5.0, 1e-9);

    // Second column at t=1500: bid qty should be 10.0
    let col1 = &columns[1];
    let bid_qty_1: f64 = col1.levels[..col1.bids_len]
        .iter()
        .find(|(p, _)| (*p - 100.0).abs() < 1e-9)
        .map(|(_, q)| *q)
        .unwrap_or(0.0);
    assert_close(bid_qty_1, 10.0, 1e-9);
}

#[test]
fn reset_from_book_clears_old_state() {
    let mut book = OrderBook::new();
    book.bids.insert(OrderedFloat(100.0), 1.0);

    let mut history = EventDepthHistory::new();
    history.reset_from_book(&book, 1_000, 1);
    history.push_event(
        2_000,
        2,
        vec![DepthLevelDelta {
            side: DepthSide::Bid,
            price: 99.0,
            qty: 2.0,
        }],
        &book,
    );

    // Reset should clear everything
    let mut new_book = OrderBook::new();
    new_book.bids.insert(OrderedFloat(200.0), 5.0);
    history.reset_from_book(&new_book, 10_000, 100);

    assert_eq!(history.checkpoints.len(), 1);
    assert!(history.deltas.is_empty());
    assert_eq!(history.checkpoints[0].timestamp_ms, 10_000);
}

#[test]
fn trade_history_prunes_without_new_trades() {
    let mut history = TradeHistory::new(HISTORY_MAX_AGE_MS);
    history.trades.push_back(Trade {
        timestamp_ms: 1_000,
        received_at_ms: 1_000,
        price: 100.0,
        quantity: 1.0,
        is_buy: true,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 2_000,
        received_at_ms: 2_000,
        price: 101.0,
        quantity: 1.0,
        is_buy: false,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 310_000,
        received_at_ms: 310_000,
        price: 102.0,
        quantity: 1.0,
        is_buy: true,
    });

    let removed = history.prune_now(400_000);

    assert_eq!(removed, 2);
    assert_eq!(history.trades.len(), 1);
    assert_eq!(
        history.trades.front().map(|trade| trade.received_at_ms),
        Some(310_000)
    );
}

#[test]
fn trade_history_uses_received_at_for_retention() {
    let mut history = TradeHistory::new(HISTORY_MAX_AGE_MS);
    history.trades.push_back(Trade {
        timestamp_ms: 399_000,  // recent exchange timestamp
        received_at_ms: 90_000, // stale local timestamp
        price: 101.0,
        quantity: 1.0,
        is_buy: false,
    });
    history.trades.push_back(Trade {
        timestamp_ms: 1_000,     // old exchange timestamp
        received_at_ms: 350_000, // recent local timestamp
        price: 100.0,
        quantity: 1.0,
        is_buy: true,
    });

    let removed = history.prune_now(400_000);

    assert_eq!(removed, 1);
    assert_eq!(history.trades.len(), 1);
    assert_eq!(
        history.trades.front().map(|trade| trade.timestamp_ms),
        Some(1_000)
    );
    assert_eq!(
        history.trades.front().map(|trade| trade.received_at_ms),
        Some(350_000)
    );
}

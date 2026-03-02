use super::{build_tape_rows, format_hms_millis, OrderBookApp};
use crate::models::{DepthLevelDelta, DepthSide, DepthSlice, SharedState};
use eframe::egui;
use ordered_float::OrderedFloat;
use std::sync::Arc;

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn build_tape_rows_returns_newest_first() {
    let trades = vec![
        (1_000, 100.0, 1.0, true),
        (2_000, 101.0, 1.0, false),
        (3_000, 102.0, 1.0, true),
    ];

    let rows = build_tape_rows(&trades, None, 10);

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].timestamp_ms, 3_000);
    assert_eq!(rows[1].timestamp_ms, 2_000);
    assert_eq!(rows[2].timestamp_ms, 1_000);
}

#[test]
fn build_tape_rows_min_filter_is_inclusive() {
    let trades = vec![
        (1_000, 10.0, 9.0, true),   // 90
        (2_000, 10.0, 10.0, false), // 100
        (3_000, 10.0, 12.0, true),  // 120
    ];

    let rows = build_tape_rows(&trades, Some(100.0), 10);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].timestamp_ms, 3_000);
    assert_eq!(rows[1].timestamp_ms, 2_000);
    assert!(rows.iter().all(|row| row.notional_usd >= 100.0));
}

#[test]
fn build_tape_rows_without_filter_returns_all_rows() {
    let trades = vec![
        (1_000, 10.0, 1.0, true),
        (2_000, 11.0, 1.0, false),
        (3_000, 12.0, 1.0, true),
    ];

    let rows = build_tape_rows(&trades, None, 10);

    assert_eq!(rows.len(), 3);
}

#[test]
fn build_tape_rows_applies_row_cap() {
    let trades = vec![
        (1_000, 10.0, 1.0, true),
        (2_000, 11.0, 1.0, false),
        (3_000, 12.0, 1.0, true),
    ];

    let rows = build_tape_rows(&trades, None, 2);

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].timestamp_ms, 3_000);
    assert_eq!(rows[1].timestamp_ms, 2_000);
}

#[test]
fn build_tape_rows_with_zero_row_cap_returns_empty() {
    let trades = vec![
        (1_000, 10.0, 1.0, true),
        (2_000, 11.0, 1.0, false),
        (3_000, 12.0, 1.0, true),
    ];
    let rows = build_tape_rows(&trades, None, 0);
    assert!(rows.is_empty());
}

#[test]
fn build_tape_rows_handles_empty_input() {
    let rows = build_tape_rows(&[], Some(100.0), 100);
    assert!(rows.is_empty());
}

#[test]
fn format_hms_millis_formats_known_timestamps() {
    assert_eq!(format_hms_millis(0), "00:00:00.000");
    assert_eq!(format_hms_millis(3_723_004), "01:02:03.004");
}

#[test]
fn format_hms_millis_handles_boundaries() {
    assert_eq!(format_hms_millis(59_999), "00:00:59.999");
    assert_eq!(format_hms_millis(60_000), "00:01:00.000");
    assert_eq!(format_hms_millis(3_599_999), "00:59:59.999");
    assert_eq!(format_hms_millis(3_600_000), "01:00:00.000");
    assert_eq!(format_hms_millis(86_400_001), "00:00:00.001");
}

#[test]
fn nearest_depth_slice_index_picks_closest() {
    let slices = vec![
        Arc::new(DepthSlice {
            timestamp_ms: 1_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 2_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 4_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
    ];

    assert_eq!(
        OrderBookApp::nearest_depth_slice_index_from_slices(&slices, 500),
        0
    );
    assert_eq!(
        OrderBookApp::nearest_depth_slice_index_from_slices(&slices, 1_500),
        0
    );
    assert_eq!(
        OrderBookApp::nearest_depth_slice_index_from_slices(&slices, 3_500),
        2
    );
    assert_eq!(
        OrderBookApp::nearest_depth_slice_index_from_slices(&slices, 9_000),
        2
    );
}

#[test]
fn build_slice_index_map_matches_binary_search() {
    let slices = vec![
        Arc::new(DepthSlice {
            timestamp_ms: 1_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 2_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 4_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 8_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
    ];
    let img_width: usize = 17;
    let view_time_start = 750.0;
    let time_span = 8_300.0;
    let x_denom = (img_width.saturating_sub(1)).max(1) as f64;

    let map = OrderBookApp::build_slice_index_map(&slices, img_width, view_time_start, time_span);

    assert_eq!(map.len(), img_width);
    for (x, &idx) in map.iter().enumerate() {
        let t_ms = (view_time_start + (x as f64 / x_denom) * time_span).max(0.0) as u64;
        let expected = OrderBookApp::nearest_depth_slice_index_from_slices(&slices, t_ms);
        assert_eq!(idx, expected);
    }
}

#[test]
fn clone_snapshot_reuses_depth_slices_until_epoch_changes() {
    let mut state = SharedState::new();
    state.order_book.bids.insert(OrderedFloat(100.0), 5.0);
    state.order_book.asks.insert(OrderedFloat(101.0), 3.0);
    state
        .depth_history
        .reset_from_book(&state.order_book, 1_000, 1);
    state.depth_history_epoch = 1;

    let snapshot_1 = state.clone_snapshot(1_000.0);
    let snapshot_2 = state.clone_snapshot(1_000.0);

    assert!(Arc::ptr_eq(
        &snapshot_1.depth_slices,
        &snapshot_2.depth_slices
    ));

    state.order_book.bids.insert(OrderedFloat(100.0), 7.0);
    state.depth_history.push_event(
        1_500,
        2,
        vec![DepthLevelDelta {
            side: DepthSide::Bid,
            price: 100.0,
            qty: 7.0,
        }],
        &state.order_book,
    );
    state.depth_history_epoch = 2;

    let snapshot_3 = state.clone_snapshot(1_000.0);
    assert!(!Arc::ptr_eq(
        &snapshot_2.depth_slices,
        &snapshot_3.depth_slices
    ));
}

#[test]
fn hover_row_price_mapping_is_centered() {
    let img_h = 4;
    let price_min = 100.0;
    let price_max = 200.0;

    assert_close(
        OrderBookApp::price_at_row(0, img_h, price_min, price_max),
        187.5,
    );
    assert_close(
        OrderBookApp::price_at_row(2, img_h, price_min, price_max),
        137.5,
    );
    assert_close(
        OrderBookApp::price_at_row(3, img_h, price_min, price_max),
        112.5,
    );
}

#[test]
fn split_side_grids_preserve_overlap() {
    let img_h = 8;
    let mut bid_grid = vec![0.0_f32; img_h];
    let mut ask_grid = vec![0.0_f32; img_h];
    let idx = OrderBookApp::heatmap_cell_idx(0, 3, img_h);

    let total_after_bid =
        OrderBookApp::accumulate_side_qty(&mut bid_grid, &mut ask_grid, idx, 6.0, true);
    let total_after_ask =
        OrderBookApp::accumulate_side_qty(&mut bid_grid, &mut ask_grid, idx, 4.0, false);

    assert_close(bid_grid[idx] as f64, 6.0);
    assert_close(ask_grid[idx] as f64, 4.0);
    assert_close(total_after_bid as f64, 6.0);
    assert_close(total_after_ask as f64, 10.0);
}

#[test]
fn latest_trade_for_pointer_returns_newest_timestamp() {
    let trades = vec![
        (1_000, 100.0, 1.0, true),
        (2_500, 101.0, 2.0, false),
        (2_000, 99.5, 3.0, true),
    ];
    assert_eq!(
        OrderBookApp::latest_trade_for_pointer(&trades),
        Some((2_500, 101.0))
    );
}

#[test]
fn latest_trade_for_pointer_returns_none_for_empty_input() {
    assert_eq!(OrderBookApp::latest_trade_for_pointer(&[]), None);
}

#[test]
fn is_live_time_view_uses_explicit_tolerance_ms() {
    assert!(OrderBookApp::is_live_time_view(99_999.5, 100_000.0, 1.0));
    assert!(!OrderBookApp::is_live_time_view(99_998.0, 100_000.0, 1.0));
}

#[test]
fn latest_trade_auto_center_price_returns_latest_when_live_follow() {
    let price = OrderBookApp::latest_trade_auto_center_price(true, Some((2_500, 101.25_f64)));
    assert_eq!(price, Some(101.25));
}

#[test]
fn latest_trade_auto_center_price_returns_none_when_not_live_follow() {
    let price = OrderBookApp::latest_trade_auto_center_price(false, Some((2_500, 101.25_f64)));
    assert_eq!(price, None);
}

#[test]
fn latest_trade_auto_center_price_returns_none_when_no_trades() {
    let price = OrderBookApp::latest_trade_auto_center_price(true, None);
    assert_eq!(price, None);
}

#[test]
fn latest_trade_auto_center_price_returns_none_when_price_is_non_finite() {
    let price = OrderBookApp::latest_trade_auto_center_price(true, Some((2_500, f64::NAN)));
    assert_eq!(price, None);
}

#[test]
fn pointer_y_fraction_filters_out_of_range_price() {
    assert_eq!(OrderBookApp::pointer_y_fraction(95.0, 100.0, 110.0), None);
    let inside = OrderBookApp::pointer_y_fraction(105.0, 100.0, 110.0);
    assert!(inside.is_some());
}

#[test]
fn live_strip_width_scales_and_clamps() {
    assert_close(OrderBookApp::live_strip_width(100.0) as f64, 28.0);
    assert!((OrderBookApp::live_strip_width(240.0) - 38.4).abs() < 1e-4);
    assert_close(OrderBookApp::live_strip_width(1000.0) as f64, 88.0);
}

#[test]
fn split_heatmap_rects_preserves_dimensions() {
    let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(300.0, 120.0));
    let (data_rect, live_strip_rect) = OrderBookApp::split_heatmap_rects(rect);

    assert_close(data_rect.height() as f64, rect.height() as f64);
    assert_close(live_strip_rect.height() as f64, rect.height() as f64);
    assert_close(
        (data_rect.width() + live_strip_rect.width()) as f64,
        rect.width() as f64,
    );
    assert_close(data_rect.left() as f64, rect.left() as f64);
    assert_close(live_strip_rect.right() as f64, rect.right() as f64);
}

#[test]
fn depth_time_bounds_uses_latest_depth_timestamp_for_end() {
    let slices = vec![
        Arc::new(DepthSlice {
            timestamp_ms: 10_000,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 10_500,
            levels: Vec::new(),
            bids_len: 0,
        }),
        Arc::new(DepthSlice {
            timestamp_ms: 11_250,
            levels: Vec::new(),
            bids_len: 0,
        }),
    ];
    let (start, end) = OrderBookApp::depth_time_bounds(&slices).expect("time bounds");
    assert_eq!(start, 10_000.0);
    assert_eq!(end, 11_250.0);
}

#[test]
fn depth_time_bounds_returns_none_for_empty_input() {
    assert!(OrderBookApp::depth_time_bounds(&[]).is_none());
}

use crate::micro::{
    BurstDirection, CumulativeSample, FillKillKpis, FillKillSample, RatioValue, ROLLING_WINDOW_MS,
};
use crate::models::{
    DepthCheckpoint, DepthDeltaEvent, DepthSlice, MarketImpact, PickerSharedState, PickerStatus,
    SharedState, HISTORY_MAX_AGE_MS,
};
use crate::workspace::{
    build_default_tree, ensure_all_panes, migrate_legacy_prefs, migrate_v1_store,
    sanitize_layout_store, LayoutStoreV2, LegacyLayoutPrefs, LegacyLayoutStore, PaneKind,
    PaneRenderer, WorkspaceBehavior, DEFAULT_LAYOUT_PROFILE_NAME, LAYOUT_PREFS_KEY,
    LAYOUT_STORE_V1_KEY, LAYOUT_STORE_V2_KEY,
};
use crate::{spawn_ticker_task, spawn_ws_task};
use eframe::egui;
use ordered_float::OrderedFloat;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEPTH_LEVELS: usize = 15;

const BID_COLOR: egui::Color32 = egui::Color32::from_rgb(0, 200, 80);
const BID_BRIGHT: egui::Color32 = egui::Color32::from_rgb(50, 255, 100);
const BID_DIM: egui::Color32 = egui::Color32::from_rgb(0, 100, 40);
const ASK_COLOR: egui::Color32 = egui::Color32::from_rgb(220, 50, 50);
const ASK_BRIGHT: egui::Color32 = egui::Color32::from_rgb(255, 80, 80);
const ASK_DIM: egui::Color32 = egui::Color32::from_rgb(120, 0, 0);
const AMBER_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 180, 70);
const CYAN_COLOR: egui::Color32 = egui::Color32::from_rgb(0, 200, 255);
const DIM_GRAY: egui::Color32 = egui::Color32::from_rgb(110, 110, 110);
const EVENT_LOG_RANGE: f64 = 3.0;
const TAPE_ROW_CAP_MIN: usize = 20;
const TAPE_ROW_CAP_MAX: usize = 300;
const PICKER_MAX_ROWS: usize = 8;
const REPAINT_MS_IDLE: u64 = 66;
const REPAINT_MS_ACTIVE: u64 = 16;
const INTERACTION_DECAY_MS: u64 = 200;

struct FillKillTimeDomain<'a> {
    start_ms: u64,
    end_ms: u64,
    cumulative: &'a [CumulativeSample],
    events: &'a [FillKillSample],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeatmapRenderKey {
    depth_history_epoch: u64,
    img_w: usize,
    img_h: usize,
    heatmap_price_center_bits: u64,
    heatmap_price_range_bits: u64,
    heatmap_time_offset_bits: u64,
    heatmap_time_window_bits: u64,
    heatmap_min_qty_bits: u64,
}

#[derive(Clone, Copy, Debug)]
struct HoverCell {
    slice_idx: usize,
    row: usize,
    timestamp_ms: u64,
    price: f64,
}

#[derive(Clone, Copy, Debug)]
struct HeatmapHoverLookup {
    data_rect: egui::Rect,
    heatmap_h: f32,
    view_time_start: f64,
    view_time_end: f64,
    img_h: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct PerfWindowSnapshot {
    fps: f64,
    frame_avg_ms: f64,
    frame_max_ms: f64,
    heatmap_rebuilds_per_sec: f64,
    heatmap_rebuild_avg_ms: f64,
    heatmap_rebuild_last_ms: f64,
    heatmap_rebuild_max_ms: f64,
    repaint_target_ms: f64,
}

// Temporary perf instrumentation for FPS/rebuild benchmarking.
struct PerfStats {
    window_start: Instant,
    frame_count: u32,
    frame_total_ms: f64,
    frame_max_ms: f64,
    heatmap_rebuild_count: u32,
    heatmap_rebuild_total_ms: f64,
    heatmap_rebuild_max_ms: f64,
    heatmap_rebuild_last_ms: f64,
    snapshot: PerfWindowSnapshot,
}

impl PerfStats {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            frame_count: 0,
            frame_total_ms: 0.0,
            frame_max_ms: 0.0,
            heatmap_rebuild_count: 0,
            heatmap_rebuild_total_ms: 0.0,
            heatmap_rebuild_max_ms: 0.0,
            heatmap_rebuild_last_ms: 0.0,
            snapshot: PerfWindowSnapshot {
                repaint_target_ms: REPAINT_MS_IDLE as f64,
                ..PerfWindowSnapshot::default()
            },
        }
    }

    fn set_repaint_target_ms(&mut self, repaint_target_ms: f64) {
        self.snapshot.repaint_target_ms = repaint_target_ms.max(0.0);
    }

    fn record_frame(&mut self, elapsed: Duration) {
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        self.frame_count = self.frame_count.saturating_add(1);
        self.frame_total_ms += elapsed_ms;
        self.frame_max_ms = self.frame_max_ms.max(elapsed_ms);
        self.roll_window_if_ready(Instant::now());
    }

    fn record_heatmap_rebuild(&mut self, elapsed: Duration) {
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
        self.heatmap_rebuild_count = self.heatmap_rebuild_count.saturating_add(1);
        self.heatmap_rebuild_total_ms += elapsed_ms;
        self.heatmap_rebuild_max_ms = self.heatmap_rebuild_max_ms.max(elapsed_ms);
        self.heatmap_rebuild_last_ms = elapsed_ms;
        self.roll_window_if_ready(Instant::now());
    }

    fn snapshot(&self) -> PerfWindowSnapshot {
        self.snapshot
    }

    fn roll_window_if_ready(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.window_start);
        let elapsed_s = elapsed.as_secs_f64();
        if elapsed_s < 1.0 {
            return;
        }

        let frame_count = self.frame_count as f64;
        let rebuild_count = self.heatmap_rebuild_count as f64;
        self.snapshot.fps = frame_count / elapsed_s;
        self.snapshot.frame_avg_ms = if self.frame_count > 0 {
            self.frame_total_ms / frame_count
        } else {
            0.0
        };
        self.snapshot.frame_max_ms = self.frame_max_ms;
        self.snapshot.heatmap_rebuilds_per_sec = rebuild_count / elapsed_s;
        self.snapshot.heatmap_rebuild_avg_ms = if self.heatmap_rebuild_count > 0 {
            self.heatmap_rebuild_total_ms / rebuild_count
        } else {
            0.0
        };
        self.snapshot.heatmap_rebuild_last_ms = self.heatmap_rebuild_last_ms;
        self.snapshot.heatmap_rebuild_max_ms = self.heatmap_rebuild_max_ms;

        self.window_start = now;
        self.frame_count = 0;
        self.frame_total_ms = 0.0;
        self.frame_max_ms = 0.0;
        self.heatmap_rebuild_count = 0;
        self.heatmap_rebuild_total_ms = 0.0;
        self.heatmap_rebuild_max_ms = 0.0;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerSortMode {
    Volume,
    Change,
    Alphabetical,
}

#[derive(Clone, Debug)]
struct PickerRow {
    symbol: String,
    base_asset: String,
    last_price: f64,
    change_pct: f64,
    quote_volume: f64,
    fuzzy_score: i32,
}

#[derive(Clone, Debug, PartialEq)]
struct TapeRow {
    timestamp_ms: u64,
    is_buy: bool,
    price: f64,
    qty: f64,
    notional_usd: f64,
}

fn format_price(value: f64, decimals: usize) -> String {
    format!("{:.*}", decimals, value)
}

fn format_volume(vol: f64) -> String {
    if vol >= 1_000_000_000.0 {
        format!("{:.1}B", vol / 1_000_000_000.0)
    } else if vol >= 1_000_000.0 {
        format!("{:.1}M", vol / 1_000_000.0)
    } else if vol >= 1_000.0 {
        format!("{:.1}K", vol / 1_000.0)
    } else {
        format!("{:.0}", vol)
    }
}

fn build_tape_rows(
    trades: &[(u64, f64, f64, bool)],
    min_notional: Option<f64>,
    row_cap: usize,
) -> Vec<TapeRow> {
    if row_cap == 0 {
        return Vec::new();
    }

    let min_notional = min_notional.and_then(|value| {
        if value.is_finite() {
            Some(value.max(0.0))
        } else {
            None
        }
    });
    let mut rows = Vec::with_capacity(row_cap.min(trades.len()));

    for &(timestamp_ms, price, qty, is_buy) in trades.iter().rev() {
        let notional_usd = price * qty;
        if let Some(min) = min_notional {
            if notional_usd < min {
                continue;
            }
        }

        rows.push(TapeRow {
            timestamp_ms,
            is_buy,
            price,
            qty,
            notional_usd,
        });

        if rows.len() >= row_cap {
            break;
        }
    }

    rows
}

fn format_hms_millis(timestamp_ms: u64) -> String {
    let day_ms = timestamp_ms % 86_400_000;
    let hours = day_ms / 3_600_000;
    let minutes = (day_ms % 3_600_000) / 60_000;
    let seconds = (day_ms % 60_000) / 1_000;
    let millis = day_ms % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

pub struct OrderBookApp {
    shared: Arc<Mutex<SharedState>>,
    shutdown_flag: Arc<AtomicBool>,
    picker_state: Arc<Mutex<PickerSharedState>>,
    ticker_shutdown: Arc<AtomicBool>,
    symbol_input: String,
    picker_open: bool,
    picker_query: String,
    picker_selected_idx: usize,
    picker_sort_mode: PickerSortMode,
    picker_last_epoch: u64,
    picker_last_query: String,
    picker_filtered: Vec<PickerRow>,
    bin_width_input: String,
    active_symbol: String,
    active_bin_width: f64,
    ws_running: bool,
    impact_notional_usd: f64,
    impact_notional_input: String,
    impact_side_is_buy: bool,
    dock_tree: egui_tiles::Tree<PaneKind>,
    pane_tile_ids: HashMap<PaneKind, egui_tiles::TileId>,
    layout_store: LayoutStoreV2,
    active_profile_idx: usize,
    selected_profile_idx: usize,
    layout_dirty: bool,
    save_as_modal_open: bool,
    save_as_name_input: String,
    rename_modal_open: bool,
    rename_name_input: String,
    delete_confirm_open: bool,
    layout_error_message: Option<String>,
    persist_layout_store_on_next_frame: bool,
    tape_min_notional_enabled: bool,
    tape_min_notional_usd: f64,
    tape_min_notional_input: String,
    tape_row_cap: usize,
    fill_kill_show_infinity: bool,
    fill_kill_highlight_overfill: bool,
    fill_kill_hover_ts: Option<f64>,
    heatmap_min_qty: f64,
    heatmap_min_qty_input: String,
    // Heatmap state
    heatmap_texture: Option<egui::TextureHandle>,
    heatmap_render_key: Option<HeatmapRenderKey>,
    heatmap_price_center: f64,
    heatmap_price_center_ema: f64,
    heatmap_price_range: f64,
    heatmap_auto_center: bool,
    heatmap_time_offset: f64,
    heatmap_time_window: f64,
    heatmap_grid_bid_qty: Vec<f32>,
    heatmap_grid_ask_qty: Vec<f32>,
    heatmap_pixels: Vec<egui::Color32>,
    heatmap_grid_cols: usize,
    heatmap_grid_rows: usize,
    desync_reconnect_deadline: Option<Instant>,
    last_interaction_time: Instant,
    show_perf_overlay: bool,
    perf_stats: PerfStats,
}

struct AppPaneRenderer<'a> {
    app: &'a mut OrderBookApp,
    state: &'a StateSnapshot,
    ctx: &'a egui::Context,
}

impl PaneRenderer for AppPaneRenderer<'_> {
    fn render_pane(&mut self, ui: &mut egui::Ui, pane: PaneKind) {
        match pane {
            PaneKind::Heatmap => self.app.render_heatmap_pane_body(ui, self.ctx, self.state),
            PaneKind::OrderBook => self.app.render_order_book_pane_body(ui, self.state),
            PaneKind::MarketImpact => self.app.render_market_impact_pane_body(ui, self.state),
            PaneKind::FillKill => self.app.render_fill_kill_pane_body(ui, self.state),
            PaneKind::TradesTape => self.app.render_trades_tape_pane_body(ui, self.state),
        }
    }
}

impl OrderBookApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Dark theme
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(13, 17, 23);
        visuals.window_fill = egui::Color32::from_rgb(13, 17, 23);
        visuals.extreme_bg_color = egui::Color32::from_rgb(22, 27, 34);
        visuals.faint_bg_color = egui::Color32::from_rgb(22, 27, 34);
        cc.egui_ctx.set_visuals(visuals);

        // Use a monospace font for numbers
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "mono".to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(include_bytes!(
                "../assets/JetBrainsMono-Regular.ttf"
            ))),
        );
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "mono".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "mono".to_owned());
        cc.egui_ctx.set_fonts(fonts);

        let shared = Arc::new(Mutex::new(SharedState::new()));
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let picker_state = Arc::new(Mutex::new(PickerSharedState::new()));
        let ticker_shutdown = Arc::new(AtomicBool::new(false));

        let default_symbol = "btcusdt".to_string();
        let default_bin = 1.0;
        let (mut layout_store, persist_layout_store_on_next_frame) = if let Some(store) = cc
            .storage
            .and_then(|storage| eframe::get_value::<LayoutStoreV2>(storage, LAYOUT_STORE_V2_KEY))
            .map(sanitize_layout_store)
        {
            (store, false)
        } else if let Some(store_v1) = cc.storage.and_then(|storage| {
            eframe::get_value::<LegacyLayoutStore>(storage, LAYOUT_STORE_V1_KEY)
        }) {
            (migrate_v1_store(store_v1), true)
        } else if let Some(prefs) = cc
            .storage
            .and_then(|storage| eframe::get_value::<LegacyLayoutPrefs>(storage, LAYOUT_PREFS_KEY))
        {
            (migrate_legacy_prefs(prefs), true)
        } else {
            (LayoutStoreV2::default(), false)
        };
        let active_profile_idx = layout_store.active_index();
        let mut dock_tree = layout_store
            .profiles
            .get(active_profile_idx)
            .map(|profile| profile.dock_tree.clone())
            .unwrap_or_else(build_default_tree);
        let pane_tile_ids = ensure_all_panes(&mut dock_tree);
        if let Some(profile) = layout_store.profiles.get_mut(active_profile_idx) {
            profile.dock_tree = dock_tree.clone();
        }

        // Start WS immediately
        spawn_ws_task(
            default_symbol.clone(),
            default_bin,
            Arc::clone(&shared),
            Arc::clone(&shutdown_flag),
        );
        spawn_ticker_task(Arc::clone(&picker_state), Arc::clone(&ticker_shutdown));

        Self {
            shared,
            shutdown_flag,
            picker_state,
            ticker_shutdown,
            symbol_input: "btcusdt".to_string(),
            picker_open: false,
            picker_query: String::new(),
            picker_selected_idx: 0,
            picker_sort_mode: PickerSortMode::Volume,
            picker_last_epoch: 0,
            picker_last_query: String::new(),
            picker_filtered: Vec::new(),
            bin_width_input: "1".to_string(),
            active_symbol: default_symbol,
            active_bin_width: default_bin,
            ws_running: true,
            impact_notional_usd: 10_000.0,
            impact_notional_input: "10000".to_string(),
            impact_side_is_buy: true,
            dock_tree,
            pane_tile_ids,
            layout_store,
            active_profile_idx,
            selected_profile_idx: active_profile_idx,
            layout_dirty: false,
            save_as_modal_open: false,
            save_as_name_input: String::new(),
            rename_modal_open: false,
            rename_name_input: String::new(),
            delete_confirm_open: false,
            layout_error_message: None,
            persist_layout_store_on_next_frame,
            tape_min_notional_enabled: false,
            tape_min_notional_usd: 10_000.0,
            tape_min_notional_input: "10000".to_string(),
            tape_row_cap: 120,
            fill_kill_show_infinity: true,
            fill_kill_highlight_overfill: true,
            fill_kill_hover_ts: None,
            heatmap_min_qty: 0.0,
            heatmap_min_qty_input: "0".to_string(),
            heatmap_texture: None,
            heatmap_render_key: None,
            heatmap_price_center: 0.0,
            heatmap_price_center_ema: 0.0,
            heatmap_price_range: 0.0,
            heatmap_auto_center: true,
            heatmap_time_offset: 0.0,
            heatmap_time_window: 0.0,
            heatmap_grid_bid_qty: Vec::new(),
            heatmap_grid_ask_qty: Vec::new(),
            heatmap_pixels: Vec::new(),
            heatmap_grid_cols: 0,
            heatmap_grid_rows: 0,
            desync_reconnect_deadline: None,
            last_interaction_time: Instant::now(),
            show_perf_overlay: true,
            perf_stats: PerfStats::new(),
        }
    }

    fn reconnect(&mut self) {
        if !self.picker_query.trim().is_empty() {
            self.symbol_input = self.picker_query.trim().to_lowercase();
        }

        // Shutdown old connection
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Give the old thread a moment, then create new flag
        self.shutdown_flag = Arc::new(AtomicBool::new(false));
        self.shared = Arc::new(Mutex::new(SharedState::new()));

        let symbol = self.symbol_input.trim().to_lowercase();
        let symbol = if symbol.is_empty() {
            "btcusdt".to_string()
        } else {
            symbol
        };
        let bin_width: f64 = self.bin_width_input.trim().parse().unwrap_or(1.0);
        let bin_width = if bin_width <= 0.0 { 1.0 } else { bin_width };

        self.active_symbol = symbol.clone();
        self.active_bin_width = bin_width;

        // Reset heatmap state
        self.heatmap_texture = None;
        self.heatmap_render_key = None;
        self.heatmap_price_center = 0.0;
        self.heatmap_price_center_ema = 0.0;
        self.heatmap_price_range = 0.0;
        self.heatmap_auto_center = true;
        self.heatmap_time_offset = 0.0;
        self.heatmap_time_window = 0.0;
        self.heatmap_grid_bid_qty.clear();
        self.heatmap_grid_ask_qty.clear();
        self.heatmap_pixels.clear();
        self.heatmap_grid_cols = 0;
        self.heatmap_grid_rows = 0;
        self.fill_kill_hover_ts = None;
        self.desync_reconnect_deadline = None;

        spawn_ws_task(
            symbol,
            bin_width,
            Arc::clone(&self.shared),
            Arc::clone(&self.shutdown_flag),
        );
        self.ws_running = true;
    }

    fn recompute_picker_list(&mut self) {
        let state = self.picker_state.lock().unwrap();
        let query = self.picker_query.trim().to_uppercase();

        let mut rows: Vec<PickerRow> = Vec::with_capacity(state.catalog.len());
        for entry in &state.catalog {
            let score = if query.is_empty() {
                0
            } else if entry.symbol.starts_with(&query) {
                100
            } else if entry.base_asset.starts_with(&query) {
                90
            } else if entry.symbol.contains(&query) {
                70
            } else if entry.base_asset.contains(&query) {
                60
            } else {
                let mut qi = query.chars().peekable();
                for ch in entry.symbol.chars() {
                    if qi.peek() == Some(&ch) {
                        qi.next();
                    }
                }
                if qi.peek().is_none() {
                    30
                } else {
                    -1
                }
            };

            if !query.is_empty() && score < 0 {
                continue;
            }

            let ticker = state.live_tickers.get(&entry.symbol);
            rows.push(PickerRow {
                symbol: entry.symbol.clone(),
                base_asset: entry.base_asset.clone(),
                last_price: ticker.map(|t| t.last_price).unwrap_or(0.0),
                change_pct: ticker.map(|t| t.change_pct_24h).unwrap_or(0.0),
                quote_volume: ticker.map(|t| t.quote_volume_24h).unwrap_or(0.0),
                fuzzy_score: score,
            });
        }

        rows.sort_by(|a, b| {
            if !query.is_empty() {
                let score_cmp = b.fuzzy_score.cmp(&a.fuzzy_score);
                if score_cmp != std::cmp::Ordering::Equal {
                    return score_cmp;
                }
            }
            match self.picker_sort_mode {
                PickerSortMode::Volume => b
                    .quote_volume
                    .partial_cmp(&a.quote_volume)
                    .unwrap_or(std::cmp::Ordering::Equal),
                PickerSortMode::Change => b
                    .change_pct
                    .partial_cmp(&a.change_pct)
                    .unwrap_or(std::cmp::Ordering::Equal),
                PickerSortMode::Alphabetical => a.symbol.cmp(&b.symbol),
            }
        });

        rows.truncate(PICKER_MAX_ROWS);

        self.picker_last_epoch = state.ticker_epoch;
        self.picker_last_query = self.picker_query.clone();
        drop(state);
        self.picker_filtered = rows;
        if self.picker_selected_idx >= self.picker_filtered.len() {
            self.picker_selected_idx = self.picker_filtered.len().saturating_sub(1);
        }
    }

    fn select_symbol_for_reconnect(&mut self, symbol: &str) {
        self.picker_query = symbol.to_lowercase();
        self.symbol_input = self.picker_query.clone();
        self.picker_open = false;
    }

    fn pane_visible(&self, pane: PaneKind) -> bool {
        self.pane_tile_ids
            .get(&pane)
            .copied()
            .is_some_and(|tile_id| self.dock_tree.tiles.is_visible(tile_id))
    }

    fn set_pane_visible(&mut self, pane: PaneKind, visible: bool) {
        if let Some(tile_id) = self.pane_tile_ids.get(&pane).copied() {
            self.dock_tree.tiles.set_visible(tile_id, visible);
            self.layout_dirty = true;
        }
    }

    fn sync_profile_indices(&mut self) {
        if self.layout_store.profiles.is_empty() {
            self.layout_store = LayoutStoreV2::default();
        }
        self.active_profile_idx = self.layout_store.active_index();
        if self.selected_profile_idx >= self.layout_store.profiles.len() {
            self.selected_profile_idx = self.active_profile_idx;
        }
    }

    fn persist_layout_store(&self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, LAYOUT_STORE_V2_KEY, &self.layout_store);
    }

    fn save_active_profile(&mut self) {
        self.sync_profile_indices();
        if let Some(profile) = self.layout_store.profiles.get_mut(self.active_profile_idx) {
            profile.dock_tree = self.dock_tree.clone();
            self.layout_store.active_profile = profile.name.clone();
        }
        self.layout_dirty = false;
    }

    fn apply_profile(&mut self, idx: usize) {
        if idx >= self.layout_store.profiles.len() {
            return;
        }
        self.dock_tree = self.layout_store.profiles[idx].dock_tree.clone();
        self.pane_tile_ids = ensure_all_panes(&mut self.dock_tree);
        if let Some(profile) = self.layout_store.profiles.get_mut(idx) {
            profile.dock_tree = self.dock_tree.clone();
        }
        self.layout_store.set_active_index(idx);
        self.active_profile_idx = idx;
        self.selected_profile_idx = idx;
        self.layout_dirty = false;
    }

    fn create_profile(&mut self, name: &str) -> Result<(), String> {
        let idx = self
            .layout_store
            .create_profile(name, self.dock_tree.clone())?;
        self.active_profile_idx = idx;
        self.selected_profile_idx = idx;
        self.layout_dirty = false;
        Ok(())
    }

    fn rename_profile(&mut self, idx: usize, name: &str) -> Result<(), String> {
        self.layout_store.rename_profile(idx, name)?;
        self.sync_profile_indices();
        Ok(())
    }

    fn delete_profile(&mut self, idx: usize) -> Result<bool, String> {
        let deleted_active = self.layout_store.delete_profile(idx)?;
        self.sync_profile_indices();
        Ok(deleted_active)
    }

    fn reset_layout(&mut self) {
        self.dock_tree = build_default_tree();
        self.pane_tile_ids = ensure_all_panes(&mut self.dock_tree);
        self.layout_dirty = true;
    }
}

impl eframe::App for OrderBookApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.save_active_profile();
        self.persist_layout_store(storage);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.ticker_shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let frame_start = Instant::now();

        let repaint_ms =
            if self.last_interaction_time.elapsed() < Duration::from_millis(INTERACTION_DECAY_MS) {
                REPAINT_MS_ACTIVE
            } else {
                REPAINT_MS_IDLE
            };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));
        self.perf_stats.set_repaint_target_ms(repaint_ms as f64);

        if let Ok(val) = self.impact_notional_input.trim().parse::<f64>() {
            self.impact_notional_usd = val.max(0.0);
        }

        let state = {
            let mut shared = self.shared.lock().unwrap();
            shared.clone_snapshot(self.impact_notional_usd)
        };

        if !state.connected && state.status_msg.contains("Desynced") {
            if let Some(deadline) = self.desync_reconnect_deadline {
                if Instant::now() >= deadline {
                    self.desync_reconnect_deadline = None;
                    self.reconnect();
                    self.perf_stats.record_frame(frame_start.elapsed());
                    return;
                }
            } else {
                self.desync_reconnect_deadline = Some(Instant::now() + Duration::from_secs(1));
            }
        } else {
            self.desync_reconnect_deadline = None;
        }

        // Auto-center heatmap on mid price with EMA smoothing to prevent jitter.
        // Keep an unsnapped EMA center so bin snapping does not introduce quantization lock.
        if self.heatmap_auto_center && state.mid_price > 0.0 {
            let alpha = 0.03; // ~1-second settling at 30 FPS
            let target = state.mid_price;
            if self.heatmap_price_center_ema <= 0.0 || !self.heatmap_price_center_ema.is_finite() {
                self.heatmap_price_center_ema = if self.heatmap_price_center > 0.0 {
                    self.heatmap_price_center
                } else {
                    target
                };
            }
            self.heatmap_price_center_ema += (target - self.heatmap_price_center_ema) * alpha;
            self.heatmap_price_center =
                Self::snap_price_to_bin(self.heatmap_price_center_ema, self.active_bin_width);
        }
        // Initialize center on first valid mid price
        if self.heatmap_price_center == 0.0 && state.mid_price > 0.0 {
            self.heatmap_price_center = state.mid_price;
            if self.heatmap_price_center_ema == 0.0 {
                self.heatmap_price_center_ema = state.mid_price;
            }
        }
        if self.heatmap_auto_center && self.heatmap_price_range == 0.0 && state.mid_price > 0.0 {
            let best_bid = state
                .bids
                .first()
                .map(|(price, _)| *price)
                .unwrap_or(state.mid_price);
            let best_ask = state
                .asks
                .first()
                .map(|(price, _)| *price)
                .unwrap_or(state.mid_price);
            let book_span = (best_ask - best_bid).abs();

            let bid_low = state
                .bids
                .iter()
                .take(DEPTH_LEVELS)
                .next_back()
                .map(|(price, _)| *price)
                .unwrap_or(best_bid);
            let ask_high = state
                .asks
                .iter()
                .take(DEPTH_LEVELS)
                .next_back()
                .map(|(price, _)| *price)
                .unwrap_or(best_ask);
            let depth_span = (ask_high - bid_low).abs().max(book_span);
            let min_range = (state.tick_size * 20.0).max(1e-9);
            self.heatmap_price_range = (depth_span * 1.5).max(min_range).clamp(min_range, 5000.0);
        }

        self.sync_profile_indices();
        if self.persist_layout_store_on_next_frame {
            if let Some(storage) = frame.storage_mut() {
                self.persist_layout_store(storage);
                self.persist_layout_store_on_next_frame = false;
            }
        }

        let popup_id = egui::Id::new("symbol_picker_popup");
        let mut picker_input_rect: Option<egui::Rect> = None;
        let mut picker_popup_rect: Option<egui::Rect> = None;
        let mut symbol_has_focus = false;
        let mut trigger_reconnect = false;
        let mut reset_layout = false;
        let mut picker_force_recompute = false;
        let mut load_profile_idx: Option<usize> = None;
        let mut save_layout = false;
        let mut open_save_as = false;
        let mut open_rename = false;
        let mut open_delete_confirm = false;
        let mut persist_layout_store_now = false;

        // Top panel: controls + status
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("⬡ DEPTH HEATMAP")
                        .color(egui::Color32::from_rgb(255, 200, 50))
                        .strong()
                        .size(16.0),
                );

                ui.separator();

                ui.label("Symbol:");
                let sym_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.picker_query)
                        .desired_width(120.0)
                        .font(egui::TextStyle::Monospace)
                        .hint_text("Search symbols..."),
                );
                picker_input_rect = Some(sym_resp.rect);
                symbol_has_focus = sym_resp.has_focus();

                if sym_resp.gained_focus() || sym_resp.changed() {
                    self.picker_open = true;
                }
                if sym_resp.gained_focus() {
                    picker_force_recompute = true;
                }
                if sym_resp.changed() {
                    self.picker_selected_idx = 0;
                    picker_force_recompute = true;
                }

                if ui.button("▶ Connect").clicked() {
                    trigger_reconnect = true;
                    self.picker_open = false;
                }

                ui.separator();

                ui.menu_button("Layout ▾", |ui| {
                    let active_name = self
                        .layout_store
                        .profiles
                        .get(self.active_profile_idx)
                        .map(|profile| profile.name.as_str())
                        .unwrap_or(DEFAULT_LAYOUT_PROFILE_NAME);
                    ui.label(
                        egui::RichText::new(format!("Active: {}", active_name))
                            .color(egui::Color32::GRAY)
                            .small(),
                    );
                    ui.separator();

                    ui.label(
                        egui::RichText::new("Profile")
                            .color(egui::Color32::GRAY)
                            .small(),
                    );
                    egui::ComboBox::from_id_salt("layout_profile_combo")
                        .selected_text(
                            self.layout_store
                                .profiles
                                .get(self.selected_profile_idx)
                                .map(|profile| profile.name.clone())
                                .unwrap_or_else(|| DEFAULT_LAYOUT_PROFILE_NAME.to_string()),
                        )
                        .show_ui(ui, |ui| {
                            for (idx, profile) in self.layout_store.profiles.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.selected_profile_idx,
                                    idx,
                                    profile.name.as_str(),
                                );
                            }
                        });

                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("Load").clicked() {
                            load_profile_idx = Some(self.selected_profile_idx);
                            ui.close();
                        }
                        if ui.button("Save").clicked() {
                            save_layout = true;
                            ui.close();
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Save As").clicked() {
                            open_save_as = true;
                            ui.close();
                        }
                        if ui.button("Rename").clicked() {
                            open_rename = true;
                            ui.close();
                        }
                    });

                    if ui
                        .add_enabled(
                            self.layout_store.profiles.len() > 1,
                            egui::Button::new("Delete"),
                        )
                        .clicked()
                    {
                        open_delete_confirm = true;
                        ui.close();
                    }

                    if ui.button("Reset Layout").clicked() {
                        reset_layout = true;
                        ui.close();
                    }

                    ui.separator();
                    for (pane, label) in [
                        (PaneKind::Heatmap, "Heatmap"),
                        (PaneKind::OrderBook, "Book"),
                        (PaneKind::MarketImpact, "Impact"),
                        (PaneKind::FillKill, "F:K"),
                        (PaneKind::TradesTape, "Tape"),
                    ] {
                        ui.push_id(pane.id_str(), |ui| {
                            let mut visible = self.pane_visible(pane);
                            if ui.checkbox(&mut visible, label).changed() {
                                self.set_pane_visible(pane, visible);
                            }
                        });
                    }

                    if self.layout_dirty {
                        ui.separator();
                        ui.label(
                            egui::RichText::new("Unsaved layout changes")
                                .color(AMBER_COLOR)
                                .small(),
                        );
                    }
                    if let Some(message) = &self.layout_error_message {
                        ui.label(egui::RichText::new(message).color(ASK_BRIGHT).small());
                    }
                });

                ui.separator();

                // Connection status
                let (status_color, status_icon) = if state.connected {
                    (egui::Color32::from_rgb(50, 255, 100), "●")
                } else {
                    (egui::Color32::from_rgb(255, 200, 50), "◌")
                };
                ui.label(
                    egui::RichText::new(format!("{} {}", status_icon, state.status_msg))
                        .color(status_color),
                );

                // Right-aligned info
                let history_span_secs = state
                    .depth_slices
                    .first()
                    .zip(state.depth_slices.last())
                    .map(|(first, last)| last.timestamp_ms.saturating_sub(first.timestamp_ms))
                    .unwrap_or(0)
                    .min(HISTORY_MAX_AGE_MS)
                    / 1_000;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let latency_text = if state.latency_ms >= 0 {
                        format!("{}ms", state.latency_ms)
                    } else {
                        "N/A".to_string()
                    };
                    ui.label(
                        egui::RichText::new(latency_text)
                            .color(egui::Color32::from_rgb(255, 200, 50))
                            .small(),
                    );
                    ui.label(
                        egui::RichText::new("Latency:")
                            .color(egui::Color32::GRAY)
                            .small(),
                    );

                    // Heatmap slice count
                    ui.label(
                        egui::RichText::new(format!("{}s", history_span_secs))
                            .color(egui::Color32::from_rgb(100, 180, 255))
                            .small(),
                    );
                    ui.label(
                        egui::RichText::new("History:")
                            .color(egui::Color32::GRAY)
                            .small(),
                    );
                });
            });
            ui.add_space(2.0);
        });

        if save_layout {
            self.save_active_profile();
            self.layout_error_message = None;
            persist_layout_store_now = true;
        }
        if let Some(idx) = load_profile_idx {
            self.apply_profile(idx);
            self.layout_error_message = None;
            persist_layout_store_now = true;
        }
        if open_save_as {
            self.save_as_modal_open = true;
            self.save_as_name_input.clear();
            self.layout_error_message = None;
        }
        if open_rename {
            self.rename_modal_open = true;
            self.rename_name_input = self
                .layout_store
                .profiles
                .get(self.selected_profile_idx)
                .map(|profile| profile.name.clone())
                .unwrap_or_default();
            self.layout_error_message = None;
        }
        if open_delete_confirm {
            self.delete_confirm_open = true;
            self.layout_error_message = None;
        }

        let mut apply_after_delete: Option<usize> = None;

        if self.save_as_modal_open {
            let mut open = self.save_as_modal_open;
            let mut create_clicked = false;
            let mut cancel_clicked = false;
            egui::Window::new("Save Layout As")
                .id(egui::Id::new("layout_save_as_modal"))
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("New profile name");
                    let input = ui.add(
                        egui::TextEdit::singleline(&mut self.save_as_name_input)
                            .desired_width(260.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        create_clicked = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Create").clicked() {
                            create_clicked = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_clicked = true;
                        }
                    });
                });
            if create_clicked {
                let profile_name = self.save_as_name_input.clone();
                match self.create_profile(&profile_name) {
                    Ok(()) => {
                        open = false;
                        self.layout_error_message = None;
                        persist_layout_store_now = true;
                    }
                    Err(err) => self.layout_error_message = Some(err),
                }
            }
            if cancel_clicked {
                open = false;
            }
            self.save_as_modal_open = open;
        }

        if self.rename_modal_open {
            let mut open = self.rename_modal_open;
            let mut rename_clicked = false;
            let mut cancel_clicked = false;
            egui::Window::new("Rename Layout")
                .id(egui::Id::new("layout_rename_modal"))
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Profile name");
                    let input = ui.add(
                        egui::TextEdit::singleline(&mut self.rename_name_input)
                            .desired_width(260.0)
                            .font(egui::TextStyle::Monospace),
                    );
                    if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        rename_clicked = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() {
                            rename_clicked = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_clicked = true;
                        }
                    });
                });
            if rename_clicked {
                let idx = self.selected_profile_idx;
                let profile_name = self.rename_name_input.clone();
                match self.rename_profile(idx, &profile_name) {
                    Ok(()) => {
                        open = false;
                        self.layout_error_message = None;
                        persist_layout_store_now = true;
                    }
                    Err(err) => self.layout_error_message = Some(err),
                }
            }
            if cancel_clicked {
                open = false;
            }
            self.rename_modal_open = open;
        }

        if self.delete_confirm_open {
            let mut open = self.delete_confirm_open;
            let mut delete_clicked = false;
            let mut cancel_clicked = false;
            let selected_name = self
                .layout_store
                .profiles
                .get(self.selected_profile_idx)
                .map(|profile| profile.name.as_str())
                .unwrap_or(DEFAULT_LAYOUT_PROFILE_NAME);
            egui::Window::new("Delete Layout")
                .id(egui::Id::new("layout_delete_modal"))
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(format!("Delete profile \"{}\"?", selected_name));
                    ui.horizontal(|ui| {
                        if ui.button("Delete").clicked() {
                            delete_clicked = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel_clicked = true;
                        }
                    });
                });
            if delete_clicked {
                let idx = self.selected_profile_idx;
                match self.delete_profile(idx) {
                    Ok(deleted_active) => {
                        if deleted_active {
                            apply_after_delete = Some(self.active_profile_idx);
                        }
                        open = false;
                        self.layout_error_message = None;
                        persist_layout_store_now = true;
                    }
                    Err(err) => self.layout_error_message = Some(err),
                }
            }
            if cancel_clicked {
                open = false;
            }
            self.delete_confirm_open = open;
        }

        if let Some(idx) = apply_after_delete {
            self.apply_profile(idx);
            persist_layout_store_now = true;
        }

        if persist_layout_store_now {
            if let Some(storage) = frame.storage_mut() {
                self.persist_layout_store(storage);
            }
        }

        let needs_recompute = {
            let picker_state = self.picker_state.lock().unwrap();
            self.picker_query != self.picker_last_query
                || (self.picker_open && picker_state.ticker_epoch != self.picker_last_epoch)
        };
        if picker_force_recompute || needs_recompute {
            self.recompute_picker_list();
        }

        if self.picker_open {
            let max_idx = self.picker_filtered.len().saturating_sub(1);
            let mut selected_idx = self.picker_selected_idx.min(max_idx);
            let mut down_pressed = false;
            let mut up_pressed = false;
            let mut enter_pressed = false;
            let mut escape_pressed = false;
            ctx.input(|i| {
                down_pressed = i.key_pressed(egui::Key::ArrowDown);
                up_pressed = i.key_pressed(egui::Key::ArrowUp);
                enter_pressed = i.key_pressed(egui::Key::Enter);
                escape_pressed = i.key_pressed(egui::Key::Escape);
            });
            if down_pressed {
                selected_idx = (selected_idx + 1).min(max_idx);
            }
            if up_pressed {
                selected_idx = selected_idx.saturating_sub(1);
            }
            self.picker_selected_idx = selected_idx;

            if escape_pressed {
                self.picker_open = false;
            } else if enter_pressed {
                if self.picker_filtered.is_empty() {
                    self.picker_open = false;
                    trigger_reconnect = true;
                } else {
                    let symbol = self.picker_filtered[self.picker_selected_idx.min(max_idx)]
                        .symbol
                        .clone();
                    self.select_symbol_for_reconnect(&symbol);
                    trigger_reconnect = true;
                }
            }
        }

        if !self.picker_open && symbol_has_focus && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            trigger_reconnect = true;
        }

        if self.picker_open {
            if let Some(input_rect) = picker_input_rect {
                let popup_response = egui::Area::new(popup_id)
                    .order(egui::Order::Foreground)
                    .fixed_pos(egui::pos2(input_rect.left(), input_rect.bottom() + 2.0))
                    .show(ctx, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_min_width(500.0);

                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("Sort:")
                                        .color(egui::Color32::GRAY)
                                        .small(),
                                );
                                for (mode, label) in [
                                    (PickerSortMode::Volume, "Vol"),
                                    (PickerSortMode::Change, "Chg"),
                                    (PickerSortMode::Alphabetical, "ABC"),
                                ] {
                                    let text = if self.picker_sort_mode == mode {
                                        egui::RichText::new(label).strong().color(AMBER_COLOR)
                                    } else {
                                        egui::RichText::new(label).color(egui::Color32::GRAY)
                                    };
                                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                                        self.picker_sort_mode = mode;
                                        self.recompute_picker_list();
                                    }
                                }

                                let picker_status = {
                                    let picker_state = self.picker_state.lock().unwrap();
                                    picker_state.status.clone()
                                };
                                let (status_text, status_color) = match picker_status {
                                    PickerStatus::Live => {
                                        ("●", egui::Color32::from_rgb(50, 255, 100))
                                    }
                                    PickerStatus::Loading => ("◌", egui::Color32::YELLOW),
                                    PickerStatus::Stale => ("◌ stale", egui::Color32::YELLOW),
                                    PickerStatus::Reconnecting => ("↻", egui::Color32::YELLOW),
                                    PickerStatus::Error(_) => ("✗", egui::Color32::RED),
                                };
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new(status_text)
                                                .color(status_color)
                                                .small(),
                                        );
                                    },
                                );
                            });

                            ui.separator();

                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [170.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new("Symbol")
                                            .color(egui::Color32::GRAY)
                                            .small(),
                                    ),
                                );
                                ui.add_sized(
                                    [95.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new("Last")
                                            .color(egui::Color32::GRAY)
                                            .small(),
                                    ),
                                );
                                ui.add_sized(
                                    [70.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new("24h %")
                                            .color(egui::Color32::GRAY)
                                            .small(),
                                    ),
                                );
                                ui.add_sized(
                                    [90.0, 16.0],
                                    egui::Label::new(
                                        egui::RichText::new("Volume")
                                            .color(egui::Color32::GRAY)
                                            .small(),
                                    ),
                                );
                            });

                            let mut clicked_symbol: Option<String> = None;
                            egui::ScrollArea::vertical()
                                .max_height(400.0)
                                .show(ui, |ui| {
                                    for (idx, row) in self.picker_filtered.iter().enumerate() {
                                        let is_selected = idx == self.picker_selected_idx;
                                        let bg = if is_selected {
                                            egui::Color32::from_rgb(40, 50, 70)
                                        } else {
                                            egui::Color32::TRANSPARENT
                                        };

                                        let price_text = if row.last_price > 0.0 {
                                            if row.last_price >= 1000.0 {
                                                format!("{:.2}", row.last_price)
                                            } else {
                                                format!("{:.4}", row.last_price)
                                            }
                                        } else {
                                            "—".to_string()
                                        };
                                        let sign = if row.change_pct > 0.0 { "+" } else { "" };
                                        let change_color = if row.change_pct > 0.0 {
                                            BID_COLOR
                                        } else if row.change_pct < 0.0 {
                                            ASK_COLOR
                                        } else {
                                            egui::Color32::GRAY
                                        };

                                        let row_response = egui::Frame::NONE
                                            .fill(bg)
                                            .inner_margin(egui::Margin::symmetric(4, 2))
                                            .show(ui, |ui| {
                                                ui.horizontal(|ui| {
                                                    ui.add_sized(
                                                        [170.0, 18.0],
                                                        egui::Label::new(
                                                            egui::RichText::new(format!(
                                                                "{} ({})",
                                                                row.symbol, row.base_asset
                                                            ))
                                                            .color(egui::Color32::WHITE)
                                                            .monospace(),
                                                        ),
                                                    );
                                                    ui.add_sized(
                                                        [95.0, 18.0],
                                                        egui::Label::new(
                                                            egui::RichText::new(price_text)
                                                                .color(egui::Color32::from_rgb(
                                                                    200, 200, 200,
                                                                ))
                                                                .monospace(),
                                                        ),
                                                    );
                                                    ui.add_sized(
                                                        [70.0, 18.0],
                                                        egui::Label::new(
                                                            egui::RichText::new(format!(
                                                                "{}{:.2}%",
                                                                sign, row.change_pct
                                                            ))
                                                            .color(change_color)
                                                            .monospace(),
                                                        ),
                                                    );
                                                    ui.add_sized(
                                                        [90.0, 18.0],
                                                        egui::Label::new(
                                                            egui::RichText::new(format_volume(
                                                                row.quote_volume,
                                                            ))
                                                            .color(egui::Color32::from_rgb(
                                                                150, 150, 150,
                                                            ))
                                                            .monospace(),
                                                        ),
                                                    );
                                                });
                                            })
                                            .response
                                            .interact(egui::Sense::click());

                                        if is_selected {
                                            row_response.scroll_to_me(Some(egui::Align::Center));
                                        }
                                        if row_response.hovered() {
                                            self.picker_selected_idx = idx;
                                        }
                                        if row_response.clicked() {
                                            clicked_symbol = Some(row.symbol.clone());
                                        }
                                    }
                                });

                            if self.picker_filtered.is_empty() {
                                ui.label(
                                    egui::RichText::new("No symbols found")
                                        .color(egui::Color32::GRAY)
                                        .italics(),
                                );
                            }

                            if let Some(symbol) = clicked_symbol {
                                self.select_symbol_for_reconnect(&symbol);
                                trigger_reconnect = true;
                            }
                        });
                    });
                picker_popup_rect = Some(popup_response.response.rect);
            }
        }

        if self.picker_open {
            let clicked_elsewhere = ctx.input(|i| {
                if !i.pointer.any_click() {
                    return false;
                }
                let pos = i.pointer.interact_pos().unwrap_or(egui::Pos2::ZERO);
                let in_input = picker_input_rect.is_some_and(|rect| rect.contains(pos));
                let in_popup = picker_popup_rect.is_some_and(|rect| rect.contains(pos));
                !in_input && !in_popup
            });
            if clicked_elsewhere {
                self.picker_open = false;
            }
        }

        if trigger_reconnect {
            self.reconnect();
            self.perf_stats.record_frame(frame_start.elapsed());
            return;
        }

        if reset_layout {
            self.reset_layout();
        }

        // Stats bar
        egui::TopBottomPanel::top("stats_bar").show(ctx, |ui| {
            let spread = state.spread;
            let mid = state.mid_price;
            let spread_pct = if mid > 0.0 {
                (spread / mid) * 100.0
            } else {
                0.0
            };

            let imbalance = state.imbalance;
            let imb_color = if imbalance > 0.0 {
                BID_COLOR
            } else if imbalance < 0.0 {
                ASK_COLOR
            } else {
                egui::Color32::GRAY
            };

            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Mid:").color(egui::Color32::GRAY));
                ui.label(
                    egui::RichText::new(format_price(mid, state.price_decimals))
                        .color(egui::Color32::WHITE)
                        .strong(),
                );
                ui.separator();
                ui.label(egui::RichText::new("Spread:").color(egui::Color32::GRAY));
                ui.label(
                    egui::RichText::new(format!(
                        "{} ({:.4}%)",
                        format_price(spread, state.price_decimals),
                        spread_pct
                    ))
                    .color(egui::Color32::from_rgb(200, 100, 255)),
                );
                ui.separator();
                ui.label(egui::RichText::new("Imbalance:").color(egui::Color32::GRAY));
                let imb_str = if imbalance > 0.0 {
                    format!("+{:.1}%", imbalance)
                } else {
                    format!("{:.1}%", imbalance)
                };
                ui.label(egui::RichText::new(imb_str).color(imb_color));
                ui.separator();
                ui.label(egui::RichText::new("TPS:").color(egui::Color32::GRAY));
                ui.label(
                    egui::RichText::new(format!("{:.1}/s", state.tps))
                        .color(egui::Color32::from_rgb(100, 180, 255)),
                );
                ui.separator();
                ui.label(egui::RichText::new("Bids:").color(egui::Color32::GRAY));
                ui.label(egui::RichText::new(format!("{}", state.book_bid_count)).color(BID_COLOR));
                ui.label(egui::RichText::new("Asks:").color(egui::Color32::GRAY));
                ui.label(egui::RichText::new(format!("{}", state.book_ask_count)).color(ASK_COLOR));
            });
        });

        let mut dock_tree = std::mem::replace(
            &mut self.dock_tree,
            egui_tiles::Tree::empty("workspace_tmp"),
        );
        let mut tree_edited = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut renderer = AppPaneRenderer {
                app: self,
                state: &state,
                ctx,
            };
            let mut behavior = WorkspaceBehavior {
                renderer: &mut renderer,
                edited: &mut tree_edited,
            };
            dock_tree.ui(&mut behavior, ui);
        });
        self.dock_tree = dock_tree;
        if tree_edited {
            self.layout_dirty = true;
        }

        self.perf_stats.record_frame(frame_start.elapsed());
    }
}

impl OrderBookApp {
    fn snap_price_to_bin(price: f64, bin_width: f64) -> f64 {
        if bin_width > 0.0 {
            (price / bin_width).round() * bin_width
        } else {
            price
        }
    }

    fn enable_heatmap_auto_center(&mut self) {
        self.heatmap_auto_center = true;
        if self.heatmap_price_center > 0.0 && self.heatmap_price_center.is_finite() {
            self.heatmap_price_center_ema = self.heatmap_price_center;
        }
    }

    fn render_heatmap_pane_body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        state: &StateSnapshot,
    ) {
        ui.set_min_size(egui::vec2(48.0, 48.0));
        self.render_heatmap(ui, ctx, state);
    }

    fn render_order_book_pane_body(&mut self, ui: &mut egui::Ui, state: &StateSnapshot) {
        ui.set_min_size(egui::vec2(48.0, 48.0));
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Bin:").color(egui::Color32::GRAY));
            ui.add(
                egui::TextEdit::singleline(&mut self.bin_width_input)
                    .desired_width(70.0)
                    .font(egui::TextStyle::Monospace),
            );
            if let Ok(val) = self.bin_width_input.trim().parse::<f64>() {
                if val > 0.0 {
                    self.active_bin_width = val;
                }
            }
        });
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.render_order_book_vertical(ui, state);
            });
    }

    /// Vertical stacked order book: asks on top (descending), spread in middle, bids below (descending)
    fn render_order_book_vertical(&self, ui: &mut egui::Ui, state: &StateSnapshot) {
        let bucket_size = self.active_bin_width;

        // Aggregate into buckets
        let mut bid_buckets: BTreeMap<OrderedFloat<f64>, f64> = BTreeMap::new();
        for &(price, qty) in state.bids.iter() {
            let bucket = (price / bucket_size).floor() * bucket_size;
            *bid_buckets.entry(OrderedFloat(bucket)).or_insert(0.0) += qty;
        }
        let bids: Vec<(f64, f64)> = bid_buckets
            .iter()
            .rev()
            .take(DEPTH_LEVELS)
            .map(|(p, &q)| (p.0, q))
            .collect();

        let mut ask_buckets: BTreeMap<OrderedFloat<f64>, f64> = BTreeMap::new();
        for &(price, qty) in state.asks.iter() {
            let bucket = (price / bucket_size).floor() * bucket_size;
            *ask_buckets.entry(OrderedFloat(bucket)).or_insert(0.0) += qty;
        }
        let asks: Vec<(f64, f64)> = ask_buckets
            .iter()
            .take(DEPTH_LEVELS)
            .map(|(p, &q)| (p.0, q))
            .collect();

        let max_qty = bids
            .iter()
            .chain(asks.iter())
            .map(|(_, q)| *q)
            .fold(0.0f64, f64::max);

        let available_w = ui.available_width();
        let row_h = 18.0_f32;
        let price_w = 90.0_f32;
        let qty_w = 80.0_f32;
        let bar_w = (available_w - price_w - qty_w - 12.0).max(20.0);

        // Header
        ui.horizontal(|ui| {
            ui.allocate_ui(egui::vec2(price_w, 14.0), |ui| {
                ui.label(
                    egui::RichText::new("PRICE")
                        .color(egui::Color32::GRAY)
                        .small(),
                );
            });
            ui.allocate_ui(egui::vec2(qty_w, 14.0), |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new("QTY")
                            .color(egui::Color32::GRAY)
                            .small(),
                    );
                });
            });
        });
        ui.add_space(2.0);

        // Asks (reversed so lowest ask is at bottom, meeting the spread)
        for (i, &(price, qty)) in asks.iter().enumerate().rev() {
            let is_best = i == 0;
            let color = if is_best { ASK_BRIGHT } else { ASK_COLOR };
            let ratio = if max_qty > 0.0 { qty / max_qty } else { 0.0 };

            ui.horizontal(|ui| {
                // Price
                ui.allocate_ui(egui::vec2(price_w, row_h), |ui| {
                    ui.label(
                        egui::RichText::new(format_price(price, state.price_decimals))
                            .color(color)
                            .strong(),
                    );
                });
                // Qty
                ui.allocate_ui(egui::vec2(qty_w, row_h), |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{:.4}", qty)).color(color));
                    });
                });
                // Bar
                let (bar_rect, _) =
                    ui.allocate_exact_size(egui::vec2(bar_w, row_h), egui::Sense::hover());
                let fill_w = (ratio as f32 * bar_w).max(0.0);
                if fill_w > 0.0 {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(bar_rect.left_top(), egui::vec2(fill_w, row_h)),
                        0.0,
                        if is_best {
                            ASK_COLOR.linear_multiply(0.5)
                        } else {
                            ASK_DIM
                        },
                    );
                }
            });
        }

        // Last traded price separator
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let (ltp_text, ltp_color) = if let Some(&(_, price, _, is_buy)) = state.trades.last() {
                let color = if is_buy {
                    egui::Color32::from_rgb(0, 255, 0)
                } else {
                    egui::Color32::from_rgb(255, 80, 80)
                };
                (
                    format!("--- LTP: {} ---", format_price(price, state.price_decimals)),
                    color,
                )
            } else {
                (
                    format!(
                        "--- Spread: {} ---",
                        format_price(state.spread, state.price_decimals)
                    ),
                    egui::Color32::from_rgb(200, 100, 255),
                )
            };
            ui.label(egui::RichText::new(ltp_text).color(ltp_color).small());
        });
        ui.add_space(2.0);

        // Bids (highest at top, descending)
        for (i, &(price, qty)) in bids.iter().enumerate() {
            let is_best = i == 0;
            let color = if is_best { BID_BRIGHT } else { BID_COLOR };
            let ratio = if max_qty > 0.0 { qty / max_qty } else { 0.0 };

            ui.horizontal(|ui| {
                // Price
                ui.allocate_ui(egui::vec2(price_w, row_h), |ui| {
                    ui.label(
                        egui::RichText::new(format_price(price, state.price_decimals))
                            .color(color)
                            .strong(),
                    );
                });
                // Qty
                ui.allocate_ui(egui::vec2(qty_w, row_h), |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{:.4}", qty)).color(color));
                    });
                });
                // Bar
                let (bar_rect, _) =
                    ui.allocate_exact_size(egui::vec2(bar_w, row_h), egui::Sense::hover());
                let fill_w = (ratio as f32 * bar_w).max(0.0);
                if fill_w > 0.0 {
                    ui.painter().rect_filled(
                        egui::Rect::from_min_size(bar_rect.left_top(), egui::vec2(fill_w, row_h)),
                        0.0,
                        if is_best {
                            BID_COLOR.linear_multiply(0.5)
                        } else {
                            BID_DIM
                        },
                    );
                }
            });
        }
    }

    fn base_asset_symbol(&self) -> String {
        let upper = self.active_symbol.to_uppercase();
        if let Some(base) = upper.strip_suffix("USDT") {
            base.to_string()
        } else if let Some(base) = upper.strip_suffix("USD") {
            base.to_string()
        } else if upper.is_empty() {
            "BASE".to_string()
        } else {
            upper
        }
    }

    fn render_market_impact_pane_body(&mut self, ui: &mut egui::Ui, state: &StateSnapshot) {
        ui.set_min_size(egui::vec2(48.0, 48.0));
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(2.0);

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Size ($):").color(egui::Color32::GRAY));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.impact_notional_input)
                            .desired_width(100.0)
                            .font(egui::TextStyle::Monospace),
                    );
                });
                if let Ok(val) = self.impact_notional_input.trim().parse::<f64>() {
                    self.impact_notional_usd = val.max(0.0);
                }

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Side:").color(egui::Color32::GRAY));
                    ui.radio_value(&mut self.impact_side_is_buy, true, "Buy");
                    ui.radio_value(&mut self.impact_side_is_buy, false, "Sell");
                });
                ui.add_space(4.0);

                if self.impact_notional_usd <= 0.0 {
                    ui.label(
                        egui::RichText::new("Enter a positive notional value.")
                            .color(egui::Color32::GRAY),
                    );
                    return;
                }

                let impact = if self.impact_side_is_buy {
                    &state.buy_impact
                } else {
                    &state.sell_impact
                };

                if impact.total_qty_filled <= 0.0 {
                    ui.label(
                        egui::RichText::new("No order book data yet.")
                            .color(egui::Color32::GRAY)
                            .italics(),
                    );
                    return;
                }

                let side_color = if self.impact_side_is_buy {
                    BID_BRIGHT
                } else {
                    ASK_BRIGHT
                };
                let slippage_color = if impact.slippage_bps < 5.0 {
                    BID_COLOR
                } else if impact.slippage_bps <= 20.0 {
                    AMBER_COLOR
                } else {
                    ASK_COLOR
                };
                let qty_symbol = self.base_asset_symbol();

                let mut row = |label: &str, value: String, value_color: egui::Color32| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(label).color(egui::Color32::GRAY));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(value).color(value_color).monospace());
                        });
                    });
                };

                row(
                    "Avg Fill:",
                    format_price(impact.avg_fill_price, state.price_decimals),
                    side_color,
                );
                row(
                    "Worst Fill:",
                    format_price(impact.worst_fill_price, state.price_decimals),
                    side_color,
                );
                row(
                    "Slippage:",
                    format!(
                        "{:.3}% ({:.1} bps)",
                        impact.slippage_pct, impact.slippage_bps
                    ),
                    slippage_color,
                );
                row(
                    "Levels Used:",
                    format!("{}", impact.levels_consumed),
                    egui::Color32::WHITE,
                );
                row(
                    "Qty Filled:",
                    format!("{:.6} {}", impact.total_qty_filled, qty_symbol),
                    egui::Color32::WHITE,
                );
                row(
                    "Notional:",
                    format!("${:.2}", impact.total_notional),
                    egui::Color32::WHITE,
                );

                let status = if impact.fully_filled {
                    (
                        format!("Fully filled (${:.2})", impact.total_notional),
                        BID_COLOR,
                    )
                } else {
                    (
                        format!(
                            "Partial fill (${:.2} of ${:.2})",
                            impact.total_notional, self.impact_notional_usd
                        ),
                        AMBER_COLOR,
                    )
                };
                row("Status:", status.0, status.1);
            });
    }

    fn render_fill_kill_pane_body(&mut self, ui: &mut egui::Ui, state: &StateSnapshot) {
        ui.set_min_size(egui::vec2(48.0, 48.0));
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.fill_kill_show_infinity, "Show Infinity");
            ui.checkbox(&mut self.fill_kill_highlight_overfill, "Highlight Overfill");
        });

        self.render_fill_kill_kpis(ui, &state.fill_kill_kpis);
        ui.add_space(6.0);
        let domain = self.compute_shared_time_domain(
            state.cumulative_series.as_slice(),
            state.fill_kill_series.as_slice(),
        );

        let top_h = (ui.available_height() * 0.48).max(96.0);
        let top_hovered = self.render_fill_kill_cumulative_chart(ui, &domain, top_h);
        ui.add_space(6.0);
        let bottom_hovered =
            self.render_fill_kill_event_chart(ui, &domain, ui.available_height().max(96.0));
        if !top_hovered && !bottom_hovered {
            self.fill_kill_hover_ts = None;
        }
    }

    fn render_trades_tape_pane_body(&mut self, ui: &mut egui::Ui, state: &StateSnapshot) {
        ui.set_min_size(egui::vec2(48.0, 48.0));
        ui.add_space(2.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Rows:").color(egui::Color32::GRAY));
            let mut row_cap = self.tape_row_cap as i64;
            ui.add(
                egui::DragValue::new(&mut row_cap)
                    .range(TAPE_ROW_CAP_MIN as i64..=TAPE_ROW_CAP_MAX as i64)
                    .speed(1.0),
            );
            self.tape_row_cap =
                row_cap.clamp(TAPE_ROW_CAP_MIN as i64, TAPE_ROW_CAP_MAX as i64) as usize;

            ui.separator();
            ui.checkbox(&mut self.tape_min_notional_enabled, "Min $");
            ui.add(
                egui::TextEdit::singleline(&mut self.tape_min_notional_input)
                    .desired_width(100.0)
                    .font(egui::TextStyle::Monospace),
            );
        });

        if let Ok(value) = self.tape_min_notional_input.trim().parse::<f64>() {
            if value.is_finite() {
                self.tape_min_notional_usd = value.max(0.0);
            }
        }

        let min_notional = if self.tape_min_notional_enabled {
            Some(self.tape_min_notional_usd.max(0.0))
        } else {
            None
        };

        let rows_unfiltered = state.trades.len();
        let rows_after_filter = if let Some(min) = min_notional {
            state
                .trades
                .iter()
                .filter(|(_, price, qty, _)| (*price * *qty) >= min)
                .count()
        } else {
            rows_unfiltered
        };
        let rows = build_tape_rows(state.trades.as_slice(), min_notional, self.tape_row_cap);

        ui.label(
            egui::RichText::new(format!(
                "Showing {}/{} (of {})",
                rows.len(),
                rows_after_filter,
                rows_unfiltered
            ))
            .color(egui::Color32::GRAY)
            .small(),
        );
        ui.add_space(4.0);

        if rows_unfiltered == 0 {
            ui.label(
                egui::RichText::new("No trades yet.")
                    .color(egui::Color32::GRAY)
                    .italics(),
            );
            return;
        }

        if rows_after_filter == 0 {
            ui.label(
                egui::RichText::new("No trades match current min size filter.")
                    .color(egui::Color32::GRAY)
                    .italics(),
            );
            return;
        }

        let time_col_w = 12_usize; // HH:MM:SS.mmm
        let side_col_w = 4_usize; // BUY/SELL
        let price_col_w = rows
            .iter()
            .map(|row| format_price(row.price, state.price_decimals).len())
            .max()
            .unwrap_or(5)
            .max("Price".len());
        let qty_col_w = rows
            .iter()
            .map(|row| format!("{:.4}", row.qty).len())
            .max()
            .unwrap_or(3)
            .max("Qty".len());
        let notional_col_w = rows
            .iter()
            .map(|row| format!("${:.2}", row.notional_usd).len())
            .max()
            .unwrap_or(8)
            .max("Notional".len());

        let header_text = format!(
            "{:<time_col_w$} {:<side_col_w$} {:>price_col_w$} {:>qty_col_w$} {:>notional_col_w$}",
            "Time", "Side", "Price", "Qty", "Notional",
        );
        ui.label(
            egui::RichText::new(header_text)
                .color(egui::Color32::GRAY)
                .monospace(),
        );
        ui.separator();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let base_fmt = egui::TextFormat {
                    font_id: egui::FontId::monospace(13.0),
                    color: egui::Color32::WHITE,
                    ..Default::default()
                };
                for row in &rows {
                    let (side_text, side_color) = if row.is_buy {
                        ("BUY", BID_BRIGHT)
                    } else {
                        ("SELL", ASK_BRIGHT)
                    };
                    let mut side_fmt = base_fmt.clone();
                    side_fmt.color = side_color;
                    let mut notional_fmt = base_fmt.clone();
                    notional_fmt.color = side_color;

                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        &format!("{:<time_col_w$} ", format_hms_millis(row.timestamp_ms)),
                        0.0,
                        base_fmt.clone(),
                    );
                    job.append(&format!("{:<side_col_w$} ", side_text), 0.0, side_fmt);
                    job.append(
                        &format!(
                            "{:>price_col_w$} ",
                            format_price(row.price, state.price_decimals)
                        ),
                        0.0,
                        base_fmt.clone(),
                    );
                    job.append(
                        &format!("{:>qty_col_w$} ", format!("{:.4}", row.qty)),
                        0.0,
                        base_fmt.clone(),
                    );
                    job.append(
                        &format!("{:>notional_col_w$}", format!("${:.2}", row.notional_usd)),
                        0.0,
                        notional_fmt,
                    );
                    ui.label(job);
                }
            });
    }

    fn render_fill_kill_kpis(&self, ui: &mut egui::Ui, kpis: &FillKillKpis) {
        let (ratio_text, ratio_color) = match kpis.cum_ratio {
            RatioValue::Na => ("N/A".to_string(), DIM_GRAY),
            RatioValue::Infinite => ("∞".to_string(), egui::Color32::WHITE),
            RatioValue::Finite(value) => (format!("{value:.2}"), egui::Color32::WHITE),
        };
        let net_color = if kpis.cum_net_qty > 0.0 {
            BID_COLOR
        } else if kpis.cum_net_qty < 0.0 {
            ASK_COLOR
        } else {
            egui::Color32::GRAY
        };

        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new(format!("5m Fill {:.3}", kpis.cum_fill_qty))
                    .color(CYAN_COLOR)
                    .small(),
            );
            ui.label(
                egui::RichText::new(format!("5m Kill {:.3}", kpis.cum_kill_qty))
                    .color(AMBER_COLOR)
                    .small(),
            );
            ui.label(
                egui::RichText::new(format!("5m F:K {}", ratio_text))
                    .color(ratio_color)
                    .small()
                    .strong(),
            );
            ui.label(
                egui::RichText::new(format!("5m Net {:.3}", kpis.cum_net_qty))
                    .color(net_color)
                    .small(),
            );
            ui.label(
                egui::RichText::new(format!("Overfill {:.1}%", kpis.overfill_pct))
                    .color(AMBER_COLOR)
                    .small(),
            );
        });
    }

    fn compute_shared_time_domain<'a>(
        &self,
        cumulative: &'a [CumulativeSample],
        events: &'a [FillKillSample],
    ) -> FillKillTimeDomain<'a> {
        let mut has_samples = false;
        let mut last_ts = 0_u64;

        if let Some(sample) = cumulative.last() {
            has_samples = true;
            last_ts = last_ts.max(sample.timestamp_ms);
        }
        if let Some(sample) = events.last() {
            has_samples = true;
            last_ts = last_ts.max(sample.timestamp_ms);
        }

        if !has_samples {
            return FillKillTimeDomain {
                start_ms: 0,
                end_ms: 1,
                cumulative: &[],
                events: &[],
            };
        }

        let start_ms = last_ts.saturating_sub(ROLLING_WINDOW_MS);
        let end_ms = if last_ts <= start_ms {
            start_ms.saturating_add(1)
        } else {
            last_ts
        };

        let cum_start = cumulative.partition_point(|sample| sample.timestamp_ms < start_ms);
        let cum_end = cumulative.partition_point(|sample| sample.timestamp_ms <= end_ms);
        let event_start = events.partition_point(|sample| sample.timestamp_ms < start_ms);
        let event_end = events.partition_point(|sample| sample.timestamp_ms <= end_ms);

        FillKillTimeDomain {
            start_ms,
            end_ms,
            cumulative: &cumulative[cum_start..cum_end],
            events: &events[event_start..event_end],
        }
    }

    fn project_time_x(plot: egui::Rect, timestamp_ms: f64, start_ms: u64, end_ms: u64) -> f32 {
        let span = end_ms.saturating_sub(start_ms).max(1) as f64;
        let x_frac = ((timestamp_ms - start_ms as f64) / span).clamp(0.0, 1.0) as f32;
        plot.left() + x_frac * plot.width()
    }

    fn nearest_depth_slice_index_from_slices(
        depth_slices: &[Arc<DepthSlice>],
        target_ts_ms: u64,
    ) -> usize {
        match depth_slices.binary_search_by_key(&target_ts_ms, |s| s.timestamp_ms) {
            Ok(i) => i,
            Err(i) => {
                if i == 0 {
                    0
                } else if i >= depth_slices.len() {
                    depth_slices.len().saturating_sub(1)
                } else {
                    let target = target_ts_ms as i128;
                    let dt_left = (target - depth_slices[i - 1].timestamp_ms as i128).abs();
                    let dt_right = (depth_slices[i].timestamp_ms as i128 - target).abs();
                    if dt_left <= dt_right {
                        i - 1
                    } else {
                        i
                    }
                }
            }
        }
    }

    fn nearest_depth_slice_index(state: &StateSnapshot, target_ts_ms: u64) -> usize {
        Self::nearest_depth_slice_index_from_slices(&state.depth_slices, target_ts_ms)
    }

    fn build_slice_index_map(
        depth_slices: &[Arc<DepthSlice>],
        img_width: usize,
        view_time_start: f64,
        time_span: f64,
    ) -> Vec<usize> {
        let x_denom = (img_width.saturating_sub(1)).max(1) as f64;
        let num_slices = depth_slices.len();
        let mut map = Vec::with_capacity(img_width);
        let mut cursor: usize = 0;

        for x in 0..img_width {
            let t_ms = (view_time_start + (x as f64 / x_denom) * time_span).max(0.0) as u64;
            while cursor + 1 < num_slices {
                let dt_cur =
                    (depth_slices[cursor].timestamp_ms as i128 - t_ms as i128).unsigned_abs();
                let dt_next =
                    (depth_slices[cursor + 1].timestamp_ms as i128 - t_ms as i128).unsigned_abs();
                if dt_next <= dt_cur {
                    cursor += 1;
                } else {
                    break;
                }
            }
            map.push(cursor);
        }

        map
    }

    fn heatmap_cell_idx(slice_idx: usize, row: usize, img_height: usize) -> usize {
        slice_idx * img_height + row
    }

    fn price_at_row(row: usize, img_height: usize, price_min: f64, price_max: f64) -> f64 {
        let span = price_max - price_min;
        if img_height == 0 || !span.is_finite() || span <= 0.0 {
            return price_min;
        }
        price_max - ((row as f64 + 0.5) / img_height as f64) * span
    }

    fn accumulate_side_qty(
        bid_grid: &mut [f32],
        ask_grid: &mut [f32],
        idx: usize,
        qty: f64,
        is_bid: bool,
    ) -> f32 {
        if is_bid {
            bid_grid[idx] += qty as f32;
        } else {
            ask_grid[idx] += qty as f32;
        }
        bid_grid[idx] + ask_grid[idx]
    }

    fn hover_cell_from_pointer(
        &self,
        pointer: egui::Pos2,
        state: &StateSnapshot,
        lookup: HeatmapHoverLookup,
    ) -> Option<HoverCell> {
        if state.depth_slices.is_empty()
            || lookup.img_h == 0
            || lookup.data_rect.width() <= 0.0
            || lookup.heatmap_h <= 0.0
            || !lookup.data_rect.contains(pointer)
        {
            return None;
        }

        let price_min = self.heatmap_price_center - self.heatmap_price_range / 2.0;
        let price_max = self.heatmap_price_center + self.heatmap_price_range / 2.0;
        let price_span = price_max - price_min;
        if !price_span.is_finite() || price_span <= 0.0 {
            return None;
        }

        let time_span = (lookup.view_time_end - lookup.view_time_start).max(1.0);
        let x_frac =
            ((pointer.x - lookup.data_rect.left()) / lookup.data_rect.width()).clamp(0.0, 1.0);
        let hover_ts = (lookup.view_time_start + x_frac as f64 * time_span).max(0.0) as u64;
        let slice_idx = Self::nearest_depth_slice_index(state, hover_ts);

        let y_frac = ((pointer.y - lookup.data_rect.top()) / lookup.heatmap_h).clamp(0.0, 1.0);
        let row = ((y_frac * lookup.img_h as f32) as usize).min(lookup.img_h.saturating_sub(1));
        let price = Self::price_at_row(row, lookup.img_h, price_min, price_max);
        let timestamp_ms = state.depth_slices[slice_idx].timestamp_ms;

        Some(HoverCell {
            slice_idx,
            row,
            timestamp_ms,
            price,
        })
    }

    fn render_fill_kill_cumulative_chart(
        &mut self,
        ui: &mut egui::Ui,
        domain: &FillKillTimeDomain<'_>,
        panel_h: f32,
    ) -> bool {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), panel_h.max(96.0)),
            egui::Sense::hover(),
        );
        let painter = ui.painter();

        painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(16, 20, 27));
        painter.text(
            egui::pos2(rect.left() + 8.0, rect.top() + 6.0),
            egui::Align2::LEFT_TOP,
            "Rolling 5m",
            egui::FontId::proportional(10.0),
            egui::Color32::GRAY,
        );

        let pad_left = 52.0;
        let pad_right = 8.0;
        let pad_top = 20.0;
        let pad_bottom = 18.0;
        let plot = egui::Rect::from_min_max(
            egui::pos2(rect.left() + pad_left, rect.top() + pad_top),
            egui::pos2(rect.right() - pad_right, rect.bottom() - pad_bottom),
        );
        if plot.width() <= 2.0 || plot.height() <= 2.0 {
            return false;
        }

        if domain.cumulative.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Collecting 5m series...",
                egui::FontId::proportional(11.0),
                DIM_GRAY,
            );
            return false;
        }

        let y_max = domain
            .cumulative
            .iter()
            .fold(0.0_f64, |acc, sample| {
                acc.max(sample.cum_fill_qty).max(sample.cum_kill_qty)
            })
            .max(1.0);

        for i in 0..=3 {
            let y = plot.top() + (i as f32 / 3.0) * plot.height();
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(32, 36, 44)),
            );
        }

        painter.text(
            egui::pos2(plot.left() - 4.0, plot.top()),
            egui::Align2::RIGHT_TOP,
            format!("{y_max:.2}"),
            egui::FontId::monospace(10.0),
            DIM_GRAY,
        );
        painter.text(
            egui::pos2(plot.left() - 4.0, plot.bottom()),
            egui::Align2::RIGHT_BOTTOM,
            "0.0",
            egui::FontId::monospace(10.0),
            DIM_GRAY,
        );
        painter.text(
            egui::pos2(plot.left(), plot.bottom() + 3.0),
            egui::Align2::LEFT_TOP,
            "5m ago",
            egui::FontId::proportional(9.0),
            DIM_GRAY,
        );
        painter.text(
            egui::pos2(plot.right(), plot.bottom() + 3.0),
            egui::Align2::RIGHT_TOP,
            "now",
            egui::FontId::proportional(9.0),
            DIM_GRAY,
        );

        let project = |timestamp_ms: u64, value: f64| -> egui::Pos2 {
            let x = Self::project_time_x(plot, timestamp_ms as f64, domain.start_ms, domain.end_ms);
            let y_frac = (value / y_max).clamp(0.0, 1.0) as f32;
            egui::pos2(x, plot.bottom() - y_frac * plot.height())
        };

        let fill_points: Vec<egui::Pos2> = domain
            .cumulative
            .iter()
            .map(|sample| project(sample.timestamp_ms, sample.cum_fill_qty))
            .collect();
        let kill_points: Vec<egui::Pos2> = domain
            .cumulative
            .iter()
            .map(|sample| project(sample.timestamp_ms, sample.cum_kill_qty))
            .collect();

        if fill_points.len() >= 2 {
            painter.add(egui::Shape::line(
                fill_points,
                egui::Stroke::new(1.8, CYAN_COLOR),
            ));
        }
        if kill_points.len() >= 2 {
            painter.add(egui::Shape::line(
                kill_points,
                egui::Stroke::new(1.8, AMBER_COLOR),
            ));
        }

        painter.text(
            egui::pos2(plot.left() + 6.0, plot.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Fill",
            egui::FontId::proportional(9.0),
            CYAN_COLOR,
        );
        painter.text(
            egui::pos2(plot.left() + 36.0, plot.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Kill",
            egui::FontId::proportional(9.0),
            AMBER_COLOR,
        );

        let hovered_here = response
            .hover_pos()
            .is_some_and(|pointer| response.hovered() && plot.contains(pointer));
        if hovered_here {
            if let Some(pointer) = response.hover_pos() {
                let x_frac = ((pointer.x - plot.left()) / plot.width()).clamp(0.0, 1.0) as f64;
                let hover_ts = domain.start_ms as f64
                    + x_frac * domain.end_ms.saturating_sub(domain.start_ms).max(1) as f64;
                self.fill_kill_hover_ts = Some(hover_ts);
            }
        }

        if let Some(hover_ts) = self.fill_kill_hover_ts {
            let x = Self::project_time_x(plot, hover_ts, domain.start_ms, domain.end_ms);
            if x >= plot.left() && x <= plot.right() {
                painter.line_segment(
                    [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 130, 150)),
                );
            }
        }

        hovered_here
    }

    fn render_fill_kill_event_chart(
        &mut self,
        ui: &mut egui::Ui,
        domain: &FillKillTimeDomain<'_>,
        panel_h: f32,
    ) -> bool {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), panel_h.max(96.0)),
            egui::Sense::hover(),
        );
        let painter = ui.painter();

        painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(16, 20, 27));
        painter.text(
            egui::pos2(rect.left() + 8.0, rect.top() + 6.0),
            egui::Align2::LEFT_TOP,
            "Events (signed log ratio)",
            egui::FontId::proportional(10.0),
            egui::Color32::GRAY,
        );

        let pad_left = 52.0;
        let pad_right = 8.0;
        let pad_top = 24.0;
        let pad_bottom = 18.0;
        let plot = egui::Rect::from_min_max(
            egui::pos2(rect.left() + pad_left, rect.top() + pad_top),
            egui::pos2(rect.right() - pad_right, rect.bottom() - pad_bottom),
        );
        if plot.width() <= 2.0 || plot.height() <= 2.0 {
            return false;
        }

        for y_level in -2..=2 {
            let frac = (y_level as f64 + EVENT_LOG_RANGE) / (2.0 * EVENT_LOG_RANGE);
            let y = plot.bottom() - frac as f32 * plot.height();
            let stroke = if y_level == 0 {
                egui::Stroke::new(1.2, egui::Color32::from_rgb(70, 90, 120))
            } else {
                egui::Stroke::new(1.0, egui::Color32::from_rgb(32, 36, 44))
            };
            painter.line_segment(
                [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
                stroke,
            );
            let label = if y_level > 0 {
                format!("+{y_level}")
            } else {
                format!("{y_level}")
            };
            painter.text(
                egui::pos2(plot.left() - 4.0, y),
                egui::Align2::RIGHT_CENTER,
                label,
                egui::FontId::monospace(10.0),
                DIM_GRAY,
            );
        }

        painter.text(
            egui::pos2(plot.left(), plot.bottom() + 3.0),
            egui::Align2::LEFT_TOP,
            "5m ago",
            egui::FontId::proportional(9.0),
            DIM_GRAY,
        );
        painter.text(
            egui::pos2(plot.right(), plot.bottom() + 3.0),
            egui::Align2::RIGHT_TOP,
            "now",
            egui::FontId::proportional(9.0),
            DIM_GRAY,
        );

        if domain.events.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Collecting event samples...",
                egui::FontId::proportional(11.0),
                DIM_GRAY,
            );
        } else {
            let max_fill = domain
                .events
                .iter()
                .map(|sample| sample.fill_qty)
                .fold(0.0_f64, f64::max);

            let mut last_infinite_x = f32::NEG_INFINITY;
            for sample in domain.events {
                let x = Self::project_time_x(
                    plot,
                    sample.timestamp_ms as f64,
                    domain.start_ms,
                    domain.end_ms,
                );

                if self.fill_kill_show_infinity && sample.ratio == RatioValue::Infinite {
                    let marker_color = if self.fill_kill_highlight_overfill && sample.overfill {
                        AMBER_COLOR
                    } else {
                        egui::Color32::WHITE
                    };
                    let top_pos = egui::pos2(x, plot.top() - 8.0);
                    painter.circle_filled(top_pos, 2.4, marker_color);
                    if x - last_infinite_x >= 24.0 {
                        painter.text(
                            egui::pos2(x, plot.top() - 15.0),
                            egui::Align2::CENTER_TOP,
                            "∞",
                            egui::FontId::monospace(9.0),
                            marker_color,
                        );
                        last_infinite_x = x;
                    }
                }

                let Some(log_ratio) = sample.signed_log_ratio else {
                    continue;
                };
                let y_frac = ((log_ratio + EVENT_LOG_RANGE) / (2.0 * EVENT_LOG_RANGE))
                    .clamp(0.0, 1.0) as f32;
                let y = plot.bottom() - y_frac * plot.height();
                let point = egui::pos2(x, y);
                let side_color = match sample.direction {
                    BurstDirection::Buy => BID_COLOR,
                    BurstDirection::Sell => ASK_COLOR,
                };
                let radius = if max_fill > 0.0 {
                    2.0 + ((sample.fill_qty / max_fill).clamp(0.0, 1.0) as f32) * 4.0
                } else {
                    3.0
                };
                painter.circle_filled(point, radius, side_color.linear_multiply(0.9));
                if self.fill_kill_highlight_overfill && sample.overfill {
                    painter.circle_stroke(point, radius + 1.4, egui::Stroke::new(1.0, AMBER_COLOR));
                }
            }
        }

        painter.text(
            egui::pos2(plot.left() + 6.0, plot.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Buy",
            egui::FontId::proportional(9.0),
            BID_COLOR,
        );
        painter.text(
            egui::pos2(plot.left() + 30.0, plot.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "Sell",
            egui::FontId::proportional(9.0),
            ASK_COLOR,
        );
        painter.text(
            egui::pos2(plot.left() + 58.0, plot.top() + 4.0),
            egui::Align2::LEFT_TOP,
            "y=log10((fill+1e-2)/(kill+1e-2))",
            egui::FontId::proportional(9.0),
            egui::Color32::GRAY,
        );

        let hovered_here = response
            .hover_pos()
            .is_some_and(|pointer| response.hovered() && plot.contains(pointer));
        if hovered_here {
            if let Some(pointer) = response.hover_pos() {
                let x_frac = ((pointer.x - plot.left()) / plot.width()).clamp(0.0, 1.0) as f64;
                let hover_ts = domain.start_ms as f64
                    + x_frac * domain.end_ms.saturating_sub(domain.start_ms).max(1) as f64;
                self.fill_kill_hover_ts = Some(hover_ts);
            }
        }

        if let Some(hover_ts) = self.fill_kill_hover_ts {
            let x = Self::project_time_x(plot, hover_ts, domain.start_ms, domain.end_ms);
            if x >= plot.left() && x <= plot.right() {
                painter.line_segment(
                    [
                        egui::pos2(x, plot.top() - 16.0),
                        egui::pos2(x, plot.bottom()),
                    ],
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 130, 150)),
                );
            }
        }

        hovered_here
    }

    /// Build the heatmap ColorImage from depth slices
    fn build_heatmap_image(
        &mut self,
        state: &StateSnapshot,
        img_width: usize,
        img_height: usize,
        view_time_start: f64,
        view_time_end: f64,
    ) -> egui::ColorImage {
        let price_min = self.heatmap_price_center - self.heatmap_price_range / 2.0;
        let price_max = self.heatmap_price_center + self.heatmap_price_range / 2.0;
        let bg = egui::Color32::from_rgb(13, 17, 23);

        let num_slices = state.depth_slices.len();
        let w = img_width.max(1);
        let h = img_height.max(1);
        if num_slices == 0 || img_width == 0 || img_height == 0 {
            return egui::ColorImage::new([w, h], vec![bg; w * h]);
        }
        let price_span = price_max - price_min;
        if !price_span.is_finite() || price_span <= 0.0 {
            return egui::ColorImage::new([w, h], vec![bg; w * h]);
        }

        let grid_len = num_slices * img_height;
        if self.heatmap_grid_cols != num_slices || self.heatmap_grid_rows != img_height {
            self.heatmap_grid_cols = num_slices;
            self.heatmap_grid_rows = img_height;
            self.heatmap_grid_bid_qty.resize(grid_len, 0.0);
            self.heatmap_grid_ask_qty.resize(grid_len, 0.0);
        }
        self.heatmap_grid_bid_qty[..grid_len].fill(0.0);
        self.heatmap_grid_ask_qty[..grid_len].fill(0.0);

        let mut global_max_qty: f32 = 0.0;
        for (si, slice) in state.depth_slices.iter().enumerate() {
            for (li, &(price, qty)) in slice.levels.iter().enumerate() {
                if qty < self.heatmap_min_qty {
                    continue;
                }
                if price < price_min || price > price_max {
                    continue;
                }
                let frac = (price_max - price) / price_span;
                let row = (frac * img_height as f64) as usize;
                if row < img_height {
                    let idx = Self::heatmap_cell_idx(si, row, img_height);
                    let total = Self::accumulate_side_qty(
                        &mut self.heatmap_grid_bid_qty,
                        &mut self.heatmap_grid_ask_qty,
                        idx,
                        qty,
                        li < slice.bids_len,
                    );
                    global_max_qty = global_max_qty.max(total);
                }
            }
        }

        self.heatmap_pixels.resize(img_width * img_height, bg);
        self.heatmap_pixels.fill(bg);
        if global_max_qty <= 0.0 {
            let pixels = std::mem::take(&mut self.heatmap_pixels);
            return egui::ColorImage::new([img_width, img_height], pixels);
        }

        let log_max = (1.0 + global_max_qty as f64).ln().max(1e-12);
        let time_span = (view_time_end - view_time_start).max(1.0);
        let slice_map =
            Self::build_slice_index_map(&state.depth_slices, img_width, view_time_start, time_span);
        for (x, &si) in slice_map.iter().enumerate() {
            let row_base = si * img_height;
            for y in 0..img_height {
                let idx = row_base + y;
                let bid = self.heatmap_grid_bid_qty[idx] as f64;
                let ask = self.heatmap_grid_ask_qty[idx] as f64;
                let qty_total = bid + ask;
                if qty_total <= 0.0 {
                    continue;
                }
                let intensity = (1.0 + qty_total).ln() / log_max;
                let color = heatmap_color(intensity, bid >= ask);
                self.heatmap_pixels[y * img_width + x] = color;
            }
        }

        let pixels = std::mem::take(&mut self.heatmap_pixels);
        egui::ColorImage::new([img_width, img_height], pixels)
    }

    fn apply_price_zoom_at_cursor(
        &mut self,
        scroll_delta: f32,
        hover_pos: Option<egui::Pos2>,
        rect: egui::Rect,
        heatmap_h: f32,
        tick_size: f64,
    ) {
        let zoom_factor = if scroll_delta > 0.0 { 0.9 } else { 1.1 };
        let min_range = tick_size * 5.0;

        if let Some(mouse_pos) = hover_pos {
            let price_min = self.heatmap_price_center - self.heatmap_price_range / 2.0;
            let price_max = self.heatmap_price_center + self.heatmap_price_range / 2.0;
            let frac = (mouse_pos.y - rect.top()) as f64 / heatmap_h as f64;
            let price_at_cursor = price_max - frac * (price_max - price_min);

            let new_range = (self.heatmap_price_range * zoom_factor).clamp(min_range, 5000.0);
            let new_center = price_at_cursor - new_range / 2.0 + frac * new_range;
            self.heatmap_price_range = new_range;
            self.heatmap_price_center = new_center;
        } else {
            self.heatmap_price_range =
                (self.heatmap_price_range * zoom_factor).clamp(min_range, 5000.0);
        }
        self.heatmap_auto_center = false;
        self.last_interaction_time = Instant::now();
    }

    fn apply_time_zoom_at_cursor(
        &mut self,
        scroll_delta: f32,
        hover_pos: Option<egui::Pos2>,
        data_rect: egui::Rect,
        total_span_secs: f64,
    ) {
        let zoom_factor = if scroll_delta > 0.0 { 0.9 } else { 1.1 };
        let min_window_secs = 5.0_f64.min(total_span_secs);
        let max_window_secs = total_span_secs.max(min_window_secs);
        let current_window = if self.heatmap_time_window <= 0.0 {
            total_span_secs
        } else {
            self.heatmap_time_window
        };
        let new_window = (current_window * zoom_factor).clamp(min_window_secs, max_window_secs);

        let mut new_offset = self.heatmap_time_offset;
        if let Some(mouse_pos) = hover_pos {
            let x_frac =
                ((mouse_pos.x - data_rect.left()) / data_rect.width()).clamp(0.0, 1.0) as f64;
            let right_frac = 1.0 - x_frac;
            new_offset += right_frac * (current_window - new_window);
        }

        let max_offset_secs = (total_span_secs - new_window).max(0.0);
        self.heatmap_time_offset = new_offset.clamp(0.0, max_offset_secs);
        self.heatmap_time_window = new_window;
        self.last_interaction_time = Instant::now();
    }

    fn latest_trade_for_pointer(trades: &[(u64, f64, f64, bool)]) -> Option<(u64, f64)> {
        trades
            .iter()
            .max_by_key(|&&(timestamp_ms, _, _, _)| timestamp_ms)
            .map(|&(timestamp_ms, price, _, _)| (timestamp_ms, price))
    }

    fn is_live_time_view(view_time_end: f64, total_time_end: f64, tolerance_ms: f64) -> bool {
        let tolerance_ms = tolerance_ms.max(0.0);
        total_time_end >= view_time_end && (total_time_end - view_time_end) <= tolerance_ms
    }

    fn latest_trade_auto_center_price(
        is_live_follow: bool,
        latest_trade: Option<(u64, f64)>,
    ) -> Option<f64> {
        if !is_live_follow {
            return None;
        }
        let (_, price) = latest_trade?;
        if price.is_finite() {
            Some(price)
        } else {
            None
        }
    }

    fn pointer_y_fraction(price: f64, price_min: f64, price_max: f64) -> Option<f32> {
        if !price.is_finite() || !price_min.is_finite() || !price_max.is_finite() {
            return None;
        }
        if price < price_min || price > price_max {
            return None;
        }
        let span = price_max - price_min;
        if span <= f64::EPSILON {
            return None;
        }
        Some(((price_max - price) / span) as f32)
    }

    fn live_strip_width(heatmap_w: f32) -> f32 {
        (heatmap_w * 0.16).clamp(28.0, 88.0).min(heatmap_w * 0.4)
    }

    fn split_heatmap_rects(rect: egui::Rect) -> (egui::Rect, egui::Rect) {
        let live_strip_w = Self::live_strip_width(rect.width());
        let data_w = (rect.width() - live_strip_w).max(1.0);
        let data_rect = egui::Rect::from_min_size(rect.min, egui::vec2(data_w, rect.height()));
        let live_strip_rect =
            egui::Rect::from_min_max(egui::pos2(data_rect.right(), rect.top()), rect.max);
        (data_rect, live_strip_rect)
    }

    fn depth_time_bounds(depth_slices: &[Arc<DepthSlice>]) -> Option<(f64, f64)> {
        let start = depth_slices.first()?.timestamp_ms as f64;
        let end = depth_slices.last()?.timestamp_ms as f64;
        Some((start, end))
    }

    fn render_heatmap(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, state: &StateSnapshot) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Min Size:").color(egui::Color32::GRAY));
            ui.add(
                egui::TextEdit::singleline(&mut self.heatmap_min_qty_input)
                    .desired_width(70.0)
                    .font(egui::TextStyle::Monospace),
            );
            if let Ok(val) = self.heatmap_min_qty_input.trim().parse::<f64>() {
                self.heatmap_min_qty = val.max(0.0);
            }
            ui.separator();
            ui.checkbox(&mut self.show_perf_overlay, "Perf");
        });
        ui.add_space(4.0);

        let available = ui.available_size();

        if state.depth_slices.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Collecting depth data for heatmap...")
                        .color(egui::Color32::GRAY)
                        .italics(),
                );
            });
            return;
        }

        // Reserve space for axis labels
        let y_axis_w = available.x.clamp(24.0, 70.0);
        let x_axis_h = 20.0_f32;
        let legend_h = 10.0_f32;
        let heatmap_w = (available.x - y_axis_w - 8.0).max(24.0);
        let heatmap_h = (available.y - x_axis_h - legend_h - 8.0).max(24.0);

        let img_w = (heatmap_w as usize).clamp(1, 800);
        let img_h = (heatmap_h as usize).clamp(1, 600);

        let Some((total_time_start, total_time_end)) =
            Self::depth_time_bounds(state.depth_slices.as_ref())
        else {
            return;
        };
        let total_span_ms = (total_time_end - total_time_start).max(1.0);
        let total_span_secs = total_span_ms / 1000.0;
        let (view_time_start, view_time_end) = if self.heatmap_time_window <= 0.0 {
            (total_time_start, total_time_end)
        } else {
            let min_window_secs = 5.0_f64.min(total_span_secs);
            let max_window_secs = total_span_secs.max(min_window_secs);
            self.heatmap_time_window = self
                .heatmap_time_window
                .clamp(min_window_secs, max_window_secs);

            let max_offset_secs = (total_span_secs - self.heatmap_time_window).max(0.0);
            self.heatmap_time_offset = self.heatmap_time_offset.clamp(0.0, max_offset_secs);

            let window_ms = self.heatmap_time_window * 1000.0;
            let right_edge = total_time_end - self.heatmap_time_offset * 1000.0;
            let left_edge = right_edge - window_ms;
            (
                left_edge.max(total_time_start),
                right_edge.min(total_time_end),
            )
        };
        let is_live_follow = Self::is_live_time_view(view_time_end, total_time_end, 1.0);
        let latest_trade = Self::latest_trade_for_pointer(state.trades.as_ref());
        // Trade-price auto-center only when NOT auto-centering on mid-price,
        // to avoid a second source of center-price jumps that amplifies jitter.
        if !self.heatmap_auto_center {
            if let Some(latest_price) =
                Self::latest_trade_auto_center_price(is_live_follow, latest_trade)
            {
                self.heatmap_price_center = latest_price;
            }
        }

        let render_key = HeatmapRenderKey {
            depth_history_epoch: state.depth_history_epoch,
            img_w,
            img_h,
            heatmap_price_center_bits: self.heatmap_price_center.to_bits(),
            heatmap_price_range_bits: self.heatmap_price_range.to_bits(),
            heatmap_time_offset_bits: self.heatmap_time_offset.to_bits(),
            heatmap_time_window_bits: self.heatmap_time_window.to_bits(),
            heatmap_min_qty_bits: self.heatmap_min_qty.to_bits(),
        };
        if self.heatmap_render_key != Some(render_key) {
            let build_start = Instant::now();
            let image =
                self.build_heatmap_image(state, img_w, img_h, view_time_start, view_time_end);
            self.perf_stats
                .record_heatmap_rebuild(build_start.elapsed());
            if let Some(handle) = self.heatmap_texture.as_mut() {
                handle.set(image, egui::TextureOptions::NEAREST);
            } else {
                self.heatmap_texture =
                    Some(ctx.load_texture("heatmap", image, egui::TextureOptions::NEAREST));
            }
            self.heatmap_render_key = Some(render_key);
        }

        // Show a live pointer from the latest trade only while viewing live time.
        let pointer_price_min = self.heatmap_price_center - self.heatmap_price_range / 2.0;
        let pointer_price_max = self.heatmap_price_center + self.heatmap_price_range / 2.0;
        let pointer_time_span = (view_time_end - view_time_start).max(1.0);
        let last_trade_info: Option<(f32, f32, f64)> = if is_live_follow {
            latest_trade.and_then(|(timestamp_ms, price)| {
                Self::pointer_y_fraction(price, pointer_price_min, pointer_price_max).map(
                    |y_frac| {
                        let x_frac = ((timestamp_ms as f64 - view_time_start) / pointer_time_span)
                            .clamp(0.0, 1.0) as f32;
                        (x_frac, y_frac, price)
                    },
                )
            })
        } else {
            None
        };

        ui.horizontal(|ui| {
            // Heatmap image + overlays
            ui.vertical(|ui| {
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(heatmap_w, heatmap_h),
                    egui::Sense::click_and_drag(),
                );
                let (data_rect, _live_strip_rect) = Self::split_heatmap_rects(rect);
                let data_w = data_rect.width();

                // Draw the heatmap texture
                if let Some(tex) = &self.heatmap_texture {
                    ui.painter().image(
                        tex.id(),
                        data_rect,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }

                // Draw live pointer from latest trade x-anchor through the right strip.
                if let Some((x_frac, y_frac, _price)) = last_trade_info {
                    if (0.0..=1.0).contains(&y_frac) {
                        let px = data_rect.left() + x_frac * data_w;
                        let py = rect.top() + y_frac * heatmap_h;
                        let pointer_color = egui::Color32::from_rgb(255, 200, 50);
                        ui.painter().line_segment(
                            [egui::pos2(px, py), egui::pos2(rect.right(), py)],
                            egui::Stroke::new(1.5, pointer_color),
                        );
                    }
                }

                // Overlay trade markers
                if !state.trades.is_empty() && !state.depth_slices.is_empty() {
                    let price_min = self.heatmap_price_center - self.heatmap_price_range / 2.0;
                    let price_max = self.heatmap_price_center + self.heatmap_price_range / 2.0;
                    let time_span = (view_time_end - view_time_start).max(1.0);

                    let max_trade_qty = state.trades.iter().map(|t| t.2).fold(0.0f64, f64::max);

                    for &(ts, price, qty, is_buy) in state.trades.iter() {
                        let t = ts as f64;
                        if t < view_time_start || t > view_time_end {
                            continue;
                        }
                        if price < price_min || price > price_max {
                            continue;
                        }
                        let x_frac = (t - view_time_start) / time_span;
                        let y_frac = (price_max - price) / (price_max - price_min);
                        let px = data_rect.left() + x_frac as f32 * data_w;
                        let py = rect.top() + y_frac as f32 * heatmap_h;

                        let radius = if max_trade_qty > 0.0 {
                            2.0 + (qty / max_trade_qty) as f32 * 5.0
                        } else {
                            3.0
                        };
                        let color = if is_buy {
                            egui::Color32::from_rgb(50, 255, 100)
                        } else {
                            egui::Color32::from_rgb(255, 80, 80)
                        };
                        ui.painter()
                            .circle_filled(egui::pos2(px, py), radius, color);
                    }
                }

                let hint_rect = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + 6.0, rect.top() + 6.0),
                    egui::vec2(185.0, 50.0),
                );
                ui.painter().rect_filled(
                    hint_rect,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(25, 25, 25, 160),
                );
                ui.painter().text(
                    egui::pos2(hint_rect.left() + 6.0, hint_rect.top() + 4.0),
                    egui::Align2::LEFT_TOP,
                    "Wheel: Time Zoom\nWheel on Price Axis: Price Zoom\nDrag: Pan\nDouble-click: Re-center",
                    egui::FontId::monospace(9.0),
                    egui::Color32::from_rgb(170, 170, 170),
                );

                if self.show_perf_overlay {
                    let perf = self.perf_stats.snapshot();
                    let snap_delta = self.heatmap_price_center - self.heatmap_price_center_ema;
                    let snap_delta_bins = if self.active_bin_width > 0.0 {
                        snap_delta / self.active_bin_width
                    } else {
                        0.0
                    };
                    let perf_rect = egui::Rect::from_min_size(
                        egui::pos2(rect.left() + 6.0, hint_rect.bottom() + 4.0),
                        egui::vec2(310.0, 116.0),
                    );
                    let perf_text = format!(
                        "FPS: {:.1} (avg {:.2}ms, max {:.2}ms)\nHeatmap rebuild: {:.1}/s\nRebuild avg/last/max: {:.2}/{:.2}/{:.2}ms\nRepaint target: {:.0}ms\nAuto-center: {}\nMid/EMA/Snapped: {:.2} / {:.2} / {:.2}\nSnap delta: {:+.4} ({:+.3} bins @ {:.4})",
                        perf.fps,
                        perf.frame_avg_ms,
                        perf.frame_max_ms,
                        perf.heatmap_rebuilds_per_sec,
                        perf.heatmap_rebuild_avg_ms,
                        perf.heatmap_rebuild_last_ms,
                        perf.heatmap_rebuild_max_ms,
                        perf.repaint_target_ms,
                        if self.heatmap_auto_center { "ON" } else { "OFF" },
                        state.mid_price,
                        self.heatmap_price_center_ema,
                        self.heatmap_price_center,
                        snap_delta,
                        snap_delta_bins,
                        self.active_bin_width,
                    );
                    ui.painter().rect_filled(
                        perf_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(25, 25, 25, 170),
                    );
                    ui.painter().text(
                        egui::pos2(perf_rect.left() + 6.0, perf_rect.top() + 4.0),
                        egui::Align2::LEFT_TOP,
                        perf_text,
                        egui::FontId::monospace(9.0),
                        egui::Color32::from_rgb(190, 190, 190),
                    );
                }

                let hover_lookup = HeatmapHoverLookup {
                    data_rect,
                    heatmap_h,
                    view_time_start,
                    view_time_end,
                    img_h,
                };
                if response.hovered() {
                    let hover_cell = response.hover_pos().and_then(|pointer| {
                        self.hover_cell_from_pointer(pointer, state, hover_lookup)
                    });
                    if let Some(cell) = hover_cell {
                        let idx = Self::heatmap_cell_idx(cell.slice_idx, cell.row, img_h);
                        let bid = self
                            .heatmap_grid_bid_qty
                            .get(idx)
                            .copied()
                            .unwrap_or_default() as f64;
                        let ask = self
                            .heatmap_grid_ask_qty
                            .get(idx)
                            .copied()
                            .unwrap_or_default() as f64;
                        let size = bid + ask;
                        let notional = size * cell.price;

                        response.clone().on_hover_ui_at_pointer(|ui| {
                            ui.label(format!("Time: {}", format_hms_millis(cell.timestamp_ms)));
                            ui.label(format!(
                                "Price: {}",
                                format_price(cell.price, state.price_decimals)
                            ));
                            ui.label(format!("Size: {:.4}", size));
                            ui.label(format!("Notional: {:.2}", notional));
                        });
                    }
                }

                // Handle zoom (scroll) and pan (drag)
                let scroll_delta = ui.input(|i| i.smooth_scroll_delta.y);
                if response.hovered() && scroll_delta != 0.0 {
                    self.apply_time_zoom_at_cursor(
                        scroll_delta,
                        response.hover_pos(),
                        data_rect,
                        total_span_secs,
                    );
                }

                if response.dragged() {
                    let drag_delta = ui.input(|i| i.pointer.delta());

                    // Vertical pan (price)
                    let price_per_pixel = self.heatmap_price_range / heatmap_h as f64;
                    self.heatmap_price_center += drag_delta.y as f64 * price_per_pixel;
                    self.heatmap_auto_center = false;

                    // Horizontal pan (time)
                    if drag_delta.x != 0.0 {
                        let current_window = if self.heatmap_time_window <= 0.0 {
                            total_span_secs
                        } else {
                            self.heatmap_time_window
                        };
                        let secs_per_pixel = current_window / data_w as f64;

                        if self.heatmap_time_window <= 0.0 {
                            self.heatmap_time_window = total_span_secs;
                        }

                        let max_offset_secs = (total_span_secs - self.heatmap_time_window).max(0.0);
                        self.heatmap_time_offset = (self.heatmap_time_offset
                            + drag_delta.x as f64 * secs_per_pixel)
                            .clamp(0.0, max_offset_secs);
                    }
                    self.last_interaction_time = Instant::now();
                }

                if response.double_clicked() {
                    self.enable_heatmap_auto_center();
                    self.heatmap_price_range = 0.0;
                    self.heatmap_time_offset = 0.0;
                    self.heatmap_time_window = 0.0;
                    self.last_interaction_time = Instant::now();
                }

                // Auto-center indicator
                let time_is_custom =
                    self.heatmap_time_window > 0.0 || self.heatmap_time_offset > 0.0;
                if !self.heatmap_auto_center || time_is_custom {
                    let btn_rect = egui::Rect::from_min_size(
                        egui::pos2(rect.right() - 70.0, rect.top() + 4.0),
                        egui::vec2(66.0, 18.0),
                    );
                    ui.painter().rect_filled(
                        btn_rect,
                        4.0,
                        egui::Color32::from_rgba_unmultiplied(30, 30, 30, 200),
                    );
                    ui.painter().text(
                        btn_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "Re-center",
                        egui::FontId::monospace(10.0),
                        egui::Color32::from_rgb(255, 200, 50),
                    );
                    if ui.allocate_rect(btn_rect, egui::Sense::click()).clicked() {
                        self.enable_heatmap_auto_center();
                        self.heatmap_price_range = 0.0;
                        self.heatmap_time_offset = 0.0;
                        self.heatmap_time_window = 0.0;
                        self.last_interaction_time = Instant::now();
                    }
                }
            });

            // Y-axis labels (price) on right side
            ui.allocate_ui(egui::vec2(y_axis_w, heatmap_h), |ui| {
                let price_min = self.heatmap_price_center - self.heatmap_price_range / 2.0;
                let price_max = self.heatmap_price_center + self.heatmap_price_range / 2.0;
                let axis_rect = ui.available_rect_before_wrap();
                let num_labels = 8;
                for i in 0..=num_labels {
                    let frac = i as f32 / num_labels as f32;
                    let price = price_max - (frac as f64) * (price_max - price_min);
                    let y = axis_rect.top() + frac * heatmap_h;
                    ui.painter().text(
                        egui::pos2(axis_rect.left() + 4.0, y),
                        egui::Align2::LEFT_CENTER,
                        format_price(price, state.price_decimals),
                        egui::FontId::monospace(10.0),
                        egui::Color32::from_rgb(150, 150, 150),
                    );
                }
                // Draw last trade price pointer and badge
                if let Some((_x_frac, y_frac, price)) = last_trade_info {
                    if (0.0..=1.0).contains(&y_frac) {
                        let y = axis_rect.top() + y_frac * heatmap_h;
                        let badge_color = egui::Color32::from_rgb(255, 200, 50);
                        let price_text = format_price(price, state.price_decimals);
                        let font = egui::FontId::monospace(10.0);

                        // Pointer line continuation into y-axis area
                        ui.painter().line_segment(
                            [
                                egui::pos2(axis_rect.left(), y),
                                egui::pos2(axis_rect.left() + 6.0, y),
                            ],
                            egui::Stroke::new(1.5, badge_color),
                        );

                        // Price badge
                        let text_galley = ui.painter().layout_no_wrap(
                            price_text.clone(),
                            font.clone(),
                            badge_color,
                        );
                        let badge_w = text_galley.size().x + 8.0;
                        let badge_h = text_galley.size().y + 4.0;
                        let badge_rect = egui::Rect::from_min_size(
                            egui::pos2(axis_rect.left(), y - badge_h / 2.0),
                            egui::vec2(badge_w, badge_h),
                        );
                        ui.painter().rect_filled(badge_rect, 3.0, badge_color);
                        ui.painter().text(
                            badge_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            price_text,
                            font,
                            egui::Color32::BLACK,
                        );
                    }
                }

                let y_axis_response = ui.allocate_rect(
                    egui::Rect::from_min_size(
                        axis_rect.min,
                        egui::vec2(y_axis_w, heatmap_h),
                    ),
                    egui::Sense::hover(),
                );
                let y_axis_scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if y_axis_response.hovered() && y_axis_scroll != 0.0 {
                    self.apply_price_zoom_at_cursor(
                        y_axis_scroll,
                        y_axis_response.hover_pos(),
                        axis_rect,
                        heatmap_h,
                        state.tick_size,
                    );
                }
            });
        });

        // Color legend bar
        ui.horizontal(|ui| {
            self.render_color_legend(ui, heatmap_w, legend_h);
            ui.allocate_space(egui::vec2(y_axis_w, legend_h));
        });

        // X-axis labels (time)
        ui.horizontal(|ui| {
            let (axis_rect, axis_response) =
                ui.allocate_exact_size(egui::vec2(heatmap_w, x_axis_h), egui::Sense::hover());
            let axis_data_w = (heatmap_w - Self::live_strip_width(heatmap_w)).max(1.0);
            let time_span = (view_time_end - view_time_start).max(1.0);
            let num_labels = 6;
            for i in 0..=num_labels {
                let frac = i as f64 / num_labels as f64;
                let t_ms = view_time_start + frac * time_span;
                let total_secs = (t_ms / 1000.0) as u64;
                let h = (total_secs / 3600) % 24;
                let m = (total_secs / 60) % 60;
                let s = total_secs % 60;
                let x = axis_rect.left() + frac as f32 * axis_data_w;
                ui.painter().text(
                    egui::pos2(x, axis_rect.top() + 4.0),
                    egui::Align2::CENTER_TOP,
                    format!("{:02}:{:02}:{:02}", h, m, s),
                    egui::FontId::monospace(10.0),
                    egui::Color32::from_rgb(150, 150, 150),
                );
            }

            let axis_scroll = ui.input(|i| i.smooth_scroll_delta);
            let axis_scroll_delta = if axis_scroll.y != 0.0 {
                axis_scroll.y
            } else {
                axis_scroll.x
            };
            if axis_response.hovered() && axis_scroll_delta != 0.0 {
                let axis_data_rect =
                    egui::Rect::from_min_size(axis_rect.min, egui::vec2(axis_data_w, x_axis_h));
                self.apply_time_zoom_at_cursor(
                    axis_scroll_delta,
                    axis_response.hover_pos(),
                    axis_data_rect,
                    total_span_secs,
                );
            }

            ui.allocate_space(egui::vec2(y_axis_w, x_axis_h));
        });
    }

    fn render_color_legend(&self, ui: &mut egui::Ui, width: f32, height: f32) {
        let width_px = width.max(1.0).round() as usize;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter();
        let denom = (width_px.saturating_sub(1)).max(1) as f32;

        for x in 0..width_px {
            let t_pos = x as f32 / denom;
            let (intensity, is_bid) = if t_pos < 0.5 {
                (1.0 - (t_pos / 0.5), false)
            } else {
                ((t_pos - 0.5) / 0.5, true)
            };
            let color = heatmap_color(intensity as f64, is_bid);
            let x0 = rect.left() + x as f32;
            let x1 = (x0 + 1.0).min(rect.right());
            let col_rect =
                egui::Rect::from_min_max(egui::pos2(x0, rect.top()), egui::pos2(x1, rect.bottom()));
            painter.rect_filled(col_rect, 0.0, color);
        }

        painter.text(
            egui::pos2(rect.left() + 3.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            "Asks",
            egui::FontId::proportional(9.0),
            egui::Color32::GRAY,
        );
        painter.text(
            egui::pos2(rect.right() - 3.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            "Bids",
            egui::FontId::proportional(9.0),
            egui::Color32::GRAY,
        );
    }
}

/// Map heatmap intensity (0..1) to a color, tinted by bid/ask side
fn heatmap_color(intensity: f64, is_bid: bool) -> egui::Color32 {
    let t = intensity.clamp(0.0, 1.0) as f32;

    if is_bid {
        // Green-tinted: dark → dark green → cyan → yellow → white
        if t < 0.2 {
            let s = t / 0.2;
            lerp_color(
                egui::Color32::from_rgb(5, 10, 5),
                egui::Color32::from_rgb(0, 60, 30),
                s,
            )
        } else if t < 0.5 {
            let s = (t - 0.2) / 0.3;
            lerp_color(
                egui::Color32::from_rgb(0, 60, 30),
                egui::Color32::from_rgb(0, 180, 120),
                s,
            )
        } else if t < 0.8 {
            let s = (t - 0.5) / 0.3;
            lerp_color(
                egui::Color32::from_rgb(0, 180, 120),
                egui::Color32::from_rgb(180, 230, 80),
                s,
            )
        } else {
            let s = (t - 0.8) / 0.2;
            lerp_color(
                egui::Color32::from_rgb(180, 230, 80),
                egui::Color32::from_rgb(255, 255, 220),
                s,
            )
        }
    } else {
        // Red-tinted: dark → dark red → magenta → orange → white
        if t < 0.2 {
            let s = t / 0.2;
            lerp_color(
                egui::Color32::from_rgb(10, 5, 5),
                egui::Color32::from_rgb(60, 0, 20),
                s,
            )
        } else if t < 0.5 {
            let s = (t - 0.2) / 0.3;
            lerp_color(
                egui::Color32::from_rgb(60, 0, 20),
                egui::Color32::from_rgb(180, 30, 60),
                s,
            )
        } else if t < 0.8 {
            let s = (t - 0.5) / 0.3;
            lerp_color(
                egui::Color32::from_rgb(180, 30, 60),
                egui::Color32::from_rgb(230, 160, 50),
                s,
            )
        } else {
            let s = (t - 0.8) / 0.2;
            lerp_color(
                egui::Color32::from_rgb(230, 160, 50),
                egui::Color32::from_rgb(255, 255, 220),
                s,
            )
        }
    }
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    egui::Color32::from_rgb(
        (a.r() as f32 + (b.r() as f32 - a.r() as f32) * t) as u8,
        (a.g() as f32 + (b.g() as f32 - a.g() as f32) * t) as u8,
        (a.b() as f32 + (b.b() as f32 - a.b() as f32) * t) as u8,
    )
}

// A cloneable snapshot of the shared state for the UI to consume.
#[derive(Clone)]
pub struct StateSnapshot {
    pub bids: Arc<Vec<(f64, f64)>>,
    pub asks: Arc<Vec<(f64, f64)>>,
    pub depth_history_epoch: u64,
    pub trade_epoch: u64,
    pub fill_kill_epoch: u64,
    pub cumulative_epoch: u64,
    pub spread: f64,
    pub mid_price: f64,
    pub imbalance: f64,
    pub tps: f64,
    pub connected: bool,
    pub status_msg: String,
    pub latency_ms: i64,
    pub book_bid_count: usize,
    pub book_ask_count: usize,
    pub trades: Arc<Vec<(u64, f64, f64, bool)>>, // (timestamp, price, qty, is_buy)
    pub depth_checkpoints: Arc<Vec<Arc<DepthCheckpoint>>>,
    pub depth_deltas: Arc<Vec<Arc<DepthDeltaEvent>>>,
    pub depth_slices: Arc<Vec<Arc<DepthSlice>>>,
    pub fill_kill_series: Arc<Vec<FillKillSample>>,
    pub cumulative_series: Arc<Vec<CumulativeSample>>,
    pub fill_kill_kpis: FillKillKpis,
    pub buy_impact: MarketImpact,
    pub sell_impact: MarketImpact,
    pub tick_size: f64,
    pub price_decimals: usize,
}

impl SharedState {
    pub fn clone_snapshot(&mut self, impact_notional: f64) -> StateSnapshot {
        let local_now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let now_exchange_ms = (local_now_ms - self.order_book.clock_offset).max(0) as u64;
        let tps = self.trade_history.rolling_tps(now_exchange_ms, 10_000);

        let best_bid = self.order_book.best_bid().map(|(p, _)| p).unwrap_or(0.0);
        let best_ask = self.order_book.best_ask().map(|(p, _)| p).unwrap_or(0.0);
        let mid = (best_bid + best_ask) / 2.0;
        let spread = self.order_book.spread().unwrap_or(0.0);
        let buy_impact = self
            .order_book
            .estimate_market_impact(impact_notional, true, mid);
        let sell_impact = self
            .order_book
            .estimate_market_impact(impact_notional, false, mid);

        if self.snapshot_book_epoch != self.depth_epoch {
            self.snapshot_bids = Arc::new(
                self.order_book
                    .bids
                    .iter()
                    .rev()
                    .map(|(p, q)| (p.0, *q))
                    .collect(),
            );
            self.snapshot_asks = Arc::new(
                self.order_book
                    .asks
                    .iter()
                    .map(|(p, q)| (p.0, *q))
                    .collect(),
            );
            self.snapshot_book_epoch = self.depth_epoch;
        }

        let total_bid: f64 = self
            .snapshot_bids
            .iter()
            .take(DEPTH_LEVELS)
            .map(|(_, q)| *q)
            .sum();
        let total_ask: f64 = self
            .snapshot_asks
            .iter()
            .take(DEPTH_LEVELS)
            .map(|(_, q)| *q)
            .sum();
        let imbalance = if total_bid + total_ask > 0.0 {
            ((total_bid - total_ask) / (total_bid + total_ask)) * 100.0
        } else {
            0.0
        };

        if self.snapshot_trade_epoch != self.trade_epoch {
            self.snapshot_trades = Arc::new(
                self.trade_history
                    .trades
                    .iter()
                    .map(|t| (t.timestamp_ms, t.price, t.quantity, t.is_buy))
                    .collect(),
            );
            self.snapshot_trade_epoch = self.trade_epoch;
        }
        if self.snapshot_depth_history_epoch != self.depth_history_epoch {
            self.snapshot_depth_checkpoints =
                Arc::new(self.depth_history.checkpoints.iter().cloned().collect());
            self.snapshot_depth_deltas =
                Arc::new(self.depth_history.deltas.iter().cloned().collect());
            self.snapshot_depth_history_epoch = self.depth_history_epoch;
        }
        if self.snapshot_fill_kill_epoch != self.fill_kill_epoch {
            self.snapshot_fill_kill_series = Arc::new(
                self.micro_metrics
                    .fill_kill_history
                    .samples
                    .iter()
                    .cloned()
                    .collect(),
            );
            self.snapshot_fill_kill_epoch = self.fill_kill_epoch;
        }
        if self.snapshot_cumulative_epoch != self.cumulative_epoch {
            self.snapshot_cumulative_series = Arc::new(
                self.micro_metrics
                    .cumulative_history
                    .samples
                    .iter()
                    .cloned()
                    .collect(),
            );
            self.snapshot_cumulative_epoch = self.cumulative_epoch;
        }
        let fill_kill_kpis = self.micro_metrics.kpi_snapshot();

        // Materialize depth slices only when depth history has new data.
        if self.snapshot_depth_slices_epoch != self.depth_history_epoch {
            self.snapshot_depth_slices = if let Some((start, end)) = self.depth_history.time_range()
            {
                let max_columns = 800; // matches img_w max in render_heatmap
                Arc::new(
                    self.depth_history
                        .materialize_columns(start, end, max_columns),
                )
            } else {
                Arc::new(Vec::new())
            };
            self.snapshot_depth_slices_epoch = self.depth_history_epoch;
        }
        let depth_slices = Arc::clone(&self.snapshot_depth_slices);

        StateSnapshot {
            bids: Arc::clone(&self.snapshot_bids),
            asks: Arc::clone(&self.snapshot_asks),
            depth_history_epoch: self.depth_history_epoch,
            trade_epoch: self.trade_epoch,
            fill_kill_epoch: self.fill_kill_epoch,
            cumulative_epoch: self.cumulative_epoch,
            spread,
            mid_price: mid,
            imbalance,
            tps,
            connected: self.connected,
            status_msg: self.status_msg.clone(),
            latency_ms: self.latency_ms,
            book_bid_count: self.order_book.bids.len(),
            book_ask_count: self.order_book.asks.len(),
            trades: Arc::clone(&self.snapshot_trades),
            depth_checkpoints: Arc::clone(&self.snapshot_depth_checkpoints),
            depth_deltas: Arc::clone(&self.snapshot_depth_deltas),
            depth_slices,
            fill_kill_series: Arc::clone(&self.snapshot_fill_kill_series),
            cumulative_series: Arc::clone(&self.snapshot_cumulative_series),
            fill_kill_kpis,
            buy_impact,
            sell_impact,
            tick_size: self.tick_size,
            price_decimals: self.price_decimals,
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/ui_tests.rs"]
mod tests;

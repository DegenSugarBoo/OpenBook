use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
    event::{self, Event, KeyCode, KeyModifiers},
};
use futures_util::{SinkExt, StreamExt};
use ordered_float::OrderedFloat;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const DEPTH_LEVELS: usize = 10;

// ═══════════════════════════════════════════════════════════════════════════
// CONFIGURATION
// ═══════════════════════════════════════════════════════════════════════════

fn get_user_config() -> (String, f64) {
    use std::io::{stdin, BufRead};
    
    println!("\x1b[1;36m╔═══════════════════════════════════════════════════════════════╗\x1b[0m");
    println!("\x1b[1;36m║                       ORDER BOOK VIEWER.                      ║\x1b[0m");
    println!("\x1b[1;36m╚═══════════════════════════════════════════════════════════════╝\x1b[0m");
    println!();
    
    // Get symbol
    println!("\x1b[1;33mEnter trading symbol (e.g., btcusdt, ethusdt, solusdt):\x1b[0m");
    print!("\x1b[97m> \x1b[0m");
    std::io::stdout().flush().ok();
    
    let stdin = stdin();
    let mut symbol = String::new();
    stdin.lock().read_line(&mut symbol).ok();
    let symbol = symbol.trim().to_lowercase();
    let symbol = if symbol.is_empty() { "btcusdt".to_string() } else { symbol };
    
    println!();
    
    // Get bin width
    println!("\x1b[1;33mEnter price bin width (e.g., 0.001, 1, 10, 100):\x1b[0m");
    print!("\x1b[97m> \x1b[0m");
    std::io::stdout().flush().ok();
    
    let mut bin_input = String::new();
    stdin.lock().read_line(&mut bin_input).ok();
    let bucket_size: f64 = bin_input.trim().parse().unwrap_or(1.0);
    let bucket_size = if bucket_size <= 0.0 { 1.0 } else { bucket_size };
    
    println!();
    println!("\x1b[32m✓ Symbol: {}\x1b[0m", symbol.to_uppercase());
    println!("\x1b[32m✓ Bin width: {}\x1b[0m", bucket_size);
    println!();
    
    (symbol, bucket_size)
}

// ═══════════════════════════════════════════════════════════════════════════
// PROFILING STATISTICS
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Default)]
struct ProfilingStats {
    // Timing stats in microseconds
    ws_parse_time: AtomicU64,
    ws_parse_count: AtomicU64,
    
    orderbook_update_time: AtomicU64,
    orderbook_update_count: AtomicU64,
    
    lock_acquire_time: AtomicU64,
    lock_acquire_count: AtomicU64,
    
    render_time: AtomicU64,
    render_count: AtomicU64,
    
    aggregation_time: AtomicU64,
    aggregation_count: AtomicU64,
    
    // Network latency (from event_time)
    network_latency: AtomicU64,
    network_count: AtomicU64,
    
    // Processing latency (time from receiving WS message to finishing update)
    processing_latency: AtomicU64,
    processing_count: AtomicU64,
}

impl ProfilingStats {
    fn record(&self, field: &AtomicU64, count_field: &AtomicU64, micros: u64) {
        // Exponential moving average (keep recent 100 samples weighted)
        field.fetch_add(micros, Ordering::Relaxed);
        count_field.fetch_add(1, Ordering::Relaxed);
    }
    
    fn get_avg(&self, field: &AtomicU64, count_field: &AtomicU64) -> f64 {
        let total = field.load(Ordering::Relaxed);
        let count = count_field.load(Ordering::Relaxed);
        if count == 0 { 0.0 } else { total as f64 / count as f64 }
    }
    
    fn summary(&self) -> String {
        format!(
            "PROFILING (avg μs):\n\
             ├─ WS Parse:     {:>8.1} μs ({} samples)\n\
             ├─ Lock Acquire: {:>8.1} μs ({} samples)\n\
             ├─ OB Update:    {:>8.1} μs ({} samples)\n\
             ├─ Aggregation:  {:>8.1} μs ({} samples)\n\
             ├─ Render:       {:>8.1} μs ({} samples)\n\
             ├─ Processing:   {:>8.1} μs ({} samples)\n\
             └─ Network Lat:  {:>8.1} ms ({} samples)",
            self.get_avg(&self.ws_parse_time, &self.ws_parse_count),
            self.ws_parse_count.load(Ordering::Relaxed),
            self.get_avg(&self.lock_acquire_time, &self.lock_acquire_count),
            self.lock_acquire_count.load(Ordering::Relaxed),
            self.get_avg(&self.orderbook_update_time, &self.orderbook_update_count),
            self.orderbook_update_count.load(Ordering::Relaxed),
            self.get_avg(&self.aggregation_time, &self.aggregation_count),
            self.aggregation_count.load(Ordering::Relaxed),
            self.get_avg(&self.render_time, &self.render_count) / 1000.0,  // Convert to ms
            self.render_count.load(Ordering::Relaxed),
            self.get_avg(&self.processing_latency, &self.processing_count) / 1000.0,  // Convert to ms
            self.processing_count.load(Ordering::Relaxed),
            self.get_avg(&self.network_latency, &self.network_count) / 1000.0,  // Already in ms
            self.network_count.load(Ordering::Relaxed),
        )
    }
    
    fn reset(&self) {
        self.ws_parse_time.store(0, Ordering::Relaxed);
        self.ws_parse_count.store(0, Ordering::Relaxed);
        self.orderbook_update_time.store(0, Ordering::Relaxed);
        self.orderbook_update_count.store(0, Ordering::Relaxed);
        self.lock_acquire_time.store(0, Ordering::Relaxed);
        self.lock_acquire_count.store(0, Ordering::Relaxed);
        self.render_time.store(0, Ordering::Relaxed);
        self.render_count.store(0, Ordering::Relaxed);
        self.aggregation_time.store(0, Ordering::Relaxed);
        self.aggregation_count.store(0, Ordering::Relaxed);
        self.network_latency.store(0, Ordering::Relaxed);
        self.network_count.store(0, Ordering::Relaxed);
        self.processing_latency.store(0, Ordering::Relaxed);
        self.processing_count.store(0, Ordering::Relaxed);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// DATA STRUCTURES
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
struct RestDepthResponse {
    #[serde(rename = "lastUpdateId")]
    last_update_id: u64,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

#[derive(Debug, Deserialize)]
struct ServerTimeResponse {
    #[serde(rename = "serverTime")]
    server_time: u64,
}

#[derive(Debug, Deserialize, Clone)]
struct WsDepthUpdate {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "T")]
    transaction_time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "U")]
    first_update_id: u64,
    #[serde(rename = "u")]
    final_update_id: u64,
    #[serde(rename = "pu")]
    prev_final_update_id: u64,
    #[serde(rename = "b")]
    bids: Vec<[String; 2]>,
    #[serde(rename = "a")]
    asks: Vec<[String; 2]>,
}

#[derive(Clone)]
struct OrderBook {
    bids: BTreeMap<OrderedFloat<f64>, f64>,
    asks: BTreeMap<OrderedFloat<f64>, f64>,
    last_update_id: u64,
    last_event_time: u64,
    clock_offset: i64,
}

impl OrderBook {
    fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
            last_event_time: 0,
            clock_offset: 0,
        }
    }

    fn apply_snapshot(&mut self, snapshot: &RestDepthResponse) {
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

    fn apply_update(&mut self, update: &WsDepthUpdate) {
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

    fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.iter().next_back().map(|(p, q)| (p.0, *q))
    }

    fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.iter().next().map(|(p, q)| (p.0, *q))
    }

    fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((bid, _)), Some((ask, _))) => Some(ask - bid),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// TERMINAL RENDERING
// ═══════════════════════════════════════════════════════════════════════════



fn render_orderbook(book: &OrderBook, stats: &ProfilingStats, symbol: &str, bucket_size: f64) {
    let render_start = Instant::now();
    
    let mut stdout = stdout();

    let (term_width, term_height) = terminal::size().unwrap_or((120, 40));
    let term_width = term_width as usize;
    let term_height = term_height as usize;

    // Clear the entire screen and reset cursor to prevent ghosting
    let _ = execute!(
        stdout,
        cursor::Hide,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    );

    // Stats
    let spread = book.spread().unwrap_or(0.0);
    let best_bid = book.best_bid().map(|(p, _)| p).unwrap_or(0.0);
    let best_ask = book.best_ask().map(|(p, _)| p).unwrap_or(0.0);
    let mid_price = (best_bid + best_ask) / 2.0;
    let spread_pct = if mid_price > 0.0 { (spread / mid_price) * 100.0 } else { 0.0 };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let event_time = book.last_event_time as i64;
    let latency = if event_time > 0 { now - event_time - book.clock_offset } else { 0 };

    // PROFILED: Aggregation timing
    let agg_start = Instant::now();
    
    let mut bid_buckets: std::collections::BTreeMap<i64, f64> = std::collections::BTreeMap::new();
    for (price, qty) in book.bids.iter() {
        let bucket = (price.0 / bucket_size).floor() as i64 * bucket_size as i64;
        *bid_buckets.entry(bucket).or_insert(0.0) += *qty;
    }
    let bids: Vec<(i64, f64)> = bid_buckets.iter().rev().take(DEPTH_LEVELS).map(|(&p, &q)| (p, q)).collect();
    
    let mut ask_buckets: std::collections::BTreeMap<i64, f64> = std::collections::BTreeMap::new();
    for (price, qty) in book.asks.iter() {
        let bucket = (price.0 / bucket_size).floor() as i64 * bucket_size as i64;
        *ask_buckets.entry(bucket).or_insert(0.0) += *qty;
    }
    let asks: Vec<(i64, f64)> = ask_buckets.iter().take(DEPTH_LEVELS).map(|(&p, &q)| (p, q)).collect();

    let agg_elapsed = agg_start.elapsed().as_micros() as u64;
    stats.record(&stats.aggregation_time, &stats.aggregation_count, agg_elapsed);

    // Calculate cumulative quantities
    let mut bid_cumulative: Vec<f64> = Vec::with_capacity(bids.len());
    let mut cumsum = 0.0;
    for (_, qty) in &bids {
        cumsum += *qty;
        bid_cumulative.push(cumsum);
    }
    
    let mut ask_cumulative: Vec<f64> = Vec::with_capacity(asks.len());
    cumsum = 0.0;
    for (_, qty) in &asks {
        cumsum += *qty;
        ask_cumulative.push(cumsum);
    }
    
    let max_cum_bid = bid_cumulative.last().copied().unwrap_or(0.0);
    let max_cum_ask = ask_cumulative.last().copied().unwrap_or(0.0);
    let max_cumulative = max_cum_bid.max(max_cum_ask);

    // Dynamic column widths based on terminal size
    // Minimum widths for readability
    let min_price_col = 8;
    let min_qty_col = 8;
    let min_bar_width = 5;
    let gap_width = 4;
    
    // Calculate available space and distribute it
    let min_total = (min_price_col + min_qty_col + min_bar_width) * 2 + gap_width + 4;
    
    let (price_col, qty_col, bar_max_width) = if term_width < min_total {
        // Very narrow terminal - use minimum sizes
        (min_price_col, min_qty_col, min_bar_width)
    } else if term_width < 100 {
        // Medium terminal - moderate sizes
        let extra = term_width - min_total;
        let bar_extra = extra / 2;
        (min_price_col + 2, min_qty_col + 1, min_bar_width + bar_extra.min(15))
    } else {
        // Wide terminal - full sizes
        let price_col = 12;
        let qty_col = 10;
        let fixed_width = (price_col + qty_col) * 2 + gap_width + 10;
        let available_for_bars = term_width.saturating_sub(fixed_width);
        let bar_width = (available_for_bars / 2).max(min_bar_width).min(40);
        (price_col, qty_col, bar_width)
    };

    // Helper to write a line that's properly padded/truncated to terminal width
    macro_rules! write_centered_line {
        ($stdout:expr, $content:expr) => {{
            let content = $content;
            let visible_len = strip_ansi_len(&content);
            if visible_len < term_width {
                let pad = (term_width - visible_len) / 2;
                write!($stdout, "{}{}\x1b[K\r\n", " ".repeat(pad), content).ok();
            } else {
                write!($stdout, "{}\x1b[K\r\n", content).ok();
            }
        }};
    }

    // Header - adapt to terminal width
    let symbol_upper = symbol.to_uppercase();
    let header_width = 67; // Width of the decorative header
    if term_width >= header_width {
        let header = "╔═══════════════════════════════════════════════════════════════╗";
        let title = format!("║         {} PERPETUAL FUTURES ORDER BOOK{}", 
            symbol_upper, " ".repeat(63 - 35 - symbol_upper.len().min(28)));
        let title = format!("{}║", &title[..title.len().min(64)]);
        let footer = "╚═══════════════════════════════════════════════════════════════╝";
        write_centered_line!(stdout, format!("\x1b[1;33m{}\x1b[0m", header));
        write_centered_line!(stdout, format!("\x1b[1;33m{}\x1b[0m", title));
        write_centered_line!(stdout, format!("\x1b[1;33m{}\x1b[0m", footer));
    } else {
        // Compact header for narrow terminals
        write_centered_line!(stdout, format!("\x1b[1;33m═══ {} ORDER BOOK ═══\x1b[0m", symbol_upper));
    }
    write!(stdout, "\x1b[K\r\n").ok();
    
    // Stats line - adapt to terminal width
    if term_width >= 70 {
        write_centered_line!(stdout, format!(
            "Mid: \x1b[1;97m{:.2}\x1b[0m  │  Spread: \x1b[1;35m{:.2}\x1b[0m (\x1b[35m{:.4}%\x1b[0m)  │  Latency: \x1b[93m{:>3}ms\x1b[0m",
            mid_price, spread, spread_pct, latency
        ));
    } else {
        // Compact stats for narrow terminals
        write_centered_line!(stdout, format!(
            "Mid: \x1b[97m{:.2}\x1b[0m | Sprd: \x1b[35m{:.4}%\x1b[0m | Lat: \x1b[93m{:>3}ms\x1b[0m",
            mid_price, spread_pct, latency
        ));
    }
    write!(stdout, "\x1b[K\r\n").ok();

    // Calculate row layout
    let row_content_width = bar_max_width * 2 + qty_col * 2 + price_col * 2 + gap_width + 4;
    let left_pad = if term_width > row_content_width { (term_width - row_content_width) / 2 } else { 0 };
    let pad = " ".repeat(left_pad);

    // Column headers
    write!(
        stdout,
        "{}{:>width_bar$}  \x1b[1;32m{:>width_qty$}  {:>width_price$}\x1b[0m  {}  \x1b[1;31m{:<width_price$}  {:<width_qty$}\x1b[0m  {:<width_bar$}\x1b[K\r\n",
        pad, "CUM.DEPTH", "BID QTY", "BID PRICE", " ▐▌ ", "ASK PRICE", "ASK QTY", "CUM.DEPTH",
        width_bar = bar_max_width, width_qty = qty_col, width_price = price_col
    ).ok();
    
    // Separator line
    let sep_bid = "─".repeat(bar_max_width + qty_col + price_col + 4);
    let sep_ask = "─".repeat(bar_max_width + qty_col + price_col + 4);
    write!(
        stdout,
        "{}\x1b[32m{}\x1b[0m{}\x1b[31m{}\x1b[0m\x1b[K\r\n",
        pad, sep_bid, " ─┼─ ", sep_ask
    ).ok();

    // Order book rows
    for i in 0..DEPTH_LEVELS {
        let (bid_bar, bid_qty_str, bid_price_str) = if i < bids.len() {
            let (price, qty) = bids[i];
            let individual_qty = qty;
            let cumulative_qty = bid_cumulative[i];
            
            let cum_bar_width = if max_cumulative > 0.0 { 
                ((cumulative_qty / max_cumulative) * bar_max_width as f64) as usize 
            } else { 0 };
            let individual_bar_width = if max_cumulative > 0.0 { 
                ((individual_qty / max_cumulative) * bar_max_width as f64) as usize 
            } else { 0 };
            
            let darker_width = cum_bar_width.saturating_sub(individual_bar_width);
            let empty_width = bar_max_width.saturating_sub(cum_bar_width);
            
            let empty_part = " ".repeat(empty_width);
            let darker_part = format!("\x1b[48;5;22m\x1b[38;5;28m{}\x1b[0m", "█".repeat(darker_width));
            let lighter_part = format!("\x1b[48;5;34m\x1b[38;5;46m{}\x1b[0m", "█".repeat(individual_bar_width.min(bar_max_width)));
            
            (
                format!("{}{}{}", empty_part, darker_part, lighter_part),
                format!("{:>width$.4}", individual_qty, width = qty_col),
                format!("{:>width$}", price, width = price_col),
            )
        } else {
            (
                " ".repeat(bar_max_width),
                format!("{:>width$}", "", width = qty_col),
                format!("{:>width$}", "", width = price_col),
            )
        };

        let (ask_price_str, ask_qty_str, ask_bar) = if i < asks.len() {
            let (price, qty) = asks[i];
            let individual_qty = qty;
            let cumulative_qty = ask_cumulative[i];
            
            let cum_bar_width = if max_cumulative > 0.0 { 
                ((cumulative_qty / max_cumulative) * bar_max_width as f64) as usize 
            } else { 0 };
            let individual_bar_width = if max_cumulative > 0.0 { 
                ((individual_qty / max_cumulative) * bar_max_width as f64) as usize 
            } else { 0 };
            
            let darker_width = cum_bar_width.saturating_sub(individual_bar_width);
            let empty_width = bar_max_width.saturating_sub(cum_bar_width);
            
            let lighter_part = format!("\x1b[48;5;160m\x1b[38;5;196m{}\x1b[0m", "█".repeat(individual_bar_width.min(bar_max_width)));
            let darker_part = format!("\x1b[48;5;52m\x1b[38;5;88m{}\x1b[0m", "█".repeat(darker_width));
            let empty_part = " ".repeat(empty_width);
            
            (
                format!("{:<width$}", price, width = price_col),
                format!("{:<width$.4}", individual_qty, width = qty_col),
                format!("{}{}{}", lighter_part, darker_part, empty_part),
            )
        } else {
            (
                format!("{:<width$}", "", width = price_col),
                format!("{:<width$}", "", width = qty_col),
                " ".repeat(bar_max_width),
            )
        };

        let (bid_color, ask_color) = if i == 0 {
            ("\x1b[1;32m", "\x1b[1;31m")
        } else {
            ("\x1b[32m", "\x1b[31m")
        };

        write!(
            stdout,
            "{}{}  {}{}  {}\x1b[0m  \x1b[90m▐▌\x1b[0m  {}{}  {}\x1b[0m  {}\x1b[K\r\n",
            pad, bid_bar, bid_color, bid_qty_str, bid_price_str,
            ask_color, ask_price_str, ask_qty_str, ask_bar
        ).ok();
    }

    write!(stdout, "\x1b[K\r\n").ok();
    
    // Summary line
    let total_bid_qty: f64 = bids.iter().map(|(_, q)| *q).sum();
    let total_ask_qty: f64 = asks.iter().map(|(_, q)| *q).sum();
    let imbalance = if total_bid_qty + total_ask_qty > 0.0 {
        ((total_bid_qty - total_ask_qty) / (total_bid_qty + total_ask_qty)) * 100.0
    } else {
        0.0
    };
    
    let imbalance_color = if imbalance > 0.0 { "\x1b[32m" } else if imbalance < 0.0 { "\x1b[31m" } else { "\x1b[37m" };
    let imbalance_str = if imbalance > 0.0 { format!("+{:.1}", imbalance) } else { format!("{:.1}", imbalance) };
    
    // Extract base asset name for display (e.g., BTCUSDT -> BTC)
    let base_asset = symbol.to_uppercase();
    let base_asset = if let Some(stripped) = base_asset.strip_suffix("USDT") {
        stripped
    } else if let Some(stripped) = base_asset.strip_suffix("USDC") {
        stripped
    } else if let Some(stripped) = base_asset.strip_suffix("BUSD") {
        stripped
    } else {
        &base_asset
    };

    if term_width >= 75 {
        write_centered_line!(stdout, format!(
            "Total Bids: \x1b[32m{:.4}\x1b[0m {}  │  Total Asks: \x1b[31m{:.4}\x1b[0m {}  │  Imbalance: {}{}%\x1b[0m",
            total_bid_qty, base_asset, total_ask_qty, base_asset, imbalance_color, imbalance_str
        ));
    } else {
        write_centered_line!(stdout, format!(
            "Bids: \x1b[32m{:.2}\x1b[0m | Asks: \x1b[31m{:.2}\x1b[0m | Imb: {}{}%\x1b[0m",
            total_bid_qty, total_ask_qty, imbalance_color, imbalance_str
        ));
    }
    
    write!(stdout, "\x1b[K\r\n").ok();
    write_centered_line!(stdout, format!("Update ID: \x1b[90m{}\x1b[0m", book.last_update_id));
    
    // PROFILING OUTPUT - adapt to terminal width
    write!(stdout, "\x1b[K\r\n").ok();
    
    let prof_header_width = 67;
    if term_width >= prof_header_width {
        write!(stdout, "\x1b[1;36m╔═══════════════════════════════════════════════════════════════╗\x1b[0m\x1b[K\r\n").ok();
        write!(stdout, "\x1b[1;36m║                     PROFILING STATISTICS                      ║\x1b[0m\x1b[K\r\n").ok();
        write!(stdout, "\x1b[1;36m╚═══════════════════════════════════════════════════════════════╝\x1b[0m\x1b[K\r\n").ok();
    } else {
        write!(stdout, "\x1b[1;36m─── PROFILING ───\x1b[0m\x1b[K\r\n").ok();
    }
    
    let ws_parse = stats.get_avg(&stats.ws_parse_time, &stats.ws_parse_count);
    let lock_acquire = stats.get_avg(&stats.lock_acquire_time, &stats.lock_acquire_count);
    let ob_update = stats.get_avg(&stats.orderbook_update_time, &stats.orderbook_update_count);
    let aggregation = stats.get_avg(&stats.aggregation_time, &stats.aggregation_count);
    let render_elapsed = render_start.elapsed().as_micros() as u64;
    let processing = stats.get_avg(&stats.processing_latency, &stats.processing_count) / 1000.0;
    let network_lat = stats.get_avg(&stats.network_latency, &stats.network_count);
    
    if term_width >= 50 {
        write!(stdout, "\x1b[33m├─ WS Parse:     {:>8.1} μs\x1b[0m\x1b[K\r\n", ws_parse).ok();
        write!(stdout, "\x1b[33m├─ Lock Acquire: {:>8.1} μs\x1b[0m\x1b[K\r\n", lock_acquire).ok();
        write!(stdout, "\x1b[33m├─ OB Update:    {:>8.1} μs\x1b[0m\x1b[K\r\n", ob_update).ok();
        write!(stdout, "\x1b[33m├─ Aggregation:  {:>8.1} μs\x1b[0m\x1b[K\r\n", aggregation).ok();
        write!(stdout, "\x1b[33m├─ Render:       {:>8.1} ms\x1b[0m\x1b[K\r\n", render_elapsed as f64 / 1000.0).ok();
        write!(stdout, "\x1b[33m├─ Processing:   {:>8.1} ms\x1b[0m\x1b[K\r\n", processing).ok();
        write!(stdout, "\x1b[1;93m└─ Network Lat:  {:>8.1} ms\x1b[0m\x1b[K\r\n", network_lat).ok();
    } else {
        // Compact profiling for narrow terminals
        write!(stdout, "\x1b[33mWS:{:.0}μs Lock:{:.0}μs OB:{:.0}μs\x1b[0m\x1b[K\r\n", ws_parse, lock_acquire, ob_update).ok();
        write!(stdout, "\x1b[33mAgg:{:.0}μs Rnd:{:.1}ms Proc:{:.1}ms\x1b[0m\x1b[K\r\n", aggregation, render_elapsed as f64 / 1000.0, processing).ok();
        write!(stdout, "\x1b[1;93mNetwork: {:.1}ms\x1b[0m\x1b[K\r\n", network_lat).ok();
    }
    
    write!(stdout, "\x1b[K\r\n").ok();
    if term_width >= 60 {
        write!(stdout, "\x1b[90mBook size: {} bids, {} asks | Press 'q' or Ctrl+C to exit\x1b[0m\x1b[K\r\n", 
            book.bids.len(), book.asks.len()).ok();
    } else {
        write!(stdout, "\x1b[90m{} bids, {} asks | 'q' to exit\x1b[0m\x1b[K\r\n", 
            book.bids.len(), book.asks.len()).ok();
    }
    
    // Clear any remaining lines below our content
    // This prevents ghosting when terminal is resized larger then smaller
    let _ = execute!(stdout, terminal::Clear(ClearType::FromCursorDown));
    
    // Record render time (for next iteration)
    stats.record(&stats.render_time, &stats.render_count, render_elapsed);

    let _ = stdout.flush();
}

/// Helper function to calculate visible length of a string (ignoring ANSI escape codes)
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            len += 1;
        }
    }
    len
}

// ═══════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════

fn cleanup_terminal() {
    let mut stdout = stdout();
    let _ = terminal::disable_raw_mode();
    let _ = execute!(stdout, cursor::Show, terminal::Clear(ClearType::All), cursor::MoveTo(0, 0));
    println!("\nGoodbye!");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get user configuration before entering raw mode
    let (symbol, bucket_size) = get_user_config();
    
    let _ = terminal::enable_raw_mode();

    // Shared profiling stats
    let stats = Arc::new(ProfilingStats::default());

    let order_book = Arc::new(RwLock::new(OrderBook::new()));
    let order_book_ws = Arc::clone(&order_book);
    let order_book_render = Arc::clone(&order_book);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(WsDepthUpdate, Instant)>();

    let ws_url = format!("wss://fstream.binance.com/stream?streams={}@depth@100ms", symbol);

    println!("\x1b[33mConnecting to WebSocket...\x1b[0m");

    let (ws_stream, _) = connect_async(&ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    println!("\x1b[32mWebSocket connected! Buffering events...\x1b[0m");

    let tx_clone = tx.clone();
    let stats_ws = Arc::clone(&stats);
    tokio::spawn(async move {
        while let Some(msg_result) = read.next().await {
            let recv_time = Instant::now();
            match msg_result {
                Ok(Message::Text(text)) => {
                    // PROFILED: JSON parsing
                    let parse_start = Instant::now();
                    
                    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(data) = wrapper.get("data") {
                            if let Ok(update) = serde_json::from_value::<WsDepthUpdate>(data.clone()) {
                                let parse_elapsed = parse_start.elapsed().as_micros() as u64;
                                stats_ws.record(&stats_ws.ws_parse_time, &stats_ws.ws_parse_count, parse_elapsed);
                                
                                let _ = tx_clone.send((update, recv_time));
                            }
                        }
                    }
                }
                Ok(Message::Ping(_)) => {}
                Ok(Message::Close(_)) => {
                    eprintln!("\x1b[31mWebSocket closed by server\x1b[0m");
                    break;
                }
                Err(e) => {
                    eprintln!("\x1b[31mWebSocket error: {}\x1b[0m", e);
                    break;
                }
                _ => {}
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    println!("\x1b[33mFetching REST depth snapshot...\x1b[0m");

    let client = reqwest::Client::new();
    let rest_url = format!(
        "https://fapi.binance.com/fapi/v1/depth?symbol={}&limit=1000",
        symbol.to_uppercase()
    );

    let snapshot: RestDepthResponse = client.get(&rest_url).send().await?.json().await?;
    let snapshot_update_id = snapshot.last_update_id;

    println!(
        "\x1b[32mSnapshot received! lastUpdateId: {}\x1b[0m",
        snapshot_update_id
    );

    println!("\x1b[33mCalibrating clock offset...\x1b[0m");
    let time_url = "https://fapi.binance.com/fapi/v1/time";
    let local_before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let server_time: ServerTimeResponse = client.get(time_url).send().await?.json().await?;
    let local_after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let local_mid = (local_before + local_after) / 2;
    let clock_offset = local_mid - server_time.server_time as i64;
    println!("\x1b[32mClock offset: {}ms (local {} Binance)\x1b[0m", 
        clock_offset.abs(),
        if clock_offset > 0 { "ahead of" } else { "behind" }
    );

    {
        let mut book = order_book.write().await;
        book.apply_snapshot(&snapshot);
        book.clock_offset = clock_offset;
    }

    println!("\x1b[33mSyncing with buffered events...\x1b[0m");

    let mut synced = false;
    let mut last_final_update_id: u64 = 0;

    while let Ok((update, _recv_time)) = rx.try_recv() {
        if update.final_update_id < snapshot_update_id {
            continue;
        }

        if !synced {
            if update.first_update_id <= snapshot_update_id && update.final_update_id > snapshot_update_id {
                let mut book = order_book.write().await;
                book.apply_update(&update);
                last_final_update_id = update.final_update_id;
                synced = true;
                println!("\x1b[32mSynced! First valid update: U={}, u={}\x1b[0m", 
                    update.first_update_id, update.final_update_id);
            }
            continue;
        }

        if update.prev_final_update_id == last_final_update_id {
            let mut book = order_book.write().await;
            book.apply_update(&update);
            last_final_update_id = update.final_update_id;
        }
    }

    if !synced {
        println!("\x1b[33mWaiting for sync event from live stream...\x1b[0m");
    }

    write!(stdout(), "\r\n\x1b[32mEntering live mode (PROFILED)!\x1b[0m\r\n").ok();
    stdout().flush().ok();
    tokio::time::sleep(Duration::from_millis(500)).await;
    
    let _ = execute!(stdout(), terminal::Clear(ClearType::All), cursor::MoveTo(0, 0));

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_render = Arc::clone(&shutdown);
    let shutdown_keyboard = Arc::clone(&shutdown);

    std::thread::spawn(move || {
        loop {
            if shutdown_keyboard.load(Ordering::Relaxed) {
                break;
            }
            if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(Event::Key(key_event)) = event::read() {
                    if key_event.modifiers.contains(KeyModifiers::CONTROL)
                        && key_event.code == KeyCode::Char('c')
                    {
                        shutdown_keyboard.store(true, Ordering::Relaxed);
                        break;
                    }
                    if key_event.code == KeyCode::Char('q') {
                        shutdown_keyboard.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    });

    let stats_render = Arc::clone(&stats);
    let symbol_render = symbol.clone();
    let render_handle = tokio::spawn(async move {
        let mut last_render = Instant::now();
        let render_interval = Duration::from_millis(50); // ~20 FPS

        loop {
            if shutdown_render.load(Ordering::Relaxed) {
                break;
            }
            if last_render.elapsed() >= render_interval {
                let book = order_book_render.read().await;
                render_orderbook(&book, &stats_render, &symbol_render, bucket_size);
                last_render = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    let stats_main = Arc::clone(&stats);
    
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
            }
            update = rx.recv() => {
                let Some((update, recv_time)) = update else { break };
                
                // Record network latency
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                let event_time = update.event_time as i64;
                let network_latency = (now_ms - event_time - {
                    let book = order_book_ws.read().await;
                    book.clock_offset
                }).max(0) as u64;
                stats_main.record(&stats_main.network_latency, &stats_main.network_count, network_latency);
                
                if update.final_update_id <= last_final_update_id {
                    continue;
                }

                if !synced {
                    if update.first_update_id <= snapshot_update_id && update.final_update_id > snapshot_update_id {
                        let mut book = order_book_ws.write().await;
                        book.apply_update(&update);
                        last_final_update_id = update.final_update_id;
                        synced = true;
                    }
                    continue;
                }

                if update.prev_final_update_id != last_final_update_id {
                    eprintln!(
                        "\x1b[31mSequence gap! Expected pu={}, got pu={}\x1b[0m",
                        last_final_update_id, update.prev_final_update_id
                    );
                    continue;
                }

                // PROFILED: Lock acquisition and update
                let lock_start = Instant::now();
                let mut book = order_book_ws.write().await;
                let lock_elapsed = lock_start.elapsed().as_micros() as u64;
                stats_main.record(&stats_main.lock_acquire_time, &stats_main.lock_acquire_count, lock_elapsed);
                
                let update_start = Instant::now();
                book.apply_update(&update);
                let update_elapsed = update_start.elapsed().as_micros() as u64;
                stats_main.record(&stats_main.orderbook_update_time, &stats_main.orderbook_update_count, update_elapsed);
                
                drop(book);
                
                last_final_update_id = update.final_update_id;
                
                // Record total processing latency
                let processing_elapsed = recv_time.elapsed().as_micros() as u64;
                stats_main.record(&stats_main.processing_latency, &stats_main.processing_count, processing_elapsed);
            }
        }
    }

    cleanup_terminal();

    Ok(())
}

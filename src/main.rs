use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ordered_float::OrderedFloat;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};
use serde::Deserialize;
use std::collections::{BTreeMap, VecDeque};
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
    println!("\x1b[1;36m║                       ORDER BOOK VIEWER                       ║\x1b[0m");
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
    
    network_latency: AtomicU64,
    network_count: AtomicU64,
    
    processing_latency: AtomicU64,
    processing_count: AtomicU64,
}

impl ProfilingStats {
    fn record(&self, field: &AtomicU64, count_field: &AtomicU64, micros: u64) {
        field.fetch_add(micros, Ordering::Relaxed);
        count_field.fetch_add(1, Ordering::Relaxed);
    }
    
    fn get_avg(&self, field: &AtomicU64, count_field: &AtomicU64) -> f64 {
        let total = field.load(Ordering::Relaxed);
        let count = count_field.load(Ordering::Relaxed);
        if count == 0 { 0.0 } else { total as f64 / count as f64 }
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
#[allow(dead_code)]
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

// Aggregate trade from WebSocket
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct WsAggTrade {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "a")]
    agg_trade_id: u64,
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "q")]
    quantity: String,
    #[serde(rename = "f")]
    first_trade_id: u64,
    #[serde(rename = "l")]
    last_trade_id: u64,
    #[serde(rename = "T")]
    trade_time: u64,
    #[serde(rename = "m")]
    is_buyer_maker: bool,
}

// Parsed trade for display
#[derive(Clone, Debug)]
struct Trade {
    timestamp_ms: u64,
    price: f64,
    quantity: f64,
    is_buy: bool,  // true = buyer was taker (market buy)
}

// Rolling window of price and trade history
#[derive(Clone)]
struct TradeHistory {
    price_points: VecDeque<(u64, f64)>,  // (timestamp_ms, mid_price)
    trades: VecDeque<Trade>,
    window_ms: u64,  // Time window to keep (e.g., 60000 for 60s)
}

impl TradeHistory {
    fn new(window_ms: u64) -> Self {
        Self {
            price_points: VecDeque::new(),
            trades: VecDeque::new(),
            window_ms,
        }
    }

    fn add_price(&mut self, timestamp_ms: u64, price: f64) {
        self.price_points.push_back((timestamp_ms, price));
        self.cleanup(timestamp_ms);
    }

    fn add_trade(&mut self, trade: Trade) {
        let timestamp = trade.timestamp_ms;
        self.trades.push_back(trade);
        self.cleanup(timestamp);
    }

    fn cleanup(&mut self, current_time_ms: u64) {
        let cutoff = current_time_ms.saturating_sub(self.window_ms);
        while let Some(&(ts, _)) = self.price_points.front() {
            if ts < cutoff {
                self.price_points.pop_front();
            } else {
                break;
            }
        }
        while let Some(trade) = self.trades.front() {
            if trade.timestamp_ms < cutoff {
                self.trades.pop_front();
            } else {
                break;
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// APP STATE
// ═══════════════════════════════════════════════════════════════════════════

struct App {
    symbol: String,
    bucket_size: f64,
    render_time_us: u64,
}

impl App {
    fn new(symbol: String, bucket_size: f64) -> Self {
        Self {
            symbol,
            bucket_size,
            render_time_us: 0,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// UI RENDERING
// ═══════════════════════════════════════════════════════════════════════════

// Color theme
const BID_COLOR: Color = Color::Rgb(0, 180, 0);
const BID_BRIGHT: Color = Color::Rgb(50, 255, 50);
const BID_DIM: Color = Color::Rgb(0, 80, 0);
const ASK_COLOR: Color = Color::Rgb(180, 0, 0);
const ASK_BRIGHT: Color = Color::Rgb(255, 50, 50);
const ASK_DIM: Color = Color::Rgb(80, 0, 0);
const HEADER_COLOR: Color = Color::Yellow;
const ACCENT_COLOR: Color = Color::Cyan;

fn ui(f: &mut Frame, app: &App, book: &OrderBook, stats: &ProfilingStats, trade_history: &TradeHistory) {
    let agg_start = Instant::now();
    
    // Calculate aggregated data
    let mut bid_buckets: BTreeMap<OrderedFloat<f64>, f64> = BTreeMap::new();
    for (price, qty) in book.bids.iter() {
        let bucket = (price.0 / app.bucket_size).floor() * app.bucket_size;
        *bid_buckets.entry(OrderedFloat(bucket)).or_insert(0.0) += *qty;
    }
    let bids: Vec<(f64, f64)> = bid_buckets.iter().rev().take(DEPTH_LEVELS).map(|(p, &q)| (p.0, q)).collect();
    
    let mut ask_buckets: BTreeMap<OrderedFloat<f64>, f64> = BTreeMap::new();
    for (price, qty) in book.asks.iter() {
        let bucket = (price.0 / app.bucket_size).floor() * app.bucket_size;
        *ask_buckets.entry(OrderedFloat(bucket)).or_insert(0.0) += *qty;
    }
    let asks: Vec<(f64, f64)> = ask_buckets.iter().take(DEPTH_LEVELS).map(|(p, &q)| (p.0, q)).collect();
    
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
    
    // Stats calculations
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
    
    // Layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),   // Header
            Constraint::Length(2),   // Stats line
            Constraint::Length(1),   // Column headers
            Constraint::Min(12),     // Order book
            Constraint::Length(2),   // Summary
            Constraint::Length(1),   // Update ID
            Constraint::Length(10),  // Price chart
            Constraint::Length(11),  // Profiling
            Constraint::Length(1),   // Footer
        ])
        .split(f.area());
    
    // Header
    let header_text = format!("{} PERPETUAL FUTURES ORDER BOOK", app.symbol.to_uppercase());
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(HEADER_COLOR).add_modifier(Modifier::BOLD))
        .alignment(ratatui::layout::Alignment::Center)
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(HEADER_COLOR)));
    f.render_widget(header, chunks[0]);
    
    // Stats line
    let stats_text = Line::from(vec![
        Span::raw("Mid: "),
        Span::styled(format!("{:.2}", mid_price), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::raw("  │  Spread: "),
        Span::styled(format!("{:.2}", spread), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::raw(" ("),
        Span::styled(format!("{:.4}%", spread_pct), Style::default().fg(Color::Magenta)),
        Span::raw(")  │  Latency: "),
        Span::styled(format!("{:>3}ms", latency), Style::default().fg(Color::Yellow)),
    ]);
    let stats_paragraph = Paragraph::new(stats_text)
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(stats_paragraph, chunks[1]);
    
    // Order book area - split into bids and asks
    let orderbook_area = chunks[3];
    let ob_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(orderbook_area);
    
    // Column headers - use same layout as data for proper alignment
    let header_area = chunks[2];
    let header_ob_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),
            Constraint::Percentage(50),
        ])
        .split(header_area);
    
    // Bid side headers (matching render_bids layout)
    let bid_header_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(10),     // Depth bar area
            Constraint::Length(12),  // Qty
            Constraint::Length(12),  // Price
        ])
        .split(header_ob_chunks[0]);
    
    let bid_depth_header = Paragraph::new("CUM.DEPTH")
        .style(Style::default().fg(Color::DarkGray))
        .alignment(ratatui::layout::Alignment::Right);
    f.render_widget(bid_depth_header, bid_header_chunks[0]);
    
    let bid_qty_header = Paragraph::new("BID QTY")
        .style(Style::default().fg(BID_COLOR).add_modifier(Modifier::BOLD))
        .alignment(ratatui::layout::Alignment::Right);
    f.render_widget(bid_qty_header, bid_header_chunks[1]);
    
    let bid_price_header = Paragraph::new("BID PRICE")
        .style(Style::default().fg(BID_COLOR).add_modifier(Modifier::BOLD))
        .alignment(ratatui::layout::Alignment::Right);
    f.render_widget(bid_price_header, bid_header_chunks[2]);
    
    // Ask side headers (matching render_asks layout)
    let ask_header_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(12),  // Price
            Constraint::Length(12),  // Qty
            Constraint::Min(10),     // Depth bar area
        ])
        .split(header_ob_chunks[1]);
    
    let ask_price_header = Paragraph::new("ASK PRICE")
        .style(Style::default().fg(ASK_COLOR).add_modifier(Modifier::BOLD));
    f.render_widget(ask_price_header, ask_header_chunks[0]);
    
    let ask_qty_header = Paragraph::new("ASK QTY")
        .style(Style::default().fg(ASK_COLOR).add_modifier(Modifier::BOLD));
    f.render_widget(ask_qty_header, ask_header_chunks[1]);
    
    let ask_depth_header = Paragraph::new("CUM.DEPTH")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(ask_depth_header, ask_header_chunks[2]);
    
    // Render bids side
    render_bids(f, ob_chunks[0], &bids, &bid_cumulative, max_cumulative);
    
    // Render asks side
    render_asks(f, ob_chunks[1], &asks, &ask_cumulative, max_cumulative);
    
    // Summary line
    let total_bid_qty: f64 = bids.iter().map(|(_, q)| *q).sum();
    let total_ask_qty: f64 = asks.iter().map(|(_, q)| *q).sum();
    let imbalance = if total_bid_qty + total_ask_qty > 0.0 {
        ((total_bid_qty - total_ask_qty) / (total_bid_qty + total_ask_qty)) * 100.0
    } else {
        0.0
    };
    
    let imbalance_color = if imbalance > 0.0 { BID_COLOR } else if imbalance < 0.0 { ASK_COLOR } else { Color::Gray };
    let imbalance_str = if imbalance > 0.0 { format!("+{:.1}", imbalance) } else { format!("{:.1}", imbalance) };
    
    let base_asset = app.symbol.to_uppercase();
    let base_asset = if let Some(stripped) = base_asset.strip_suffix("USDT") {
        stripped
    } else if let Some(stripped) = base_asset.strip_suffix("USDC") {
        stripped
    } else if let Some(stripped) = base_asset.strip_suffix("BUSD") {
        stripped
    } else {
        &base_asset
    };
    
    let summary_text = Line::from(vec![
        Span::raw("Total Bids: "),
        Span::styled(format!("{:.4}", total_bid_qty), Style::default().fg(BID_COLOR)),
        Span::raw(format!(" {}  │  Total Asks: ", base_asset)),
        Span::styled(format!("{:.4}", total_ask_qty), Style::default().fg(ASK_COLOR)),
        Span::raw(format!(" {}  │  Imbalance: ", base_asset)),
        Span::styled(format!("{}%", imbalance_str), Style::default().fg(imbalance_color)),
    ]);
    let summary_para = Paragraph::new(summary_text)
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(summary_para, chunks[4]);
    
    // Update ID
    let update_id_text = Line::from(vec![
        Span::raw("Update ID: "),
        Span::styled(format!("{}", book.last_update_id), Style::default().fg(Color::DarkGray)),
    ]);
    let update_id_para = Paragraph::new(update_id_text)
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(update_id_para, chunks[5]);
    
    // Price chart with trade bubbles
    render_price_chart(f, chunks[6], trade_history);
    
    // Profiling stats
    render_profiling(f, chunks[7], stats, app.render_time_us);
    
    // Footer
    let footer = Paragraph::new(format!(
        "Book size: {} bids, {} asks │ Press 'q' or Ctrl+C to exit",
        book.bids.len(), book.asks.len()
    ))
    .style(Style::default().fg(Color::DarkGray))
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(footer, chunks[8]);
}

fn render_bids(f: &mut Frame, area: Rect, bids: &[(f64, f64)], cumulative: &[f64], max_cumulative: f64) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(10),     // Depth bar
            Constraint::Length(12),  // Qty
            Constraint::Length(12),  // Price
        ])
        .split(area);
    
    // Build rows for each level
    for i in 0..DEPTH_LEVELS {
        if i >= area.height as usize {
            break;
        }
        
        let y = area.y + i as u16;
        
        if i < bids.len() {
            let (price, qty) = bids[i];
            let cum_qty = cumulative[i];
            
            // Calculate bar widths
            let bar_area_width = chunks[0].width as usize;
            let cum_ratio = if max_cumulative > 0.0 { cum_qty / max_cumulative } else { 0.0 };
            let ind_ratio = if max_cumulative > 0.0 { qty / max_cumulative } else { 0.0 };
            
            let cum_bar_width = (cum_ratio * bar_area_width as f64) as usize;
            let ind_bar_width = (ind_ratio * bar_area_width as f64) as usize;
            let darker_width = cum_bar_width.saturating_sub(ind_bar_width);
            
            // Render depth bar (right-aligned for bids)
            let bar_x = chunks[0].x;
            let bar_y = y;
            
            let empty_width = bar_area_width.saturating_sub(cum_bar_width);
            
            // Empty space
            if empty_width > 0 {
                let empty_span = Span::raw(" ".repeat(empty_width));
                f.render_widget(Paragraph::new(empty_span), Rect::new(bar_x, bar_y, empty_width as u16, 1));
            }
            
            // Darker cumulative part
            if darker_width > 0 {
                let darker_span = Span::styled(
                    "█".repeat(darker_width),
                    Style::default().fg(BID_DIM).bg(BID_DIM)
                );
                f.render_widget(
                    Paragraph::new(darker_span),
                    Rect::new(bar_x + empty_width as u16, bar_y, darker_width as u16, 1)
                );
            }
            
            // Brighter individual part
            if ind_bar_width > 0 {
                let bright_span = Span::styled(
                    "█".repeat(ind_bar_width),
                    Style::default().fg(BID_BRIGHT).bg(BID_COLOR)
                );
                f.render_widget(
                    Paragraph::new(bright_span),
                    Rect::new(bar_x + empty_width as u16 + darker_width as u16, bar_y, ind_bar_width as u16, 1)
                );
            }
            
            // Qty
            let qty_style = if i == 0 {
                Style::default().fg(BID_BRIGHT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(BID_COLOR)
            };
            let qty_text = Paragraph::new(format!("{:>10.4}", qty))
                .style(qty_style)
                .alignment(ratatui::layout::Alignment::Right);
            f.render_widget(qty_text, Rect::new(chunks[1].x, y, chunks[1].width, 1));
            
            // Price
            let price_style = if i == 0 {
                Style::default().fg(BID_BRIGHT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(BID_COLOR)
            };
            let price_text = Paragraph::new(format!("{:>10.4}", price))
                .style(price_style)
                .alignment(ratatui::layout::Alignment::Right);
            f.render_widget(price_text, Rect::new(chunks[2].x, y, chunks[2].width, 1));
        }
    }
}

fn render_asks(f: &mut Frame, area: Rect, asks: &[(f64, f64)], cumulative: &[f64], max_cumulative: f64) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(12),  // Price
            Constraint::Length(12),  // Qty
            Constraint::Min(10),     // Depth bar
        ])
        .split(area);
    
    for i in 0..DEPTH_LEVELS {
        if i >= area.height as usize {
            break;
        }
        
        let y = area.y + i as u16;
        
        if i < asks.len() {
            let (price, qty) = asks[i];
            let cum_qty = cumulative[i];
            
            // Calculate bar widths
            let bar_area_width = chunks[2].width as usize;
            let cum_ratio = if max_cumulative > 0.0 { cum_qty / max_cumulative } else { 0.0 };
            let ind_ratio = if max_cumulative > 0.0 { qty / max_cumulative } else { 0.0 };
            
            let cum_bar_width = (cum_ratio * bar_area_width as f64) as usize;
            let ind_bar_width = (ind_ratio * bar_area_width as f64) as usize;
            let darker_width = cum_bar_width.saturating_sub(ind_bar_width);
            
            // Price
            let price_style = if i == 0 {
                Style::default().fg(ASK_BRIGHT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ASK_COLOR)
            };
            let price_text = Paragraph::new(format!("{:<10.4}", price))
                .style(price_style);
            f.render_widget(price_text, Rect::new(chunks[0].x, y, chunks[0].width, 1));
            
            // Qty
            let qty_style = if i == 0 {
                Style::default().fg(ASK_BRIGHT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ASK_COLOR)
            };
            let qty_text = Paragraph::new(format!("{:<10.4}", qty))
                .style(qty_style);
            f.render_widget(qty_text, Rect::new(chunks[1].x, y, chunks[1].width, 1));
            
            // Render depth bar (left-aligned for asks)
            let bar_x = chunks[2].x;
            let bar_y = y;
            
            // Brighter individual part first
            if ind_bar_width > 0 {
                let bright_span = Span::styled(
                    "█".repeat(ind_bar_width),
                    Style::default().fg(ASK_BRIGHT).bg(ASK_COLOR)
                );
                f.render_widget(
                    Paragraph::new(bright_span),
                    Rect::new(bar_x, bar_y, ind_bar_width as u16, 1)
                );
            }
            
            // Darker cumulative part
            if darker_width > 0 {
                let darker_span = Span::styled(
                    "█".repeat(darker_width),
                    Style::default().fg(ASK_DIM).bg(ASK_DIM)
                );
                f.render_widget(
                    Paragraph::new(darker_span),
                    Rect::new(bar_x + ind_bar_width as u16, bar_y, darker_width as u16, 1)
                );
            }
        }
    }
}

fn render_price_chart(f: &mut Frame, area: Rect, trade_history: &TradeHistory) {
    let chart_block = Block::default()
        .title(" PRICE & TRADES ")
        .title_style(Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    
    let inner = chart_block.inner(area);
    f.render_widget(chart_block, area);
    
    // Reserve space for Y-axis (8 chars) and X-axis (1 row)
    let y_axis_width: u16 = 10;
    let x_axis_height: u16 = 1;
    
    if inner.width < y_axis_width + 20 || inner.height < x_axis_height + 3 {
        return; // Too small to render
    }
    
    let chart_area = Rect::new(
        inner.x + y_axis_width,
        inner.y,
        inner.width - y_axis_width,
        inner.height - x_axis_height,
    );
    
    let chart_width = chart_area.width as usize;
    let chart_height = chart_area.height as usize;
    
    // Get current time
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    
    // Time range: last 60 seconds
    let window_ms = trade_history.window_ms;
    let time_start = now_ms.saturating_sub(window_ms);
    
    // Filter trades in window and find price range
    let trades_in_window: Vec<&Trade> = trade_history.trades.iter()
        .filter(|t| t.timestamp_ms >= time_start)
        .collect();
    
    if trades_in_window.is_empty() {
        let no_data = Paragraph::new("Waiting for trades...")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(no_data, chart_area);
        return;
    }
    
    let min_price = trades_in_window.iter().map(|t| t.price).fold(f64::MAX, |a, b| a.min(b));
    let max_price = trades_in_window.iter().map(|t| t.price).fold(f64::MIN, |a, b| a.max(b));
    
    // Add padding to price range
    let price_range = max_price - min_price;
    let padding = if price_range > 0.0 { price_range * 0.15 } else { max_price * 0.001 };
    let min_price = min_price - padding;
    let max_price = max_price + padding;
    let price_range = max_price - min_price;
    
    // Create a 2D grid for the chart
    let mut grid: Vec<Vec<(char, Color)>> = vec![vec![(' ', Color::Reset); chart_width]; chart_height];
    
    // Build price line from trades (using last traded price)
    // Group trades by X position and take the last price for each
    let mut price_by_x: Vec<Option<f64>> = vec![None; chart_width];
    
    for trade in &trades_in_window {
        let x = ((trade.timestamp_ms - time_start) as f64 / window_ms as f64 * (chart_width - 1) as f64) as usize;
        let x = x.min(chart_width - 1);
        price_by_x[x] = Some(trade.price);
    }
    
    // Fill gaps with previous price (forward fill)
    let mut last_price: Option<f64> = None;
    for i in 0..chart_width {
        if price_by_x[i].is_some() {
            last_price = price_by_x[i];
        } else if last_price.is_some() {
            price_by_x[i] = last_price;
        }
    }
    
    // Draw price line
    let mut prev_y: Option<usize> = None;
    for (x, price_opt) in price_by_x.iter().enumerate() {
        if let Some(price) = price_opt {
            let y = ((max_price - price) / price_range * (chart_height - 1) as f64) as usize;
            let y = y.min(chart_height - 1);
            
            // Connect to previous point
            if let Some(py) = prev_y {
                if py != y {
                    let (y_start, y_end) = if py < y { (py, y) } else { (y, py) };
                    for yy in y_start..=y_end {
                        if grid[yy][x].0 == ' ' {
                            grid[yy][x] = ('│', Color::Cyan);
                        }
                    }
                }
            }
            
            grid[y][x] = ('─', Color::Cyan);
            prev_y = Some(y);
        }
    }
    
    // Find max trade quantity for intensity scaling
    let max_qty = trades_in_window.iter()
        .map(|t| t.quantity)
        .fold(0.0f64, |a, b| a.max(b));
    
    // Draw trade bubbles on top
    for trade in &trades_in_window {
        let x = ((trade.timestamp_ms - time_start) as f64 / window_ms as f64 * (chart_width - 1) as f64) as usize;
        let y = ((max_price - trade.price) / price_range * (chart_height - 1) as f64) as usize;
        
        let x = x.min(chart_width - 1);
        let y = y.min(chart_height - 1);
        
        // Determine bubble character based on size
        let qty_ratio = if max_qty > 0.0 { trade.quantity / max_qty } else { 0.5 };
        let bubble_char = if qty_ratio > 0.6 {
            '●'
        } else if qty_ratio > 0.2 {
            '•'
        } else {
            '·'
        };
        
        // Color based on buy/sell with intensity
        let (base_r, base_g, base_b) = if trade.is_buy {
            (50u8, 255u8, 50u8) // Green for buy
        } else {
            (255u8, 50u8, 50u8) // Red for sell
        };
        
        // Increase intensity for larger trades
        let intensity = 0.6 + qty_ratio * 0.4;
        let r = (base_r as f64 * intensity).min(255.0) as u8;
        let g = (base_g as f64 * intensity).min(255.0) as u8;
        let b = (base_b as f64 * intensity).min(255.0) as u8;
        
        grid[y][x] = (bubble_char, Color::Rgb(r, g, b));
    }
    
    // Render Y-axis (price labels)
    let y_axis_area = Rect::new(inner.x, inner.y, y_axis_width, chart_area.height);
    for row in 0..chart_height {
        let price = max_price - (row as f64 / (chart_height - 1).max(1) as f64) * price_range;
        let label = if row == 0 || row == chart_height - 1 || row == chart_height / 2 {
            format!("{:>9.2}", price)
        } else {
            " ".repeat(y_axis_width as usize)
        };
        let label_para = Paragraph::new(label)
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(label_para, Rect::new(y_axis_area.x, y_axis_area.y + row as u16, y_axis_width, 1));
    }
    
    // Render X-axis (time labels)
    let x_axis_y = inner.y + inner.height - x_axis_height;
    let time_labels = format!(
        "{:<10}{:^width$}{:>10}",
        "-60s",
        "-30s", 
        "now",
        width = (chart_width - 20).max(0)
    );
    let x_axis_para = Paragraph::new(time_labels)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(x_axis_para, Rect::new(chart_area.x, x_axis_y, chart_area.width, 1));
    
    // Render the chart grid
    for (row_idx, row) in grid.iter().enumerate() {
        let mut spans: Vec<Span> = Vec::new();
        
        for &(ch, color) in row {
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(color)
            ));
        }
        
        let line = Line::from(spans);
        f.render_widget(
            Paragraph::new(line),
            Rect::new(chart_area.x, chart_area.y + row_idx as u16, chart_area.width, 1)
        );
    }
}

fn render_profiling(f: &mut Frame, area: Rect, stats: &ProfilingStats, render_time_us: u64) {
    let ws_parse = stats.get_avg(&stats.ws_parse_time, &stats.ws_parse_count);
    let lock_acquire = stats.get_avg(&stats.lock_acquire_time, &stats.lock_acquire_count);
    let ob_update = stats.get_avg(&stats.orderbook_update_time, &stats.orderbook_update_count);
    let aggregation = stats.get_avg(&stats.aggregation_time, &stats.aggregation_count);
    let processing = stats.get_avg(&stats.processing_latency, &stats.processing_count) / 1000.0;
    let network_lat = stats.get_avg(&stats.network_latency, &stats.network_count);
    
    let profiling_block = Block::default()
        .title(" PROFILING STATISTICS ")
        .title_style(Style::default().fg(ACCENT_COLOR).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT_COLOR));
    
    let inner = profiling_block.inner(area);
    f.render_widget(profiling_block, area);
    
    let stats_lines = vec![
        Line::from(vec![
            Span::styled("├─ WS Parse:     ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:>8.1} μs", ws_parse), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("├─ Lock Acquire: ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:>8.1} μs", lock_acquire), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("├─ OB Update:    ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:>8.1} μs", ob_update), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("├─ Aggregation:  ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:>8.1} μs", aggregation), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("├─ Render:       ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:>8.1} ms", render_time_us as f64 / 1000.0), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("├─ Processing:   ", Style::default().fg(Color::Yellow)),
            Span::styled(format!("{:>8.1} ms", processing), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("└─ Network Lat:  ", Style::default().fg(Color::Rgb(255, 200, 0)).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:>8.1} ms", network_lat), Style::default().fg(Color::Rgb(255, 200, 0)).add_modifier(Modifier::BOLD)),
        ]),
    ];
    
    let stats_para = Paragraph::new(stats_lines);
    f.render_widget(stats_para, inner);
}

// ═══════════════════════════════════════════════════════════════════════════
// MAIN
// ═══════════════════════════════════════════════════════════════════════════

fn setup_terminal() -> std::io::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> std::io::Result<()> {
    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Get user configuration before entering TUI mode
    let (symbol, bucket_size) = get_user_config();
    
    let mut app = App::new(symbol.clone(), bucket_size);

    // Shared profiling stats
    let stats = Arc::new(ProfilingStats::default());

    let order_book = Arc::new(RwLock::new(OrderBook::new()));
    let order_book_ws = Arc::clone(&order_book);
    let order_book_render = Arc::clone(&order_book);

    // Trade history for chart (60 second window)
    let trade_history = Arc::new(RwLock::new(TradeHistory::new(60_000)));
    let trade_history_ws = Arc::clone(&trade_history);
    let trade_history_render = Arc::clone(&trade_history);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(WsDepthUpdate, Instant)>();
    let (trade_tx, mut trade_rx) = tokio::sync::mpsc::unbounded_channel::<Trade>();

    // Subscribe to both depth and aggTrade streams
    let ws_url = format!(
        "wss://fstream.binance.com/stream?streams={}@depth@100ms/{}@aggTrade",
        symbol, symbol
    );

    println!("\x1b[33mConnecting to WebSocket...\x1b[0m");

    let (ws_stream, _) = connect_async(&ws_url).await?;
    let (_write, mut read) = ws_stream.split();

    println!("\x1b[32mWebSocket connected! Buffering events...\x1b[0m");

    let tx_clone = tx.clone();
    let trade_tx_clone = trade_tx.clone();
    let stats_ws = Arc::clone(&stats);
    tokio::spawn(async move {
        while let Some(msg_result) = read.next().await {
            let recv_time = Instant::now();
            match msg_result {
                Ok(Message::Text(text)) => {
                    let parse_start = Instant::now();
                    
                    if let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(&text) {
                        // Check stream name to determine message type
                        if let Some(stream) = wrapper.get("stream").and_then(|s| s.as_str()) {
                            if let Some(data) = wrapper.get("data") {
                                if stream.contains("@depth") {
                                    // Depth update message
                                    if let Ok(update) = serde_json::from_value::<WsDepthUpdate>(data.clone()) {
                                        let parse_elapsed = parse_start.elapsed().as_micros() as u64;
                                        stats_ws.record(&stats_ws.ws_parse_time, &stats_ws.ws_parse_count, parse_elapsed);
                                        let _ = tx_clone.send((update, recv_time));
                                    }
                                } else if stream.contains("@aggTrade") {
                                    // Aggregate trade message
                                    if let Ok(agg_trade) = serde_json::from_value::<WsAggTrade>(data.clone()) {
                                        if let (Ok(price), Ok(qty)) = (
                                            agg_trade.price.parse::<f64>(),
                                            agg_trade.quantity.parse::<f64>()
                                        ) {
                                            let trade = Trade {
                                                timestamp_ms: agg_trade.trade_time,
                                                price,
                                                quantity: qty,
                                                is_buy: !agg_trade.is_buyer_maker, // buyer was taker = market buy
                                            };
                                            let _ = trade_tx_clone.send(trade);
                                        }
                                    }
                                }
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

    println!("\x1b[32mEntering live mode with ratatui!\x1b[0m");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Setup terminal with ratatui
    let mut terminal = setup_terminal()?;
    
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_main = Arc::clone(&shutdown);

    let stats_main = Arc::clone(&stats);
    
    let render_interval = Duration::from_millis(50); // ~20 FPS
    let mut last_render = Instant::now();
    
    loop {
        if shutdown_main.load(Ordering::Relaxed) {
            break;
        }

        // Handle keyboard events
        if event::poll(Duration::from_millis(10))? {
            if let Event::Key(key_event) = event::read()? {
                if key_event.modifiers.contains(KeyModifiers::CONTROL)
                    && key_event.code == KeyCode::Char('c')
                {
                    break;
                }
                if key_event.code == KeyCode::Char('q') {
                    break;
                }
            }
        }

        // Process WebSocket updates
        while let Ok((update, recv_time)) = rx.try_recv() {
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
                continue;
            }

            // Lock acquisition and update
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

        // Process trade updates
        while let Ok(trade) = trade_rx.try_recv() {
            let mut history = trade_history_ws.write().await;
            history.add_trade(trade);
        }

        // Render at fixed interval
        if last_render.elapsed() >= render_interval {
            let render_start = Instant::now();
            let book = order_book_render.read().await;
            
            // Sample mid-price for chart
            if let (Some((bid, _)), Some((ask, _))) = (book.best_bid(), book.best_ask()) {
                let mid_price = (bid + ask) / 2.0;
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let mut history = trade_history_render.write().await;
                history.add_price(now_ms, mid_price);
            }
            
            let history = trade_history_render.read().await;
            terminal.draw(|f| ui(f, &app, &book, &stats, &history))?;
            drop(book);
            drop(history);
            app.render_time_us = render_start.elapsed().as_micros() as u64;
            stats.record(&stats.render_time, &stats.render_count, app.render_time_us);
            last_render = Instant::now();
        }

        // Small sleep to prevent busy loop
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    restore_terminal(&mut terminal)?;
    println!("\nGoodbye!");

    Ok(())
}

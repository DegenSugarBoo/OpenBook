use crate::models::{App, OrderBook, ProfilingStats, Trade, TradeHistory};
use ordered_float::OrderedFloat;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::collections::BTreeMap;
use std::time::Instant;

const DEPTH_LEVELS: usize = 10;

const BID_COLOR: Color = Color::Rgb(0, 180, 0);
const BID_BRIGHT: Color = Color::Rgb(50, 255, 50);
const BID_DIM: Color = Color::Rgb(0, 80, 0);
const ASK_COLOR: Color = Color::Rgb(180, 0, 0);
const ASK_BRIGHT: Color = Color::Rgb(255, 50, 50);
const ASK_DIM: Color = Color::Rgb(80, 0, 0);
const HEADER_COLOR: Color = Color::Yellow;

pub fn ui(f: &mut Frame, app: &App, book: &OrderBook, stats: &ProfilingStats, trade_history: &TradeHistory) {
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
            Constraint::Min(8),      // Price chart (expanded)
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
        .style(Style::default().fg(Color::Rgb(140, 140, 140)))
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
        .style(Style::default().fg(Color::Rgb(140, 140, 140)));
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
        Span::styled(format!("{}", book.last_update_id), Style::default().fg(Color::Rgb(140, 140, 140))),
    ]);
    let update_id_para = Paragraph::new(update_id_text)
        .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(update_id_para, chunks[5]);
    
    // Price chart with trade bubbles
    render_price_chart(f, chunks[6], trade_history);
    
    // Footer
    let footer = Paragraph::new(format!(
        "Book size: {} bids, {} asks │ Press 'q' or Ctrl+C to exit",
        book.bids.len(), book.asks.len()
    ))
    .style(Style::default().fg(Color::Rgb(140, 140, 140)))
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(footer, chunks[7]);
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
    
    if inner.width < 30 || inner.height < 6 {
        return; // Too small to render
    }
    
    // Get current time
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    
    // Time range: last 60 seconds
    let window_ms = trade_history.window_ms;
    let time_start = now_ms.saturating_sub(window_ms);
    
    // Filter trades in window
    let trades_in_window: Vec<&Trade> = trade_history.trades.iter()
        .filter(|t| t.timestamp_ms >= time_start)
        .collect();
    
    if trades_in_window.is_empty() {
        let no_data = Paragraph::new("Waiting for trades...")
            .style(Style::default().fg(Color::Rgb(140, 140, 140)))
            .alignment(ratatui::layout::Alignment::Center);
        f.render_widget(no_data, inner);
        return;
    }
    
    // Layout: Y-axis | Price chart | Volume bars | X-axis
    let y_axis_width: u16 = 11;
    let volume_height: u16 = 3;  // Volume bar area
    let x_axis_height: u16 = 1;
    
    let price_chart_height = inner.height.saturating_sub(volume_height + x_axis_height);
    if price_chart_height < 3 {
        return;
    }
    
    let chart_width = (inner.width - y_axis_width) as usize;
    let chart_height = price_chart_height as usize;
    
    // Find price range from trades
    let min_price = trades_in_window.iter().map(|t| t.price).fold(f64::MAX, |a, b| a.min(b));
    let max_price = trades_in_window.iter().map(|t| t.price).fold(f64::MIN, |a, b| a.max(b));
    
    // Dynamic padding based on price volatility
    let price_range = max_price - min_price;
    let padding = if price_range > 0.0 { 
        price_range * 0.2  // 20% padding for better visibility
    } else { 
        max_price * 0.0005  // Minimal padding if flat
    };
    let min_price = min_price - padding;
    let max_price = max_price + padding;
    let price_range = max_price - min_price;
    
    // Create price chart grid
    let mut grid: Vec<Vec<(char, Color)>> = vec![vec![(' ', Color::Reset); chart_width]; chart_height];
    
    // Create volume data per column
    let mut buy_volume: Vec<f64> = vec![0.0; chart_width];
    let mut sell_volume: Vec<f64> = vec![0.0; chart_width];
    let mut price_at_x: Vec<Option<f64>> = vec![None; chart_width];
    
    // Aggregate trades by X position
    for trade in &trades_in_window {
        let x = ((trade.timestamp_ms - time_start) as f64 / window_ms as f64 * (chart_width - 1) as f64) as usize;
        let x = x.min(chart_width - 1);
        
        if trade.is_buy {
            buy_volume[x] += trade.quantity;
        } else {
            sell_volume[x] += trade.quantity;
        }
        price_at_x[x] = Some(trade.price);  // Last price at this x
    }
    
    // Find max volume for scaling
    let max_volume = buy_volume.iter().chain(sell_volume.iter()).fold(0.0f64, |a, &b| a.max(b));
    
    // Forward fill prices for continuous line
    let mut last_price: Option<f64> = None;
    for i in 0..chart_width {
        if price_at_x[i].is_some() {
            last_price = price_at_x[i];
        } else {
            price_at_x[i] = last_price;
        }
    }
    
    // Draw price line with proper connecting characters
    let mut prev_y: Option<usize> = None;
    for (x, price_opt) in price_at_x.iter().enumerate() {
        if let Some(price) = price_opt {
            let y = ((max_price - price) / price_range * (chart_height - 1) as f64) as usize;
            let y = y.min(chart_height - 1);
            
            // Connect to previous point with vertical line if Y changed
            if let Some(py) = prev_y {
                if py != y {
                    let (y_start, y_end) = if py < y { (py, y) } else { (y, py) };
                    for yy in y_start..=y_end {
                        let ch = if yy == y_start || yy == y_end { '│' } else { '│' };
                        if grid[yy][x].0 == ' ' || grid[yy][x].0 == '─' {
                            grid[yy][x] = (ch, Color::Yellow);
                        }
                    }
                }
            }
            
            // Draw horizontal line segment
            grid[y][x] = ('━', Color::Yellow);
            prev_y = Some(y);
        }
    }
    
    // Draw trade markers on top of the line
    for trade in &trades_in_window {
        let x = ((trade.timestamp_ms - time_start) as f64 / window_ms as f64 * (chart_width - 1) as f64) as usize;
        let y = ((max_price - trade.price) / price_range * (chart_height - 1) as f64) as usize;
        
        let x = x.min(chart_width - 1);
        let y = y.min(chart_height - 1);
        
        // Size based on quantity relative to max
        let qty_ratio = if max_volume > 0.0 { trade.quantity / max_volume } else { 0.5 };
        let marker = if qty_ratio > 0.5 { '●' } else if qty_ratio > 0.1 { '◉' } else { '○' };
        
        let color = if trade.is_buy {
            Color::Rgb(50, 255, 100)  // Bright green
        } else {
            Color::Rgb(255, 80, 80)   // Bright red
        };
        
        grid[y][x] = (marker, color);
    }
    
    // === RENDER Y-AXIS ===
    let y_axis_x = inner.x;
    for row in 0..chart_height {
        let price = max_price - (row as f64 / (chart_height - 1).max(1) as f64) * price_range;
        // Show labels at top, middle, and bottom
        let label = if row == 0 || row == chart_height - 1 || row == chart_height / 2 {
            format!("{:>10.2}", price)
        } else {
            " ".repeat(y_axis_width as usize)
        };
        let label_para = Paragraph::new(label).style(Style::default().fg(Color::Rgb(140, 140, 140)));
        f.render_widget(label_para, Rect::new(y_axis_x, inner.y + row as u16, y_axis_width, 1));
    }
    
    // === RENDER PRICE CHART GRID ===
    let chart_x = inner.x + y_axis_width;
    for (row_idx, row) in grid.iter().enumerate() {
        let spans: Vec<Span> = row.iter()
            .map(|&(ch, color)| Span::styled(ch.to_string(), Style::default().fg(color)))
            .collect();
        let line = Line::from(spans);
        f.render_widget(
            Paragraph::new(line),
            Rect::new(chart_x, inner.y + row_idx as u16, chart_width as u16, 1)
        );
    }
    
    // === RENDER VOLUME BARS ===
    let volume_y = inner.y + price_chart_height;
    
    // Draw a separator line
    let separator = "─".repeat(chart_width);
    f.render_widget(
        Paragraph::new(separator).style(Style::default().fg(Color::Rgb(140, 140, 140))),
        Rect::new(chart_x, volume_y, chart_width as u16, 1)
    );
    
    // Volume bars (remaining height - 1 for separator)
    let vol_bar_height = (volume_height - 1) as usize;
    for row in 0..vol_bar_height {
        let mut spans: Vec<Span> = Vec::new();
        
        for x in 0..chart_width {
            let buy_ratio = if max_volume > 0.0 { buy_volume[x] / max_volume } else { 0.0 };
            let sell_ratio = if max_volume > 0.0 { sell_volume[x] / max_volume } else { 0.0 };
            
            // Calculate which level this row represents (from bottom up)
            let level = (vol_bar_height - 1 - row) as f64 / vol_bar_height as f64;
            
            let ch = if buy_ratio > 0.0 && buy_ratio >= level {
                Span::styled("▄", Style::default().fg(Color::Rgb(50, 200, 50)))
            } else if sell_ratio > 0.0 && sell_ratio >= level {
                Span::styled("▄", Style::default().fg(Color::Rgb(200, 50, 50)))
            } else {
                Span::raw(" ")
            };
            spans.push(ch);
        }
        
        let line = Line::from(spans);
        f.render_widget(
            Paragraph::new(line),
            Rect::new(chart_x, volume_y + 1 + row as u16, chart_width as u16, 1)
        );
    }
    
    // Volume axis label
    let vol_label = "VOL";
    f.render_widget(
        Paragraph::new(vol_label).style(Style::default().fg(Color::Rgb(140, 140, 140))),
        Rect::new(y_axis_x + y_axis_width - 4, volume_y + 1, 4, 1)
    );
    
    // === RENDER X-AXIS ===
    let x_axis_y = inner.y + inner.height - x_axis_height;
    let time_labels = format!(
        "{:<12}{:^width$}{:>12}",
        "-60s",
        "-30s", 
        "now",
        width = chart_width.saturating_sub(24)
    );
    let x_axis_para = Paragraph::new(time_labels).style(Style::default().fg(Color::Rgb(140, 140, 140)));
    f.render_widget(x_axis_para, Rect::new(chart_x, x_axis_y, chart_width as u16, 1));
}

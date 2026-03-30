use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
    Frame, Terminal,
};
use rust_decimal::Decimal;
use std::io;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::error;

// ── Shared dashboard state ───────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MarketDisplayState {
    pub symbol: String,
    pub price: f64,
    pub rsi: f64,
    pub macd_hist: f64,
    pub stoch_k: f64,
    pub stoch_d: f64,
    pub ema_dir: String,
    pub obv_status: String,
    pub vwap_dev_pct: f64,
    pub atr_regime: String,
    pub signal_state: String,
    pub signal_score: f64,
}

#[derive(Debug, Clone, Default)]
pub struct PositionDisplay {
    pub market: String,
    pub side: String,
    pub size: Decimal,
    pub entry_price: Decimal,
    pub current_price: Decimal,
    pub unrealized_pnl: Decimal,
}

#[derive(Debug, Clone, Default)]
pub struct DashboardState {
    pub markets: Vec<MarketDisplayState>,
    pub positions: Vec<PositionDisplay>,
    pub balance: Decimal,
    pub deployed: Decimal,
    pub peak_balance: Decimal,
    pub today_pnl: Decimal,
    pub today_wins: u32,
    pub today_losses: u32,
    pub circuit_open: bool,
    pub log_lines: Vec<String>,
}

impl DashboardState {
    pub fn drawdown_pct(&self) -> f64 {
        if self.peak_balance.is_zero() {
            return 0.0;
        }
        ((self.peak_balance - self.balance) / self.peak_balance)
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0)
            * 100.0
    }

    pub fn deployed_pct(&self) -> f64 {
        if self.balance.is_zero() {
            return 0.0;
        }
        (self.deployed / self.balance)
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0)
            * 100.0
    }

    pub fn push_log(&mut self, line: String) {
        self.log_lines.push(line);
        if self.log_lines.len() > 100 {
            self.log_lines.remove(0);
        }
    }
}

pub type SharedDashboard = Arc<RwLock<DashboardState>>;

// ── TuiApp ────────────────────────────────────────────────────────────────────

pub struct TuiApp {
    pub state: SharedDashboard,
}

impl TuiApp {
    pub fn new() -> (Self, SharedDashboard) {
        let state = Arc::new(RwLock::new(DashboardState::default()));
        let app = Self {
            state: state.clone(),
        };
        (app, state)
    }

    /// Blocking run — call from a dedicated thread or tokio::task::spawn_blocking.
    pub fn run(self) -> io::Result<()> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let result = self.event_loop(&mut terminal);

        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;
        result
    }

    fn event_loop(
        &self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> io::Result<()> {
        loop {
            // Grab a snapshot of the state (non-blocking read via try_read)
            let snapshot = self
                .state
                .try_read()
                .map(|s| s.clone())
                .unwrap_or_default();

            terminal.draw(|f| Self::render(f, &snapshot))?;

            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn render(f: &mut Frame, state: &DashboardState) {
        let size = f.size();

        // Top-level: left (60%) | right (40%)
        let top_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(size);

        // Left: market state (top) | status bar (bottom)
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(20), Constraint::Length(3)])
            .split(top_chunks[0]);

        // Right: positions (top) | signal log (bottom)
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(top_chunks[1]);

        Self::render_market_state(f, state, left_chunks[0]);
        Self::render_status_bar(f, state, left_chunks[1]);
        Self::render_positions(f, state, right_chunks[0]);
        Self::render_signal_log(f, state, right_chunks[1]);
    }

    fn render_market_state(f: &mut Frame, state: &DashboardState, area: Rect) {
        let block = Block::default()
            .title(" MARKET STATE ")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::Cyan));

        let mut lines: Vec<Line> = Vec::new();

        for m in &state.markets {
            let regime_color = match m.atr_regime.as_str() {
                "optimal" => Color::Green,
                "extreme" => Color::Red,
                _ => Color::Yellow,
            };

            let rsi_bar = bar_chart(m.rsi, 100.0, 10);
            let score_style = if m.signal_score >= 3.5 {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else if m.signal_score <= -3.5 {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<12} ${:.0}", m.symbol, m.price),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw(format!("  RSI: {:.1}  {rsi_bar}", m.rsi)),
            ]));
            lines.push(Line::from(vec![Span::raw(format!(
                "  MACD hist: {:.4}  Stoch: {:.1}/{:.1}",
                m.macd_hist, m.stoch_k, m.stoch_d
            ))]));
            lines.push(Line::from(vec![
                Span::raw(format!("  EMA: {}  OBV: {}", m.ema_dir, m.obv_status)),
            ]));
            lines.push(Line::from(vec![
                Span::raw(format!("  VWAP dev: {:.2}%  ATR: ", m.vwap_dev_pct)),
                Span::styled(&m.atr_regime, Style::default().fg(regime_color)),
            ]));
            lines.push(Line::from(vec![
                Span::raw(format!("  Signal: {}  Score: ", m.signal_state)),
                Span::styled(format!("{:.1}/7", m.signal_score), score_style),
            ]));
            lines.push(Line::from(Span::raw("")));
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::raw("  Waiting for data...")));
        }

        let paragraph = Paragraph::new(lines).block(block);
        f.render_widget(paragraph, area);
    }

    fn render_status_bar(f: &mut Frame, state: &DashboardState, area: Rect) {
        let circuit_style = if state.circuit_open {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        let circuit_label = if state.circuit_open { "CIRCUIT: OPEN" } else { "CIRCUIT: OK" };

        let pnl_style = if state.today_pnl >= Decimal::ZERO {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        let dd_pct = state.drawdown_pct();

        let text = Line::from(vec![
            Span::styled(circuit_label, circuit_style),
            Span::raw(format!(
                "  |  Balance: ${:.2}  Deployed: ${:.2} ({:.0}%)",
                state.balance,
                state.deployed,
                state.deployed_pct()
            )),
            Span::raw(format!("  |  DD: {dd_pct:.1}%")),
            Span::raw("  |  Today PnL: "),
            Span::styled(format!("{}", state.today_pnl), pnl_style),
            Span::raw(format!(
                "  {}W {}L",
                state.today_wins, state.today_losses
            )),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::DarkGray));
        let paragraph = Paragraph::new(text).block(block);
        f.render_widget(paragraph, area);
    }

    fn render_positions(f: &mut Frame, state: &DashboardState, area: Rect) {
        let block = Block::default()
            .title(" POSITIONS ")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::Yellow));

        let items: Vec<ListItem> = if state.positions.is_empty() {
            vec![ListItem::new("  No open positions")]
        } else {
            state
                .positions
                .iter()
                .map(|p| {
                    let pnl_style = if p.unrealized_pnl >= Decimal::ZERO {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Red)
                    };
                    let sign = if p.unrealized_pnl >= Decimal::ZERO { "+" } else { "" };
                    ListItem::new(Line::from(vec![
                        Span::raw(format!(
                            "  {}-{}  ${:.2}  entry:{:.2}  now:{:.2}  ",
                            p.market, p.side, p.size, p.entry_price, p.current_price
                        )),
                        Span::styled(
                            format!("{sign}{:.2}", p.unrealized_pnl),
                            pnl_style,
                        ),
                    ]))
                })
                .collect()
        };

        let deployed_gauge_pct = (state.deployed_pct() as u16).min(100);
        // Show positions list
        let list = List::new(items).block(block);
        f.render_widget(list, area);
    }

    fn render_signal_log(f: &mut Frame, state: &DashboardState, area: Rect) {
        let block = Block::default()
            .title(" SIGNAL LOG ")
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::DarkGray));

        let visible_lines = area.height.saturating_sub(2) as usize;
        let start = state.log_lines.len().saturating_sub(visible_lines);
        let items: Vec<ListItem> = state.log_lines[start..]
            .iter()
            .map(|l| ListItem::new(Span::raw(l.clone())))
            .collect();

        let list = List::new(items).block(block);
        f.render_widget(list, area);
    }
}

impl Default for TuiApp {
    fn default() -> Self {
        let (app, _) = Self::new();
        app
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn bar_chart(value: f64, max: f64, width: usize) -> String {
    // Guard against NaN, Infinity, or zero/negative max to prevent UB in cast.
    if !value.is_finite() || !max.is_finite() || max < 1e-12 || width == 0 {
        return format!("[{}]", "░".repeat(width));
    }
    let ratio = (value / max).clamp(0.0, 1.0);
    let filled = (ratio * width as f64) as usize; // safe: ratio in [0,1]
    let empty = width - filled;
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(empty)
    )
}

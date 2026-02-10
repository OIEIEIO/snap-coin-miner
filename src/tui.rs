use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, List, ListItem, Paragraph},
};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Write},
    process::exit,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

use crate::stats::StatEvent;
use crate::utils;

const MAX_EVENTS: usize = 100;
const GRAPH_POINTS: usize = 60;

struct TuiState {
    current_hashrate: f64,
    hashrate_history: VecDeque<(f64, f64)>,
    accepted_count: u64,
    rejected_count: u64,
    thread_hashes: Vec<f64>,
    thread_hashes_last_update: Vec<u64>,
    events: VecDeque<String>,
    current_job_id: u64,
    is_pool: bool,
    start_time: Instant,
    share_timestamps: VecDeque<Instant>,
}

impl TuiState {
    fn new(thread_count: usize, is_pool: bool) -> Self {
        if fs::exists(".snap-coin-miner.log").unwrap() {
            fs::remove_file(".snap-coin-miner.log").unwrap();
        }
        Self {
            current_hashrate: 0.0,
            hashrate_history: VecDeque::with_capacity(GRAPH_POINTS),
            accepted_count: 0,
            rejected_count: 0,
            thread_hashes: vec![0.0; thread_count],
            thread_hashes_last_update: vec![0; thread_count],
            events: VecDeque::with_capacity(MAX_EVENTS),
            current_job_id: 0,
            is_pool,
            start_time: Instant::now(),
            share_timestamps: VecDeque::with_capacity(500),
        }
    }

    fn add_event(&mut self, event: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S");
        self.events.push_front(format!("[{}] {}", timestamp, event));
        if self.events.len() > MAX_EVENTS {
            self.events.pop_back();
        }
    }

    fn update_hashrate(&mut self, hashrate: f64) {
        self.current_hashrate = hashrate;
        let elapsed = self.start_time.elapsed().as_secs_f64();
        self.hashrate_history.push_back((elapsed, hashrate));
        if self.hashrate_history.len() > GRAPH_POINTS {
            self.hashrate_history.pop_front();
        }
    }

    fn record_share(&mut self) {
        let now = Instant::now();
        self.share_timestamps.push_back(now);

        let window = Duration::from_secs(600);
        while let Some(front) = self.share_timestamps.front() {
            if now.duration_since(*front) > window {
                self.share_timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    fn estimated_shares_per_hour(&self) -> f64 {
        if self.share_timestamps.len() < 2 {
            return 0.0;
        }

        let first = self.share_timestamps.front().unwrap();
        let last = self.share_timestamps.back().unwrap();

        let elapsed = last.duration_since(*first).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }

        (self.share_timestamps.len() as f64 / elapsed) * 3600.0
    }
}

pub struct TuiManager {
    state: Arc<Mutex<TuiState>>,
}

impl TuiManager {
    pub fn new(thread_count: usize, is_pool: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(TuiState::new(thread_count, is_pool))),
        }
    }

    pub fn start(self, mut stat_rx: mpsc::UnboundedReceiver<StatEvent>) {
        let state = self.state.clone();

        thread::spawn(move || {
            while let Some(event) = stat_rx.blocking_recv() {
                let mut state = state.lock().unwrap();
                match event {
                    StatEvent::HashRate(rate) => {
                        state.update_hashrate(rate);
                    }
                    StatEvent::ThreadHash(thread_id, hashes) => {
                        if thread_id < state.thread_hashes.len() {
                            let now = chrono::Utc::now().timestamp_millis() as u64;
                            state.thread_hashes[thread_id] = hashes as f64
                                / (now - state.thread_hashes_last_update[thread_id]) as f64
                                * 1000.0;
                            state.thread_hashes_last_update[thread_id] = now;
                        }
                    }
                    StatEvent::ShareAccepted => {
                        state.accepted_count += 1;
                        state.record_share();
                    }
                    StatEvent::ShareRejected => {
                        state.rejected_count += 1;
                    }
                    StatEvent::BlockAccepted => {
                        state.accepted_count += 1;
                        state.record_share();
                    }
                    StatEvent::BlockRejected => {
                        state.rejected_count += 1;
                    }
                    StatEvent::NewJob(job_id) => {
                        state.current_job_id = job_id;
                        state.add_event(format!("New job received (ID: {})", job_id));
                    }
                    StatEvent::Event(msg) => {
                        // Log to miner log
                        let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                        let mut log = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(".snap-coin-miner.log")
                            .unwrap();
                        writeln!(log, "[{}] {}", ts, msg).unwrap();
                        state.add_event(msg);
                    }
                }
            }
        });

        let state = self.state;
        thread::spawn(move || {
            if let Err(e) = run_tui(state) {
                eprintln!("TUI error: {}", e);
            }
        });
    }
}

fn run_tui(state: Arc<Mutex<TuiState>>) -> Result<(), io::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &state))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if let KeyCode::Char('q') = key.code {
                    break;
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    exit(0);
}

fn ui(f: &mut Frame, state: &Arc<Mutex<TuiState>>) {
    let state = state.lock().unwrap();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(10),
            Constraint::Min(10),
            Constraint::Length(8),
            Constraint::Min(8),
        ])
        .split(f.area());

    render_header(f, chunks[0], &state);
    render_stats(f, chunks[1], &state);
    render_hashrate_graph(f, chunks[2], &state);
    render_threads(f, chunks[3], &state);
    render_events(f, chunks[4], &state);
}

fn render_header(f: &mut Frame, area: Rect, state: &TuiState) {
    let mode = if state.is_pool { "POOL" } else { "SOLO" };
    let title = format!("SnapCoin Miner - {} Mode - Press 'q' to quit", mode);

    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan))
        .title(title);

    f.render_widget(block, area);
}

fn render_stats(f: &mut Frame, area: Rect, state: &TuiState) {
    let (hashrate, unit) = utils::format_hash_rate(state.current_hashrate);

    let total = state.accepted_count + state.rejected_count;
    let accept_rate = if total > 0 {
        (state.accepted_count as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    let item_name = if state.is_pool { "Shares" } else { "Blocks" };

    let estimated_per_hour = state.estimated_shares_per_hour();

    let stats_text = vec![
        Line::from(vec![
            Span::styled("Hashrate: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{:.2} {}", hashrate, unit),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{}: ", item_name),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("{}", state.accepted_count),
                Style::default().fg(Color::Green),
            ),
            Span::styled(" accepted, ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{}", state.rejected_count),
                Style::default().fg(Color::Red),
            ),
            Span::styled(" rejected", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Accept Rate: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{:.1}%", accept_rate),
                Style::default().fg(if accept_rate > 95.0 {
                    Color::Green
                } else if accept_rate > 80.0 {
                    Color::Yellow
                } else {
                    Color::Red
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Est. ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{}/hour: ", item_name),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("{:.2}", estimated_per_hour),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Current Job: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{}", state.current_job_id),
                Style::default().fg(Color::Yellow),
            ),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Statistics")
        .style(Style::default().fg(Color::White));

    let paragraph = Paragraph::new(stats_text).block(block);
    f.render_widget(paragraph, area);
}

fn render_hashrate_graph(f: &mut Frame, area: Rect, state: &TuiState) {
    let data: Vec<(f64, f64)> = state.hashrate_history.iter().copied().collect();

    if data.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Hashrate History")
            .style(Style::default().fg(Color::White));
        f.render_widget(block, area);
        return;
    }

    let max_hashrate = data
        .iter()
        .map(|(_, h)| *h)
        .fold(0.0f64, |a, b| a.max(b))
        .max(1.0);

    let min_time = data.first().map(|(t, _)| *t).unwrap_or(0.0);
    let max_time = data.last().map(|(t, _)| *t).unwrap_or(1.0);

    let (_display_max, unit) = utils::format_hash_rate(max_hashrate);

    let datasets = vec![
        Dataset::default()
            .name(format!("Hashrate ({})", unit))
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(Color::Cyan))
            .graph_type(GraphType::Line)
            .data(&data),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Hashrate History (60s)")
                .style(Style::default().fg(Color::White)),
        )
        .x_axis(
            Axis::default()
                .title("Time (s)")
                .style(Style::default().fg(Color::Gray))
                .bounds([min_time, max_time]),
        )
        .y_axis(
            Axis::default()
                .title(format!("Rate ({})", unit))
                .style(Style::default().fg(Color::Gray))
                .bounds([0.0, max_hashrate]),
        );

    f.render_widget(chart, area);
}

fn render_threads(f: &mut Frame, area: Rect, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(
            state
                .thread_hashes
                .iter()
                .map(|_| Constraint::Ratio(1, state.thread_hashes.len() as u32))
                .collect::<Vec<_>>(),
        )
        .split(area);

    for (i, &hashes) in state.thread_hashes.iter().enumerate() {
        let (rate, unit) = utils::format_hash_rate(hashes as f64);

        let gauge = Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("T{}", i + 1)),
            )
            .gauge_style(Style::default().fg(Color::Green))
            .label(format!("{:.0}{}", rate, unit))
            .ratio(1.0);

        f.render_widget(gauge, chunks[i]);
    }
}

fn render_events(f: &mut Frame, area: Rect, state: &TuiState) {
    let events: Vec<ListItem> = state
        .events
        .iter()
        .map(|e| {
            let style = if e.contains("accepted") || e.contains("Connected") {
                Style::default().fg(Color::Green)
            } else if e.contains("rejected") || e.contains("error") || e.contains("Error") {
                Style::default().fg(Color::Red)
            } else if e.contains("New job") {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(e.as_str()).style(style)
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Events")
        .style(Style::default().fg(Color::White));

    let list = List::new(events).block(block);
    f.render_widget(list, area);
}

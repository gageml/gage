//! ratatui prototypes — three layouts share one render loop.
//!
//! - `Dense`     : inline viewport, one row per worker.
//! - `TwoLine`   : inline viewport, two rows per worker (label + activity).
//! - `Full`      : alt-screen with a worker table on top and a scrolling
//!   event log on the bottom.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout as RLayout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use gage_scan::event::TaskRef;

use crate::scan::{RunSummary, Setup, run_scan};
use crate::sink::UiEvent;

#[derive(Copy, Clone, Debug)]
pub enum Layout {
    Dense,
    TwoLine,
    Full,
}

struct WorkerView {
    id: usize,
    task: TaskRef,
    started: Instant,
}

struct State {
    total: u64,
    progress: u64,
    workers: Vec<WorkerView>,
    log: VecDeque<String>,
    bytes: u64,
    started: Instant,
}

impl State {
    fn new() -> Self {
        Self {
            total: 0,
            progress: 0,
            workers: Vec::new(),
            log: VecDeque::new(),
            bytes: 0,
            started: Instant::now(),
        }
    }

    fn push_log(&mut self, line: String, cap: usize) {
        while self.log.len() >= cap {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn apply(&mut self, ev: UiEvent) -> bool {
        match ev {
            UiEvent::Status(s) => {
                self.total = s.total as u64;
                self.progress = s.progress as u64;
                let mut seen = Vec::with_capacity(s.workers.len());
                for w in &s.workers {
                    seen.push(w.id);
                    let existing = self.workers.iter_mut().find(|v| v.id == w.id);
                    match (&w.current, existing) {
                        (Some(t), Some(v)) => {
                            if v.task.scanner != t.scanner || v.task.task != t.task {
                                v.task = t.clone();
                                v.started = Instant::now();
                            }
                        }
                        (Some(t), None) => self.workers.push(WorkerView {
                            id: w.id,
                            task: t.clone(),
                            started: Instant::now(),
                        }),
                        (None, _) => {}
                    }
                }
                self.workers.retain(|v| {
                    seen.contains(&v.id)
                        && s.workers
                            .iter()
                            .any(|w| w.id == v.id && w.current.is_some())
                });
                false
            }
            UiEvent::Bytes(n) => {
                self.bytes += n;
                false
            }
            UiEvent::Log(line) => {
                self.push_log(line, 200);
                false
            }
            UiEvent::Warning {
                scanner,
                task,
                message,
            } => {
                self.push_log(format!("warning: {scanner}::{task}: {message}"), 200);
                false
            }
            UiEvent::Failed {
                scanner,
                task,
                message,
            } => {
                self.push_log(format!("error: {scanner}::{task}"), 200);
                for line in message.lines() {
                    self.push_log(format!("  {line}"), 200);
                }
                false
            }
            UiEvent::Finished => true,
        }
    }
}

pub async fn run(setup: Setup, layout: Layout) -> Result<RunSummary> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    let scan_task = tokio::spawn(async move { run_scan(setup, tx).await });

    let mut state = State::new();

    // Set up terminal.
    let mut stdout = io::stdout();
    let backend = CrosstermBackend::new(&mut stdout);
    let viewport = match layout {
        Layout::Full => Viewport::Fullscreen,
        Layout::Dense | Layout::TwoLine => Viewport::Inline(12),
    };
    if matches!(layout, Layout::Full) {
        enable_raw_mode().ok();
        execute!(io::stdout(), EnterAlternateScreen).ok();
    }
    let mut terminal = Terminal::with_options(backend, TerminalOptions { viewport })
        .expect("ratatui terminal init");

    let mut tick = tokio::time::interval(Duration::from_millis(120));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut done = false;
    while !done {
        tokio::select! {
            biased;
            ev = rx.recv() => {
                match ev {
                    Some(e) => { done = state.apply(e); }
                    None => { done = true; }
                }
                // Drain any other events queued without waiting.
                while let Ok(e) = rx.try_recv() {
                    if state.apply(e) { done = true; }
                }
            }
            _ = tick.tick() => {}
        }
        terminal
            .draw(|f| draw(f, &state, layout))
            .expect("ratatui draw");
    }

    // Final paint, then clean up.
    terminal
        .draw(|f| draw(f, &state, layout))
        .expect("ratatui final draw");
    if matches!(layout, Layout::Full) {
        execute!(io::stdout(), LeaveAlternateScreen).ok();
        disable_raw_mode().ok();
    } else {
        terminal.clear().ok();
    }
    drop(terminal);

    scan_task.await.expect("scan task joins")
}

fn draw(f: &mut ratatui::Frame, state: &State, layout: Layout) {
    let area = f.area();
    match layout {
        Layout::Dense => draw_dense(f, area, state, 1),
        Layout::TwoLine => draw_dense(f, area, state, 2),
        Layout::Full => draw_full(f, area, state),
    }
}

fn draw_dense(f: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &State, rows: u16) {
    let header_h = 1;
    let chunks = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_h), Constraint::Min(0)])
        .split(area);

    f.render_widget(header_paragraph(state), chunks[0]);

    let worker_h = rows;
    let mut y = chunks[1].y;
    let max_y = chunks[1].y + chunks[1].height;
    for w in &state.workers {
        if y + worker_h > max_y {
            break;
        }
        let row = ratatui::layout::Rect {
            x: chunks[1].x,
            y,
            width: chunks[1].width,
            height: worker_h,
        };
        draw_worker(f, row, w, state.started.elapsed().as_secs_f64(), rows);
        y += worker_h;
    }
}

fn draw_full(f: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &State) {
    let chunks = RLayout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Min(5),
        ])
        .split(area);

    f.render_widget(header_gauge(state), chunks[0]);

    // Worker table.
    let mut lines: Vec<Line> = Vec::new();
    for w in &state.workers {
        let elapsed = w.started.elapsed().as_secs_f64();
        lines.push(Line::from(vec![
            Span::styled(
                format!("#{:<2} ", w.id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                w.task.scanner.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("::"),
            Span::raw(w.task.task.clone()),
            Span::raw("  "),
            Span::styled(
                format!("{:>5.0}s", elapsed),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Workers ")),
        chunks[1],
    );

    // Log pane.
    let log_lines: Vec<Line> = state
        .log
        .iter()
        .rev()
        .take(chunks[2].height.saturating_sub(2) as usize)
        .map(|l| Line::raw(l.clone()))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    f.render_widget(
        Paragraph::new(log_lines).block(Block::default().borders(Borders::ALL).title(" Events ")),
        chunks[2],
    );
}

fn header_paragraph(state: &State) -> Paragraph<'static> {
    let pct = if state.total > 0 {
        (state.progress as f64 / state.total as f64 * 100.0) as u16
    } else {
        0
    };
    let line = Line::from(vec![
        Span::styled(
            "Scan ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "{}/{} ({}%)  elapsed {:.0}s  bytes {}",
            state.progress,
            state.total,
            pct,
            state.started.elapsed().as_secs_f64(),
            state.bytes,
        )),
    ]);
    Paragraph::new(line)
}

fn header_gauge(state: &State) -> Gauge<'static> {
    let ratio = if state.total > 0 {
        (state.progress as f64 / state.total as f64).min(1.0)
    } else {
        0.0
    };
    Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Scan "))
        .gauge_style(Style::default().fg(Color::Magenta))
        .ratio(ratio)
        .label(format!(
            "{}/{}  {:.0}s  {}B",
            state.progress,
            state.total,
            state.started.elapsed().as_secs_f64(),
            state.bytes,
        ))
}

fn draw_worker(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    w: &WorkerView,
    _scan_elapsed: f64,
    rows: u16,
) {
    let elapsed = w.started.elapsed().as_secs_f64();
    let header = Line::from(vec![
        Span::styled(
            format!("#{:<2} ", w.id),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            w.task.scanner.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("::"),
        Span::raw(w.task.task.clone()),
        Span::raw("  "),
        Span::styled(
            format!("{:>5.0}s", elapsed),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    if rows == 1 {
        f.render_widget(Paragraph::new(header), area);
    } else {
        let chunks = RLayout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        f.render_widget(Paragraph::new(header), chunks[0]);
        let activity = Line::from(vec![Span::styled(
            "    running…",
            Style::default().fg(Color::DarkGray),
        )]);
        f.render_widget(Paragraph::new(activity), chunks[1]);
    }
}

//! The `--verbose` status line: one terminal line redrawn in place with a
//! spinner, the current phase, a gradient progress bar, throughput, ETA and
//! the file being written. A render thread draws it from counters the
//! workers bump, so the copy never waits on the terminal. When stdout is not
//! a terminal, or NO_COLOR is set, progress is printed as plain lines.
//!
//! Without `--verbose` every method prints exactly what the tool printed
//! before the status line existed.

use std::collections::HashSet;
use std::fmt::{self, Write as _};
use std::io::{self, IsTerminal, Write};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// About 15 redraws a second: smooth, and nothing next to the copy.
const FRAME: Duration = Duration::from_millis(66);

/// Without a terminal, one progress line this often.
const PLAIN_INTERVAL: Duration = Duration::from_secs(5);

/// Time constant of the smoothed throughput, in seconds.
const SPEED_TAU: f64 = 1.5;

/// Every frame starts by erasing from the start of the line to the end of
/// the screen, and ends with a carriage return so the hidden cursor rests at
/// the start of the line. If the terminal narrows and rewraps a long frame
/// over several rows, the cursor stays on the first of them, so the next
/// erase removes them all. Moving the cursor up instead would erase log
/// lines on terminals that cut lines rather than rewrap them.
const CLEAR: &str = "\r\x1b[J";
const HIDE_CURSOR: &str = "\x1b[?25l";
/// Erases the line, resets colours and shows the cursor again.
const RESTORE: &str = "\r\x1b[0m\x1b[J\x1b[?25h";
/// What the signal handler writes. A frame is one `write` call, but a
/// signal can still cut that call short in the middle of an escape
/// sequence; CAN (0x18) aborts a sequence in progress and is ignored
/// anywhere else, so what follows is read as fresh sequences.
const SIGNAL_RESTORE: &str = "\x18\r\x1b[0m\x1b[J\x1b[?25h";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Partial cells of the bar, in eighths of a cell.
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Rgb(u8, u8, u8);

/// The bar runs cyan, blue, violet, pink from left to right.
const GRADIENT: [Rgb; 4] = [Rgb(0, 224, 255), Rgb(94, 129, 255), Rgb(178, 102, 255), Rgb(255, 94, 168)];
const TRACK: Rgb = Rgb(48, 50, 66);
const WHITE: Rgb = Rgb(255, 255, 255);
const TEXT: Rgb = Rgb(230, 232, 245);
const DIM: Rgb = Rgb(120, 124, 150);
const GOOD: Rgb = Rgb(80, 230, 140);
const BAD: Rgb = Rgb(255, 85, 95);

/// Set while a live status line owns the terminal, for the panic hook and
/// the signal handler.
static LIVE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Reading,
    Unwrapping,
    Copying,
    Relinking,
}

impl Phase {
    const ALL: [Phase; 4] = [Phase::Reading, Phase::Unwrapping, Phase::Copying, Phase::Relinking];

    fn label(self) -> &'static str {
        match self {
            Phase::Reading => "reading package",
            Phase::Unwrapping => "unwrapping executables",
            Phase::Copying => "copying app0",
            Phase::Relinking => "relinking",
        }
    }

    /// For terminals too narrow for the full label.
    fn short_label(self) -> &'static str {
        match self {
            Phase::Reading => "reading",
            Phase::Unwrapping => "unwrapping",
            Phase::Copying => "copying",
            Phase::Relinking => "relinking",
        }
    }

    /// Phases with a byte count get a filling bar; the others a sweeping one.
    fn determinate(self) -> bool {
        matches!(self, Phase::Unwrapping | Phase::Copying)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// No `--verbose`: print what the tool always printed.
    Quiet,
    /// `--verbose` without a terminal: phase and progress lines, no escapes.
    Plain,
    /// `--verbose` on a terminal: the animated line.
    Live,
}

pub struct Status {
    shared: Arc<Shared>,
    render: Option<JoinHandle<()>>,
    ended: bool,
}

struct Shared {
    mode: Mode,
    steps: u8,
    started: Instant,
    stop: AtomicBool,
    /// Set while a child process owns the terminal: no redraws.
    suspended: AtomicBool,
    phase: AtomicU8,
    step: AtomicU8,
    /// Bumped on every phase change so the throughput meter starts over.
    generation: AtomicU64,
    phase_started_us: AtomicU64,
    /// Time spent in the phases that move bytes, for the average speed.
    moving_us: AtomicU64,
    done: AtomicU64,
    total: AtomicU64,
    files_done: AtomicU64,
    files_total: AtomicU64,
    tally: Tally,
    file: Mutex<String>,
    /// The line on screen. Held while anything writes to the terminal, so a
    /// log line and a redraw never interleave.
    screen: Mutex<String>,
}

impl Status {
    /// `steps` is how many phases this run goes through, for the "2/4" field.
    pub fn new(verbose: bool, steps: u8, started: Instant) -> Status {
        let mode = match verbose {
            false => Mode::Quiet,
            true if live_terminal() => Mode::Live,
            true => Mode::Plain,
        };
        let shared = Arc::new(Shared {
            mode,
            steps,
            started,
            stop: AtomicBool::new(false),
            suspended: AtomicBool::new(false),
            phase: AtomicU8::new(Phase::Reading as u8),
            step: AtomicU8::new(0),
            generation: AtomicU64::new(0),
            phase_started_us: AtomicU64::new(0),
            moving_us: AtomicU64::new(0),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            files_total: AtomicU64::new(0),
            tally: Tally::default(),
            file: Mutex::new(String::new()),
            screen: Mutex::new(String::new()),
        });
        let render = match mode {
            Mode::Quiet => None,
            Mode::Plain | Mode::Live => {
                if mode == Mode::Live {
                    // The live line writes to fd 1 directly, past std's buffer.
                    let _ = io::stdout().flush();
                    install_restore_hooks();
                    shared.write_raw(HIDE_CURSOR);
                    LIVE.store(true, Ordering::SeqCst);
                }
                let shared = Arc::clone(&shared);
                Some(std::thread::spawn(move || render_loop(&shared)))
            }
        };
        Status { shared, render, ended: false }
    }

    pub fn is_verbose(&self) -> bool {
        self.shared.mode != Mode::Quiet
    }

    /// Prints a log line above the status line.
    pub fn line(&self, text: impl fmt::Display) {
        self.shared.print(&text.to_string());
    }

    /// Starts the next phase with its byte and file totals (zero when it
    /// has none).
    pub fn phase(&self, phase: Phase, bytes: u64, files: u64) {
        let s = &self.shared;
        s.close_phase();
        s.total.store(bytes, Ordering::Relaxed);
        s.done.store(0, Ordering::Relaxed);
        s.files_total.store(files, Ordering::Relaxed);
        s.files_done.store(0, Ordering::Relaxed);
        s.file.lock().unwrap().clear();
        s.phase_started_us.store(s.now_us(), Ordering::Relaxed);
        s.phase.store(phase as u8, Ordering::Relaxed);
        let step = s.step.fetch_add(1, Ordering::Relaxed) + 1;
        s.generation.fetch_add(1, Ordering::Release);
        if s.mode == Mode::Plain {
            let mut line = format!("» [{step}/{}] {}", s.steps, phase.label());
            if files > 0 {
                let _ = write!(line, ": {} files, {}", format_count(files), format_bytes(bytes));
            }
            s.print(&line);
        }
    }

    /// Names what the current phase is working on.
    pub fn set_file(&self, name: &str) {
        let mut file = self.shared.file.lock().unwrap();
        file.clear();
        file.push_str(name);
    }

    /// Progress of the current phase.
    pub fn advance(&self, bytes: u64) {
        self.shared.done.fetch_add(bytes, Ordering::Relaxed);
    }

    /// A file of the current phase is finished.
    pub fn file_done(&self) {
        self.shared.files_done.fetch_add(1, Ordering::Relaxed);
    }

    /// A package file was written, for the summary (see `Tally`).
    pub fn wrote(&self, path: &str, size: u64) {
        self.shared.tally.record(path, size);
    }

    /// Runs a command to completion with the terminal handed over to it:
    /// the live line is erased and the cursor shown while it runs, and the
    /// command writes to the inherited stdout and stderr exactly as without
    /// `--verbose`. The line comes back afterwards.
    pub fn run(&self, command: &mut Command) -> io::Result<ExitStatus> {
        if self.shared.mode != Mode::Live {
            return command.status();
        }
        {
            let mut screen = self.shared.screen.lock().unwrap();
            self.shared.suspended.store(true, Ordering::SeqCst);
            // The signal handler has nothing to restore while the child
            // has the terminal.
            LIVE.store(false, Ordering::SeqCst);
            screen.clear();
            write_stdout(RESTORE);
        }
        let result = command.status();
        {
            let _screen = self.shared.screen.lock().unwrap();
            write_stdout(HIDE_CURSOR);
            LIVE.store(true, Ordering::SeqCst);
            self.shared.suspended.store(false, Ordering::SeqCst);
        }
        if let Some(render) = &self.render {
            render.thread().unpark();
        }
        result
    }

    /// Replaces the line with the summary. Without `--verbose` this is the
    /// old "done in" line.
    pub fn finish(mut self) {
        self.end(Outcome::Done);
    }

    /// Removes the line without a summary, for runs that only list.
    pub fn close(mut self) {
        self.end(Outcome::Silent);
    }

    fn end(&mut self, outcome: Outcome) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let s = &self.shared;
        s.stop.store(true, Ordering::Release);
        if let Some(render) = self.render.take() {
            render.thread().unpark();
            let _ = render.join();
        }
        s.close_phase();
        let elapsed = s.started.elapsed();
        let (bytes, files) = s.tally.totals();
        let summary = Summary {
            outcome,
            phase: s.current_phase(),
            done: s.done.load(Ordering::Relaxed),
            total: s.total.load(Ordering::Relaxed),
            bytes,
            files,
            elapsed,
            moving: Duration::from_micros(s.moving_us.load(Ordering::Relaxed)),
        };
        match s.mode {
            Mode::Quiet => {
                if outcome == Outcome::Done {
                    println!("done in {:.1} s", elapsed.as_secs_f64());
                }
            }
            Mode::Plain => {
                if outcome != Outcome::Silent {
                    println!("{}", render_summary(&summary, false));
                }
            }
            Mode::Live => {
                let mut screen = s.screen.lock().unwrap();
                screen.clear();
                let text = match outcome {
                    Outcome::Silent => RESTORE.to_owned(),
                    _ => format!("{RESTORE}{}\n", render_summary(&summary, true)),
                };
                write_stdout(&text);
                LIVE.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// Dropped without `finish`: the run failed, so say where it stopped and
/// give the terminal back before the error is printed.
impl Drop for Status {
    fn drop(&mut self) {
        self.end(Outcome::Failed);
    }
}

impl Shared {
    fn current_phase(&self) -> Phase {
        Phase::ALL[self.phase.load(Ordering::Relaxed) as usize % Phase::ALL.len()]
    }

    fn now_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    /// Adds the time of the phase that is ending to `moving_us` if it moved
    /// bytes.
    fn close_phase(&self) {
        if self.step.load(Ordering::Relaxed) > 0 && self.current_phase().determinate() {
            let now = self.now_us();
            let since = self.phase_started_us.swap(now, Ordering::Relaxed);
            self.moving_us.fetch_add(now.saturating_sub(since), Ordering::Relaxed);
        }
    }

    fn print(&self, text: &str) {
        if self.mode != Mode::Live {
            println!("{text}");
            return;
        }
        let screen = self.screen.lock().unwrap();
        let redraw = if screen.is_empty() { String::new() } else { frame(&screen) };
        write_stdout(&format!("{CLEAR}{text}\n{redraw}"));
    }

    fn write_raw(&self, text: &str) {
        let _screen = self.screen.lock().unwrap();
        write_stdout(text);
    }

    fn view(&self, meter: &mut Meter, now: Instant) -> View {
        let generation = self.generation.load(Ordering::Acquire);
        let phase = self.current_phase();
        let started_us = self.phase_started_us.load(Ordering::Relaxed);
        let elapsed = now.saturating_duration_since(self.started + Duration::from_micros(started_us));
        let done = self.done.load(Ordering::Relaxed);
        View {
            phase,
            step: self.step.load(Ordering::Relaxed),
            steps: self.steps,
            done,
            total: self.total.load(Ordering::Relaxed),
            files_done: self.files_done.load(Ordering::Relaxed),
            files_total: self.files_total.load(Ordering::Relaxed),
            speed: meter.sample(generation, done, now, elapsed),
            elapsed,
            file: self.file.lock().unwrap().clone(),
        }
    }
}

/// What the summary counts: every package file written, once, at its size
/// in the package, however many outputs it went to. Files are keyed by
/// their path relative to source/ or app0/, which is the package path: a
/// game module outside sce_module/ is unwrapped into source/ and also
/// copied into app0/ at the same relative path, and that is one file of
/// work, not two. The CNT metadata in app0/sce_sys/ counts at its own size.
/// So `files` is the number of distinct relative paths under source/ and
/// app0/ once the run is done.
#[derive(Default)]
struct Tally {
    seen: Mutex<HashSet<String>>,
    bytes: AtomicU64,
    files: AtomicU64,
}

impl Tally {
    /// Counts `path` unless it was counted before; returns whether it was new.
    fn record(&self, path: &str, size: u64) -> bool {
        let new = self.seen.lock().unwrap().insert(path.to_owned());
        if new {
            self.bytes.fetch_add(size, Ordering::Relaxed);
            self.files.fetch_add(1, Ordering::Relaxed);
        }
        new
    }

    /// Bytes and files counted.
    fn totals(&self) -> (u64, u64) {
        (self.bytes.load(Ordering::Relaxed), self.files.load(Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Done,
    Failed,
    Silent,
}

fn render_loop(shared: &Shared) {
    let mut meter = Meter::default();
    let mut last_plain = Instant::now();
    let mut tick = 0u64;
    while !shared.stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let view = shared.view(&mut meter, now);
        match shared.mode {
            Mode::Live => {
                let line = render_line(&view, terminal_width(), tick);
                let mut screen = shared.screen.lock().unwrap();
                if !shared.suspended.load(Ordering::SeqCst) {
                    write_stdout(&frame(&line));
                    *screen = line;
                }
            }
            Mode::Plain if view.phase.determinate() && now.duration_since(last_plain) >= PLAIN_INTERVAL => {
                shared.print(&render_plain(&view));
                last_plain = now;
            }
            _ => {}
        }
        tick += 1;
        std::thread::park_timeout(FRAME);
    }
}

/// The bytes that put `line` on screen (see `CLEAR`).
fn frame(line: &str) -> String {
    format!("{CLEAR}{line}\r")
}

/// One `write` call per frame where the platform allows, so a frame reaches
/// the terminal whole as far as other writers can tell.
fn write_stdout(text: &str) {
    sys::write_stdout(text.as_bytes());
}

/// Smoothed throughput: an exponential moving average of the rate between
/// frames, so one slow block does not swing the ETA.
#[derive(Default)]
struct Meter {
    generation: u64,
    last: Option<(Instant, u64)>,
    rate: f64,
}

impl Meter {
    fn sample(&mut self, generation: u64, done: u64, now: Instant, phase_elapsed: Duration) -> f64 {
        if generation != self.generation {
            *self = Meter { generation, ..Meter::default() };
        }
        match self.last {
            Some((at, before)) => {
                let dt = now.duration_since(at).as_secs_f64();
                if dt > 0.0 {
                    let instant = done.saturating_sub(before) as f64 / dt;
                    self.rate += (instant - self.rate) * (1.0 - (-dt / SPEED_TAU).exp());
                }
            }
            None => self.rate = 0.0,
        }
        self.last = Some((now, done));
        // The average is steadier until the average has a few seconds behind it.
        let secs = phase_elapsed.as_secs_f64();
        if secs < 2.0 * SPEED_TAU { if secs > 0.0 { done as f64 / secs } else { 0.0 } } else { self.rate }
    }
}

/// One frame's worth of numbers.
#[derive(Clone, Debug)]
struct View {
    phase: Phase,
    step: u8,
    steps: u8,
    done: u64,
    total: u64,
    files_done: u64,
    files_total: u64,
    /// Bytes per second.
    speed: f64,
    /// Since the phase started.
    elapsed: Duration,
    file: String,
}

impl View {
    fn fraction(&self) -> f64 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64).clamp(0.0, 1.0) }
    }

    fn eta(&self) -> Option<Duration> {
        let left = self.total.saturating_sub(self.done);
        (self.speed > 0.0).then(|| Duration::from_secs_f64((left as f64 / self.speed).min(1e7)))
    }
}

/// A field of the line: kept or dropped as a whole by priority.
struct Field {
    priority: u8,
    width: usize,
    content: Content,
}

enum Content {
    Text(String),
    /// Grows from its fitting width towards the preferred one.
    Bar(Option<f64>),
    /// Takes whatever room is left after the bar, up to its full length.
    File(String),
}

const SEPARATOR: usize = 2;
const BAR_MIN: usize = 14;
const FILE_MIN: usize = 16;

/// The live line for a terminal `cols` wide. Fields drop out lowest
/// priority first until the rest fits with the bar and file name at their
/// minimum widths; the room left then widens the bar, then the file name.
/// It stays one column short of the edge so the terminal never wraps.
fn render_line(view: &View, cols: usize, tick: u64) -> String {
    let room = cols.saturating_sub(1);
    // Padded only within a phase: the bar may shift when the phase changes.
    let label = if cols >= 100 { view.phase.label() } else { view.phase.short_label() };
    let bar_width = if cols >= 140 { 32 } else { (cols / 5).clamp(10, 28) };
    let bar_min = BAR_MIN.min(bar_width);
    let fraction = view.fraction();
    let accent = gradient(fraction);
    let text = |priority: u8, width: usize, text: String| Field { priority, width, content: Content::Text(text) };

    let mut fields = Vec::new();
    let spinner = SPINNER[(tick % SPINNER.len() as u64) as usize];
    fields.push(text(10, 1, format!("{}{BOLD}{spinner}{RESET}", fg(gradient(ping_pong(tick, 48))))));
    fields.push(text(9, label.len(), format!("{}{BOLD}{label}{RESET}", fg(TEXT))));
    let step = format!("{}/{}", view.step, view.steps);
    fields.push(text(1, step.len(), format!("{}{step}{RESET}", fg(DIM))));
    if view.phase.determinate() {
        fields.push(Field { priority: 7, width: bar_min, content: Content::Bar(Some(fraction)) });
        let percent = format!("{:>5.1}%", fraction * 100.0);
        fields.push(text(8, percent.len(), format!("{}{BOLD}{percent}{RESET}", fg(accent))));
        let bytes = format_pair(view.done, view.total);
        let bytes_width = format_pair(view.total, view.total).len();
        fields.push(text(4, bytes_width, format!("{}{bytes:>bytes_width$}{RESET}", fg(TEXT))));
        let speed = format!("{:>10}", format_rate(view.speed));
        fields.push(text(5, speed.len(), format!("{}{speed}{RESET}", fg(GRADIENT[0]))));
        let eta = view.eta().map_or("--:--".to_owned(), format_clock);
        let eta = format!("{eta:>5}");
        fields.push(text(6, 4 + eta.len(), format!("{}ETA {RESET}{}{eta}{RESET}", fg(DIM), fg(TEXT))));
        if view.files_total > 0 {
            let total = format_count(view.files_total);
            let files = format!("{:>w$}/{total} files", format_count(view.files_done), w = total.len());
            fields.push(text(2, files.chars().count(), format!("{}{files}{RESET}", fg(DIM))));
        }
    } else {
        fields.push(Field { priority: 7, width: bar_min, content: Content::Bar(None) });
        let clock = format!("{:>5}", format_clock(view.elapsed));
        fields.push(text(8, clock.len(), format!("{}{clock}{RESET}", fg(TEXT))));
    }
    let name: String = view.file.chars().map(|c| if c.is_control() { '?' } else { c }).collect();
    let name_width = str_width(&name);
    if name_width > 0 {
        fields.push(Field { priority: 3, width: name_width.min(FILE_MIN), content: Content::File(name) });
    }

    let used = |fields: &[Field]| 1 + fields.iter().map(|f| f.width).sum::<usize>() + SEPARATOR * fields.len().saturating_sub(1);
    while used(&fields) > room && !fields.is_empty() {
        let lowest = fields.iter().enumerate().min_by_key(|(_, f)| f.priority).map(|(i, _)| i).unwrap();
        fields.remove(lowest);
    }
    let mut left = room.saturating_sub(used(&fields));
    for field in &mut fields {
        let grow = match field.content {
            Content::Bar(_) => bar_width - field.width,
            _ => continue,
        };
        field.width += grow.min(left);
        left -= grow.min(left);
    }
    for field in &mut fields {
        if matches!(field.content, Content::File(_)) {
            let grow = name_width - field.width;
            field.width += grow.min(left);
            left -= grow.min(left);
        }
    }

    let mut line = String::from(" ");
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        match &field.content {
            Content::Text(text) => line.push_str(text),
            Content::Bar(fraction) => line.push_str(&render_bar(*fraction, field.width, tick)),
            Content::File(name) => line.push_str(&format!("{}{}{RESET}", fg(DIM), truncate_left(name, field.width))),
        }
    }
    line
}

/// A bar `width` cells wide. With a fraction it fills left to right in
/// eighths of a cell, coloured along the gradient, with a soft highlight
/// sweeping across the filled part. Without one, a comet bounces from end
/// to end.
fn render_bar(fraction: Option<f64>, width: usize, tick: u64) -> String {
    let span = width.saturating_sub(1).max(1) as f64;
    let mut bar = bg(TRACK);
    let mut last = None;
    let mut paint = |bar: &mut String, color: Rgb, glyph: char| {
        if last != Some(color) {
            bar.push_str(&fg(color));
            last = Some(color);
        }
        bar.push(glyph);
    };
    match fraction {
        Some(fraction) => {
            let eighths = (fraction.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
            let (full, part) = (eighths / 8, eighths % 8);
            let sweep = (tick as f64 * 0.6) % (full as f64 + 16.0) - 8.0;
            for i in 0..width {
                let color = gradient(i as f64 / span);
                if i < full {
                    let glow = (1.0 - (i as f64 - sweep).abs() / 4.0).max(0.0) * 0.55;
                    paint(&mut bar, mix(color, WHITE, glow), '█');
                } else if i == full && part > 0 {
                    paint(&mut bar, color, EIGHTHS[part]);
                } else {
                    bar.push(' ');
                }
            }
        }
        None => {
            const TAIL: f64 = 7.0;
            let pos = (tick as f64 * 0.7) % (2.0 * span);
            let (head, rightward) = if pos <= span { (pos, true) } else { (2.0 * span - pos, false) };
            for i in 0..width {
                let behind = if rightward { head - i as f64 } else { i as f64 - head };
                if (-0.5..TAIL).contains(&behind) {
                    let strength = (1.0 - behind.max(0.0) / TAIL).powf(1.6);
                    paint(&mut bar, mix(TRACK, gradient(i as f64 / span), strength), '█');
                } else {
                    bar.push(' ');
                }
            }
        }
    }
    bar.push_str(RESET);
    bar
}

/// The `--verbose` progress line without a terminal.
fn render_plain(view: &View) -> String {
    let mut line = format!(
        "  {}  {:.1}%  {}  {}  ETA {}",
        view.phase.label(),
        view.fraction() * 100.0,
        format_pair(view.done, view.total),
        format_rate(view.speed),
        view.eta().map_or("--:--".to_owned(), format_clock)
    );
    if view.files_total > 0 {
        let _ = write!(line, "  {}/{} files", format_count(view.files_done), format_count(view.files_total));
    }
    line
}

struct Summary {
    outcome: Outcome,
    phase: Phase,
    done: u64,
    total: u64,
    bytes: u64,
    files: u64,
    elapsed: Duration,
    /// Time in the phases that move bytes (unwrapping and copying).
    moving: Duration,
}

fn render_summary(summary: &Summary, color: bool) -> String {
    let paint = |c: Rgb, bold: bool, text: &str| match color {
        false => text.to_owned(),
        true => format!("{}{}{text}{RESET}", if bold { BOLD } else { "" }, fg(c)),
    };
    let dot = paint(DIM, false, " · ");
    let secs = summary.elapsed.as_secs_f64();
    let time = if secs < 60.0 { format!("{secs:.1} s") } else { format_clock(summary.elapsed) };
    match summary.outcome {
        Outcome::Failed => {
            let mut line = format!("{} {}", paint(BAD, true, "✗"), paint(BAD, true, &format!("stopped while {}", summary.phase.label())));
            if summary.phase.determinate() && summary.total > 0 {
                line += &dot;
                line += &paint(TEXT, false, &format_pair(summary.done, summary.total));
            }
            line + &dot + &paint(TEXT, false, &time)
        }
        _ => {
            let mut line = format!("{} {}  ", paint(GOOD, true, "✓"), paint(GOOD, true, "done"));
            line += &paint(TEXT, true, &format_bytes(summary.bytes));
            line += &dot;
            line += &paint(TEXT, true, &format!("{} files", format_count(summary.files)));
            line += &dot;
            line += &paint(TEXT, true, &time);
            if let Some(speed) = average_speed(summary.bytes, summary.moving) {
                line += &dot;
                line += &paint(DIM, false, "avg ");
                line += &paint(GRADIENT[0], true, &format_rate(speed));
            }
            line
        }
    }
}

/// Bytes per second over the time spent moving bytes: reading the package
/// and relinking take time that says nothing about the copy speed. None
/// when nothing was moved.
fn average_speed(bytes: u64, moving: Duration) -> Option<f64> {
    let secs = moving.as_secs_f64();
    (bytes > 0 && secs > 0.0).then(|| bytes as f64 / secs)
}

fn fg(c: Rgb) -> String {
    format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2)
}

fn bg(c: Rgb) -> String {
    format!("\x1b[48;2;{};{};{}m", c.0, c.1, c.2)
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let channel = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    Rgb(channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
}

/// The colour at `t` in 0..=1 along the gradient stops.
fn gradient(t: f64) -> Rgb {
    let scaled = t.clamp(0.0, 1.0) * (GRADIENT.len() - 1) as f64;
    let i = (scaled.floor() as usize).min(GRADIENT.len() - 2);
    mix(GRADIENT[i], GRADIENT[i + 1], scaled - i as f64)
}

/// 0 to 1 and back over `period` ticks.
fn ping_pong(tick: u64, period: u64) -> f64 {
    let half = period / 2;
    let t = tick % period;
    (if t <= half { t } else { period - t }) as f64 / half as f64
}

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

fn unit_of(n: f64) -> usize {
    let mut unit = 0;
    let mut n = n;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    unit
}

/// Three significant digits: 9.87, 98.7, 987.
fn three_digits(v: f64) -> String {
    match v {
        v if v < 9.995 => format!("{v:.2}"),
        v if v < 99.95 => format!("{v:.1}"),
        v => format!("{v:.0}"),
    }
}

fn format_bytes(n: u64) -> String {
    let unit = unit_of(n as f64);
    if unit == 0 {
        return format!("{n} B");
    }
    format!("{} {}", three_digits(n as f64 / 1024f64.powi(unit as i32)), UNITS[unit])
}

/// Done and total in the total's unit and precision: "3.27/7.73 GiB".
fn format_pair(done: u64, total: u64) -> String {
    let unit = unit_of(total as f64);
    if unit == 0 {
        return format!("{done}/{total} B");
    }
    let scale = 1024f64.powi(unit as i32);
    let total_text = three_digits(total as f64 / scale);
    let decimals = total_text.split_once('.').map_or(0, |(_, d)| d.len());
    format!("{:.decimals$}/{total_text} {}", done as f64 / scale, UNITS[unit])
}

fn format_rate(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second < 1.0 {
        return "0 B/s".to_owned();
    }
    let unit = unit_of(bytes_per_second);
    if unit == 0 {
        return format!("{bytes_per_second:.0} B/s");
    }
    format!("{} {}/s", three_digits(bytes_per_second / 1024f64.powi(unit as i32)), UNITS[unit])
}

/// m:ss, or h:mm:ss from an hour up.
fn format_clock(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s < 3600 => format!("{}:{:02}", s / 60, s % 60),
        s => format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60),
    }
}

/// 2144 as "2,144".
fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Columns a character takes in a terminal: 0 for controls, combining
/// marks and other zero-width characters, 2 for East Asian wide and
/// fullwidth characters and emoji, 1 otherwise. A compact subset of
/// Unicode's East Asian Width and general categories, enough for file
/// names.
pub fn char_width(c: char) -> usize {
    let c = c as u32;
    const ZERO: [(u32, u32); 14] = [
        (0x0000, 0x001F),
        (0x007F, 0x009F),
        (0x0300, 0x036F),
        (0x0483, 0x0489),
        (0x0591, 0x05BD),
        (0x0610, 0x061A),
        (0x064B, 0x065F),
        (0x1AB0, 0x1AFF),
        (0x1DC0, 0x1DFF),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x2064),
        (0x20D0, 0x20FF),
        (0xFE00, 0xFE0F),
    ];
    const ZERO_MORE: [(u32, u32); 3] = [(0xFE20, 0xFE2F), (0xFEFF, 0xFEFF), (0xE0000, 0xE01EF)];
    const WIDE: [(u32, u32); 21] = [
        (0x1100, 0x115F),
        (0x231A, 0x231B),
        (0x2329, 0x232A),
        (0x23E9, 0x23EC),
        (0x25FD, 0x25FE),
        (0x2614, 0x2615),
        (0x2E80, 0x303E),
        (0x3041, 0x33FF),
        (0x3400, 0x4DBF),
        (0x4E00, 0x9FFF),
        (0xA000, 0xA4CF),
        (0xA960, 0xA97F),
        (0xAC00, 0xD7A3),
        (0xF900, 0xFAFF),
        (0xFE30, 0xFE4F),
        (0xFF00, 0xFF60),
        (0xFFE0, 0xFFE6),
        (0x1F300, 0x1F64F),
        (0x1F680, 0x1F6FF),
        (0x1F900, 0x1F9FF),
        (0x20000, 0x3FFFD),
    ];
    let within = |ranges: &[(u32, u32)]| ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&c));
    if within(&ZERO) || within(&ZERO_MORE) {
        0
    } else if within(&WIDE) {
        2
    } else {
        1
    }
}

/// Columns `text` takes in a terminal.
pub fn str_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// Keeps the end of a path, which names the file: "…/Media/level0". The
/// result is at most `max` columns wide; a wide character that would
/// straddle the limit is left out whole.
fn truncate_left(text: &str, max: usize) -> String {
    if str_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut width = 1; // the ellipsis
    for c in text.chars().rev() {
        let w = char_width(c);
        if width + w > max {
            break;
        }
        width += w;
        kept.push(c);
    }
    // A combining mark cut from its base would land on the ellipsis.
    while kept.last().is_some_and(|&c| char_width(c) == 0) {
        kept.pop();
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// The live line needs a terminal that takes escape codes, and a user who
/// has not asked for none (https://no-color.org).
fn live_terminal() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
    io::stdout().is_terminal() && !no_color && !dumb && sys::enable_escapes()
}

/// Columns of the terminal, read every frame so a resize takes effect.
fn terminal_width() -> usize {
    sys::width().or_else(|| std::env::var("COLUMNS").ok()?.trim().parse().ok()).filter(|&w| w > 0).unwrap_or(80)
}

/// Gives the cursor back if the program panics or is interrupted while the
/// live line has hidden it.
fn install_restore_hooks() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if LIVE.load(Ordering::SeqCst) {
                // The render thread may be in the middle of a frame.
                sys::write_stdout(format!("{SIGNAL_RESTORE}\n").as_bytes());
            }
            previous(info);
        }));
        sys::on_interrupt();
    });
}

#[cfg(unix)]
mod sys {
    use std::ffi::{c_int, c_ulong, c_void};

    use super::{LIVE, SIGNAL_RESTORE};

    #[repr(C)]
    struct Winsize {
        rows: u16,
        cols: u16,
        x_pixels: u16,
        y_pixels: u16,
    }

    #[cfg(all(
        any(target_os = "linux", target_os = "android"),
        not(any(
            target_arch = "powerpc",
            target_arch = "powerpc64",
            target_arch = "mips",
            target_arch = "mips64",
            target_arch = "sparc64"
        ))
    ))]
    const TIOCGWINSZ: Option<c_ulong> = Some(0x5413);
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        all(
            any(target_os = "linux", target_os = "android"),
            any(target_arch = "powerpc", target_arch = "powerpc64", target_arch = "mips", target_arch = "mips64", target_arch = "sparc64")
        )
    ))]
    const TIOCGWINSZ: Option<c_ulong> = Some(0x40087468);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    const TIOCGWINSZ: Option<c_ulong> = None;

    const SIGHUP: c_int = 1;
    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    const SIG_DFL: usize = 0;
    const SIG_IGN: usize = 1;
    const EINTR: i32 = 4;

    // The C library std already links against; no crate needed.
    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
        fn signal(signum: c_int, handler: usize) -> usize;
        fn raise(signum: c_int) -> c_int;
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    }

    pub fn enable_escapes() -> bool {
        true
    }

    /// Writes all of `bytes` to fd 1, in one call unless the terminal takes
    /// less.
    pub fn write_stdout(mut bytes: &[u8]) {
        while !bytes.is_empty() {
            // SAFETY: writes from a live slice.
            let n = unsafe { write(1, bytes.as_ptr().cast(), bytes.len()) };
            match n {
                n if n > 0 => bytes = &bytes[n as usize..],
                n if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(EINTR) => {}
                _ => return,
            }
        }
    }

    pub fn width() -> Option<usize> {
        let mut size = Winsize { rows: 0, cols: 0, x_pixels: 0, y_pixels: 0 };
        // SAFETY: TIOCGWINSZ fills exactly one winsize struct.
        let ok = unsafe { ioctl(1, TIOCGWINSZ?, &mut size as *mut Winsize) } == 0;
        (ok && size.cols > 0).then_some(size.cols as usize)
    }

    /// Ctrl-C, a closed terminal or kill: restore the cursor, then die of
    /// the same signal so the exit status still says so.
    pub fn on_interrupt() {
        extern "C" fn handler(signum: c_int) {
            if LIVE.load(std::sync::atomic::Ordering::SeqCst) {
                // SAFETY: write, signal and raise are async-signal-safe.
                unsafe { write(1, SIGNAL_RESTORE.as_ptr().cast(), SIGNAL_RESTORE.len()) };
            }
            unsafe {
                signal(signum, SIG_DFL);
                raise(signum);
            }
        }
        for signum in [SIGHUP, SIGINT, SIGTERM] {
            // SAFETY: the handler only calls async-signal-safe functions.
            // A signal the parent ignores (nohup, a background job) stays
            // ignored.
            unsafe {
                if signal(signum, handler as extern "C" fn(c_int) as usize) == SIG_IGN {
                    signal(signum, SIG_IGN);
                }
            }
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::io::Write;

    use super::{LIVE, SIGNAL_RESTORE};

    type Handle = *mut c_void;

    #[repr(C)]
    #[derive(Default)]
    struct Coord {
        x: i16,
        y: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct SmallRect {
        left: i16,
        top: i16,
        right: i16,
        bottom: i16,
    }

    #[repr(C)]
    #[derive(Default)]
    struct ScreenBufferInfo {
        size: Coord,
        cursor: Coord,
        attributes: u16,
        window: SmallRect,
        max_window: Coord,
    }

    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> Handle;
        fn GetConsoleMode(console: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(console: Handle, mode: u32) -> i32;
        fn GetConsoleScreenBufferInfo(console: Handle, info: *mut ScreenBufferInfo) -> i32;
        fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
    }

    /// Consoles before Windows 10 print escape codes literally; those get
    /// the plain output.
    pub fn enable_escapes() -> bool {
        // SAFETY: plain console calls on the process's own stdout handle.
        unsafe {
            let console = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut mode = 0;
            GetConsoleMode(console, &mut mode) != 0 && SetConsoleMode(console, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }

    /// The console takes a whole buffer per call through std's stdout.
    pub fn write_stdout(bytes: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }

    pub fn width() -> Option<usize> {
        let mut info = ScreenBufferInfo::default();
        // SAFETY: the call fills one CONSOLE_SCREEN_BUFFER_INFO.
        let ok = unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) } != 0;
        let cols = info.window.right - info.window.left + 1;
        (ok && cols > 0).then_some(cols as usize)
    }

    /// Ctrl-C: restore the cursor, then let the default handler end the
    /// process.
    pub fn on_interrupt() {
        unsafe extern "system" fn handler(_event: u32) -> i32 {
            if LIVE.load(std::sync::atomic::Ordering::SeqCst) {
                write_stdout(SIGNAL_RESTORE.as_bytes());
            }
            0
        }
        // SAFETY: registers a handler that only writes to stdout.
        unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    use std::io::Write;

    pub fn enable_escapes() -> bool {
        true
    }

    pub fn write_stdout(bytes: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }

    pub fn width() -> Option<usize> {
        None
    }

    pub fn on_interrupt() {}
}

/// `--status-demo`: drives the status line through a simulated conversion,
/// to see or record it without a package. With PKG_TO_ANYPS5_DEMO_RELINKER
/// set to a program, the relinking phase runs it the way the real relink
/// does.
pub fn demo() {
    use std::sync::atomic::AtomicUsize;
    use std::thread::sleep;

    const MIB: u64 = 1 << 20;
    let started = Instant::now();
    let status = Status::new(true, 4, started);
    status.phase(Phase::Reading, 0, 0);
    status.set_file("pfs_image.dat");
    status.line("package     demo.pkg");
    status.line("content ID  UP0000-DEMO00000_00-0000000000000000");
    sleep(Duration::from_millis(900));
    status.set_file("naps_pkg_layout.dat");
    sleep(Duration::from_millis(600));
    status.line("files       2144 under uroot, 2.41 GiB");

    let executables: [(&str, u64); 6] = [
        ("eboot.bin", 46 * MIB),
        ("sce_module/libc.prx", 3 * MIB),
        ("sce_module/libSceFios2.prx", MIB),
        ("Media/Modules/GameAssembly.prx", 92 * MIB),
        ("Media/Modules/Il2cppUserAssemblies.prx", 38 * MIB),
        ("Media/Plugins/libfmod.prx", 6 * MIB),
    ];
    status.phase(Phase::Unwrapping, executables.iter().map(|e| e.1).sum(), executables.len() as u64);
    for (path, size) in executables {
        status.set_file(path);
        sleep(Duration::from_millis(80 + size / MIB * 6));
        status.advance(size);
        status.file_done();
        status.wrote(path, size);
        status.line(format!("  source/{path:<40} ELF type 0xfe18, {size} bytes"));
    }

    let mut files: Vec<(String, u64)> =
        (0..40).map(|i| (format!("Media/StreamingAssets/aa/PS5/chunk_{i:03}.bundle"), (13 + i * 7 % 29) * MIB)).collect();
    files.push(("Media/sharedassets0.assets.resS".to_owned(), 520 * MIB));
    files.push(("Media/resources.assets".to_owned(), 310 * MIB));
    files.push(("Media/level0".to_owned(), 180 * MIB));
    files.push(("Media/ローカライズ/日本語テキスト.bundle".to_owned(), 24 * MIB));
    // Also unwrapped above: counted once in the summary.
    files.push(("Media/Modules/GameAssembly.prx".to_owned(), 92 * MIB));
    files.sort_by_key(|f| std::cmp::Reverse(f.1));
    let total: u64 = files.iter().map(|f| f.1).sum();
    status.phase(Phase::Copying, total, files.len() as u64);
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for job in 0..4u64 {
            let (files, next, status) = (&files, &next, &status);
            scope.spawn(move || {
                while let Some((path, size)) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                    status.set_file(path);
                    // 256 KiB blocks at a rate that drifts per thread.
                    let mut left = *size;
                    let mut n = 0u64;
                    while left > 0 {
                        let chunk = left.min(MIB / 4);
                        sleep(Duration::from_micros(1500 + (job * 300) + (n % 7) * 120));
                        status.advance(chunk);
                        left -= chunk;
                        n += 1;
                    }
                    status.file_done();
                    status.wrote(path, *size);
                }
            });
        }
    });
    status.line(format!("  app0/  {} files, {:.2} GiB", files.len(), total as f64 / (1u64 << 30) as f64));

    status.phase(Phase::Relinking, 0, 0);
    status.set_file("source/eboot.bin → app.elf");
    match std::env::var_os("PKG_TO_ANYPS5_DEMO_RELINKER") {
        Some(relinker) => {
            status.line(format!("relink      {}", relinker.to_string_lossy()));
            match status.run(&mut Command::new(&relinker)) {
                Ok(exit) => status.line(format!("  relinker exited ({exit})")),
                Err(err) => status.line(format!("  relinker did not run: {err}")),
            }
            // Long enough to see the line come back.
            sleep(Duration::from_millis(500));
        }
        None => {
            status.line("relink      ~/AnyPS5/build/core/relinker/relinker");
            sleep(Duration::from_millis(1400));
        }
    }
    status.line("  out/app.elf and libs/ (41 system libraries) are ready");
    status.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line as the terminal shows it.
    fn visible(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn copying(done: u64, total: u64) -> View {
        View {
            phase: Phase::Copying,
            step: 3,
            steps: 4,
            done,
            total,
            files_done: 812,
            files_total: 2144,
            speed: 412.0 * 1048576.0,
            elapsed: Duration::from_secs(9),
            file: "Media/StreamingAssets/aa/PS5/chunk_017.bundle".to_owned(),
        }
    }

    #[test]
    fn bytes_have_three_significant_digits() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(123 << 20), "123 MiB");
        assert_eq!(format_bytes(8_300_000_000), "7.73 GiB");
        assert_eq!(format_bytes(10_000 << 20), "9.77 GiB");
        assert_eq!(format_pair(3_511_000_000, 8_300_000_000), "3.27/7.73 GiB");
        assert_eq!(format_pair(0, 186 << 20), "0/186 MiB");
        assert_eq!(format_pair(7, 900), "7/900 B");
        assert_eq!(format_rate(412.0 * 1048576.0), "412 MiB/s");
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(f64::NAN), "0 B/s");
    }

    #[test]
    fn clocks_and_counts() {
        assert_eq!(format_clock(Duration::from_secs(7)), "0:07");
        assert_eq!(format_clock(Duration::from_secs(83)), "1:23");
        assert_eq!(format_clock(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(format_count(7), "7");
        assert_eq!(format_count(2144), "2,144");
        assert_eq!(format_count(1_234_567), "1,234,567");
        let view = copying(1 << 30, 2 << 30);
        assert_eq!(view.eta().map(|d| d.as_secs()), Some(2));
        assert_eq!(View { speed: 0.0, ..view }.eta(), None);
    }

    #[test]
    fn truncation_keeps_the_end_of_the_path() {
        assert_eq!(truncate_left("Media/level0", 20), "Media/level0");
        assert_eq!(truncate_left("Media/level0", 8), "…/level0");
        assert_eq!(truncate_left("Media/level0", 1), "…");
        assert_eq!(truncate_left("Media/level0", 0), "");
        assert_eq!(truncate_left("éèàçù", 3), "…çù");
    }

    #[test]
    fn the_bar_fills_in_eighths() {
        let cells = |fraction: f64| visible(&render_bar(Some(fraction), 10, 0));
        assert_eq!(cells(0.0), " ".repeat(10));
        assert_eq!(cells(1.0), "█".repeat(10));
        assert_eq!(cells(0.5), format!("{}{}", "█".repeat(5), " ".repeat(5)));
        // 0.0125 of 80 eighths is one eighth.
        assert_eq!(cells(0.0125), format!("▏{}", " ".repeat(9)));
        assert_eq!(cells(0.4625), format!("{}▋{}", "█".repeat(4), " ".repeat(5)));
        for tick in 0..100 {
            assert_eq!(visible(&render_bar(None, 10, tick)).chars().count(), 10);
        }
    }

    #[test]
    fn the_gradient_runs_through_its_stops() {
        assert_eq!(gradient(0.0), GRADIENT[0]);
        assert_eq!(gradient(1.0), GRADIENT[3]);
        assert_eq!(gradient(1.0 / 3.0), GRADIENT[1]);
        assert_eq!(gradient(-1.0), GRADIENT[0]);
        assert_eq!(mix(Rgb(0, 0, 0), Rgb(200, 100, 50), 0.5), Rgb(100, 50, 25));
        assert_eq!(ping_pong(0, 48), 0.0);
        assert_eq!(ping_pong(24, 48), 1.0);
    }

    #[test]
    fn the_line_never_reaches_the_last_column() {
        for cols in [10, 20, 40, 60, 80, 100, 120, 160, 300] {
            for view in [copying(3 << 30, 8 << 30), View { phase: Phase::Reading, ..copying(0, 0) }] {
                let line = visible(&render_line(&view, cols, 5));
                assert!(str_width(&line) < cols, "{cols}: {line:?}");
            }
        }
    }

    #[test]
    fn narrow_terminals_drop_the_least_useful_fields_first() {
        let wide = visible(&render_line(&copying(3 << 30, 8 << 30), 160, 0));
        for part in ["copying app0", "3/4", "37.5%", "3.00/8.00 GiB", "412 MiB/s", "ETA", "812/2,144 files", "chunk_017.bundle"] {
            assert!(wide.contains(part), "{part} in {wide:?}");
        }
        let narrow = visible(&render_line(&copying(3 << 30, 8 << 30), 50, 0));
        assert!(narrow.contains("copying") && narrow.contains("37.5%"), "{narrow:?}");
        assert!(!narrow.contains("files") && !narrow.contains("3/4"), "{narrow:?}");
    }

    #[test]
    fn the_summary_says_what_was_done() {
        let summary = Summary {
            outcome: Outcome::Done,
            phase: Phase::Copying,
            done: 0,
            total: 0,
            bytes: 8_300_000_000,
            files: 2150,
            elapsed: Duration::from_millis(24_100),
            moving: Duration::from_secs(20),
        };
        // The average is over the 20 s of copying, not the 24.1 s run.
        assert_eq!(render_summary(&summary, false), "✓ done  7.73 GiB · 2,150 files · 24.1 s · avg 396 MiB/s");
        assert_eq!(visible(&render_summary(&summary, true)), render_summary(&summary, false));
        let nothing_moved = Summary { moving: Duration::ZERO, ..summary };
        assert_eq!(render_summary(&nothing_moved, false), "✓ done  7.73 GiB · 2,150 files · 24.1 s");
        let long = Summary { elapsed: Duration::from_secs(125), ..summary };
        assert!(render_summary(&long, false).contains(" · 2:05 · "));
        let failed = Summary { outcome: Outcome::Failed, done: 3 << 30, total: 8 << 30, ..summary };
        assert_eq!(render_summary(&failed, false), "✗ stopped while copying app0 · 3.00/8.00 GiB · 24.1 s");
        let failed_reading = Summary { outcome: Outcome::Failed, phase: Phase::Reading, ..summary };
        assert_eq!(render_summary(&failed_reading, false), "✗ stopped while reading package · 24.1 s");
    }

    #[test]
    fn the_average_speed_counts_only_time_moving_bytes() {
        assert_eq!(average_speed(1 << 30, Duration::from_secs(2)), Some(512.0 * 1048576.0));
        assert_eq!(average_speed(0, Duration::from_secs(2)), None);
        assert_eq!(average_speed(1 << 30, Duration::ZERO), None);
    }

    #[test]
    fn character_widths() {
        for (c, width) in [
            ('a', 1),
            ('é', 1),
            ('…', 1),
            ('█', 1),
            ('▏', 1),
            ('⠋', 1),
            ('→', 1),
            ('日', 2),
            ('語', 2),
            ('ア', 2),
            ('ぁ', 2),
            ('한', 2),
            ('Ａ', 2),
            ('😀', 2),
            ('🚀', 2),
            ('🦀', 2),
            ('\u{20000}', 2),
            ('\u{0301}', 0),
            ('\u{200B}', 0),
            ('\u{200D}', 0),
            ('\u{FE0F}', 0),
            ('\u{FEFF}', 0),
            ('\x07', 0),
            ('\x1b', 0),
            ('\u{85}', 0),
        ] {
            assert_eq!(char_width(c), width, "{c:?}");
        }
        assert_eq!(str_width("Media/level0"), 12);
        assert_eq!(str_width("日本語.bundle"), 13);
        assert_eq!(str_width("e\u{301}"), 1);
        assert_eq!(str_width(""), 0);
    }

    #[test]
    fn truncation_counts_columns_not_characters() {
        let name = "Media/ローカライズ/日本語テキスト.bundle";
        for max in 0..=str_width(name) + 2 {
            let cut = truncate_left(name, max);
            assert!(str_width(&cut) <= max, "{max}: {cut:?}");
            // Only a wide character that would straddle the limit costs a column.
            if max > 0 && max < str_width(name) {
                assert!(str_width(&cut) >= max - 1, "{max}: {cut:?}");
                assert!(cut.starts_with('…'));
                assert!(name.ends_with(&cut['…'.len_utf8()..]));
            }
        }
        assert_eq!(truncate_left("日本語", 4), "…語");
        assert_eq!(truncate_left("日本語", 5), "…本語");
        assert_eq!(truncate_left("日本語", 6), "日本語");
        // A combining mark is never kept without its base.
        assert_eq!(truncate_left("ae\u{301}x", 2), "…x");
        assert_eq!(truncate_left("abce\u{301}", 3), "…ce\u{301}");
    }

    #[test]
    fn the_tally_counts_each_package_file_once() {
        let tally = Tally::default();
        assert!(tally.record("eboot.bin", 100));
        // Unwrapped into source/ and then copied into app0/.
        assert!(tally.record("Media/Modules/GameAssembly.prx", 40));
        assert!(!tally.record("Media/Modules/GameAssembly.prx", 40));
        assert!(tally.record("Media/level0", 1000));
        assert_eq!(tally.totals(), (1140, 3));
    }

    #[test]
    fn the_tally_is_shared_by_the_copy_threads() {
        let tally = Tally::default();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for i in 0..500 {
                        tally.record(&format!("file{i}"), 2);
                    }
                });
            }
        });
        assert_eq!(tally.totals(), (1000, 500));
    }

    #[test]
    fn only_the_byte_moving_phases_count_towards_the_average() {
        let status = Status::new(false, 4, Instant::now());
        status.phase(Phase::Reading, 0, 0);
        std::thread::sleep(Duration::from_millis(60));
        status.phase(Phase::Copying, 10, 1);
        std::thread::sleep(Duration::from_millis(60));
        status.phase(Phase::Relinking, 0, 0);
        std::thread::sleep(Duration::from_millis(120));
        status.shared.close_phase();
        let moving = Duration::from_micros(status.shared.moving_us.load(Ordering::Relaxed));
        assert!(moving >= Duration::from_millis(60) && moving < Duration::from_millis(170), "{moving:?}");
        status.close();
    }

    #[test]
    fn the_status_counts_what_main_reports() {
        let status = Status::new(false, 3, Instant::now());
        status.phase(Phase::Unwrapping, 30, 2);
        status.advance(10);
        status.file_done();
        status.wrote("eboot.bin", 10);
        status.advance(20);
        status.file_done();
        status.wrote("Media/Modules/a.prx", 20);
        assert_eq!(status.shared.done.load(Ordering::Relaxed), 30);
        assert_eq!(status.shared.files_done.load(Ordering::Relaxed), 2);
        status.phase(Phase::Copying, 520, 2);
        assert_eq!(status.shared.done.load(Ordering::Relaxed), 0);
        status.advance(20);
        status.file_done();
        status.wrote("Media/Modules/a.prx", 20);
        status.advance(500);
        status.file_done();
        status.wrote("Media/level0", 500);
        assert_eq!(status.shared.tally.totals(), (530, 3));
        status.close();
    }

    #[test]
    fn frames_leave_the_cursor_at_the_start_of_the_line() {
        let framed = frame(" x");
        assert!(framed.starts_with("\r\x1b[J"), "{framed:?}");
        assert!(framed.ends_with('\r') && !framed.contains('\n'), "{framed:?}");
        // Interrupted mid-sequence: CAN first, then reset, erase, cursor.
        assert!(SIGNAL_RESTORE.starts_with('\x18'));
        for part in ["\x1b[0m", "\x1b[J", "\x1b[?25h"] {
            assert!(SIGNAL_RESTORE.contains(part) && RESTORE.contains(part), "{part:?}");
        }
    }

    #[test]
    fn wide_file_names_still_fit() {
        let view = View { file: "Media/ローカライズ/日本語テキスト_🚀.bundle".to_owned(), ..copying(3 << 30, 8 << 30) };
        for cols in 10..200 {
            let line = visible(&render_line(&view, cols, 3));
            assert!(str_width(&line) < cols, "{cols}: {line:?}");
        }
        let wide = visible(&render_line(&view, 200, 3));
        assert!(wide.contains("日本語テキスト_🚀.bundle"), "{wide:?}");
    }
}

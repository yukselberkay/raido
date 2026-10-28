use crate::config::Theme;
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// What a terminal pane runs, so a restored session can start it again.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Launch {
    Shell,
    /// An AI CLI from the launcher or `:ai`: a built-in name or an `[ai.<name>]`.
    Agent {
        name: String,
        model: Option<String>,
        /// Conversation id the pane is pinned to, for CLIs that can resume one
        /// (Claude Code); absent in sessions saved before ids were recorded.
        #[serde(default)]
        session: Option<String>,
    },
    /// A `:run` command plugin; restored as a shell rather than run again.
    Command {
        name: String,
    },
}

pub struct TerminalPane {
    pub title: String,
    pub launch: Launch,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub dirty: Arc<AtomicBool>,
    pub exited: Option<String>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    colors: Arc<Mutex<(u32, u32)>>,
    /// Output arrived since the last session snapshot.
    output: Arc<AtomicBool>,
    /// Session snapshot (transcript, working directory), rebuilt only after output.
    saved: Option<(String, Option<std::path::PathBuf>)>,
    /// An AI pane's prompt in flight, from Enter until the agent goes quiet.
    turn: Option<Turn>,
    /// When the last poll saw output.
    last_output: Instant,
    /// Bells the program has rung so far (some agents ring one when done).
    bells: usize,
    /// When the AI pane last finished; taken by [`finished`](Self::finished).
    done: Option<Instant>,
    finished: bool,
}

/// A prompt sent to an AI pane.
struct Turn {
    sent: Instant,
    /// Output kept coming well after Enter: the agent is working, not just echoing.
    working: bool,
}

/// Output this long after Enter means the agent is working on the prompt.
const WORKING: Duration = Duration::from_millis(1500);
/// Agents animate a spinner while they work; this long without output, they're done
/// (or waiting for you).
const QUIET: Duration = Duration::from_millis(2500);

impl TerminalPane {
    pub fn spawn(
        program: &str,
        args: &[String],
        env: &[(String, String)],
        root: &Path,
        title: &str,
        theme: &Theme,
        history: &[u8],
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        // A full path lets Windows run `.cmd` shims (npm-installed CLIs) too.
        let mut command = CommandBuilder::new(crate::platform::resolve(program));
        command.args(args);
        command.cwd(root);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("RAIDO", "1");
        // Any agent can read Raido's manual here, with or without skill support.
        let docs = crate::skill::manual();
        if docs.is_file() {
            command.env("RAIDO_DOCS", docs);
        }
        // `raido FILE` typed here opens FILE in this Raido.
        match crate::remote::address() {
            Some(socket) => command.env(crate::remote::VAR, socket),
            None => command.env_remove(crate::remote::VAR),
        }
        // Claude Code here connects to this Raido, never to an editor Raido runs in.
        match crate::ide::port() {
            Some(port) => {
                command.env(crate::ide::PORT_VAR, port.to_string());
                command.env(crate::ide::ENABLE_VAR, "true");
            }
            None => {
                command.env_remove(crate::ide::PORT_VAR);
                command.env_remove(crate::ide::ENABLE_VAR);
            }
        }
        for (key, value) in env {
            command.env(key, value);
        }
        // Disable inherited multiplexer identities: this is a fresh terminal.
        command.env_remove("TMUX");
        command.env_remove("STY");
        let child = pair.slave.spawn_command(command).with_context(|| {
            format!("Cannot launch {program}; install it and ensure it is on PATH")
        })?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
        // Earlier output (a restored session) goes in before the program starts.
        let mut screen = vt100::Parser::new(24, 80, SCROLLBACK);
        guarded(&mut screen, |screen| screen.process(history));
        let bells = screen.screen().audible_bell_count();
        let parser = Arc::new(Mutex::new(screen));
        let dirty = Arc::new(AtomicBool::new(true));
        let output = Arc::new(AtomicBool::new(true));
        let parser_clone = Arc::clone(&parser);
        let dirty_clone = Arc::clone(&dirty);
        let output_clone = Arc::clone(&output);
        let reply_writer = Arc::clone(&writer);
        let colors = Arc::new(Mutex::new(terminal_colors(theme)));
        let query_colors = Arc::clone(&colors);
        thread::spawn(move || {
            let mut bytes = [0u8; 8192];
            let mut queries = QueryResponder::default();
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let colors = *query_colors.lock().unwrap_or_else(|e| e.into_inner());
                        let replies = if let Ok(mut parser) = parser_clone.lock() {
                            // Process in stream order so cursor reports describe the query location.
                            let mut replies = Vec::new();
                            guarded(&mut parser, |parser| {
                                for byte in &bytes[..count] {
                                    parser.process(&[*byte]);
                                    if let Some(reply) = queries.feed(
                                        *byte,
                                        parser.screen().cursor_position(),
                                        colors,
                                    ) {
                                        replies.extend_from_slice(reply.as_bytes());
                                    }
                                }
                            });
                            replies
                        } else {
                            break;
                        };
                        if !replies.is_empty() {
                            if let Ok(mut writer) = reply_writer.lock() {
                                let _ = writer.write_all(&replies);
                                let _ = writer.flush();
                            }
                        }
                        dirty_clone.store(true, Ordering::Relaxed);
                        output_clone.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
        Ok(Self {
            title: title.into(),
            launch: Launch::Shell,
            parser,
            dirty,
            exited: None,
            master: pair.master,
            writer,
            child,
            size: (24, 80),
            colors,
            output,
            saved: None,
            turn: None,
            last_output: Instant::now(),
            bells,
            done: None,
            finished: false,
        })
    }
    /// The program's process id while it runs.
    pub fn pid(&self) -> Option<u32> {
        if self.exited.is_some() {
            return None;
        }
        self.child.process_id()
    }
    /// Bring the session snapshot up to date; cheap when nothing was printed since.
    pub fn snapshot(&mut self) {
        if self.saved.is_none() || self.output.swap(false, Ordering::Relaxed) {
            self.saved = Some((self.transcript(), self.cwd()));
        }
    }
    /// The last [`snapshot`](Self::snapshot): (transcript, working directory).
    pub fn saved(&self) -> (String, Option<std::path::PathBuf>) {
        self.saved.clone().unwrap_or_default()
    }
    /// Everything the pane holds (scrollback, then screen), the row its cursor is
    /// on, and the first row it shows now (later when scrolled back), for copy mode.
    pub fn history(&self) -> (Vec<Row>, usize, usize) {
        let Ok(mut parser) = self.parser.lock() else {
            return (Vec::new(), 0, 0);
        };
        let viewing = parser.screen().scrollback();
        let cursor = usize::from(parser.screen().cursor_position().0);
        let (rows, history) = rows(&mut parser);
        (rows, history + cursor, history.saturating_sub(viewing))
    }
    /// Screen and scrollback with colors, for saving the session.
    pub fn transcript(&self) -> String {
        match self.parser.lock() {
            Ok(mut parser) => String::from_utf8_lossy(&replay(&mut parser, false)).into_owned(),
            Err(_) => String::new(),
        }
    }
    /// The running program's working directory (where the shell has `cd`'d to).
    pub fn cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child.process_id()?;
        if cfg!(target_os = "linux") {
            return std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        }
        if cfg!(target_os = "macos") {
            // lsof prints the cwd entry as a line starting with `n`.
            let out = std::process::Command::new("lsof")
                .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|l| l.strip_prefix('n'))
                .map(std::path::PathBuf::from);
        }
        None
    }
    pub fn set_theme(&mut self, theme: &Theme) {
        *self.colors.lock().unwrap_or_else(|e| e.into_inner()) = terminal_colors(theme);
    }
    /// Resize to the pane's inside. The terminal itself never gets smaller than
    /// [`MIN_ROWS`] × [`MIN_COLS`] (vt100 trips over itself below that); a pane
    /// drawn smaller shows the part around the cursor.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        let size = (rows.max(MIN_ROWS), cols.max(MIN_COLS));
        if self.size != size {
            // Reflow before the PTY resize so the program's SIGWINCH redraw lands on
            // the new layout.
            if let Ok(mut parser) = self.parser.lock() {
                reflow(&mut parser, size.0, size.1);
            }
            self.master.resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            self.size = size;
        }
        Ok(())
    }
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if self.exited.is_none() {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| anyhow::anyhow!("Terminal writer unavailable"))?;
            writer.write_all(bytes)?;
            writer.flush()?;
        }
        Ok(())
    }
    pub fn key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Enter && self.is_agent() {
            self.turn = Some(Turn {
                sent: Instant::now(),
                working: false,
            });
        }
        if let Ok(mut parser) = self.parser.lock() {
            parser.set_scrollback(0);
        }
        let app = self
            .parser
            .lock()
            .map(|p| p.screen().application_cursor())
            .unwrap_or(false);
        self.send(&encode_key(key, app))
    }
    /// The mouse wheel at (`column`, `row`) inside the pane. Programs that track the
    /// mouse (Claude Code's full-screen mode, htop) get the wheel itself; other
    /// full-screen programs (less, vim) get arrow keys, as terminals do; anything
    /// else scrolls the pane's history three lines.
    pub fn wheel(&mut self, up: bool, column: u16, row: u16) -> Result<()> {
        let (mode, encoding, alternate, application) = {
            let Ok(parser) = self.parser.lock() else {
                return Ok(());
            };
            let screen = parser.screen();
            (
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
                screen.alternate_screen(),
                screen.application_cursor(),
            )
        };
        if mode != vt100::MouseProtocolMode::None {
            let button: u16 = if up { 64 } else { 65 };
            let (x, y) = (column + 1, row + 1);
            let bytes = match encoding {
                vt100::MouseProtocolEncoding::Sgr => {
                    format!("\x1b[<{button};{x};{y}M").into_bytes()
                }
                _ => {
                    let byte = |v: u16| (32 + v.min(223)) as u8;
                    vec![0x1b, b'[', b'M', byte(button), byte(x), byte(y)]
                }
            };
            return self.send(&bytes);
        }
        if alternate {
            let arrow = KeyEvent::new(
                if up { KeyCode::Up } else { KeyCode::Down },
                KeyModifiers::NONE,
            );
            let bytes = encode_key(arrow, application);
            return self.send(&bytes.repeat(3));
        }
        if let Ok(mut parser) = self.parser.lock() {
            let back = parser.screen().scrollback();
            parser.set_scrollback(if up { back + 3 } else { back.saturating_sub(3) });
        }
        Ok(())
    }
    /// Scroll the pane's history the way a terminal window does: PgUp/PgDn (Fn-↑/↓
    /// on a Mac) move half a page; once scrolled back, ↑/↓ move a line, Home/End go
    /// to the top/bottom and Esc returns. Programs that draw their own full screen
    /// (vim, less) or track the mouse (Claude Code's full-screen mode) keep these
    /// keys. False when the key belongs to the program; any other key returns to
    /// the bottom on its way there ([`key`](Self::key)).
    pub fn scroll_key(&mut self, key: KeyEvent) -> bool {
        let Ok(mut parser) = self.parser.lock() else {
            return false;
        };
        let screen = parser.screen();
        if owns_scrolling(screen) || !(key.modifiers - KeyModifiers::SHIFT).is_empty() {
            return false;
        }
        let back = screen.scrollback();
        let half = usize::from(screen.size().0 / 2).max(1);
        let to = match key.code {
            KeyCode::PageUp => back + half,
            KeyCode::PageDown if back > 0 => back.saturating_sub(half),
            KeyCode::Up if back > 0 => back + 1,
            KeyCode::Down if back > 0 => back - 1,
            KeyCode::Home if back > 0 => usize::MAX,
            KeyCode::End | KeyCode::Esc if back > 0 => 0,
            _ => return false,
        };
        parser.set_scrollback(to);
        true
    }
    pub fn paste(&mut self, text: &str) -> Result<()> {
        let bracketed = self
            .parser
            .lock()
            .map(|p| p.screen().bracketed_paste())
            .unwrap_or(false);
        if bracketed {
            self.send(b"\x1b[200~")?;
        }
        self.send(text.as_bytes())?;
        if bracketed {
            self.send(b"\x1b[201~")?;
        }
        Ok(())
    }
    pub fn poll(&mut self) -> bool {
        let mut dirty = self.dirty.swap(false, Ordering::Relaxed);
        if self.exited.is_none() {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(format!("Exited: {status}"));
                dirty = true;
            }
        }
        if self.is_agent() {
            self.watch_turn(dirty, Instant::now());
        }
        dirty
    }
    pub fn is_agent(&self) -> bool {
        matches!(self.launch, Launch::Agent { .. })
    }
    /// An AI pane working: a prompt is in flight or its output still moves (agents
    /// animate a spinner while they work).
    pub fn agent_busy(&self) -> bool {
        self.is_agent()
            && self.exited.is_none()
            && (self.turn.is_some() || self.last_output.elapsed() < QUIET)
    }
    /// Whether an AI pane finished a prompt since the last call: it worked on it
    /// (output kept coming after Enter, as agents animate a spinner) and then went
    /// quiet, or it rang the bell. Stopping to ask you something counts too.
    pub fn finished(&mut self) -> bool {
        std::mem::take(&mut self.finished)
    }
    fn watch_turn(&mut self, output: bool, now: Instant) {
        if output {
            self.last_output = now;
        }
        // A resize rebuilds the parser, and its bell count with it.
        let bells = self
            .parser
            .lock()
            .map(|p| p.screen().audible_bell_count())
            .unwrap_or(0);
        let rang = bells > self.bells;
        self.bells = bells;
        let quiet = now.duration_since(self.last_output) >= QUIET;
        let finished = match &mut self.turn {
            Some(turn) => {
                if output && now.duration_since(turn.sent) >= WORKING {
                    turn.working = true;
                }
                if !turn.working && quiet && now.duration_since(turn.sent) >= WORKING {
                    // Answered at once (a slash command): nothing to wait for.
                    self.turn = None;
                }
                rang || self.turn.as_ref().is_some_and(|t| t.working && quiet)
            }
            // A bell on its own, unless it's the agent's own "done" right after ours.
            None => {
                rang && self
                    .done
                    .is_none_or(|at| now.duration_since(at) >= QUIET * 2)
            }
        };
        if finished {
            self.turn = None;
            self.done = Some(now);
            self.finished = true;
        }
    }
}

const SCROLLBACK: usize = 2_000;

/// Programs that draw their own full screen (vim, less) or track the mouse
/// (Claude Code's full-screen mode) scroll themselves: PgUp/PgDn go to them.
fn owns_scrolling(screen: &vt100::Screen) -> bool {
    screen.alternate_screen() || screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
}

/// Resize `parser` without losing text. vt100's own `set_size` cuts rows to the new
/// width and drops rows that no longer fit, so shrinking a pane (zoom, split) and
/// growing it back would leave the shell output truncated. Instead, rebuild the
/// screen like kitty or iTerm2 do: rejoin soft-wrapped rows into lines and replay
/// them, with their colors, into a parser of the new size, which wraps them again
/// and pushes the overflow into scrollback. Full-screen programs (alternate screen)
/// redraw themselves on SIGWINCH, so they keep the plain resize.
fn reflow(parser: &mut vt100::Parser, rows: u16, cols: u16) {
    if parser.screen().alternate_screen() {
        if !guarded(parser, |parser| parser.set_size(rows, cols)) {
            *parser = vt100::Parser::new(rows, cols, SCROLLBACK);
        }
        return;
    }
    let replay = replay(parser, true);
    let mut fresh = vt100::Parser::new(rows, cols, SCROLLBACK);
    guarded(&mut fresh, |fresh| fresh.process(&replay));
    *parser = fresh;
}

/// The smallest terminal a pane gets: vt100 0.15 panics on ordinary output (a line
/// wrapping, a shell prompt) in terminals of one row or a few columns.
pub const MIN_ROWS: u16 = 3;
pub const MIN_COLS: u16 = 10;

thread_local! {
    /// A guarded vt100 call is running on this thread: its panic is expected.
    static GUARDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` on a pane's parser. vt100 0.15 can panic on some output (a cursor left
/// outside a shrunken screen); then this pane's screen starts afresh at the same
/// size, and the program draws on it again, instead of Raido crashing. Returns
/// whether `f` finished. The panic stays inside the lock, so it isn't poisoned.
fn guarded(parser: &mut vt100::Parser, f: impl FnOnce(&mut vt100::Parser)) -> bool {
    GUARDED.with(|g| g.set(true));
    let finished =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut *parser))).is_ok();
    GUARDED.with(|g| g.set(false));
    if !finished {
        let (rows, cols) = parser.screen().size();
        *parser = vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK);
    }
    finished
}

/// Keep a guarded vt100 panic quiet: no message over the screen, and the panic
/// handler installed before (which restores your terminal and quits) doesn't run.
/// Install after that handler.
pub fn quiet_parser_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !GUARDED.with(|g| g.get()) {
            previous(info);
        }
    }));
}

/// The screen and scrollback as a byte stream that redraws them in any parser:
/// rejoined lines with their colors. With `cursor`, it also puts the cursor back and
/// restores the input modes and pen, for a live resize; without, it is a transcript.
/// One terminal row: its cells, and whether it soft-wraps into the next row.
pub type Row = (Vec<vt100::Cell>, bool);

/// Every row from the oldest scrollback line down to the bottom of the screen, and
/// how many of them are scrollback. The parser's scrollback view is left as it was.
pub(crate) fn rows(parser: &mut vt100::Parser) -> (Vec<Row>, usize) {
    let viewing = parser.screen().scrollback();
    let (height, width) = parser.screen().size();
    // A scrollback offset of `n` makes scrollback row `len - n` the first visible row.
    parser.set_scrollback(usize::MAX);
    let history = parser.screen().scrollback();
    let mut lines: Vec<Row> = Vec::new();
    let read_row = |parser: &vt100::Parser, row: u16| {
        let screen = parser.screen();
        let cells = (0..width).filter_map(|col| screen.cell(row, col).cloned());
        (cells.collect(), screen.row_wrapped(row))
    };
    for offset in (1..=history).rev() {
        parser.set_scrollback(offset);
        lines.push(read_row(parser, 0));
    }
    parser.set_scrollback(0);
    for row in 0..height {
        lines.push(read_row(parser, row));
    }
    parser.set_scrollback(viewing);
    (lines, history)
}

fn replay(parser: &mut vt100::Parser, cursor: bool) -> Vec<u8> {
    let (cursor_row, cursor_col) = parser.screen().cursor_position();
    let (mut lines, history) = rows(parser);
    let keep_cursor = cursor;
    let cursor = history + usize::from(cursor_row);
    // Blank rows below both the cursor and the last text are just unused screen.
    let last = lines
        .iter()
        .rposition(|(cells, _)| cells.iter().any(|c| !blank(c)))
        .map_or(cursor, |row| row.max(cursor));
    lines.truncate(last + 1);

    let mut replay = Vec::new();
    let mut attrs = None;
    for (index, (cells, wrapped)) in lines.iter().enumerate() {
        // A soft-wrapped row is full; only a hard line end has padding to drop.
        let mut end = if *wrapped {
            cells.len()
        } else {
            cells.iter().rposition(|c| !blank(c)).map_or(0, |c| c + 1)
        };
        if index == cursor && keep_cursor {
            end = end.max(usize::from(cursor_col));
        }
        for (col, cell) in cells.iter().enumerate().take(end) {
            if keep_cursor && index == cursor && col == usize::from(cursor_col) {
                replay.extend_from_slice(b"\x1b7");
            }
            if cell.is_wide_continuation() {
                continue;
            }
            let style = sgr(cell);
            if attrs.as_ref() != Some(&style) {
                replay.extend_from_slice(style.as_bytes());
                attrs = Some(style);
            }
            match cell.contents().as_str() {
                "" => replay.push(b' '),
                text => replay.extend_from_slice(text.as_bytes()),
            }
        }
        if keep_cursor && index == cursor && end == usize::from(cursor_col) {
            replay.extend_from_slice(b"\x1b7");
        }
        if !wrapped && index != last {
            replay.extend_from_slice(b"\x1b[0m\r\n");
            attrs = None;
        }
    }
    if keep_cursor {
        replay.extend_from_slice(b"\x1b8");
        // Keep keypad/paste/mouse modes and the pen the program left set.
        replay.extend(parser.screen().input_mode_formatted());
        replay.extend(parser.screen().attributes_formatted());
    } else {
        replay.extend_from_slice(b"\x1b[0m");
    }
    replay
}

fn blank(cell: &vt100::Cell) -> bool {
    !cell.has_contents() && cell.bgcolor() == vt100::Color::Default && !cell.inverse()
}

/// The SGR sequence that sets exactly `cell`'s attributes.
fn sgr(cell: &vt100::Cell) -> String {
    let mut code = String::from("\x1b[0");
    for (on, n) in [
        (cell.bold(), 1),
        (cell.italic(), 3),
        (cell.underline(), 4),
        (cell.inverse(), 7),
    ] {
        if on {
            code.push_str(&format!(";{n}"));
        }
    }
    for (color, base) in [(cell.fgcolor(), 38), (cell.bgcolor(), 48)] {
        match color {
            vt100::Color::Default => {}
            vt100::Color::Idx(n) => code.push_str(&format!(";{base};5;{n}")),
            vt100::Color::Rgb(r, g, b) => code.push_str(&format!(";{base};2;{r};{g};{b}")),
        }
    }
    code.push('m');
    code
}

fn terminal_colors(theme: &Theme) -> (u32, u32) {
    // Theme colors have already been validated at the configuration boundary.
    (
        u32::from_str_radix(&theme.foreground[1..], 16).unwrap_or(0xc9d8cf),
        u32::from_str_radix(&theme.background[1..], 16).unwrap_or(0x0b1215),
    )
}

fn color_reply(code: u8, value: u32) -> String {
    let r = ((value >> 16) & 255) * 257;
    let g = ((value >> 8) & 255) * 257;
    let b = (value & 255) * 257;
    format!("\x1b]{code};rgb:{r:04x}/{g:04x}/{b:04x}\x1b\\")
}

// Only answer bounded terminal capability queries. Output is parsed into cells;
// OSC clipboard, hyperlinks, file writes and host-side control sequences are never forwarded.
#[derive(Default)]
struct QueryResponder {
    sequence: Vec<u8>,
}
impl QueryResponder {
    fn feed(&mut self, byte: u8, cursor: (u16, u16), colors: (u32, u32)) -> Option<String> {
        if byte == 0x1b && self.sequence.last() != Some(&0x1b) {
            if self.sequence.starts_with(b"\x1b]") {
                self.sequence.push(byte);
                return None;
            }
            self.sequence.clear();
            self.sequence.push(byte);
            return None;
        }
        if self.sequence.is_empty() {
            return None;
        }
        self.sequence.push(byte);
        if self.sequence.len() > 128 {
            self.sequence.clear();
            return None;
        }
        if self.sequence.len() < 3 {
            return None;
        }
        let csi = self.sequence.get(1) == Some(&b'[');
        let osc = self.sequence.get(1) == Some(&b']');
        let complete = if csi {
            (0x40..=0x7e).contains(&byte)
        } else if osc {
            byte == 7 || self.sequence.ends_with(b"\x1b\\")
        } else {
            true
        };
        if !complete {
            return None;
        }
        let reply = match self.sequence.as_slice() {
            b"\x1b[6n" => Some(format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1)),
            b"\x1b[5n" => Some("\x1b[0n".into()),
            b"\x1b[c" | b"\x1b[0c" => Some("\x1b[?1;2c".into()),
            b"\x1b[>c" | b"\x1b[>0c" => Some("\x1b[>0;100;0c".into()),
            b"\x1b[?u" => Some("\x1b[?0u".into()),
            b"\x1b]10;?\x07" | b"\x1b]10;?\x1b\\" => Some(color_reply(10, colors.0)),
            b"\x1b]11;?\x07" | b"\x1b]11;?\x1b\\" => Some(color_reply(11, colors.1)),
            _ => None,
        };
        self.sequence.clear();
        reply
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn encode_key(key: KeyEvent, application: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let mut result = Vec::new();
    let seq = match key.code {
        KeyCode::Char(c) => {
            if alt {
                result.push(0x1b);
            }
            if ctrl && c.is_ascii() {
                result.push(match c {
                    ' ' | '@' => 0,
                    '?' => 127,
                    _ => (c.to_ascii_uppercase() as u8) & 0x1f,
                });
            } else {
                let mut bytes = [0; 4];
                result.extend_from_slice(c.encode_utf8(&mut bytes).as_bytes());
            }
            return result;
        }
        KeyCode::Enter => "\r",
        KeyCode::Tab => "\t",
        KeyCode::BackTab => "\x1b[Z",
        KeyCode::Backspace => "\x7f",
        KeyCode::Esc => "\x1b",
        KeyCode::Up
        | KeyCode::Down
        | KeyCode::Right
        | KeyCode::Left
        | KeyCode::Home
        | KeyCode::End => {
            let code = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                KeyCode::Left => 'D',
                KeyCode::Home => 'H',
                _ => 'F',
            };
            let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
            return if modifier > 1 {
                format!("\x1b[1;{modifier}{code}").into_bytes()
            } else {
                format!("\x1b{}{code}", if application { 'O' } else { '[' }).into_bytes()
            };
        }
        KeyCode::Delete => "\x1b[3~",
        KeyCode::Insert => "\x1b[2~",
        KeyCode::PageUp => "\x1b[5~",
        KeyCode::PageDown => "\x1b[6~",
        KeyCode::F(n) => {
            return match n {
                1..=4 => format!("\x1bO{}", (b'P' + n - 1) as char).into_bytes(),
                5..=12 => format!(
                    "\x1b[{}~",
                    [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]
                )
                .into_bytes(),
                _ => Vec::new(),
            }
        }
        _ => "",
    };
    if alt {
        result.push(0x1b);
    }
    result.extend_from_slice(seq.as_bytes());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tiny_panes_and_odd_output_never_crash_raido() {
        quiet_parser_panics();
        // Shell prompts, wide text, full-screen programs, scroll regions and cursor
        // jumps, mixed with resizes down to one cell: what crashed vt100 0.15.
        let pieces: &[&[u8]] = &[
            "➜  helios git:(dev) ✗ ".as_bytes(),
            "日本語🎉".as_bytes(),
            b"text text text ",
            b"\r\n",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[20;70H",
            b"\x1b[2;5r",
            b"\x1b[r",
            b"\x1b[2J",
            b"\x1b[5A",
            b"\x1b[999C",
            b"\x1b[L",
            b"\x1b[M",
            b"\x1b7",
            b"\x1b8",
            b"\x1bM",
            b"\t",
            b"\x1b[4h",
            b"\x1b[1;24r",
            b"\x1b[24;1H",
            b"\x1b[@",
            b"\x1b[S",
        ];
        let mut seed: u64 = 12345;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..3000 {
            let mut parser = vt100::Parser::new(24, 80, SCROLLBACK);
            for _ in 0..12 {
                match next(3) {
                    0 => reflow(&mut parser, 1 + next(5) as u16, 1 + next(8) as u16),
                    1 => reflow(&mut parser, 24, 80),
                    _ => {
                        let piece = pieces[next(pieces.len())];
                        guarded(&mut parser, |p| p.process(piece));
                    }
                }
            }
            // Still a working screen afterwards.
            guarded(&mut parser, |p| p.process(b"ok"));
            let _ = parser.screen().contents();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_pane_smaller_than_the_minimum_keeps_a_usable_terminal() {
        let mut pane = TerminalPane::spawn(
            "/bin/sh",
            &["-c".into(), "exec sleep 30".into()],
            &[],
            Path::new("."),
            "sh",
            &Theme::default(),
            b"",
        )
        .unwrap();
        pane.resize(1, 1).unwrap();
        let size = pane.parser.lock().unwrap().screen().size();
        assert_eq!(size, (MIN_ROWS, MIN_COLS));
    }

    #[test]
    fn key_encoding_preserves_cli_controls() {
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            vec![3]
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            b"\x1bOA"
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL), false),
            b"\x1b[1;5C"
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::NONE), false),
            "λ".as_bytes()
        );
    }
    #[test]
    fn terminal_queries_are_bounded_and_do_not_forward_clipboard_commands() {
        let mut responder = QueryResponder::default();
        let responses: Vec<_> = b"hello\x1b[6n\x1b]52;c;secret\x07\x1b[?u"
            .iter()
            .filter_map(|byte| responder.feed(*byte, (4, 8), terminal_colors(&Theme::default())))
            .collect();
        assert_eq!(responses, vec!["\x1b[5;9R", "\x1b[?0u"]);
    }
    #[test]
    fn color_queries_follow_theme_changes() {
        let mut responder = QueryResponder::default();
        for (name, _) in crate::config::BUNDLED_THEMES {
            let theme = crate::config::load_theme_from(name, None).unwrap();
            let colors = terminal_colors(&theme);
            let replies: Vec<_> = b"\x1b]10;?\x07\x1b]11;?\x1b\\"
                .iter()
                .filter_map(|byte| responder.feed(*byte, (0, 0), colors))
                .collect();
            assert_eq!(
                replies,
                vec![color_reply(10, colors.0), color_reply(11, colors.1)]
            );
        }
        assert_eq!(
            color_reply(11, 0x1e1e1e),
            "\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"
        );
    }
    fn shell_screen(rows: u16, cols: u16) -> vt100::Parser {
        let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK);
        parser.process(b"$ echo\r\n");
        parser.process(&b"x".repeat(70));
        parser.process(b"\r\n\x1b[31mred\x1b[0m line\r\n$ ");
        parser
    }

    #[test]
    fn narrowing_and_widening_a_pane_keeps_its_text() {
        let mut parser = shell_screen(10, 80);
        let before = parser.screen().contents();
        reflow(&mut parser, 10, 20);
        assert_eq!(parser.screen().size(), (10, 20));
        assert!(parser.screen().row_wrapped(1), "long line rewraps");
        reflow(&mut parser, 10, 80);
        assert_eq!(parser.screen().contents(), before);
        assert_eq!(parser.screen().cursor_position(), (3, 2));
        let red = parser.screen().cell(2, 0).unwrap();
        assert_eq!(red.fgcolor(), vt100::Color::Idx(1));
        assert_eq!(
            parser.screen().cell(2, 4).unwrap().fgcolor(),
            vt100::Color::Default
        );
    }

    #[test]
    fn shortening_a_pane_scrolls_lines_back_into_view_when_it_grows() {
        let mut parser = vt100::Parser::new(12, 40, SCROLLBACK);
        for n in 0..10 {
            parser.process(format!("line {n}\r\n").as_bytes());
        }
        let before = parser.screen().contents();
        reflow(&mut parser, 4, 40);
        assert!(parser.screen().contents().starts_with("line 7"));
        reflow(&mut parser, 12, 40);
        assert_eq!(parser.screen().contents(), before);
        assert_eq!(parser.screen().cursor_position(), (10, 0));
    }

    #[test]
    fn full_screen_programs_keep_a_plain_resize() {
        let mut parser = vt100::Parser::new(10, 40, SCROLLBACK);
        parser.process(b"\x1b[?1049hfull screen");
        reflow(&mut parser, 5, 20);
        assert!(parser.screen().alternate_screen());
        assert_eq!(parser.screen().size(), (5, 20));
    }

    #[cfg(unix)]
    #[test]
    fn ai_panes_finish_when_they_go_quiet_after_working_or_ring() {
        let spawn = |history: &[u8]| {
            let mut pane = TerminalPane::spawn(
                "/bin/sh",
                &["-c".into(), "exec sleep 30".into()],
                &[],
                Path::new("/tmp"),
                "claude",
                &Theme::default(),
                history,
            )
            .unwrap();
            pane.launch = Launch::Agent {
                name: "claude".into(),
                model: None,
                session: None,
            };
            pane
        };
        let ms = Duration::from_millis;
        let mut pane = spawn(b"");
        let start = Instant::now();
        let at = |n| start + ms(n);
        // A prompt the agent works on: output keeps coming, then stops.
        pane.turn = Some(Turn {
            sent: start,
            working: false,
        });
        for n in (0..=4000).step_by(100) {
            pane.watch_turn(true, at(n));
        }
        pane.watch_turn(false, at(5000));
        assert!(!pane.finished(), "still inside the quiet period");
        pane.watch_turn(false, at(6600));
        assert!(pane.finished(), "worked, then quiet");
        assert!(!pane.finished(), "reported once");
        assert!(pane.turn.is_none());
        // Answered at once (a slash command, the echo of Enter): no sound.
        pane.turn = Some(Turn {
            sent: at(10_000),
            working: false,
        });
        pane.watch_turn(true, at(10_050));
        pane.watch_turn(false, at(13_000));
        pane.watch_turn(false, at(20_000));
        assert!(!pane.finished());
        assert!(pane.turn.is_none());
        // Output without a prompt (a resize redraw) is not a finished prompt.
        pane.watch_turn(true, at(30_000));
        pane.watch_turn(false, at(40_000));
        assert!(!pane.finished());
        // An agent that rings the bell when it's done.
        pane.parser.lock().unwrap().process(b"\x07");
        pane.watch_turn(true, at(50_000));
        assert!(pane.finished(), "a bell finishes too");
        // Bells already in a restored session don't ring again; shells never ring.
        let mut restored = spawn(b"old\x07output");
        restored.watch_turn(true, Instant::now());
        assert!(!restored.finished());
        restored.launch = Launch::Shell;
        restored.parser.lock().unwrap().process(b"\x07");
        restored.poll();
        assert!(!restored.finished());
    }
    #[cfg(unix)]
    #[test]
    fn real_pty_receives_input_and_resizes() {
        let mut terminal = TerminalPane::spawn(
            "/bin/sh",
            &[
                "-c".into(),
                "read value; printf 'received:%s' \"$value\"".into(),
            ],
            &[],
            Path::new("/tmp"),
            "test",
            &Theme::default(),
            b"earlier output\r\n",
        )
        .unwrap();
        assert!(terminal.transcript().contains("earlier output"));
        terminal.resize(12, 42).unwrap();
        terminal.send(b"hello\n").unwrap();
        for _ in 0..50 {
            if terminal
                .parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("received:hello")
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("PTY did not receive shell output");
    }
}

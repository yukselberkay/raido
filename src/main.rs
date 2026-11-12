mod agent_model;
mod app;
mod clipboard;
mod config;
mod conversations;
mod copymode;
mod editor;
mod fonts;
mod gitgraph;
mod grab;
mod highlight;
mod icons;
mod ide;
mod json;
mod language;
mod layout;
mod link;
mod local_ai;
mod lsp;
mod markdown;
mod merge;
mod minimap;
mod model_server;
mod nmap;
mod omarchy;
mod outline;
mod platform;
mod pyenv;
mod quickdiff;
mod remote;
mod review;
mod screenshot;
mod search;
mod session;
#[cfg(test)]
mod shots;
mod skill;
mod sound;
mod summary;
mod terminal;
mod textsize;
mod ui;
mod vscode;
mod websocket;
mod workspace;

use anyhow::{bail, Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

/// Raido's version, from `Cargo.toml` — the one place it is set. Everything that
/// shows or reports a version uses this, never a version written out by hand.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The shortest time between two frames (120 a second): output that comes faster
/// is drawn together.
const FRAME: Duration = Duration::from_millis(8);
/// How long to wait for input while something just happened (a key, a terminal's
/// output): its echo or the next lines show within a few milliseconds. Windows'
/// timer rounds this up to about 16 ms.
const LIVELY: Duration = Duration::from_millis(4);
/// How long it stays lively after the last thing that happened.
const SETTLE: Duration = Duration::from_millis(250);
/// How long to wait while nothing happens; terminals that start printing again are
/// noticed within this.
const IDLE: Duration = Duration::from_millis(33);
/// Input handled before the next frame at most, so a flood still shows.
const BATCH: usize = 512;

/// Point the terminal's own default background/foreground (OSC 11/10) at the theme,
/// so a light terminal profile can't show through the window padding or any cell
/// Raido leaves at the default color. Terminals without OSC 10/11 ignore it.
fn set_terminal_colors(theme: &config::Theme) -> io::Result<()> {
    let hex = |value: &str| match config::color(value) {
        ratatui::style::Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    };
    let (Some(background), Some(foreground)) = (hex(&theme.background), hex(&theme.foreground))
    else {
        reset_terminal_colors();
        return Ok(());
    };
    let mut out = io::stdout();
    write!(out, "\x1b]11;{background}\x07\x1b]10;{foreground}\x07")?;
    out.flush()
}

/// Hand the terminal back its profile colors (OSC 111/110).
fn reset_terminal_colors() {
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]111\x07\x1b]110\x07{MOUSE_OFF}");
    let _ = out.flush();
}

/// Mouse clicks and the wheel, reported in SGR form; motion only while a button is
/// held (dragging the minimap).
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        reset_terminal_colors();
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            LeaveAlternateScreen,
            Show
        );
    }
}

fn main() -> Result<()> {
    let mut path = None;
    let mut demo = false;
    let mut fresh = false;
    let mut nested = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("Raido — all systems in view\n\nUsage: raido [FILE|DIRECTORY] [--demo] [--fresh]\n\n  --demo     Open an editor, AI launcher and shell\n  --fresh    Start without reopening the project's last session\n  --nested   Inside a Raido terminal, start a separate Raido instead of opening FILE there\n  --install-skill  Teach your AI agents about Raido (Agent Skills; also automatic)\n  --remove-skill   Remove that skill again\n  --keys     Show the keys and clicks your terminal sends to Raido (to check shortcuts)\n  --version  Print version\n\nKeys: F1 help · Ctrl-e files · Ctrl-b panes · Ctrl-s save · Ctrl-q quit\nConfig: {}",config::config_dir().display());
                return Ok(());
            }
            "-V" | "--version" => {
                println!("raido {VERSION}");
                return Ok(());
            }
            "--demo" => demo = true,
            "--install-skill" => {
                for place in skill::install()? {
                    println!("{}", place.display());
                }
                println!(
                    "Raido skill installed for your AI agents (from {})",
                    skill::source().display()
                );
                return Ok(());
            }
            "--remove-skill" => {
                println!("Removed the Raido skill from {} place(s)", skill::remove()?);
                return Ok(());
            }
            "--keys" => return show_keys(),
            "--fresh" => fresh = true,
            "--nested" => nested = true,
            _ if arg.starts_with('-') => bail!("Unknown option: {arg}"),
            _ => {
                if path.is_some() {
                    bail!("Pass one file or project directory");
                }
                path = Some(PathBuf::from(arg));
            }
        }
    }
    let cwd = std::env::current_dir()?;
    // Typed in one of Raido's own terminals: open it in that Raido, don't nest one.
    if let (Some(socket), false) = (std::env::var_os(remote::VAR), nested || demo) {
        let target = match &path {
            Some(p) if p.is_absolute() => p.clone(),
            Some(p) => cwd.join(p),
            None => cwd.clone(),
        };
        match remote::send(std::path::Path::new(&socket), &target) {
            Ok(remote::Reply::Opened(message)) => {
                println!("{message}");
                return Ok(());
            }
            Ok(remote::Reply::Failed(error)) => bail!("{error}"),
            // Another project, or that Raido is gone: start here as usual.
            Ok(remote::Reply::Elsewhere) | Err(_) => {}
        }
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Raido needs an interactive terminal. Run `raido` in your terminal.");
    }
    let path = path
        .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
        .unwrap_or(cwd);
    let (root, initial) = if path.is_dir() {
        let root = path.canonicalize()?;
        let initial = if demo {
            let example = root.join("examples/beacon.rs");
            if example.exists() {
                Some(example)
            } else {
                None
            }
        } else {
            None
        };
        (root, initial)
    } else {
        let parent = path
            .parent()
            .context("File has no parent")?
            .canonicalize()?;
        let root = parent
            .ancestors()
            .find(|p| p.join(".git").exists())
            .unwrap_or(&parent)
            .to_path_buf();
        (root, Some(path))
    };
    // Settings and themes saved under Raido's old name, Odin, carry over.
    config::adopt_odin_config();
    // A saved session reopens first; a file named on the command line opens on top.
    let restore = !demo && !fresh;
    // Listen before any terminal pane starts, so every pane knows where Raido is.
    let server = remote::listen().ok();
    let claude_ide = config::Config::load().is_ok_and(|c| c.claude_ide);
    let ide = claude_ide.then(|| ide::listen(&root).ok()).flatten();
    let mut app = app::App::new(root, if restore { None } else { initial.clone() }, demo)?;
    // Your model server, before restored panes start on it; its SSH tunnel closes
    // when Raido ends.
    model_server::configure(app.config.model_server.clone());
    let _model_server = model_server::Guard;
    app.remote = server;
    app.ide = ide;
    if restore {
        app.restore_session();
        if let Some(path) = &initial {
            app.open(path)?;
        }
    }
    // Keep the Raido skill current for the AI agents installed here (Claude Code or
    // Codex installed after Raido, or a newer Raido), without waiting for Raido's
    // launcher. Machines without an agent are left alone.
    if app.config.ai_skill {
        std::thread::spawn(|| {
            if skill::agents_present() {
                let _ = skill::install();
            }
        });
    }
    // Raido's own font for this window while it runs; the terminal's comes back on exit.
    let _font = textsize::WindowFont::apply(&app.config.font, app.config.font_size);
    // Restore the user's terminal even on panic; no shell or raw escape output survives.
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        reset_terminal_colors();
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableFocusChange,
            LeaveAlternateScreen,
            Show
        );
        old_hook(info);
    }));
    // A terminal pane whose screen vt100 can't handle starts afresh instead.
    terminal::quiet_parser_panics();
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableFocusChange,
        Hide
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut applied_colors = None;
    let mut mouse = false;
    let mut drawn = Instant::now() - FRAME;
    let mut lively = Instant::now();
    while app.running {
        app.tick();
        // Output, an answer from a server, a key: something's moving.
        if app.redraw {
            lively = Instant::now();
        }
        // Follow runtime theme switches (`:theme NAME`).
        let colors = (app.theme.background.clone(), app.theme.foreground.clone());
        if applied_colors.as_ref() != Some(&colors) {
            set_terminal_colors(&app.theme)?;
            applied_colors = Some(colors);
        }
        // The mouse is Raido's (wheel, clicks) except in copy view, where the
        // terminal's own selection needs it.
        if app.mouse_wanted() != mouse {
            mouse = !mouse;
            let mut out = io::stdout();
            write!(out, "{}", if mouse { MOUSE_ON } else { MOUSE_OFF })?;
            out.flush()?;
        }
        let due = FRAME.saturating_sub(drawn.elapsed());
        if app.redraw && due.is_zero() {
            app.redraw = false;
            drawn = Instant::now();
            // One frame reaches the screen whole, never half drawn, in terminals that
            // support it; the others ignore the request.
            execute!(io::stdout(), BeginSynchronizedUpdate)?;
            let frame = terminal.draw(|frame| ui::draw(frame, &mut app));
            execute!(io::stdout(), EndSynchronizedUpdate)?;
            frame?;
        }
        if event::poll(wait(app.redraw, drawn.elapsed(), lively.elapsed()))? {
            // Everything already waiting is handled before the next frame: a burst of
            // wheel turns, a held key or a typed-out paste draws once, not per event.
            app.event(event::read()?);
            let mut handled = 1;
            while handled < BATCH && app.running && event::poll(Duration::ZERO)? {
                app.event(event::read()?);
                handled += 1;
            }
            lively = Instant::now();
        }
    }
    Ok(())
}

/// How long to wait for input: until the next frame is due when one is waiting,
/// briefly while things are moving, else a while.
fn wait(redraw: bool, since_frame: Duration, since_lively: Duration) -> Duration {
    if redraw {
        FRAME.saturating_sub(since_frame)
    } else if since_lively < SETTLE {
        LIVELY
    } else {
        IDLE
    }
}

/// `raido --keys`: print every key and click as Raido receives it. A combination
/// that prints nothing is kept by the terminal (or the OS) and never reaches Raido.
fn show_keys() -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
    println!("Press keys or click to see what reaches Raido; Ctrl-c twice to stop.");
    enable_raw_mode()?;
    let held = |modifiers: KeyModifiers| {
        let mut name = String::new();
        for (flag, label) in [
            (KeyModifiers::CONTROL, "Ctrl"),
            (KeyModifiers::ALT, platform::ALT_KEY),
            (KeyModifiers::SUPER, platform::SUPER_KEY),
            (KeyModifiers::SHIFT, "Shift"),
        ] {
            if modifiers.contains(flag) {
                name.push_str(label);
                name.push('+');
            }
        }
        name
    };
    let mut out = io::stdout();
    write!(out, "{MOUSE_ON}")?;
    out.flush()?;
    let mut last_ctrl_c = false;
    let result = (|| -> Result<()> {
        loop {
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Mouse(mouse) => {
                    if let MouseEventKind::Down(button) = mouse.kind {
                        print!(
                            "{}{button:?} click at {}, {}\r\n",
                            held(mouse.modifiers),
                            mouse.column,
                            mouse.row
                        );
                        io::Write::flush(&mut io::stdout())?;
                    }
                    continue;
                }
                _ => continue,
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let mut name = held(key.modifiers);
            name.push_str(&match key.code {
                KeyCode::Char(' ') => "Space".to_string(),
                KeyCode::Char(c) => c.to_string(),
                KeyCode::F(n) => format!("F{n}"),
                code => format!("{code:?}"),
            });
            print!("{name}\r\n");
            io::Write::flush(&mut io::stdout())?;
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl_c && last_ctrl_c {
                return Ok(());
            }
            last_ctrl_c = ctrl_c;
        }
    })();
    write!(out, "{MOUSE_OFF}")?;
    disable_raw_mode()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every version gets a CHANGELOG.md entry: bumping `version` in Cargo.toml
    /// without one fails here.
    #[test]
    fn the_changelog_has_an_entry_for_this_version() {
        let changelog = include_str!("../CHANGELOG.md");
        let heading = format!("## [{VERSION}] - ");
        assert!(
            changelog.contains(&heading),
            "CHANGELOG.md needs a `{heading}YYYY-MM-DD` section for this version"
        );
        assert!(
            changelog.contains(&format!("[{VERSION}]: ")),
            "CHANGELOG.md needs a link for [{VERSION}] at the bottom"
        );
    }

    #[test]
    fn waits_for_the_next_frame_briefly_when_lively_and_longer_when_idle() {
        let ms = Duration::from_millis;
        // A frame waiting: drawn as soon as the last one is a frame old.
        assert_eq!(wait(true, ms(3), ms(0)), ms(5));
        assert_eq!(wait(true, ms(50), ms(0)), Duration::ZERO);
        // Output or a key just now: a quick look for the next.
        assert_eq!(wait(false, ms(50), ms(100)), LIVELY);
        // Quiet for a while: idle, as before.
        assert_eq!(wait(false, ms(50), ms(400)), IDLE);
        assert!(LIVELY < FRAME && FRAME < IDLE);
    }
}

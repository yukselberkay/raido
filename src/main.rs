mod app;
mod clipboard;
mod config;
mod copymode;
mod editor;
mod fonts;
mod gitgraph;
mod highlight;
mod icons;
mod language;
mod layout;
mod local_ai;
mod lsp;
mod markdown;
mod omarchy;
mod platform;
mod quickdiff;
mod remote;
mod review;
mod search;
mod session;
mod skill;
mod sound;
mod terminal;
mod textsize;
mod ui;
mod vscode;
mod workspace;

use anyhow::{bail, Context, Result};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

/// Point the terminal's own default background/foreground (OSC 11/10) at the theme,
/// so a light terminal profile can't show through the window padding or any cell
/// Odin leaves at the default color. Terminals without OSC 10/11 ignore it.
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

/// Mouse clicks and the wheel, reported in SGR form; no motion events.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1000l";

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        reset_terminal_colors();
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
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
                println!("Odin — all systems in view\n\nUsage: odin [FILE|DIRECTORY] [--demo] [--fresh]\n\n  --demo     Open an editor, AI launcher and shell\n  --fresh    Start without reopening the project's last session\n  --nested   Inside an Odin terminal, start a separate Odin instead of opening FILE there\n  --install-skill  Teach your AI agents about Odin (Agent Skills; also automatic)\n  --remove-skill   Remove that skill again\n  --keys     Show the keys your terminal sends to Odin (to check shortcuts)\n  --version  Print version\n\nKeys: F1 help · Ctrl-e files · Ctrl-b panes · Ctrl-s save · Ctrl-q quit\nConfig: {}",config::config_dir().display());
                return Ok(());
            }
            "-V" | "--version" => {
                println!("odin {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--demo" => demo = true,
            "--install-skill" => {
                for place in skill::install()? {
                    println!("{}", place.display());
                }
                println!(
                    "Odin skill installed for your AI agents (from {})",
                    skill::source().display()
                );
                return Ok(());
            }
            "--remove-skill" => {
                println!("Removed the Odin skill from {} place(s)", skill::remove()?);
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
    // Typed in one of Odin's own terminals: open it in that Odin, don't nest one.
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
            // Another project, or that Odin is gone: start here as usual.
            Ok(remote::Reply::Elsewhere) | Err(_) => {}
        }
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Odin needs an interactive terminal. Run `odin` in your terminal.");
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
    // A saved session reopens first; a file named on the command line opens on top.
    let restore = !demo && !fresh;
    // Listen before any terminal pane starts, so every pane knows where Odin is.
    let server = remote::listen().ok();
    let mut app = app::App::new(root, if restore { None } else { initial.clone() }, demo)?;
    app.remote = server;
    if restore {
        app.restore_session();
        if let Some(path) = &initial {
            app.open(path)?;
        }
    }
    // Keep the Odin skill current for the AI agents installed here (Claude Code or
    // Codex installed after Odin, or a newer Odin), without waiting for Odin's
    // launcher. Machines without an agent are left alone.
    if app.config.ai_skill {
        std::thread::spawn(|| {
            if skill::agents_present() {
                let _ = skill::install();
            }
        });
    }
    // Odin's own font for this window while it runs; the terminal's comes back on exit.
    let _font = textsize::WindowFont::apply(&app.config.font, app.config.font_size);
    // Restore the user's terminal even on panic; no shell or raw escape output survives.
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        reset_terminal_colors();
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
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
        Hide
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut applied_colors = None;
    let mut mouse = false;
    while app.running {
        app.tick();
        // Follow runtime theme switches (`:theme NAME`).
        let colors = (app.theme.background.clone(), app.theme.foreground.clone());
        if applied_colors.as_ref() != Some(&colors) {
            set_terminal_colors(&app.theme)?;
            applied_colors = Some(colors);
        }
        // The mouse is Odin's (wheel, clicks) except in copy view, where the
        // terminal's own selection needs it.
        if app.mouse_wanted() != mouse {
            mouse = !mouse;
            let mut out = io::stdout();
            write!(out, "{}", if mouse { MOUSE_ON } else { MOUSE_OFF })?;
            out.flush()?;
        }
        if app.redraw {
            app.redraw = false;
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
        }
        if event::poll(Duration::from_millis(33))? {
            app.event(event::read()?);
        }
    }
    Ok(())
}

/// `odin --keys`: print every key as Odin receives it. A combination that prints
/// nothing is kept by the terminal (or the OS) and never reaches Odin.
fn show_keys() -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    println!("Press keys to see what reaches Odin; Ctrl-c twice to stop.");
    enable_raw_mode()?;
    let mut last_ctrl_c = false;
    let result = (|| -> Result<()> {
        loop {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            let mut name = String::new();
            for (flag, label) in [
                (KeyModifiers::CONTROL, "Ctrl"),
                (KeyModifiers::ALT, platform::ALT_KEY),
                (KeyModifiers::SUPER, platform::SUPER_KEY),
                (KeyModifiers::SHIFT, "Shift"),
            ] {
                if key.modifiers.contains(flag) {
                    name.push_str(label);
                    name.push('+');
                }
            }
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
    disable_raw_mode()?;
    result
}

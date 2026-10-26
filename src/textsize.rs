//! Text size (zoom in / out) for the terminal window Raido runs in. A terminal
//! program can't scale its own text, so Raido asks the terminal: Terminal.app through
//! AppleScript (only Raido's own tab changes, never the profile), xterm through its
//! font escape, kitty through remote control. Elsewhere it names the terminal's keys.
//!
//! [`WindowFont`] also gives Raido's Terminal.app tab its own font (config `font`,
//! `font_size`) while Raido runs.
use anyhow::{bail, Context, Result};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::{
        mpsc::{self, Receiver},
        OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

/// The size Raido gave its window; Ctrl-b 0 returns to it.
static RAIDO_SIZE: OnceLock<f32> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    In,
    Out,
    Reset,
}

impl Step {
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "in" | "+" | "bigger" => Some(Self::In),
            "out" | "-" | "smaller" => Some(Self::Out),
            "reset" | "0" => Some(Self::Reset),
            _ => None,
        }
    }
    /// Ctrl-b `+` (or `=`), `-` and `0`.
    pub fn key(c: char) -> Option<Self> {
        match c {
            '+' | '=' => Some(Self::In),
            '-' | '_' => Some(Self::Out),
            '0' => Some(Self::Reset),
            _ => None,
        }
    }
    fn word(self) -> &'static str {
        match self {
            Self::In => "bigger",
            Self::Out => "smaller",
            Self::Reset => "back to normal",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Host {
    AppleTerminal,
    Xterm,
    /// kitty with a remote-control socket (`listen_on`).
    Kitty(String),
    Other,
}

fn host(env: &dyn Fn(&str) -> Option<String>) -> Host {
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if cfg!(target_os = "macos") && program == "Apple_Terminal" {
        Host::AppleTerminal
    } else if let Some(socket) = env("KITTY_LISTEN_ON").filter(|_| env("KITTY_WINDOW_ID").is_some())
    {
        Host::Kitty(socket)
    } else if env("XTERM_VERSION").is_some() && program.is_empty() {
        Host::Xterm
    } else {
        Host::Other
    }
}

/// What to tell someone whose terminal Raido can't drive.
fn keys_hint() -> String {
    let keys = if cfg!(target_os = "macos") {
        "Cmd-+ / Cmd-- / Cmd-0"
    } else {
        "Ctrl-+ / Ctrl-- / Ctrl-0 (Cmd on a Mac)"
    };
    format!("This terminal sets its own text size: {keys}")
}

/// Change the text size. The answer (a status line) arrives on the receiver, since
/// asking another program can take a moment.
pub fn change(step: Step) -> Receiver<Result<String>> {
    let (sender, receiver) = mpsc::channel();
    match host(&|name| std::env::var(name).ok()) {
        Host::Xterm => {
            // OSC 50 with `#` walks xterm's font menu; a bare `#` is its default.
            let menu = match step {
                Step::In => "#+1",
                Step::Out => "#-1",
                Step::Reset => "#",
            };
            let mut out = std::io::stdout();
            let sent = write!(out, "\x1b]50;{menu}\x07").and_then(|_| out.flush());
            let _ = sender.send(
                sent.map(|_| format!("Text {}", step.word()))
                    .map_err(Into::into),
            );
        }
        Host::AppleTerminal => {
            thread::spawn(move || {
                let _ = sender.send(apple_terminal(step));
            });
        }
        Host::Kitty(socket) => {
            thread::spawn(move || {
                let size = match step {
                    Step::In => "+1",
                    Step::Out => "-1",
                    Step::Reset => "0",
                };
                let result = run(Command::new("kitty").args([
                    "@",
                    "--to",
                    &socket,
                    "set-font-size",
                    "--",
                    size,
                ]))
                .map(|_| format!("Text {}", step.word()))
                .or_else(|_| Ok(keys_hint()));
                let _ = sender.send(result);
            });
        }
        Host::Other => {
            let _ = sender.send(Ok(keys_hint()));
        }
    }
    receiver
}

/// Resize the font of the Terminal.app tab Raido runs in (found by its tty), leaving
/// the profile and every other tab alone. Reset returns to the profile's size.
const APPLE_SCRIPT: &str = r#"on run argv
    set target to item 1 of argv
    set change to item 2 of argv
    tell application "Terminal"
        repeat with w in windows
            repeat with t in tabs of w
                if tty of t is target then
                    set s to current settings of t
                    if change starts with "=" then
                        set pts to (text 2 thru -1 of change) as number
                    else if change is "reset" then
                        try
                            set pts to font size of settings set (name of s)
                        on error
                            set pts to font size of default settings
                        end try
                    else
                        set pts to (font size of s) + (change as integer)
                    end if
                    if pts < 6 then set pts to 6
                    if pts > 72 then set pts to 72
                    set font size of s to pts
                    return pts
                end if
            end repeat
        end repeat
    end tell
    error "Raido's Terminal tab was not found"
end run"#;

/// The tty of the Terminal.app tab Raido runs in, as Terminal names it.
fn own_tty() -> Result<String> {
    let tty = run(Command::new("ps").args(["-o", "tty=", "-p", &std::process::id().to_string()]))?;
    let tty = tty.trim();
    if tty.is_empty() || tty.contains('?') {
        bail!("Raido has no terminal tab");
    }
    Ok(format!("/dev/{tty}"))
}

fn apple_terminal(step: Step) -> Result<String> {
    let tty = own_tty()?;
    // Reset returns to Raido's own size when it set one, else to the profile's.
    let change = match (step, RAIDO_SIZE.get()) {
        (Step::In, _) => "1".to_string(),
        (Step::Out, _) => "-1".to_string(),
        (Step::Reset, Some(size)) => format!("={size}"),
        (Step::Reset, None) => "reset".to_string(),
    };
    let size = run(Command::new("osascript").args(["-e", APPLE_SCRIPT, &tty, &change]))
        .context("Terminal.app did not change the text size")?;
    Ok(format!(
        "Text size {} pt · Ctrl-b + / - / 0",
        size.trim().trim_end_matches(".0")
    ))
}

/// Raido's font for the terminal window it runs in, put back when Raido exits (on
/// drop). Only Terminal.app lets Raido change its own tab's font from outside (the
/// profile is untouched); every other terminal keeps its own font, as does
/// Terminal.app when the font is not installed.
pub struct WindowFont {
    worker: Option<thread::JoinHandle<Option<Restore>>>,
}

/// How to give the tab its own font back.
struct Restore {
    tty: String,
    name: String,
    size: String,
}

impl WindowFont {
    pub fn apply(family: &str, size: f32) -> Option<Self> {
        let family = family.trim().to_string();
        if family.is_empty()
            || family.contains('"')
            || !(4.0..=96.0).contains(&size)
            || !crate::icons::font_installed(&family)
        {
            return None;
        }
        let worker = match host(&|name| std::env::var(name).ok()) {
            Host::AppleTerminal => thread::spawn(move || {
                let tty = own_tty().ok()?;
                let old = apple_font(&tty, &family, &size.to_string()).ok()?;
                let (name, size) = old.trim_end().split_once('\n')?;
                Some(Restore {
                    tty,
                    name: name.to_string(),
                    size: size.to_string(),
                })
            }),
            _ => return None,
        };
        let _ = RAIDO_SIZE.set(size);
        Some(Self {
            worker: Some(worker),
        })
    }
}

impl Drop for WindowFont {
    fn drop(&mut self) {
        let restore = self.worker.take().and_then(|w| w.join().ok()).flatten();
        if let Some(Restore { tty, name, size }) = restore {
            let _ = apple_font(&tty, &name, &size);
        }
    }
}

/// Set the font of the Terminal.app tab on `tty`; returns the previous name and size
/// (one per line) so they can be put back.
const APPLE_FONT_SCRIPT: &str = r#"on run argv
    set target to item 1 of argv
    set fontName to item 2 of argv
    set pts to (item 3 of argv) as number
    tell application "Terminal"
        repeat with w in windows
            repeat with t in tabs of w
                if tty of t is target then
                    set s to current settings of t
                    set previous to (font name of s) & linefeed & ((font size of s) as text)
                    set font name of s to fontName
                    set font size of s to pts
                    return previous
                end if
            end repeat
        end repeat
    end tell
    error "Raido's Terminal tab was not found"
end run"#;

fn apple_font(tty: &str, name: &str, size: &str) -> Result<String> {
    run(Command::new("osascript").args(["-e", APPLE_FONT_SCRIPT, tty, name, size]))
}

/// Run a helper with a short deadline; a stuck permission prompt must not hang a thread.
fn run(command: &mut Command) -> Result<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        bail!("{}", error.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raido_drives_the_terminals_it_can_and_names_keys_elsewhere() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let apple = host(&env(&[("TERM_PROGRAM", "Apple_Terminal")]));
        if cfg!(target_os = "macos") {
            assert_eq!(apple, Host::AppleTerminal);
        } else {
            assert_eq!(apple, Host::Other, "AppleScript only exists on macOS");
        }
        assert_eq!(
            host(&env(&[
                ("KITTY_WINDOW_ID", "1"),
                ("KITTY_LISTEN_ON", "unix:/tmp/kitty")
            ])),
            Host::Kitty("unix:/tmp/kitty".into())
        );
        assert_eq!(host(&env(&[("KITTY_WINDOW_ID", "1")])), Host::Other);
        assert_eq!(host(&env(&[("XTERM_VERSION", "XTerm(390)")])), Host::Xterm);
        // A terminal started from xterm passes XTERM_VERSION on; trust TERM_PROGRAM.
        assert_eq!(
            host(&env(&[
                ("XTERM_VERSION", "XTerm(390)"),
                ("TERM_PROGRAM", "iTerm.app")
            ])),
            Host::Other
        );
        assert_eq!(host(&env(&[("TERM_PROGRAM", "WezTerm")])), Host::Other);
        // No font asked for, or one that isn't installed: the terminal keeps its own.
        assert!(WindowFont::apply("", 14.0).is_none());
        assert!(WindowFont::apply("No Such Font Raido Test", 14.0).is_none());
        assert!(WindowFont::apply(crate::config::DEFAULT_FONT, 500.0).is_none());
        assert!(keys_hint().contains("-0"));
        assert_eq!(Step::key('='), Some(Step::In));
        assert_eq!(Step::key('-'), Some(Step::Out));
        assert_eq!(Step::key('0'), Some(Step::Reset));
        assert_eq!(Step::parse("out"), Some(Step::Out));
        assert_eq!(Step::parse("sideways"), None);
    }
}

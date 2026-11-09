//! Text size (zoom in / out) for the terminal window Raido runs in. A terminal
//! program can't scale its own text, so Raido asks the terminal: Terminal.app through
//! AppleScript (only Raido's own tab changes, never the profile), xterm through its
//! font escape, kitty through remote control. Elsewhere it names the terminal's keys.
//!
//! [`WindowFont`] also gives Raido's Terminal.app tab its own font (config `font`,
//! `font_size`) while Raido runs. There the size you zoom to is kept: the app saves
//! it as `font_size` ([`Remember`]), so the next launch starts at it.
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

/// The size Ctrl-b 0 returns to when Raido gave the window its own font; without
/// one it returns to the terminal profile's size.
static DEFAULT_SIZE: OnceLock<f32> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    In,
    Out,
    Reset,
    /// An exact size in points (`:font size 13`).
    To(f32),
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
            Self::To(_) => "set",
        }
    }
}

/// A text size change, for the status line.
#[derive(Debug, PartialEq)]
pub struct Changed {
    pub status: String,
    pub remember: Remember,
}

/// What the next launch should start at after a change.
#[derive(Debug, PartialEq)]
pub enum Remember {
    /// Keep this size (points).
    Size(f32),
    /// Back to the default: forget the kept size.
    Default,
    /// The terminal sized itself; Raido can't set it at the next launch.
    Nothing,
}

impl Changed {
    fn only(status: String) -> Self {
        Self {
            status,
            remember: Remember::Nothing,
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

/// The terminal Raido runs in; tests never drive the one they run in.
fn live_host() -> Host {
    if cfg!(test) {
        Host::Other
    } else {
        host(&|name| std::env::var(name).ok())
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

/// Whether Raido sets this terminal's text size in points, and so can keep it for
/// the next launch (Terminal.app).
pub fn sets_size() -> bool {
    live_host() == Host::AppleTerminal
}

/// Change the text size. The answer arrives on the receiver, since asking another
/// program can take a moment.
pub fn change(step: Step) -> Receiver<Result<Changed>> {
    let (sender, receiver) = mpsc::channel();
    match live_host() {
        Host::Xterm => {
            // OSC 50 with `#` walks xterm's font menu; a bare `#` is its default.
            let menu = match step {
                Step::In => "#+1",
                Step::Out => "#-1",
                Step::Reset => "#",
                Step::To(_) => {
                    let _ = sender.send(Ok(Changed::only(
                        "xterm steps through its font menu: Ctrl-b + / - / 0".into(),
                    )));
                    return receiver;
                }
            };
            let mut out = std::io::stdout();
            let sent = write!(out, "\x1b]50;{menu}\x07").and_then(|_| out.flush());
            let _ = sender.send(
                sent.map(|_| Changed::only(format!("Text {}", step.word())))
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
                    Step::In => "+1".to_string(),
                    Step::Out => "-1".to_string(),
                    Step::Reset => "0".to_string(),
                    Step::To(size) => size.to_string(),
                };
                let result = run(Command::new("kitty").args([
                    "@",
                    "--to",
                    &socket,
                    "set-font-size",
                    "--",
                    &size,
                ]))
                .map(|_| Changed::only(format!("Text {}", step.word())))
                .or_else(|_| Ok(Changed::only(keys_hint())));
                let _ = sender.send(result);
            });
        }
        Host::Other => {
            let _ = sender.send(Ok(Changed::only(keys_hint())));
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

fn apple_terminal(step: Step) -> Result<Changed> {
    let tty = own_tty()?;
    let change = apple_change(step, DEFAULT_SIZE.get().copied());
    let size = run(Command::new("osascript").args(["-e", APPLE_SCRIPT, &tty, &change]))
        .context("Terminal.app did not change the text size")?;
    let size = size.trim().trim_end_matches(".0");
    let points: f32 = size
        .parse()
        .context("Terminal.app answered without a size")?;
    Ok(match step {
        Step::Reset => Changed {
            status: format!("Text size {size} pt, the default"),
            remember: Remember::Default,
        },
        _ => Changed {
            status: format!("Text size {size} pt"),
            remember: Remember::Size(points),
        },
    })
}

/// The AppleScript's change: a step, `=N` for a size, or `reset` for the profile's.
/// Reset goes to the default size when Raido gave the tab its own font.
fn apple_change(step: Step, default: Option<f32>) -> String {
    match (step, default) {
        (Step::In, _) => "1".to_string(),
        (Step::Out, _) => "-1".to_string(),
        (Step::To(size), _) | (Step::Reset, Some(size)) => format!("={size}"),
        (Step::Reset, None) => "reset".to_string(),
    }
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
    pub fn apply(family: &str, size: Option<f32>) -> Option<Self> {
        let (family, size) = startup_font(family, size, crate::icons::font_installed)?;
        let own = !family.is_empty();
        let worker = match live_host() {
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
        if own {
            let _ = DEFAULT_SIZE.set(crate::config::DEFAULT_FONT_SIZE);
        }
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

/// The font and size the window starts with: Raido's font (if installed) at the kept
/// size or 14 pt, or, without it, only a kept size in the terminal's own font (`""`).
/// Neither: the window is left alone.
fn startup_font(
    family: &str,
    size: Option<f32>,
    installed: impl Fn(&str) -> bool,
) -> Option<(String, f32)> {
    let family = family.trim();
    let family = if family.is_empty() || family.contains('"') || !installed(family) {
        ""
    } else {
        family
    };
    let size = match (family, size) {
        ("", None) => return None,
        (_, size) => size.unwrap_or(crate::config::DEFAULT_FONT_SIZE),
    };
    (4.0..=96.0)
        .contains(&size)
        .then(|| (family.to_string(), size))
}

/// Set the font of the Terminal.app tab on `tty` (an empty name keeps the tab's);
/// returns the previous name and size (one per line) so they can be put back.
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
                    if fontName is not "" then set font name of s to fontName
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
        // Raido's font comes at the kept size, or 14 pt.
        let installed = |name: &str| name == "Menlo";
        assert_eq!(
            startup_font("Menlo", None, installed),
            Some(("Menlo".into(), 14.0))
        );
        assert_eq!(
            startup_font(" Menlo ", Some(12.3), installed),
            Some(("Menlo".into(), 12.3))
        );
        // Without it (none asked for, or not installed) only a kept size applies, in
        // the terminal's own font; with no size either, the window is left alone.
        assert_eq!(
            startup_font("", Some(16.0), installed),
            Some(("".into(), 16.0))
        );
        assert_eq!(
            startup_font("No Such Font", Some(16.0), installed),
            Some(("".into(), 16.0))
        );
        assert_eq!(startup_font("", None, installed), None);
        assert_eq!(startup_font("No Such Font", None, installed), None);
        assert_eq!(startup_font("Menlo", Some(500.0), installed), None);
        // Reset is the default size with Raido's font, the profile's size without.
        assert_eq!(apple_change(Step::Reset, Some(14.0)), "=14");
        assert_eq!(apple_change(Step::Reset, None), "reset");
        assert_eq!(apple_change(Step::To(12.5), None), "=12.5");
        assert_eq!(apple_change(Step::Out, Some(14.0)), "-1");
        assert!(keys_hint().contains("-0"));
        assert_eq!(Step::key('='), Some(Step::In));
        assert_eq!(Step::key('-'), Some(Step::Out));
        assert_eq!(Step::key('0'), Some(Step::Reset));
        assert_eq!(Step::parse("out"), Some(Step::Out));
        assert_eq!(Step::parse("sideways"), None);
    }
}

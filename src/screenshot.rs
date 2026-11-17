//! Screenshots for AI panes (Ctrl-b I): the image on the clipboard, else the
//! screenshot you just took, kept in Raido's cache where the agent can read it and
//! handed to the pane as a path, which Claude Code and Codex attach as an image.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

/// A screenshot file this recent counts as the one you just took.
pub const RECENT: Duration = Duration::from_secs(30 * 60);
/// Images handed to agents are kept this long, so a conversation can still read them.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// The clipboard helpers answer within this.
const WAIT: Duration = Duration::from_secs(5);
/// What counts as an image file.
const IMAGES: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// macOS's ⌘V for screenshots (`paste_screenshots`): a helper watching the
/// clipboard, which saves a screenshot put there and, while Raido has the front,
/// adds its path as text beside the image, so the terminal's ⌘V pastes the path
/// and the agent attaches the image. Other apps get the screenshot as it was
/// copied: the helper takes the path away when another app comes to the front,
/// and when it stops. Stopped when dropped (and by itself when Raido is gone).
pub struct Watcher {
    child: Option<std::process::Child>,
    /// Hears whether Raido's window has the focus; the helper stops when it's closed.
    tell: Option<std::process::ChildStdin>,
    found: std::sync::mpsc::Receiver<PathBuf>,
    focused: bool,
}

/// The terminal app Raido runs in, as the watcher knows it when it's in front.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Host {
    /// The processes Raido runs under, the terminal app among them.
    pub pids: Vec<u32>,
    /// The terminal's bundle id (`__CFBundleIdentifier`), which iTerm2's sessions
    /// carry though they don't run under it. Not under tmux, where it can be stale.
    pub bundle: Option<String>,
}

impl Host {
    /// This process's terminal app. Empty where it can't be told.
    pub fn current() -> Self {
        let table = Command::new("ps")
            .args(["-axo", "pid=,ppid="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let bundle = std::env::var("__CFBundleIdentifier")
            .ok()
            .filter(|b| !b.is_empty() && std::env::var_os("TMUX").is_none());
        Self {
            pids: ancestors(&table, std::process::id()),
            bundle,
        }
    }
}

/// The processes above `pid` in `ps -o pid=,ppid=` output, launchd excluded.
fn ancestors(table: &str, pid: u32) -> Vec<u32> {
    let parents: std::collections::HashMap<u32, u32> = table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<u32>);
            Some((fields.next()?.ok()?, fields.next()?.ok()?))
        })
        .collect();
    let mut found = Vec::new();
    let mut at = pid;
    while let Some(&parent) = parents.get(&at) {
        if parent <= 1 || found.contains(&parent) {
            break;
        }
        found.push(parent);
        at = parent;
    }
    found
}

impl Watcher {
    /// Watch the clipboard (or the private pasteboard `pasteboard`, in tests),
    /// keeping screenshots in `keep`, for Raido running in `host`. None where it
    /// can't run (not macOS).
    pub fn start(keep: &Path, pasteboard: Option<&str>, host: &Host) -> Option<Self> {
        use std::io::BufRead;
        if !cfg!(target_os = "macos") {
            return None;
        }
        fs::create_dir_all(keep).ok()?;
        clear_old(keep);
        let pids: Vec<String> = host.pids.iter().map(u32::to_string).collect();
        let mut child = Command::new("osascript")
            .args(["-l", "JavaScript", "-e", include_str!("clipboard_watch.js")])
            .arg(keep)
            .arg(pids.join(","))
            .arg(host.bundle.as_deref().unwrap_or_default())
            .arg(pasteboard.unwrap_or_default())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let tell = child.stdin.take();
        let lines = child.stderr.take()?;
        let (send, found) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(lines).lines().map_while(Result::ok) {
                if let Some(path) = line.strip_prefix("image ") {
                    if send.send(PathBuf::from(path)).is_err() {
                        break;
                    }
                }
            }
        });
        Some(Self {
            child: Some(child),
            tell,
            found,
            focused: true,
        })
    }

    /// Tell the helper whether Raido's window has the focus: only then may ⌘V
    /// paste the path (the terminal's other windows and tabs get the image).
    pub fn focus(&mut self, focused: bool) {
        use std::io::Write;
        if focused == self.focused {
            return;
        }
        if let Some(tell) = &mut self.tell {
            let _ = tell.write_all(if focused { b"1\n" } else { b"0\n" });
        }
        self.focused = focused;
    }

    /// The screenshot made ready for ⌘V since asked last, if any.
    pub fn ready(&self) -> Option<PathBuf> {
        self.found.try_iter().last()
    }
}

impl Drop for Watcher {
    /// The helper puts the screenshot back as it was copied and stops when its
    /// stdin closes; it's killed if it hasn't within two seconds. Raido doesn't
    /// wait for it.
    fn drop(&mut self) {
        self.tell = None;
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                for _ in 0..40 {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

/// Whether pasted `text` is a screenshot Raido kept in `keep`: the path the
/// clipboard watcher (or Ctrl-b I) put there.
pub fn kept_image(keep: &Path, text: &str) -> Option<PathBuf> {
    let path = Path::new(text.trim());
    (path.parent() == Some(keep) && is_image(path) && path.is_file()).then(|| path.to_path_buf())
}

/// Where the image came from.
#[derive(Debug, PartialEq, Eq)]
pub enum Source {
    Clipboard,
    /// A screenshot file, by its name.
    File(String),
}

/// The image on the clipboard, else the newest image in `screenshots` from the last
/// [`RECENT`] minutes, as a file in `keep`. `clipboard` saves the clipboard's image
/// to the path it's given, true when there was one.
pub fn take(
    keep: &Path,
    screenshots: &[PathBuf],
    clipboard: impl FnOnce(&Path) -> bool,
) -> Result<(PathBuf, Source), String> {
    fs::create_dir_all(keep).map_err(|e| format!("{}: {e}", keep.display()))?;
    clear_old(keep);
    let stamp = stamp();
    let pasted = keep.join(format!("clipboard-{stamp}.png"));
    if clipboard(&pasted) && fs::metadata(&pasted).is_ok_and(|m| m.len() > 0) {
        return Ok((pasted, Source::Clipboard));
    }
    let _ = fs::remove_file(&pasted);
    let newest = screenshots
        .iter()
        .filter_map(|folder| fs::read_dir(folder).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|entry| is_image(&entry.path()))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            let age = modified.elapsed().unwrap_or_default();
            (age <= RECENT && entry.file_type().ok()?.is_file()).then(|| (modified, entry.path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
        .ok_or_else(|| {
            "No image on the clipboard, and no screenshot in the last 30 minutes".to_string()
        })?;
    // A copy with a plain name: agents read paths with spaces differently, and the
    // original may be moved or deleted before the agent reads it.
    let extension = newest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let copy = keep.join(format!("screenshot-{stamp}.{extension}"));
    fs::copy(&newest, &copy).map_err(|e| format!("{}: {e}", newest.display()))?;
    let name = newest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((copy, Source::File(name)))
}

fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGES.contains(&e.to_ascii_lowercase().as_str()))
}

/// Seconds and milliseconds since 1970: unique enough, and sorted by time.
fn stamp() -> String {
    let since = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{:03}", since.as_secs(), since.subsec_millis())
}

/// Images handed over more than a week ago go.
fn clear_old(keep: &Path) {
    for entry in fs::read_dir(keep).into_iter().flatten().flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|m| m.elapsed().is_ok_and(|age| age > KEEP));
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Raido's folder for images handed to agents.
pub fn keep_folder() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.cache_dir().join("raido").join("images"))
}

/// Where this system saves screenshots: macOS's chosen folder (Desktop unless you
/// changed it), `~/Pictures/Screenshots` elsewhere.
pub fn screenshot_folders() -> Vec<PathBuf> {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    if cfg!(target_os = "macos") {
        let chosen = Command::new("defaults")
            .args(["read", "com.apple.screencapture", "location"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(chosen) = chosen {
            folders.push(match chosen.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(chosen),
            });
        }
        folders.push(home.join("Desktop"));
    } else {
        folders.push(home.join("Pictures").join("Screenshots"));
        folders.push(home.join("Pictures"));
    }
    folders.dedup();
    folders
}

/// Save the clipboard's image to `to` as PNG: osascript on macOS, wl-paste or xclip
/// on Linux, PowerShell on Windows. False when there's no image (or no helper).
pub fn clipboard_image(to: &Path) -> bool {
    let target = to.to_string_lossy().into_owned();
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command.args([
            "-e",
            "on run argv",
            "-e",
            "set png to (the clipboard as «class PNGf»)",
            "-e",
            "set out to open for access (POSIX file (item 1 of argv)) with write permission",
            "-e",
            "set eof out to 0",
            "-e",
            "write png to out",
            "-e",
            "close access out",
            "-e",
            "end run",
            &target,
        ]);
        command
    } else if cfg!(windows) {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms; $i = [Windows.Forms.Clipboard]::GetImage(); if (-not $i) { exit 1 }; $i.Save($args[0], [Drawing.Imaging.ImageFormat]::Png)",
            &target,
        ]);
        command
    } else {
        // Wayland, else X11: the image goes to stdout, written below.
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let mut command = Command::new(if wayland { "wl-paste" } else { "xclip" });
        if wayland {
            command.args(["--no-newline", "--type", "image/png"]);
        } else {
            command.args(["-selection", "clipboard", "-target", "image/png", "-out"]);
        }
        command
    };
    let out = crate::review::run_for(&mut command, None, WAIT, "clipboard");
    if cfg!(any(target_os = "macos", windows)) {
        return out.is_ok();
    }
    // A PNG starts with its signature; anything else isn't an image.
    match out {
        Ok(bytes) if bytes.starts_with(b"\x89PNG") => fs::write(to, bytes).is_ok(),
        _ => false,
    }
}

/// How an agent takes an image file typed into its prompt: Gemini CLI reads `@path`
/// references; `ollama run` (the Local LLM) finds an image path among the words
/// of the prompt; Claude Code, Codex and OpenCode attach a pasted path as it is.
pub fn reference(agent: Option<&str>, path: &Path) -> String {
    let path = path.to_string_lossy();
    match agent {
        Some("gemini") => format!("@{path} "),
        Some("local") => format!("{path} "),
        _ => path.into_owned(),
    }
}

/// Where this machine's Ollama answers: `OLLAMA_HOST` (`host:port`, a URL, or
/// `0.0.0.0` for all addresses), else `http://127.0.0.1:11434`.
pub fn local_ollama(host: Option<&str>) -> String {
    let host = host
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .unwrap_or("127.0.0.1");
    let (scheme, rest) = match host.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("http", host),
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.replace("0.0.0.0", "127.0.0.1");
    let with_port = if rest
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    {
        rest
    } else {
        format!("{rest}:11434")
    };
    format!("{scheme}://{with_port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_comes_first_then_the_newest_recent_screenshot() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep");
        let shots = dir.path().join("Desktop");
        fs::create_dir_all(&shots).unwrap();
        // From the clipboard.
        let (path, source) = take(&keep, std::slice::from_ref(&shots), |to| {
            fs::write(to, b"\x89PNG clipboard").is_ok()
        })
        .unwrap();
        assert_eq!(source, Source::Clipboard);
        assert_eq!(fs::read(&path).unwrap(), b"\x89PNG clipboard");
        assert!(path.starts_with(&keep));
        // Nothing there, no screenshot: say so.
        let error = take(&keep, std::slice::from_ref(&shots), |_| false).unwrap_err();
        assert!(error.contains("No image on the clipboard"), "{error}");
        // The newest recent image wins; old ones and other files don't count.
        let old = shots.join("Screenshot old.png");
        fs::write(&old, b"old").unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - RECENT - Duration::from_secs(60))
            .unwrap();
        fs::write(shots.join("notes.txt"), b"text").unwrap();
        let earlier = shots.join("Screenshot 2026-11-13 at 10.00.00.png");
        fs::write(&earlier, b"earlier").unwrap();
        fs::File::options()
            .write(true)
            .open(&earlier)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        let newest = shots.join("Bildschirmfoto 2026-11-13 um 10.05.00.PNG");
        fs::write(&newest, b"newest").unwrap();
        let (path, source) = take(&keep, std::slice::from_ref(&shots), |_| false).unwrap();
        assert_eq!(
            source,
            Source::File("Bildschirmfoto 2026-11-13 um 10.05.00.PNG".into())
        );
        // Copied under a plain name, so the agent reads it as one path.
        assert_eq!(fs::read(&path).unwrap(), b"newest");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("screenshot-") && name.ends_with(".png") && !name.contains(' '));
        // An empty clipboard file isn't an image.
        let (_, source) = take(&keep, &[shots], |to| fs::write(to, b"").is_ok()).unwrap();
        assert_ne!(source, Source::Clipboard);
    }

    /// A private pasteboard's types and text, or puts an image file on it (as
    /// TIFF), puts text on it (`text:…`), saves its TIFF (`save:…`) or lets it go.
    #[cfg(target_os = "macos")]
    fn pasteboard(name: &str, put: Option<&Path>) -> String {
        let script = r#"ObjC.import('AppKit');
function run(argv) {
  var pb = $.NSPasteboard.pasteboardWithName(argv[0]);
  if (argv[1] === 'release') { pb.releaseGlobally; return ''; }
  if (argv[1].indexOf('save:') === 0) {
    pb.dataForType('public.tiff').writeToFileAtomically(argv[1].slice(5), true);
  } else if (argv[1].indexOf('text:') === 0) {
    pb.clearContents;
    pb.setStringForType(argv[1].slice(5), 'public.utf8-plain-text');
  } else if (argv[1]) {
    pb.clearContents;
    pb.setDataForType($.NSData.dataWithContentsOfFile(argv[1]), 'public.tiff');
  }
  var text = pb.stringForType('public.utf8-plain-text');
  return ObjC.deepUnwrap(pb.types).join(',') + ' | ' + (text.isNil() ? '' : ObjC.unwrap(text));
}"#;
        let out = Command::new("osascript")
            .args(["-l", "JavaScript", "-e", script, name])
            .arg(put.map_or("".into(), |p| p.to_string_lossy().into_owned()))
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Poll the pasteboard until `done` holds for what it says, up to 10 seconds.
    #[cfg(target_os = "macos")]
    fn until(name: &str, done: impl Fn(&str) -> bool) -> String {
        let mut now = String::new();
        for _ in 0..100 {
            now = pasteboard(name, None);
            if done(&now) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        now
    }

    /// A small TIFF, as some apps copy images (small, so converting it doesn't race
    /// the waits on a busy machine), and how a pasteboard with only it looks.
    #[cfg(target_os = "macos")]
    fn small_tiff(dir: &Path) -> (PathBuf, String) {
        let tiff = dir.join("shot.tiff");
        let made = Command::new("sips")
            .args(["-s", "format", "tiff", "-Z", "64"])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/docs/raido-c-tiles.png"
            ))
            .arg("--out")
            .arg(&tiff)
            .output()
            .unwrap();
        assert!(made.status.success());
        // Its own pasteboard: the tests run side by side.
        let name = format!(
            "raido-test-copied-{}",
            dir.file_name().unwrap_or_default().to_string_lossy()
        );
        let copied = pasteboard(&name, Some(&tiff));
        pasteboard(&name, Some(Path::new("release")));
        assert!(
            copied.starts_with("public.tiff") && !copied.contains("png"),
            "{copied}"
        );
        (tiff, copied)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_screenshot_on_the_clipboard_gets_its_path_beside_it_for_cmd_v() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("images");
        // A private pasteboard: the system clipboard is left alone.
        let name = format!("raido-test-{}", std::process::id());
        let (tiff, copied) = small_tiff(dir.path());
        let mut watcher = Watcher::start(&keep, Some(&name), &Host::default()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(pasteboard(&name, Some(&tiff)).contains("public.tiff"));
        let mut ready = None;
        for _ in 0..200 {
            ready = watcher.ready();
            if ready.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let path = ready.expect("the watcher readied the screenshot");
        assert_eq!(fs::read(&path).unwrap()[..4], *b"\x89PNG");
        assert_eq!(
            kept_image(&keep, &path.to_string_lossy()),
            Some(path.clone())
        );
        // The image stays, as PNG too; its path is there as text for the terminal's ⌘V.
        let with_path = format!(" | {}", path.display());
        let now = until(&name, |now| now.ends_with(&with_path));
        assert!(
            now.contains("public.png") && now.contains("public.tiff"),
            "{now}"
        );
        assert!(now.ends_with(&with_path), "{now}");
        // Raido's window loses the focus: other apps get the screenshot exactly as
        // it was copied, with no text to paste instead of it.
        watcher.focus(false);
        assert_eq!(until(&name, |now| now == copied), copied);
        let back = dir.path().join("back.tiff");
        pasteboard(&name, Some(Path::new(&format!("save:{}", back.display()))));
        assert_eq!(fs::read(&back).unwrap(), fs::read(&tiff).unwrap());
        // Back in Raido: the same file again, not a new one.
        watcher.focus(true);
        assert!(until(&name, |now| now.ends_with(&with_path)).ends_with(&with_path));
        assert_eq!(watcher.ready(), None);
        // Text on the clipboard is left alone.
        assert!(pasteboard(&name, Some(Path::new("text:a password"))).ends_with(" | a password"));
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(watcher.ready(), None);
        assert!(pasteboard(&name, None).ends_with(" | a password"));
        // A new screenshot, then the watcher stops (the last AI pane closed, Raido
        // quit): the clipboard is the screenshot as copied.
        pasteboard(&name, Some(&tiff));
        let next = until(&name, |now| now.contains(" | /"));
        assert!(
            next.contains(&keep.to_string_lossy().into_owned()),
            "{next}"
        );
        drop(watcher);
        assert_eq!(until(&name, |now| now == copied), copied);
        pasteboard(&name, Some(Path::new("release")));
        assert_eq!(kept_image(&keep, "notes"), None);
        assert_eq!(kept_image(dir.path(), &path.to_string_lossy()), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn with_another_app_in_front_the_screenshot_is_left_as_copied() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("images");
        let name = format!("raido-test-front-{}", std::process::id());
        let (tiff, copied) = small_tiff(dir.path());
        // Raido's terminal is no app running here, so it's never in front.
        let elsewhere = Host {
            pids: vec![u32::MAX - 1],
            bundle: Some("dev.raido.nowhere".into()),
        };
        let watcher = Watcher::start(&keep, Some(&name), &elsewhere).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        pasteboard(&name, Some(&tiff));
        std::thread::sleep(Duration::from_millis(1000));
        assert_eq!(watcher.ready(), None);
        assert_eq!(pasteboard(&name, None), copied);
        drop(watcher);
        pasteboard(&name, Some(Path::new("release")));
    }

    #[test]
    fn the_terminal_app_is_found_above_raido() {
        let table =
            "    1     0\n  500     1\n  600   500\n  700   600\n  800   700\n  900     1\n";
        assert_eq!(ancestors(table, 800), vec![700, 600, 500]);
        assert_eq!(ancestors(table, 900), Vec::<u32>::new());
        assert_eq!(ancestors(table, 42), Vec::<u32>::new());
    }

    #[test]
    fn images_a_week_old_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("clipboard-1.png");
        fs::write(&old, b"x").unwrap();
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(SystemTime::now() - KEEP - Duration::from_secs(60))
            .unwrap();
        let fresh = dir.path().join("clipboard-2.png");
        fs::write(&fresh, b"x").unwrap();
        clear_old(dir.path());
        assert!(!old.exists() && fresh.exists());
    }

    #[test]
    fn each_agent_gets_the_path_its_way() {
        let path = Path::new("/cache/raido/images/clipboard-1.png");
        assert_eq!(
            reference(Some("claude"), path),
            "/cache/raido/images/clipboard-1.png"
        );
        assert_eq!(
            reference(Some("codex"), path),
            "/cache/raido/images/clipboard-1.png"
        );
        assert_eq!(
            reference(Some("gemini"), path),
            "@/cache/raido/images/clipboard-1.png "
        );
        assert_eq!(
            reference(Some("local"), path),
            "/cache/raido/images/clipboard-1.png "
        );
        assert_eq!(
            reference(Some("opencode"), path),
            "/cache/raido/images/clipboard-1.png"
        );
        assert_eq!(reference(None, path), "/cache/raido/images/clipboard-1.png");
    }

    #[test]
    fn ollama_is_found_where_ollama_host_says() {
        assert_eq!(local_ollama(None), "http://127.0.0.1:11434");
        assert_eq!(local_ollama(Some("0.0.0.0")), "http://127.0.0.1:11434");
        assert_eq!(local_ollama(Some("0.0.0.0:8080")), "http://127.0.0.1:8080");
        assert_eq!(local_ollama(Some("https://box:443/")), "https://box:443");
        assert_eq!(local_ollama(Some("gpu-box")), "http://gpu-box:11434");
    }
}

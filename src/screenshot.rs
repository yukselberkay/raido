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

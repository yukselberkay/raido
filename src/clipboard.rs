//! One clipboard for the editor, shared with the desktop: copies go to the
//! system clipboard (wl-copy, xclip, xsel, pbcopy, or OSC 52 as a fallback)
//! and pastes read it back, so text moves freely between Raido and other apps.
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

#[derive(Default)]
pub struct Clipboard {
    text: String,
    /// Whole lines (from `yy`, `dd`, `V`, or Ctrl-c without a selection).
    linewise: bool,
}

impl Clipboard {
    pub fn copy(&mut self, text: String, linewise: bool) {
        if !cfg!(test) {
            system_copy(&text);
        }
        self.text = text;
        self.linewise = linewise;
    }
    /// The newest clipboard text: the system's if it changed since our last copy.
    pub fn paste(&self) -> (String, bool) {
        if !cfg!(test) {
            if let Some(text) = system_paste() {
                let text = text.replace("\r\n", "\n");
                if !text.is_empty() && text != self.text {
                    return (text, false);
                }
            }
        }
        (self.text.clone(), self.linewise)
    }
}

fn tools(copy: bool) -> Vec<(&'static str, &'static [&'static str])> {
    let mut tools = Vec::new();
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        tools.push(if copy {
            ("wl-copy", &[][..])
        } else {
            ("wl-paste", &["--no-newline"][..])
        });
    }
    if std::env::var_os("DISPLAY").is_some() {
        tools.push(if copy {
            ("xclip", &["-selection", "clipboard", "-in"][..])
        } else {
            ("xclip", &["-selection", "clipboard", "-out"][..])
        });
        tools.push(if copy {
            ("xsel", &["--clipboard", "--input"][..])
        } else {
            ("xsel", &["--clipboard", "--output"][..])
        });
    }
    if cfg!(target_os = "macos") {
        tools.push(if copy {
            ("pbcopy", &[][..])
        } else {
            ("pbpaste", &[][..])
        });
    }
    if cfg!(windows) {
        // PowerShell handles Unicode both ways (clip.exe would mangle it).
        tools.push(if copy {
            (
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[Console]::InputEncoding=[Text.Encoding]::UTF8; Set-Clipboard -Value ([Console]::In.ReadToEnd())",
                ][..],
            )
        } else {
            (
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw",
                ][..],
            )
        });
    }
    tools
}

fn system_copy(text: &str) {
    for (program, args) in tools(true) {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // Reap in the background; xclip/wl-copy may linger to serve the selection.
        thread::spawn(move || child.wait());
        return;
    }
    // No helper installed: ask the terminal itself (works over SSH in most terminals).
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
}

fn system_paste() -> Option<String> {
    for (program, args) in tools(false) {
        let Ok(mut child) = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let mut stdout = child.stdout.take()?;
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stdout.read_to_end(&mut bytes);
            let _ = send.send(bytes);
        });
        // Never let a stuck clipboard owner freeze the editor.
        // PowerShell starts slowly; others answer at once.
        let limit = if cfg!(windows) { 3000 } else { 500 };
        let bytes = receive.recv_timeout(Duration::from_millis(limit));
        let _ = child.kill();
        let _ = child.wait();
        let mut ok = bytes.ok().and_then(|b| String::from_utf8(b).ok());
        // PowerShell ends its output with a newline that is not part of the clipboard.
        if cfg!(windows) {
            if let Some(text) = &mut ok {
                if text.ends_with("\r\n") {
                    text.truncate(text.len() - 2);
                }
            }
        }
        if ok.is_some() {
            return ok;
        }
    }
    None
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_matches_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
    #[test]
    fn internal_register_keeps_linewise_flag() {
        let mut clipboard = Clipboard::default();
        clipboard.copy("line\n".into(), true);
        assert_eq!(clipboard.paste(), ("line\n".into(), true));
    }
}

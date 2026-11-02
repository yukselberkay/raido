//! Web links in what Raido shows: found under a click and opened in the default
//! browser (`open` on macOS, `xdg-open` or WSL's `wslview` on Linux, the URL
//! handler on Windows). Only http(s) links open, so text a program prints can't
//! start anything else.
use std::process::{Command, Stdio};

/// Where links can't contain these, or end in the punctuation around them.
const STOP: &str = "<>\"'`{}|\\^";
const TRAILING: &str = ".,;:!?*'\"";

/// The link in `line` covering char `at`, as its char range and the URL to open
/// (`www.` links get `https://`).
pub fn find(line: &str, at: usize) -> Option<(std::ops::Range<usize>, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let rest: String = chars[start..chars.len().min(start + 8)].iter().collect();
        let scheme = ["https://", "http://", "www."]
            .into_iter()
            .find(|s| rest.to_ascii_lowercase().starts_with(s));
        // A link starts a word: not the `www.` inside `xwww.`.
        let boundary = start == 0 || !chars[start - 1].is_alphanumeric();
        let Some(scheme) = scheme.filter(|_| boundary) else {
            start += 1;
            continue;
        };
        let mut end = start;
        while end < chars.len() && !chars[end].is_whitespace() && !STOP.contains(chars[end]) {
            end += 1;
        }
        // Trailing punctuation belongs to the sentence, and a closing bracket to
        // the text around unless the link opened one itself.
        while let Some(&last) = chars[start..end].last() {
            let count = |c: char| chars[start..end].iter().filter(|&&x| x == c).count();
            let unmatched = match last {
                ')' => count('(') < count(')'),
                ']' => count('[') < count(']'),
                _ => false,
            };
            if TRAILING.contains(last) || unmatched {
                end -= 1;
            } else {
                break;
            }
        }
        if end > start + scheme.len() && (start..end).contains(&at) {
            let text: String = chars[start..end].iter().collect();
            let url = if scheme == "www." {
                format!("https://{text}")
            } else {
                text
            };
            return Some((start..end, url));
        }
        start = end.max(start + 1);
    }
    None
}

/// Open `url` in the default browser, without waiting for it.
pub fn open(url: &str) -> std::io::Result<()> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(std::io::Error::other("only web links open"));
    }
    if cfg!(test) {
        return Ok(());
    }
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // The URL handler itself: `start` would read `&` in the link as a new command.
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else if crate::platform::which("wslview").is_some() {
        Command::new("wslview")
    } else {
        Command::new("xdg-open")
    };
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_link_under_the_click_without_the_punctuation_around_it() {
        let line = "See (https://example.com/a_(b)?q=1&r=2). Or www.rust-lang.org, done";
        let at = line.find("example").unwrap();
        let (range, url) = find(line, at).unwrap();
        assert_eq!(url, "https://example.com/a_(b)?q=1&r=2");
        assert_eq!(range.start, line.find("https").unwrap());
        let at = line.find("rust").unwrap();
        assert_eq!(find(line, at).unwrap().1, "https://www.rust-lang.org");
        // Not on a link, or a bare scheme.
        assert_eq!(find(line, 0), None);
        assert_eq!(find("go to https:// now", 7), None);
        assert_eq!(find("<http://a.io/x>", 3).unwrap().1, "http://a.io/x");
        // Characters before the link count as chars, not bytes.
        assert_eq!(find("→ ü https://ü.de", 6).unwrap().1, "https://ü.de");
    }

    #[test]
    fn only_web_links_open() {
        assert!(open("https://example.com").is_ok());
        assert!(open("file:///etc/passwd").is_err());
        assert!(open("javascript:alert(1)").is_err());
    }
}

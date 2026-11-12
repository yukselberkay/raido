//! Finding the conversation an AI pane is in, so closing the pane and opening the
//! same CLI again reopens that conversation instead of a blank one.
//!
//! - Claude Code and Gemini CLI start under an id Raido picks (`--session-id`).
//! - Codex can't be given one, but it stamps each conversation with the time it
//!   started and the folder it ran in, so a pane's conversation is the one that
//!   started here after the pane did.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The first Gemini CLI release known to take `--session-id`.
const GEMINI_SESSION_IDS: (u64, u64) = (0, 46);

/// Whether the installed Gemini CLI can start under an id Raido picks. Read once
/// from its package.json; unknown counts as no, which falls back to reopening the
/// latest conversation in the folder.
pub fn gemini_takes_ids() -> bool {
    static ANSWER: OnceLock<bool> = OnceLock::new();
    *ANSWER.get_or_init(|| {
        crate::platform::which("gemini")
            .and_then(|path| gemini_version(&path))
            .and_then(|v| version_at_least(&v, GEMINI_SESSION_IDS))
            .unwrap_or(false)
    })
}

/// The version in the `@google/gemini-cli` package.json near the `gemini` on PATH
/// (npm, Homebrew and Windows npm shims all keep it a few folders away).
fn gemini_version(program: &Path) -> Option<String> {
    let program = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    for dir in program.ancestors().skip(1).take(6) {
        for candidate in [
            dir.join("package.json"),
            dir.join("node_modules/@google/gemini-cli/package.json"),
            dir.join("lib/node_modules/@google/gemini-cli/package.json"),
            dir.join("libexec/lib/node_modules/@google/gemini-cli/package.json"),
        ] {
            let Ok(text) = std::fs::read_to_string(&candidate) else {
                continue;
            };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if json.get("name").and_then(|n| n.as_str()) == Some("@google/gemini-cli") {
                return json.get("version")?.as_str().map(String::from);
            }
        }
    }
    None
}

/// "0.46.0" or "0.47.0-nightly.1" against (major, minor).
fn version_at_least(version: &str, (major, minor): (u64, u64)) -> Option<bool> {
    let mut parts = version.split(['.', '-']);
    let a: u64 = parts.next()?.parse().ok()?;
    let b: u64 = parts.next()?.parse().ok()?;
    Some((a, b) >= (major, minor))
}

/// Gemini CLI's own folder: `$GEMINI_CLI_HOME` (as the folder or its parent), else
/// `~/.gemini`.
fn gemini_homes() -> Vec<PathBuf> {
    match std::env::var_os("GEMINI_CLI_HOME") {
        Some(home) => {
            let home = PathBuf::from(home);
            vec![home.join(".gemini"), home]
        }
        None => directories::BaseDirs::new()
            .map(|d| vec![d.home_dir().join(".gemini")])
            .unwrap_or_default(),
    }
}

/// Whether Gemini CLI saved conversation `id` (it does once it has a message):
/// `<home>/tmp/<project>/chats/session-<time>-<first 8 of id>.json[l]`.
pub fn gemini_conversation_exists(id: &str) -> bool {
    gemini_homes()
        .iter()
        .any(|home| gemini_conversation_in(home, id))
}

fn gemini_conversation_in(home: &Path, id: &str) -> bool {
    let Some(short) = id.get(..8) else {
        return false;
    };
    let Ok(projects) = std::fs::read_dir(home.join("tmp")) else {
        return false;
    };
    projects.flatten().any(|project| {
        let Ok(chats) = std::fs::read_dir(project.path().join("chats")) else {
            return false;
        };
        chats.flatten().any(|chat| {
            let name = chat.file_name().to_string_lossy().into_owned();
            let ours = name.starts_with("session-")
                && (name.ends_with(&format!("-{short}.json"))
                    || name.ends_with(&format!("-{short}.jsonl")));
            ours && std::fs::read_to_string(chat.path()).is_ok_and(|text| text.contains(id))
        })
    })
}

/// Codex's folder: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".codex")))
}

/// Whether Codex still has conversation `id`:
/// `<home>/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
pub fn codex_conversation_exists(id: &str) -> bool {
    codex_conversation_file(id).is_some()
}

/// Where Codex keeps conversation `id`, the newest days looked through first.
pub fn codex_conversation_file(id: &str) -> Option<PathBuf> {
    let home = codex_home()?;
    let file = format!("-{id}.jsonl");
    day_folders(&home.join("sessions"), usize::MAX)
        .iter()
        .flat_map(|day| std::fs::read_dir(day).into_iter().flatten().flatten())
        .find(|f| f.file_name().to_string_lossy().ends_with(&file))
        .map(|f| f.path())
}

/// `sessions/YYYY/MM/DD` folders, newest first, at most `limit`.
fn day_folders(sessions: &Path, limit: usize) -> Vec<PathBuf> {
    let sorted = |dir: &Path| {
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        names.sort();
        names.reverse();
        names
    };
    let mut days = Vec::new();
    for year in sorted(sessions) {
        for month in sorted(&year) {
            for day in sorted(&month) {
                if days.len() >= limit {
                    return days;
                }
                days.push(day);
            }
        }
    }
    days
}

/// A Codex conversation: when it started (RFC 3339, UTC) and its id.
#[derive(Clone, Debug, PartialEq)]
pub struct Started {
    pub at: String,
    pub id: String,
}

/// Codex conversations run in `root` that were written to since `since`.
pub fn codex_conversations(root: &Path, since: SystemTime) -> Vec<Started> {
    let Some(home) = codex_home() else {
        return Vec::new();
    };
    // A pane that's been open for days is rare; a week of folders covers it.
    let mut found = Vec::new();
    for day in day_folders(&home.join("sessions"), 7) {
        for file in std::fs::read_dir(day).into_iter().flatten().flatten() {
            let fresh = file
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m >= since);
            if !fresh || !file.file_name().to_string_lossy().starts_with("rollout-") {
                continue;
            }
            if let Some(started) = codex_meta(&file.path(), root) {
                found.push(started);
            }
        }
    }
    found
}

/// The first line of a Codex conversation file says when and where it started.
fn codex_meta(path: &Path, root: &Path) -> Option<Started> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    let json: serde_json::Value = serde_json::from_str(&line).ok()?;
    if json.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let meta = json.get("payload")?;
    let cwd = PathBuf::from(meta.get("cwd")?.as_str()?);
    let same = cwd == root || std::fs::canonicalize(&cwd).ok() == std::fs::canonicalize(root).ok();
    same.then(|| {
        Some(Started {
            at: meta.get("timestamp")?.as_str()?.to_string(),
            id: meta.get("id")?.as_str()?.to_string(),
        })
    })?
}

/// Which open Codex pane each conversation belongs to: the one that started most
/// recently before it. `panes` are (pane, when it started); the answer is each
/// pane's newest conversation.
pub fn codex_owners(
    panes: &[(usize, String)],
    conversations: &[Started],
) -> HashMap<usize, String> {
    let mut owners: HashMap<usize, &Started> = HashMap::new();
    for conversation in conversations {
        let owner = panes
            .iter()
            .filter(|(_, started)| *started <= conversation.at)
            .max_by(|a, b| a.1.cmp(&b.1));
        if let Some((pane, _)) = owner {
            let newer = owners
                .get(pane)
                .is_none_or(|known| known.at < conversation.at);
            if newer {
                owners.insert(*pane, conversation);
            }
        }
    }
    owners
        .into_iter()
        .map(|(pane, c)| (pane, c.id.clone()))
        .collect()
}

/// `time` as Codex writes it: `2026-06-03T11:26:46.142Z`. A second early, so a
/// conversation that starts as the pane does still counts as the pane's.
pub fn utc(time: SystemTime) -> String {
    let since = time
        .checked_sub(Duration::from_secs(1))
        .unwrap_or(time)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since.as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_like_codex_writes_them() {
        let at = UNIX_EPOCH + Duration::from_millis(1_780_486_007_142);
        // One second early, on purpose.
        assert_eq!(utc(at), "2026-06-03T11:26:46.142Z");
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(1)),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            utc(UNIX_EPOCH + Duration::from_secs(951_782_401)),
            "2000-02-29T00:00:00.000Z"
        );
    }

    #[test]
    fn each_codex_conversation_belongs_to_the_pane_opened_just_before_it() {
        let c = |at: &str, id: &str| Started {
            at: at.into(),
            id: id.into(),
        };
        let panes = [
            (1, "2026-06-03T10:00:00.000Z".to_string()),
            (2, "2026-06-03T10:05:00.000Z".to_string()),
        ];
        let conversations = [
            c("2026-06-03T09:00:00.000Z", "before-both"),
            c("2026-06-03T10:00:02.000Z", "first"),
            c("2026-06-03T10:05:01.000Z", "second"),
        ];
        let owners = codex_owners(&panes, &conversations);
        assert_eq!(owners.get(&1).map(String::as_str), Some("first"));
        assert_eq!(owners.get(&2).map(String::as_str), Some("second"));
        // A new conversation in the newest pane (/new) replaces its old one.
        let mut more = conversations.to_vec();
        more.push(c("2026-06-03T11:00:00.000Z", "renewed"));
        assert_eq!(
            codex_owners(&panes, &more).get(&2).map(String::as_str),
            Some("renewed")
        );
        // A pane that never sent a message has nothing to reopen.
        assert!(codex_owners(&[(3, "2026-06-03T12:00:00.000Z".into())], &conversations).is_empty());
    }

    #[test]
    fn codex_conversations_are_read_from_their_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let file = dir.path().join("rollout.jsonl");
        let meta = |cwd: &Path| {
            serde_json::json!({
                "timestamp": "2026-06-03T11:26:46.392Z",
                "type": "session_meta",
                "payload": {"id": "019e0000-aaaa-7bbb-8ccc-dddddddddddd",
                            "timestamp": "2026-06-03T11:26:46.142Z",
                            "cwd": cwd, "base_instructions": "x".repeat(10_000)}
            })
            .to_string()
        };
        std::fs::write(&file, meta(&root) + "\n{\"type\":\"event_msg\"}\n").unwrap();
        assert_eq!(
            codex_meta(&file, &root),
            Some(Started {
                at: "2026-06-03T11:26:46.142Z".into(),
                id: "019e0000-aaaa-7bbb-8ccc-dddddddddddd".into()
            })
        );
        std::fs::write(&file, meta(dir.path()) + "\n").unwrap();
        assert_eq!(
            codex_meta(&file, &root),
            None,
            "another folder's conversation"
        );
    }

    #[test]
    fn gemini_versions_that_take_session_ids() {
        assert_eq!(version_at_least("0.46.0", GEMINI_SESSION_IDS), Some(true));
        assert_eq!(
            version_at_least("0.47.0-nightly.20260101", GEMINI_SESSION_IDS),
            Some(true)
        );
        assert_eq!(version_at_least("1.0.0", GEMINI_SESSION_IDS), Some(true));
        assert_eq!(version_at_least("0.45.2", GEMINI_SESSION_IDS), Some(false));
        assert_eq!(version_at_least("nightly", GEMINI_SESSION_IDS), None);
    }

    #[test]
    fn gemini_conversations_are_found_by_id() {
        let home = tempfile::tempdir().unwrap();
        let id = "0cb94691-1111-4222-8333-444444444444";
        let chats = home.path().join("tmp/abc123/chats");
        std::fs::create_dir_all(&chats).unwrap();
        assert!(!gemini_conversation_in(home.path(), id));
        std::fs::write(
            chats.join("session-2025-11-04T12-33-0cb94691.json"),
            format!(r#"{{"sessionId": "{id}", "messages": []}}"#),
        )
        .unwrap();
        assert!(gemini_conversation_in(home.path(), id));
        assert!(!gemini_conversation_in(
            home.path(),
            "0cb94691-9999-4999-8999-999999999999"
        ));
    }
}

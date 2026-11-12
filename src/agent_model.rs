//! The model and effort an AI pane's agent works with, shown at the bottom right of
//! its pane ("Opus 5.5 · xhigh"). Read from the conversation the agent writes:
//! Claude Code records both with every reply, Codex with every turn, so a model or
//! effort you switch to shows once the next answer starts.
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

/// How much of a conversation's end is read first, and at most: a reply is
/// usually within the last few, but a long tool output can come after it.
const TAIL: u64 = 256 * 1024;
const MOST: u64 = 4 * 1024 * 1024;

/// Which CLI wrote a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InUse {
    pub model: String,
    /// "low" to "max", as the agent names it; none when it isn't recorded.
    pub effort: Option<String>,
}

impl InUse {
    /// " Opus 5.5 · xhigh ", or the model alone when only that fits in `room`
    /// columns, or nothing.
    pub fn label(&self, room: usize) -> Option<String> {
        let model = model_name(&self.model);
        let full = match &self.effort {
            Some(effort) => format!(" {model} · {effort} "),
            None => format!(" {model} "),
        };
        let short = format!(" {model} ");
        [full, short]
            .into_iter()
            .find(|label| label.chars().count() <= room)
    }
}

/// A model id as people say it: `claude-opus-5-5` is "Opus 5.5",
/// `claude-3-5-haiku-20241022` "Haiku 3.5", `claude-sonnet-4-5[1m]` "Sonnet 4.5 1M",
/// an alias like `opus` "Opus". Others (`gpt-5.6-sol`) stay as they are.
pub fn model_name(id: &str) -> String {
    let (id, million) = match id.strip_suffix("[1m]") {
        Some(id) => (id, true),
        None => (id, false),
    };
    let claude = id.find("claude-").map(|at| &id[at + "claude-".len()..]);
    let name = match claude {
        Some(rest) => {
            let parts: Vec<&str> = rest.split('-').collect();
            let family = parts
                .iter()
                .find(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphabetic()));
            // Version numbers are short; a date (20241022) isn't one.
            let version: Vec<&str> = parts
                .iter()
                .copied()
                .filter(|p| (1..=2).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit()))
                .collect();
            match family {
                Some(family) if version.is_empty() => capital(family),
                Some(family) => format!("{} {}", capital(family), version.join(".")),
                None => id.to_string(),
            }
        }
        None if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphabetic()) => capital(id),
        None => id.to_string(),
    };
    if million {
        format!("{name} 1M")
    } else {
        name
    }
}

fn capital(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The model and effort last recorded in conversation `path`, read from its end.
pub fn read(agent: Agent, path: &Path) -> Option<InUse> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut window = TAIL;
    loop {
        let start = len.saturating_sub(window);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(len - start)
            .read_to_end(&mut bytes)
            .ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = text.lines().rev().peekable();
        while let Some(line) = lines.next() {
            // The window's first line may start mid-record.
            if start > 0 && lines.peek().is_none() {
                break;
            }
            let found = match agent {
                Agent::Claude => claude(line),
                Agent::Codex => codex(line),
            };
            if found.is_some() {
                return found;
            }
        }
        if start == 0 || window >= MOST {
            return None;
        }
        window *= 4;
    }
}

/// A reply of the main conversation: `message.model` and `effort`. Subagents'
/// replies and Claude Code's own notes (model `<synthetic>`) don't count.
fn claude(line: &str) -> Option<InUse> {
    if !line.contains("\"assistant\"") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    if record["type"] != "assistant" || record["isSidechain"] == true {
        return None;
    }
    let model = record["message"]["model"].as_str()?;
    if model.is_empty() || model.starts_with('<') {
        return None;
    }
    Some(InUse {
        model: model.to_string(),
        effort: text(&record["effort"]),
    })
}

/// A turn's settings: `payload.model` and `payload.effort`.
fn codex(line: &str) -> Option<InUse> {
    if !line.contains("\"turn_context\"") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    if record["type"] != "turn_context" {
        return None;
    }
    let payload = &record["payload"];
    Some(InUse {
        model: text(&payload["model"])?,
        effort: text(&payload["effort"]),
    })
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

/// One AI pane's conversation, read again whenever it grows.
#[derive(Default)]
pub struct Watch {
    conversation: String,
    file: Option<PathBuf>,
    size: Option<u64>,
    pub in_use: Option<InUse>,
}

impl Watch {
    /// Follow `agent`'s `conversation`, its file found with `find`. What was in use
    /// stays until a newer reply says otherwise: `/clear` starts a conversation, not
    /// another model. True when what's shown changed.
    pub fn follow(
        &mut self,
        agent: Agent,
        conversation: &str,
        find: impl FnOnce(&str) -> Option<PathBuf>,
    ) -> bool {
        if self.conversation != conversation {
            self.conversation = conversation.to_string();
            self.file = None;
            self.size = None;
        }
        if self.file.is_none() {
            self.file = find(conversation);
        }
        let Some(file) = &self.file else {
            return false;
        };
        let size = fs::metadata(file).map(|m| m.len()).ok();
        if size == self.size {
            return false;
        }
        self.size = size;
        match read(agent, file) {
            Some(now) if self.in_use.as_ref() != Some(&now) => {
                self.in_use = Some(now);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_read_as_people_say_them() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-fable-5-2"), "Fable 5.2");
        assert_eq!(model_name("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_name("claude-3-5-haiku-20241022"), "Haiku 3.5");
        assert_eq!(model_name("claude-opus-4-20250514"), "Opus 4");
        assert_eq!(model_name("claude-sonnet-4-5[1m]"), "Sonnet 4.5 1M");
        assert_eq!(
            model_name("us.anthropic.claude-opus-4-1-20250805-v1:0"),
            "Opus 4.1"
        );
        assert_eq!(model_name("opus"), "Opus");
        assert_eq!(model_name("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(model_name("llama3.2"), "llama3.2");
    }

    #[test]
    fn the_label_shortens_to_fit() {
        let in_use = InUse {
            model: "claude-opus-5-5".into(),
            effort: Some("xhigh".into()),
        };
        assert_eq!(in_use.label(40).as_deref(), Some(" Opus 5.5 · xhigh "));
        assert_eq!(in_use.label(12).as_deref(), Some(" Opus 5.5 "));
        assert_eq!(in_use.label(5), None);
        let plain = InUse {
            model: "gpt-5.6-sol".into(),
            effort: None,
        };
        assert_eq!(plain.label(40).as_deref(), Some(" gpt-5.6-sol "));
    }

    fn reply(model: &str, effort: &str, sidechain: bool) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":{sidechain},"effort":"{effort}","message":{{"model":"{model}","role":"assistant","content":[]}}}}"#
        )
    }

    #[test]
    fn claude_conversations_give_the_main_conversations_last_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.jsonl");
        let lines = [
            reply("claude-sonnet-4-5", "high", false),
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.to_string(),
            reply("claude-opus-5-5", "xhigh", false),
            // A subagent on another model, and one of Claude Code's own notes.
            reply("claude-haiku-4-5", "low", true),
            reply("<synthetic>", "", false),
            // A tool's output larger than the first look.
            format!(
                r#"{{"type":"user","message":{{"content":"{}"}}}}"#,
                "x".repeat(TAIL as usize * 2)
            ),
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        assert_eq!(
            read(Agent::Claude, &path),
            Some(InUse {
                model: "claude-opus-5-5".into(),
                effort: Some("xhigh".into()),
            })
        );
        // Nothing replied yet: nothing to say.
        std::fs::write(&path, &lines[1]).unwrap();
        assert_eq!(read(Agent::Claude, &path), None);
    }

    #[test]
    fn codex_conversations_give_the_last_turns_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let turn = |model: &str, effort: &str| {
            format!(
                r#"{{"type":"turn_context","payload":{{"model":"{model}","effort":"{effort}","cwd":"/"}}}}"#
            )
        };
        let lines = [
            r#"{"type":"session_meta","payload":{"id":"x"}}"#.to_string(),
            turn("gpt-5.5", "medium"),
            turn("gpt-5.6-sol", "high"),
            r#"{"type":"response_item","payload":{"type":"message"}}"#.to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(
            read(Agent::Codex, &path),
            Some(InUse {
                model: "gpt-5.6-sol".into(),
                effort: Some("high".into()),
            })
        );
    }

    #[test]
    fn a_watch_reads_again_only_when_the_conversation_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.jsonl");
        let mut watch = Watch::default();
        let find = |_: &str| path.exists().then(|| path.clone());
        // Not written yet: nothing.
        assert!(!watch.follow(Agent::Claude, "one", find));
        std::fs::write(&path, reply("claude-opus-5-5", "high", false) + "\n").unwrap();
        assert!(watch.follow(Agent::Claude, "one", find));
        assert!(!watch.follow(Agent::Claude, "one", find), "unchanged");
        // You switched effort: the next reply says so.
        let mut text = std::fs::read_to_string(&path).unwrap();
        text += &(reply("claude-opus-5-5", "max", false) + "\n");
        std::fs::write(&path, text).unwrap();
        assert!(watch.follow(Agent::Claude, "one", find));
        assert_eq!(
            watch.in_use.as_ref().unwrap().effort.as_deref(),
            Some("max")
        );
        // A new conversation (/clear) keeps showing the model until it replies.
        assert!(!watch.follow(Agent::Claude, "two", |_| None));
        assert_eq!(watch.in_use.as_ref().unwrap().model, "claude-opus-5-5");
    }
}

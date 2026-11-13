//! The model and effort an AI pane's agent works with, shown at the bottom right of
//! its pane ("Opus 5.5 · xhigh"). Read from the conversation the agent writes:
//! Claude Code records both with every reply and records what `/model` and
//! `/effort` switched to as soon as you switch; Codex records its settings each
//! turn and whenever you change them. So a switch shows within a moment.
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

/// What a conversation says is in use. A reply or a Codex turn says both; a
/// switch (`/model`, `/effort`) may say only one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Said {
    pub model: Option<String>,
    /// `Some(None)`: said, and it's not recorded.
    pub effort: Option<Option<String>>,
}

impl Said {
    /// What's in use after this, given what was: a part not said stays as it was.
    /// (Claude Code keeps the effort when you switch model.)
    pub fn over(self, was: Option<&InUse>) -> Option<InUse> {
        let model = self.model.or_else(|| was.map(|w| w.model.clone()))?;
        let effort = match self.effort {
            Some(effort) => effort,
            None => was.and_then(|w| w.effort.clone()),
        };
        Some(InUse { model, effort })
    }
}

/// The model and effort last recorded in conversation `path`, read from its end:
/// each from the newest record that says it.
pub fn read(agent: Agent, path: &Path) -> Option<Said> {
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
        let mut said = Said::default();
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
            if let Some(found) = found {
                said.model = said.model.or(found.model);
                said.effort = said.effort.or(found.effort);
                if said.model.is_some() && said.effort.is_some() {
                    return Some(said);
                }
            }
        }
        if start == 0 || window >= MOST {
            return (said != Said::default()).then_some(said);
        }
        window *= 4;
    }
}

/// A reply of the main conversation (`message.model` and `effort`), or what a
/// `/model` or `/effort` there switched to. Subagents' replies, Claude Code's own
/// notes (model `<synthetic>`) and text that only quotes a switch don't count.
fn claude(line: &str) -> Option<Said> {
    if !line.contains("\"assistant\"") && !line.contains("local-command-stdout") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    if record["isSidechain"] == true {
        return None;
    }
    match record["type"].as_str()? {
        "assistant" => {
            let model = record["message"]["model"].as_str()?;
            if model.is_empty() || model.starts_with('<') {
                return None;
            }
            Some(Said {
                model: Some(model.to_string()),
                effort: Some(text(&record["effort"])),
            })
        }
        // A command's output is a message of its own, never part of a prompt
        // or a tool's result.
        "user" => switched(record["message"]["content"].as_str()?),
        "system" if record["subtype"] == "local_command" => switched(record["content"].as_str()?),
        _ => None,
    }
}

/// What Claude Code said a `/model` or `/effort` switched to:
/// "Set model to `Opus 5.5` with `high` effort", "Set effort level to xhigh (…)".
fn switched(content: &str) -> Option<Said> {
    let out = content.trim().strip_prefix("<local-command-stdout>")?;
    let out = out.strip_suffix("</local-command-stdout>").unwrap_or(out);
    let model = ["Set model to ", "Kept model as ", "Current model: "]
        .iter()
        .find_map(|lead| out.strip_prefix(lead))
        .or_else(|| {
            // "Fast mode ON · model set to `…`"
            let at = out.find(" model set to ")?;
            Some(&out[at + " model set to ".len()..])
        })
        .and_then(|rest| display_name(&quoted(rest)));
    let effort = said_effort(out);
    let said = Said {
        model,
        effort: effort.map(Some),
    };
    (said != Said::default()).then_some(said)
}

/// The effort a command's output names, if any.
fn said_effort(out: &str) -> Option<String> {
    let word = |rest: &str| {
        let rest = strip_styles(rest);
        let word: String = rest
            .trim_start_matches(['`', '\''])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        (!word.is_empty()).then_some(word)
    };
    if let Some(rest) = out.strip_prefix("Set effort level to ") {
        return word(rest);
    }
    if let Some(rest) = out.strip_prefix("Current effort level: ") {
        return word(rest);
    }
    // Over a cap: "Effort 'max' exceeds the cap for …; set to 'xhigh' instead".
    if out.starts_with("Effort '") {
        let at = out.find("; set to '")?;
        return word(&out[at + "; set to ".len()..]);
    }
    if let Some(rest) = out.strip_prefix("Effort level: auto (currently ") {
        return word(rest);
    }
    if [
        "Effort level: auto",
        "Effort level set to auto",
        "Effort set to auto",
    ]
    .iter()
    .any(|lead| out.starts_with(lead))
    {
        return Some("auto".into());
    }
    // The model picker's: "… with `high` effort", "… · `high` effort for this
    // session only", "… · effort stays high (set by CLAUDE_CODE_EFFORT_LEVEL)".
    if let Some(at) = out.find("` effort") {
        let open = out[..at].rfind('`')?;
        return word(&out[open + 1..at]);
    }
    let at = out.find("effort stays ")?;
    word(&out[at + "effort stays ".len()..])
}

/// The name at the start of `rest`: in backticks, in bold (older versions), or up
/// to what follows it.
fn quoted(rest: &str) -> String {
    let ticks = rest.chars().take_while(|c| *c == '`').count();
    if ticks > 0 {
        let fence = "`".repeat(ticks);
        if let Some(end) = rest[ticks..].find(&fence) {
            return rest[ticks..ticks + end].trim().to_string();
        }
    }
    if let Some(bold) = rest.strip_prefix("\x1b[1m") {
        if let Some(end) = bold.find("\x1b[22m") {
            return bold[..end].to_string();
        }
    }
    let rest = strip_styles(rest);
    let end = [" and saved", " for this session", " with ", " · ", "\n"]
        .iter()
        .filter_map(|tail| rest.find(tail))
        .min()
        .unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// A picker's name as a model: "Opus 4.6 (1M context)" is "Opus 4.6 1M"; a
/// default that doesn't say which model it is says nothing.
fn display_name(name: &str) -> Option<String> {
    let name = strip_styles(name).trim().to_string();
    let name = match name.strip_suffix(" (1M context)") {
        Some(model) => format!("{model} 1M"),
        None => name,
    };
    let name = match name
        .strip_prefix("Default (")
        .and_then(|n| n.strip_suffix(')'))
    {
        Some(inner) if inner != "recommended" => inner.to_string(),
        Some(_) => return None,
        None if name == "Default" || name.is_empty() => return None,
        None => name,
    };
    Some(name)
}

/// `text` without terminal colors and styles.
fn strip_styles(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A turn's settings (`payload.model`, `payload.effort`), or settings you changed
/// (`thread_settings_applied`: `model`, `reasoning_effort`).
fn codex(line: &str) -> Option<Said> {
    if !line.contains("\"turn_context\"") && !line.contains("\"thread_settings_applied\"") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    let payload = &record["payload"];
    let (model, effort) = match record["type"].as_str()? {
        "turn_context" => (&payload["model"], &payload["effort"]),
        "event_msg" if payload["type"] == "thread_settings_applied" => {
            let settings = &payload["thread_settings"];
            (&settings["model"], &settings["reasoning_effort"])
        }
        _ => return None,
    };
    Some(Said {
        model: Some(text(model)?),
        effort: Some(text(effort)),
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
    /// stays until the conversation says otherwise: `/clear` starts a conversation,
    /// not another model. True when what's shown changed.
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
        let Some(said) = read(agent, file) else {
            return false;
        };
        let now = said.over(self.in_use.as_ref());
        let changed = now != self.in_use;
        self.in_use = now;
        changed
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
            Some(both("claude-opus-5-5", "xhigh"))
        );
        // Nothing replied yet: nothing to say.
        std::fs::write(&path, &lines[1]).unwrap();
        assert_eq!(read(Agent::Claude, &path), None);
    }

    fn both(model: &str, effort: &str) -> Said {
        Said {
            model: Some(model.into()),
            effort: Some(Some(effort.into())),
        }
    }

    /// A command's output as Claude Code records it now, a message of its own.
    fn output(text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "isSidechain": false,
            "message": {"role": "user", "content": format!("<local-command-stdout>{text}</local-command-stdout>")},
        })
        .to_string()
    }

    #[test]
    fn claude_switches_show_before_the_next_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.jsonl");
        let mut lines = vec![reply("claude-opus-5-5", "high", false)];
        let mut now = |line: String| {
            lines.push(line);
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            read(Agent::Claude, &path)
        };
        // /effort keeps the model.
        assert_eq!(
            now(output("Set effort level to xhigh (saved as your default for new sessions): Deeper reasoning than high")),
            Some(both("claude-opus-5-5", "xhigh"))
        );
        // /model keeps the effort.
        assert_eq!(
            now(output(
                "Set model to `Sonnet 5.5` and saved as your default for new sessions"
            )),
            Some(both("Sonnet 5.5", "xhigh"))
        );
        // The picker sets both.
        assert_eq!(
            now(output(
                "Set model to `Haiku 4.5` for this session only with `low` effort"
            )),
            Some(both("Haiku 4.5", "low"))
        );
        assert_eq!(
            now(output("Set model to `Opus 5.5` and saved as your default for new sessions · `max` effort for this session only")),
            Some(both("Opus 5.5", "max"))
        );
        // What only quotes a switch: a prompt, a tool's output, a subagent's.
        let quoted = "<local-command-stdout>Set model to `Fable 5.2`</local-command-stdout>";
        now(serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "content": quoted}]}})
        .to_string());
        now(
            serde_json::json!({"type": "user", "message": {"content": format!("why {quoted}")}})
                .to_string(),
        );
        assert_eq!(
            now(serde_json::json!({"type": "user", "isSidechain": true,
                "message": {"content": quoted}})
            .to_string()),
            Some(both("Opus 5.5", "max"))
        );
        // Older versions: a system note, the name in bold.
        assert_eq!(
            now(serde_json::json!({"type": "system", "subtype": "local_command",
                "content": "<local-command-stdout>Kept model as \u{1b}[1mOpus 4.7\u{1b}[22m</local-command-stdout>"}).to_string()),
            Some(both("Opus 4.7", "max"))
        );
        assert_eq!(
            now(output("Effort level set to auto")),
            Some(both("Opus 4.7", "auto"))
        );
        // The next reply says what it ran on.
        assert_eq!(
            now(reply("claude-opus-5-5", "high", false)),
            Some(both("claude-opus-5-5", "high"))
        );
    }

    #[test]
    fn what_a_switch_leaves_unsaid_stays_as_it_was() {
        let was = InUse {
            model: "claude-opus-5-5".into(),
            effort: Some("xhigh".into()),
        };
        let shown = |said: Said| said.over(Some(&was)).and_then(|now| now.label(40));
        let model = |model: &str| Said {
            model: Some(model.into()),
            effort: None,
        };
        assert_eq!(
            shown(model("Opus 5.5")).as_deref(),
            Some(" Opus 5.5 · xhigh ")
        );
        // Another model keeps the effort, as Claude Code does.
        assert_eq!(
            shown(model("Sonnet 5.5")).as_deref(),
            Some(" Sonnet 5.5 · xhigh ")
        );
        let effort = Said {
            model: None,
            effort: Some(Some("low".into())),
        };
        assert_eq!(shown(effort.clone()).as_deref(), Some(" Opus 5.5 · low "));
        // An effort alone, with no model known, shows nothing.
        assert_eq!(effort.over(None), None);
    }

    #[test]
    fn switch_messages_read_as_model_and_effort() {
        let said = |text: &str| {
            switched(&format!(
                "<local-command-stdout>{text}</local-command-stdout>"
            ))
        };
        assert_eq!(
            said("Set model to `Opus 4.6 (1M context)` for this session only")
                .and_then(|s| s.model),
            Some("Opus 4.6 1M".into())
        );
        assert_eq!(
            said("Set model to `claude-opus-5-5`").and_then(|s| s.model),
            Some("claude-opus-5-5".into())
        );
        assert_eq!(
            said("Fast mode ON · model set to `Opus 5.5`").and_then(|s| s.model),
            Some("Opus 5.5".into())
        );
        assert_eq!(
            said("Current effort level: medium (Balanced)"),
            Some(Said {
                model: None,
                effort: Some(Some("medium".into()))
            })
        );
        assert_eq!(
            said("Effort 'max' exceeds the cap for Opus 5.5 set by your settings or organization; set to 'xhigh' instead (this session only): x"),
            Some(Said { model: None, effort: Some(Some("xhigh".into())) })
        );
        assert_eq!(
            said("Set model to `Opus 5.5` · effort stays high (set by CLAUDE_CODE_EFFORT_LEVEL)"),
            Some(both("Opus 5.5", "high"))
        );
        // Says which model only by default: nothing about the model.
        assert_eq!(said("Set model to `Default (recommended)`"), None);
        assert_eq!(said("No conversations found to resume"), None);
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
        assert_eq!(read(Agent::Codex, &path), Some(both("gpt-5.6-sol", "high")));
        // /model applies its pick at once, before the next turn.
        let picked = serde_json::json!({"type": "event_msg", "payload": {
            "type": "thread_settings_applied",
            "thread_settings": {"model": "gpt-5.5", "reasoning_effort": "xhigh", "cwd": "/"}}});
        std::fs::write(&path, lines.join("\n") + "\n" + &picked.to_string()).unwrap();
        assert_eq!(read(Agent::Codex, &path), Some(both("gpt-5.5", "xhigh")));
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
        // Where /effort comes first, it keeps the model shown.
        let two = dir.path().join("two.jsonl");
        std::fs::write(
            &two,
            output("Set effort level to low (this session only): Fast") + "\n",
        )
        .unwrap();
        assert!(watch.follow(Agent::Claude, "two", |_| Some(two.clone())));
        assert_eq!(
            watch.in_use,
            Some(InUse {
                model: "claude-opus-5-5".into(),
                effort: Some("low".into()),
            })
        );
        // Then /model: the effort set before it still holds.
        let mut text = std::fs::read_to_string(&two).unwrap();
        text += &(output("Set model to `Sonnet 5.5` for this session only") + "\n");
        std::fs::write(&two, text).unwrap();
        assert!(watch.follow(Agent::Claude, "two", |_| Some(two.clone())));
        assert_eq!(
            watch.in_use,
            Some(InUse {
                model: "Sonnet 5.5".into(),
                effort: Some("low".into()),
            })
        );
    }
}

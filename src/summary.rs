//! An AI's reading of the changes under review, asked for only when you press `s`
//! in the review (it costs tokens): a sentence per file and a short account of the
//! whole change. It runs the AI CLI you have, without tools or a session: Claude
//! Code on its small, quick model, Codex, Gemini, or a local model with Ollama, on
//! the model you choose. Asked again, it's sent only the files whose changes it
//! hasn't read; what it said about the others is kept.
use crate::review::{run_for, Body, FileDiff, Kind, Status};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

/// The diff sent is cut off here, and each file's part of it here.
const MAX_DIFF: usize = 60_000;
const MAX_FILE: usize = 16_000;
const TIMEOUT: Duration = Duration::from_secs(240);

/// What the AI said about one file, and which of its changes it read then.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Note {
    pub key: u64,
    pub text: String,
    /// Its changes were too large: it read only the first part of them.
    pub partial: bool,
}

/// How much of a file's diff fit in a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Whole,
    /// The first part; the rest was cut off.
    Part,
    /// None of it: the request was full.
    Out,
}

/// What the AI said about the changes.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// By Git path.
    pub files: HashMap<String, Note>,
    pub overall: String,
    /// All the changes when the overall was written (see `key`): when they differ
    /// now, it's out of date.
    pub key: u64,
    /// Who wrote it: "Claude", "Codex" or the command.
    pub by: String,
    /// Files whose diff went with the last request; the rest were explained before.
    pub sent: usize,
    /// Files with new changes that didn't fit in the last request; `s` sends them.
    pub left_out: Vec<String>,
    /// What the last request cost, when the AI says.
    pub usage: Option<Usage>,
}

/// Tokens a request used, and its price when the AI tells it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// Everything read, cached parts included.
    pub input: u64,
    pub output: u64,
    pub cost: Option<f64>,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cost = match (self.cost, other.cost) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        };
    }
    /// "3.6k tokens ($0.003)".
    pub fn describe(&self) -> String {
        let total = self.input + self.output;
        let tokens = match total {
            0..1_000 => total.to_string(),
            1_000..1_000_000 => format!("{:.1}k", total as f64 / 1e3),
            _ => format!("{:.1}M", total as f64 / 1e6),
        };
        match self.cost {
            Some(cost) if cost >= 0.01 => format!("{tokens} tokens (${cost:.2})"),
            Some(cost) => format!("{tokens} tokens (${cost:.3})"),
            None => format!("{tokens} tokens"),
        }
    }
}

impl Summary {
    /// What it said about `file`, and whether that was about the changes it has now.
    pub fn note(&self, file: &FileDiff) -> Option<(&str, bool)> {
        let note = self.files.get(&file.path)?;
        Some((note.text.as_str(), note.key == file_key(file)))
    }
}

/// Which AI summarizes, and on which model (`None`: its own default; Claude
/// Code's is Haiku here, the local one the first model Ollama has).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude(Option<String>),
    Codex(Option<String>),
    Gemini(Option<String>),
    /// Ollama, the way the AI launcher's Local LLM entry finds it.
    Local(Option<String>),
    /// A command of your own: the prompt on its input, the answer on its output.
    Command(Vec<String>),
}

/// What `"auto"` tries, in order.
const AUTO: [&str; 4] = ["claude", "codex", "gemini", "local"];

impl Agent {
    /// One `review_ai` entry: `"claude"`, `"codex"`, `"gemini"` or `"local"`,
    /// each optionally with `:model`, or a command.
    pub fn parse(entry: &str) -> Self {
        let entry = entry.trim();
        let (name, model) = match entry.split_once(':') {
            Some((name, model)) => (
                name,
                Some(model.trim().to_string()).filter(|m| !m.is_empty()),
            ),
            None => (entry, None),
        };
        match name {
            "claude" => Self::Claude(model),
            "codex" => Self::Codex(model),
            "gemini" => Self::Gemini(model),
            "local" | "ollama" => Self::Local(model),
            _ => Self::Command(entry.split_whitespace().map(str::to_string).collect()),
        }
    }
    /// The first of config's `review_ai` entries installed here (`"auto"`: Claude
    /// Code, Codex, Gemini, a local model). `None` when they're off.
    pub fn choose(entries: &[String]) -> Result<Option<Self>, String> {
        let entries: Vec<&str> = entries
            .iter()
            .map(|e| e.trim())
            .filter(|e| !e.is_empty())
            .collect();
        if entries.is_empty()
            || entries
                .iter()
                .any(|e| matches!(*e, "off" | "false" | "none"))
        {
            return Ok(None);
        }
        let tried: Vec<&str> = entries
            .iter()
            .flat_map(|e| {
                if *e == "auto" {
                    AUTO.to_vec()
                } else {
                    vec![*e]
                }
            })
            .collect();
        tried
            .iter()
            .map(|e| Self::parse(e))
            .find(Self::installed)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "No AI to explain with: {} not installed · set review_ai in config.toml",
                    tried.join(", ")
                )
            })
    }
    fn program(&self) -> &str {
        match self {
            Self::Claude(_) => "claude",
            Self::Codex(_) => "codex",
            Self::Gemini(_) => "gemini",
            Self::Local(_) => "ollama",
            Self::Command(words) => words.first().map_or("", String::as_str),
        }
    }
    fn installed(&self) -> bool {
        // Tests never pick a real AI, which would spend tokens: the review explains
        // agent changes by itself, and tests read your config. Only commands of
        // their own answer there (the real one is `real_ai_summarizes_a_real_change`).
        if cfg!(test) && !matches!(self, Self::Command(_)) {
            return false;
        }
        // `local` on your model server is asked over HTTP, with curl.
        if matches!(self, Self::Local(_)) && crate::model_server::configured() {
            return crate::platform::which("curl").is_some();
        }
        crate::platform::which(self.program()).is_some()
    }
    /// "Claude (haiku)", "Codex", "Local (llama3.2)".
    pub fn name(&self) -> String {
        let (name, model) = match self {
            Self::Claude(model) => ("Claude", Some(model.as_deref().unwrap_or(CLAUDE_MODEL))),
            Self::Codex(model) => ("Codex", model.as_deref()),
            Self::Gemini(model) => ("Gemini", model.as_deref()),
            Self::Local(model) => match crate::model_server::name() {
                // "gpu-box (qwen3:8b)"
                Some(server) => {
                    return match model {
                        Some(model) => format!("{server} ({model})"),
                        None => server,
                    }
                }
                None => ("Local", model.as_deref()),
            },
            Self::Command(_) => (self.program(), None),
        };
        match model {
            Some(model) => format!("{name} ({model})"),
            None => name.to_string(),
        }
    }
}

/// Claude Code's model unless you choose one: quick, and cheap for a summary.
const CLAUDE_MODEL: &str = "haiku";

/// Which changes a file has, to tell whether what was said about it still holds.
pub fn file_key(file: &FileDiff) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (&file.path, file.status as u8).hash(&mut hasher);
    for hunk in file.hunks() {
        for line in &hunk.lines {
            (line.kind as u8, &line.text).hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Which changes these are, all files together.
pub fn key(files: &[FileDiff]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in files {
        file_key(file).hash(&mut hasher);
    }
    hasher.finish()
}

/// The changes as a unified diff, cut down to a size worth sending, and how much
/// of each file's went in.
fn diff(files: &[&FileDiff]) -> (String, Vec<Fit>) {
    let mut out = String::new();
    let mut fits = vec![Fit::Out; files.len()];
    for (i, file) in files.iter().enumerate() {
        let status = match file.status {
            Status::Added => "new file",
            Status::Deleted => "deleted",
            Status::Modified => "modified",
        };
        let mut part = format!(
            "=== {} ({status}, +{} -{})\n",
            file.path, file.added, file.removed
        );
        if matches!(file.body, Body::Binary) {
            part.push_str("(binary or very large)\n");
        }
        for hunk in file.hunks() {
            let span = hunk.span;
            part.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                span.old.0 + 1,
                span.old.1 - span.old.0,
                span.new.0 + 1,
                span.new.1 - span.new.0
            ));
            for line in &hunk.lines {
                let sign = match line.kind {
                    Kind::Added => '+',
                    Kind::Removed => '-',
                    Kind::Context => ' ',
                };
                part.push(sign);
                part.push_str(&line.text);
                part.push('\n');
            }
            if part.len() > MAX_FILE {
                break;
            }
        }
        fits[i] = Fit::Whole;
        if part.len() > MAX_FILE {
            part.truncate(floor(&part, MAX_FILE));
            part.push_str("\n[… rest of this file's changes left out]\n");
            fits[i] = Fit::Part;
        }
        if out.len() + part.len() > MAX_DIFF {
            fits[i] = Fit::Out;
            out.push_str("[… more files left out]\n");
            break;
        }
        out.push_str(&part);
    }
    (out, fits)
}

fn floor(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The prompt for the diffs of `files`; `known` are other files under review and
/// what was said about them before, for the overall account. With it, how much
/// of each file's diff went in.
pub fn prompt(files: &[&FileDiff], known: &[(&str, &str)]) -> (String, Vec<Fit>) {
    let mut out = String::from(
        "An AI coding agent just changed a developer's project. Explain the changes \
briefly to the developer, who is reviewing them to keep or undo.\n\n\
Answer in exactly this form, as plain text without Markdown:\n\
FILE <the path exactly as written after ===>: <one short sentence, at most 25 words: what changed in this file and how it works>\n\
(one FILE line for every file in the diff below)\n\
OVERALL: <two to four short sentences about the whole change, every file listed here included: what it does, how the pieces work together, and anything worth checking>\n",
    );
    if !known.is_empty() {
        out.push_str("\nFiles explained before, unchanged since:\n");
        for (path, said) in known {
            out.push_str(&format!("FILE {path}: {said}\n"));
        }
    }
    let mut fits = Vec::new();
    if files.is_empty() {
        out.push_str("\nNo file has new changes: write only the OVERALL line.\n");
    } else {
        let (diff, fit) = diff(files);
        out.push('\n');
        out.push_str(&diff);
        fits = fit;
    }
    (out, fits)
}

/// Ask `agent` about the changes, off the UI thread: only the files whose changes
/// aren't the ones `before` explained. `None` when there's nothing new to explain.
pub fn start(
    agent: Agent,
    files: &[FileDiff],
    before: Option<&Summary>,
) -> Option<Receiver<Result<Summary, String>>> {
    let said = |file: &FileDiff| before.and_then(|s| s.note(file)).filter(|(_, now)| *now);
    let todo: Vec<&FileDiff> = files.iter().filter(|f| said(f).is_none()).collect();
    let current = before.is_some_and(|s| s.key == key(files));
    if todo.is_empty() && current {
        return None;
    }
    let known: Vec<(&str, &str)> = files
        .iter()
        .filter_map(|f| said(f).map(|(text, _)| (f.path.as_str(), text)))
        .collect();
    let (prompt, fits) = prompt(&todo, &known);
    let key = key(files);
    let left_out: Vec<String> = todo
        .iter()
        .zip(&fits)
        .filter(|(_, fit)| **fit == Fit::Out)
        .map(|(f, _)| f.path.clone())
        .collect();
    let keys: Vec<(String, u64, bool)> = todo
        .iter()
        .zip(&fits)
        .filter(|(_, fit)| **fit != Fit::Out)
        .map(|(f, fit)| (f.path.clone(), file_key(f), *fit == Fit::Part))
        .collect();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let answer = ask(&agent, &prompt).map(|(text, usage, by)| {
            let paths: Vec<String> = keys.iter().map(|(p, ..)| p.clone()).collect();
            let (said, overall) = parse(&text, &paths);
            let files = keys
                .iter()
                .filter_map(|(path, key, partial)| {
                    let text = said.get(path)?.clone();
                    let note = Note {
                        key: *key,
                        text,
                        partial: *partial,
                    };
                    Some((path.clone(), note))
                })
                .collect();
            Summary {
                files,
                overall,
                key,
                by,
                sent: keys.len(),
                left_out,
                usage,
            }
        });
        let _ = sender.send(answer);
    });
    Some(receiver)
}

/// Run the agent outside the project, in an empty folder, so it reads nothing but
/// the prompt: the answer, what it cost when the AI says, and who answered.
fn ask(agent: &Agent, prompt: &str) -> Result<(String, Option<Usage>, String), String> {
    // `local` with a model server: its OpenAI API answers, and says what it used.
    if let (Agent::Local(model), true) = (agent, crate::model_server::configured()) {
        let (text, usage, by) = crate::model_server::chat(model.as_deref(), prompt, TIMEOUT)
            .map_err(|e| format!("{e:#}"))?;
        if text.trim().is_empty() {
            return Err(format!("{by} answered nothing"));
        }
        return Ok((text, Some(usage), by));
    }
    let quiet = std::env::temp_dir().join(format!("raido-summary-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&quiet);
    let answer_file = quiet.join("answer.txt");
    let found = |program: &str| {
        crate::platform::which(program).ok_or_else(|| format!("{program} isn't installed"))
    };
    let mut by = agent.name();
    let mut command = Command::new(found(agent.program())?);
    match agent {
        Agent::Claude(model) => {
            command.args(["-p", "--model", model.as_deref().unwrap_or(CLAUDE_MODEL)]);
            command.args([
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--output-format",
                "json",
            ]);
        }
        Agent::Codex(model) => {
            command.args(["exec", "--ephemeral", "--skip-git-repo-check"]);
            command.args(["--sandbox", "read-only", "--color", "never", "--json"]);
            if let Some(model) = model {
                command.args(["--model", model]);
            }
            command.arg("-o").arg(&answer_file).arg("-");
        }
        Agent::Gemini(model) => {
            // The prompt goes on its input; this line is added after it.
            command.args(["-p", "Answer as asked above.", "-o", "json"]);
            command.args(["--approval-mode", "plan"]);
            if let Some(model) = model {
                command.args(["--model", model]);
            }
        }
        Agent::Local(model) => {
            let model = match model {
                Some(model) => model.clone(),
                None => crate::local_ai::ollama_models()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .next()
                    .ok_or("Ollama has no models yet · pull one: ollama pull qwen3")?,
            };
            by = format!("Local ({model})");
            command.args(["run", &model, "--hidethinking", "--nowordwrap"]);
        }
        Agent::Command(words) => {
            command.args(&words[1..]);
        }
    }
    command.current_dir(&quiet);
    let output = run_for(&mut command, Some(prompt.as_bytes()), TIMEOUT, &by);
    let answer = (|| -> Result<(String, Option<Usage>), String> {
        let stdout = String::from_utf8_lossy(&output?).into_owned();
        Ok(match agent {
            Agent::Claude(_) => {
                let (text, usage) = claude_answer(&stdout)?;
                (text, Some(usage))
            }
            Agent::Codex(_) => {
                let text = std::fs::read_to_string(&answer_file).map_err(|e| e.to_string())?;
                (text, codex_usage(&stdout))
            }
            Agent::Gemini(_) => {
                let (text, usage) = gemini_answer(&stdout)?;
                (text, Some(usage))
            }
            Agent::Local(_) | Agent::Command(_) => (stdout, None),
        })
    })();
    let _ = std::fs::remove_dir_all(&quiet);
    let (text, usage) = answer?;
    if text.trim().is_empty() {
        return Err(format!("{by} answered nothing"));
    }
    Ok((text, usage, by))
}

/// Gemini CLI's JSON answer (`-o json`): the text, and the tokens of every model it
/// used.
fn gemini_answer(json: &str) -> Result<(String, Usage), String> {
    // Anything it prints before the JSON (warnings) is skipped.
    let json = json.find('{').map_or(json, |at| &json[at..]);
    let value: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|_| "Gemini answered in an unexpected form")?;
    if let Some(message) = value["error"]["message"].as_str() {
        return Err(format!("Gemini: {message}"));
    }
    let mut usage = Usage::default();
    if let Some(models) = value["stats"]["models"].as_object() {
        for model in models.values() {
            let tokens = &model["tokens"];
            let count = |key: &str| tokens[key].as_u64().unwrap_or(0);
            usage.input += count("prompt");
            usage.output += count("candidates") + count("thoughts");
        }
    }
    Ok((
        value["response"].as_str().unwrap_or_default().to_string(),
        usage,
    ))
}

/// Claude Code's JSON answer: the text, and the tokens and price it took.
fn claude_answer(json: &str) -> Result<(String, Usage), String> {
    let value: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|_| "Claude answered in an unexpected form")?;
    let text = value["result"].as_str().unwrap_or_default().to_string();
    if value["is_error"].as_bool() == Some(true) {
        return Err(format!(
            "Claude: {}",
            text.lines().next().unwrap_or("failed")
        ));
    }
    let usage = &value["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let usage = Usage {
        input: count("input_tokens")
            + count("cache_creation_input_tokens")
            + count("cache_read_input_tokens"),
        output: count("output_tokens"),
        cost: value["total_cost_usd"].as_f64(),
    };
    Ok((text, usage))
}

/// The tokens in Codex's events (`--json`), from the turn that finished.
fn codex_usage(events: &str) -> Option<Usage> {
    events.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        if value["type"] != "turn.completed" {
            return None;
        }
        let usage = &value["usage"];
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        Some(Usage {
            input: count("input_tokens"),
            output: count("output_tokens") + count("reasoning_output_tokens"),
            cost: None,
        })
    })
}

/// The FILE lines by path, and the OVERALL text. An answer not in that form is
/// kept whole as the overall text.
pub fn parse(text: &str, paths: &[String]) -> (HashMap<String, String>, String) {
    let mut files = HashMap::new();
    let mut overall = String::new();
    let mut in_overall = false;
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['*', '-', ' ']).trim();
        let line = line.replace("**", "");
        if let Some(rest) = line.strip_prefix("OVERALL:") {
            in_overall = true;
            overall.push_str(rest.trim());
            continue;
        }
        if let Some(rest) = line.strip_prefix("FILE ") {
            in_overall = false;
            // The longest path the line starts with, so `a.rs` doesn't take `a.rs.bak`.
            let found = paths
                .iter()
                .filter(|p| {
                    rest.strip_prefix(p.as_str())
                        .and_then(|r| r.trim_start().strip_prefix(':'))
                        .is_some()
                })
                .max_by_key(|p| p.len());
            if let Some(path) = found {
                let said = rest[path.len()..].trim_start()[1..].trim();
                files.insert(path.clone(), said.to_string());
            }
            continue;
        }
        if in_overall && !line.is_empty() {
            overall.push(' ');
            overall.push_str(&line);
        }
    }
    if files.is_empty() && overall.is_empty() {
        overall = text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    (files, overall.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_never_pick_a_real_ai() {
        for setting in ["auto", "claude", "codex:gpt-5", "gemini", "local"] {
            assert!(
                Agent::choose(&[setting.to_string()]).is_err(),
                "{setting} would spend tokens"
            );
        }
        let own = Agent::choose(&["sh -c cat".to_string()]).unwrap().unwrap();
        assert_eq!(own.name(), "sh");
    }

    /// A real change explained by `agent` (`RAIDO_AI`, default Claude Code).
    #[test]
    #[ignore = "asks an AI (uses tokens): RAIDO_AI=codex cargo test real_ai -- --ignored"]
    fn real_ai_summarizes_a_real_change() {
        let agent = Agent::parse(&std::env::var("RAIDO_AI").unwrap_or("claude".into()));
        let (_dir, root, mut changes) = crate::review::tests::project();
        crate::review::tests::feature(&mut changes, &root);
        let summary = start(agent, &changes.files, None)
            .unwrap()
            .recv_timeout(TIMEOUT)
            .unwrap()
            .unwrap();
        println!("{summary:#?}");
        assert_eq!(summary.files.len(), 2);
        assert!(!summary.overall.is_empty());
        assert!(summary.usage.is_some_and(|u| u.input > 0));
    }

    #[test]
    fn token_use_is_read_from_claude_and_codex() {
        let claude = r#"{"type":"result","is_error":false,"result":"FILE a: b","total_cost_usd":0.0034,
            "usage":{"input_tokens":2,"cache_creation_input_tokens":2873,"cache_read_input_tokens":649,"output_tokens":120}}"#;
        let (text, usage) = claude_answer(claude).unwrap();
        assert_eq!(text, "FILE a: b");
        assert_eq!((usage.input, usage.output), (3524, 120));
        assert_eq!(usage.describe(), "3.6k tokens ($0.003)");
        let failed =
            r#"{"type":"result","is_error":true,"result":"Not logged in · Please run /login"}"#;
        assert_eq!(
            claude_answer(failed).unwrap_err(),
            "Claude: Not logged in · Please run /login"
        );
        let codex = "{\"type\":\"thread.started\"}\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":12782,\"cached_input_tokens\":10496,\"output_tokens\":5,\"reasoning_output_tokens\":40}}\n";
        let usage = codex_usage(codex).unwrap();
        assert_eq!((usage.input, usage.output, usage.cost), (12782, 45, None));
        assert_eq!(usage.describe(), "12.8k tokens");
        assert_eq!(codex_usage("not json"), None);
        let mut total = usage;
        total.add(Usage {
            input: 1_000_000,
            output: 0,
            cost: Some(0.5),
        });
        assert_eq!(total.describe(), "1.0M tokens ($0.50)");
    }

    #[test]
    fn answers_are_read_by_file_and_overall() {
        let paths = vec!["src/a.rs".to_string(), "src/a.rs.bak".to_string()];
        let text = "Here you go:\nFILE src/a.rs: Adds a parser.\n**FILE src/a.rs.bak:** Backup.\nOVERALL: It parses.\nThen it prints.\n";
        let (files, overall) = parse(text, &paths);
        assert_eq!(files["src/a.rs"], "Adds a parser.");
        assert_eq!(files["src/a.rs.bak"], "Backup.");
        assert_eq!(overall, "It parses. Then it prints.");
        let (files, overall) = parse("Just prose,\nno format.", &paths);
        assert!(files.is_empty());
        assert_eq!(overall, "Just prose, no format.");
    }

    #[test]
    fn settings_name_the_agent_and_its_model() {
        assert_eq!(Agent::parse("claude"), Agent::Claude(None));
        assert_eq!(
            Agent::parse("claude:sonnet"),
            Agent::Claude(Some("sonnet".into()))
        );
        assert_eq!(
            Agent::parse("gemini:gemini-2.5-flash").name(),
            "Gemini (gemini-2.5-flash)"
        );
        assert_eq!(
            Agent::parse("local:qwen3:8b"),
            Agent::Local(Some("qwen3:8b".into()))
        );
        assert_eq!(Agent::parse("codex").name(), "Codex");
        assert_eq!(Agent::parse("claude").name(), "Claude (haiku)");
        assert_eq!(
            Agent::parse("llm -m mini"),
            Agent::Command(vec!["llm".into(), "-m".into(), "mini".into()])
        );
        let list = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Agent::choose(&list(&["off"])), Ok(None));
        assert_eq!(Agent::choose(&list(&[])), Ok(None));
        // The first one installed answers; `sh` is everywhere, the first isn't.
        #[cfg(unix)]
        assert_eq!(
            Agent::choose(&list(&["no-such-ai-here --x", "sh -c cat"])),
            Ok(Some(Agent::parse("sh -c cat")))
        );
        let error = Agent::choose(&list(&["no-such-ai-here"])).unwrap_err();
        assert!(error.contains("no-such-ai-here not installed"), "{error}");
    }

    #[test]
    #[cfg(unix)]
    fn a_command_of_your_own_reads_the_prompt_and_answers() {
        let (text, usage, by) = ask(&Agent::parse("sh -c cat"), "FILE a: b").unwrap();
        assert_eq!(
            (text.as_str(), usage, by.as_str()),
            ("FILE a: b", None, "sh")
        );
        assert!(ask(&Agent::parse("sh -c true"), "x")
            .unwrap_err()
            .contains("answered nothing"));
    }

    #[test]
    fn gemini_answers_and_token_use_are_read() {
        let json = r#"Loaded cached credentials.
{"response":"FILE a: b","stats":{"models":{"gemini-2.5-flash":{"tokens":{"prompt":900,"candidates":80,"total":1000,"cached":0,"thoughts":20}},
"gemini-2.5-flash-lite":{"tokens":{"prompt":100,"candidates":0}}}}}"#;
        let (text, usage) = gemini_answer(json).unwrap();
        assert_eq!(text, "FILE a: b");
        assert_eq!((usage.input, usage.output, usage.cost), (1000, 100, None));
        let failed = r#"{"error":{"type":"Error","message":"Please set an Auth method"}}"#;
        assert_eq!(
            gemini_answer(failed).unwrap_err(),
            "Gemini: Please set an Auth method"
        );
    }

    #[test]
    fn the_prompt_holds_the_diff_and_stays_small() {
        let old = "a\n".repeat(10);
        let new = format!("{}b\n", "a\n".repeat(10));
        let (old, new): (Vec<_>, Vec<_>) = (
            old.split_inclusive('\n').collect(),
            new.split_inclusive('\n').collect(),
        );
        let hunks = crate::review::hunks(&old, &new);
        let file = FileDiff {
            path: "x.txt".into(),
            name: "x.txt".into(),
            absolute: "/x.txt".into(),
            status: Status::Modified,
            by: vec![],
            body: Body::Text(hunks),
            added: 1,
            removed: 0,
            outline: Default::default(),
        };
        let (prompt, fits) = prompt(&[&file], &[]);
        assert_eq!(fits, [Fit::Whole]);
        assert!(prompt.contains("=== x.txt (modified, +1 -0)\n@@ -8,3 +8,4 @@\n a\n a\n a\n+b\n"));
        let many = vec![file.clone(); 2000];
        assert!(prompt.len() < MAX_DIFF + 2000);
        let (all, fits) = super::prompt(&many.iter().collect::<Vec<_>>(), &[]);
        assert!(all.len() < MAX_DIFF + 2000);
        // What didn't fit is known, to say so and to send next time.
        let whole = fits.iter().filter(|f| **f == Fit::Whole).count();
        assert!(whole > 100 && fits[whole..].iter().all(|f| *f == Fit::Out));
        assert_ne!(key(&many[..1]), key(&many[..2]));
        // Asked again, only what's new goes; with nothing new, nothing is asked.
        let mut other = file.clone();
        other.path = "y.txt".into();
        let before = Summary {
            files: [(
                "x.txt".to_string(),
                Note {
                    key: file_key(&file),
                    text: "Adds b.".into(),
                    partial: false,
                },
            )]
            .into(),
            key: key(std::slice::from_ref(&file)),
            ..Default::default()
        };
        let both = [file.clone(), other];
        let said = |f: &FileDiff| before.note(f).filter(|(_, now)| *now).is_some();
        assert!(said(&both[0]) && !said(&both[1]));
        let (again, _) = super::prompt(&[&both[1]], &[("x.txt", "Adds b.")]);
        assert!(again.contains("unchanged since:\nFILE x.txt: Adds b.\n"));
        assert!(again.contains("=== y.txt") && !again.contains("=== x.txt"));
        assert!(start(
            Agent::Claude(None),
            std::slice::from_ref(&file),
            Some(&before)
        )
        .is_none());
    }
}

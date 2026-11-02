//! An AI's reading of the changes under review, asked for only when you press `s`
//! in the review (it costs tokens): a sentence or two per file and a short account
//! of the whole change. It runs the AI CLI you have, without tools or a session:
//! Claude Code on its small, quick model, or Codex.
use crate::review::{run_for, Body, FileDiff, Kind, Status};
use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};

/// The diff sent is cut off here, and each file's part of it here.
const MAX_DIFF: usize = 60_000;
const MAX_FILE: usize = 16_000;
const TIMEOUT: Duration = Duration::from_secs(240);

/// What the AI said about the changes.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// By Git path.
    pub files: HashMap<String, String>,
    pub overall: String,
    /// The changes it read (see `key`): when they differ now, it's out of date.
    pub key: u64,
    /// Who wrote it: "Claude", "Codex" or the command.
    pub by: String,
}

/// The AI CLI that summarizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
    /// A command of your own: the prompt on its input, the answer on its output.
    Command(Vec<String>),
}

impl Agent {
    /// From config `review_ai`: "auto" (Claude Code, else Codex), "claude",
    /// "codex", a command, or "off" (`None`).
    pub fn from_setting(setting: &str) -> Result<Option<Self>, String> {
        let installed = |name| crate::platform::which(name).is_some();
        match setting.trim() {
            "" | "off" | "false" | "none" => Ok(None),
            "auto" if installed("claude") => Ok(Some(Self::Claude)),
            "auto" if installed("codex") => Ok(Some(Self::Codex)),
            "auto" => Err("AI summaries need Claude Code or Codex installed, or review_ai set to a command in config.toml".into()),
            "claude" => Ok(Some(Self::Claude)),
            "codex" => Ok(Some(Self::Codex)),
            command => Ok(Some(Self::Command(
                command.split_whitespace().map(str::to_string).collect(),
            ))),
        }
    }
    pub fn name(&self) -> String {
        match self {
            Self::Claude => "Claude".into(),
            Self::Codex => "Codex".into(),
            Self::Command(words) => words.first().cloned().unwrap_or_default(),
        }
    }
}

/// Which changes these are, to tell whether a summary still describes them.
pub fn key(files: &[FileDiff]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in files {
        file.path.hash(&mut hasher);
        for hunk in file.hunks() {
            for line in &hunk.lines {
                (line.kind as u8, &line.text).hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

/// The changes as a unified diff, cut down to a size worth sending.
fn diff(files: &[FileDiff]) -> String {
    let mut out = String::new();
    for file in files {
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
        if part.len() > MAX_FILE {
            part.truncate(floor(&part, MAX_FILE));
            part.push_str("\n[… rest of this file's changes left out]\n");
        }
        if out.len() + part.len() > MAX_DIFF {
            out.push_str("[… more files left out]\n");
            break;
        }
        out.push_str(&part);
    }
    out
}

fn floor(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

pub fn prompt(files: &[FileDiff]) -> String {
    format!(
        "An AI coding agent just made the changes below to a developer's project. \
Explain them briefly to the developer, who is reviewing them to keep or undo.\n\n\
Answer in exactly this form, as plain text without Markdown:\n\
FILE <the path exactly as written after ===>: <one short sentence, at most 25 words: what changed in this file and how it works>\n\
(one FILE line for every file)\n\
OVERALL: <two to four short sentences: what the change does as a whole, how the pieces work together, and anything worth checking>\n\n\
{}",
        diff(files)
    )
}

/// Ask `agent` about the changes, off the UI thread.
pub fn start(agent: Agent, files: &[FileDiff]) -> Receiver<Result<Summary, String>> {
    let prompt = prompt(files);
    let key = key(files);
    let paths: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let answer = ask(&agent, &prompt).map(|text| {
            let (files, overall) = parse(&text, &paths);
            Summary {
                files,
                overall,
                key,
                by: agent.name(),
            }
        });
        let _ = sender.send(answer);
    });
    receiver
}

/// Run the agent outside the project, so it reads nothing but the prompt.
fn ask(agent: &Agent, prompt: &str) -> Result<String, String> {
    let quiet = std::env::temp_dir();
    let mut answer_file: Option<PathBuf> = None;
    let mut command = match agent {
        Agent::Claude => {
            let mut command = Command::new(
                crate::platform::which("claude").ok_or("Claude Code isn't installed")?,
            );
            command.args([
                "-p",
                "--model",
                "haiku",
                "--tools",
                "",
                "--strict-mcp-config",
                "--no-session-persistence",
            ]);
            command
        }
        Agent::Codex => {
            let file = quiet.join(format!("raido-summary-{}.txt", std::process::id()));
            let mut command =
                Command::new(crate::platform::which("codex").ok_or("Codex isn't installed")?);
            command
                .args([
                    "exec",
                    "--ephemeral",
                    "--skip-git-repo-check",
                    "--sandbox",
                    "read-only",
                    "--color",
                    "never",
                    "-o",
                ])
                .arg(&file)
                .arg("-");
            answer_file = Some(file);
            command
        }
        Agent::Command(words) => {
            let (program, args) = words.split_first().ok_or("review_ai is empty")?;
            let mut command = Command::new(
                crate::platform::which(program).ok_or(format!("{program} isn't installed"))?,
            );
            command.args(args);
            command
        }
    };
    command.current_dir(&quiet);
    let output = run_for(
        &mut command,
        Some(prompt.as_bytes()),
        TIMEOUT,
        &agent.name(),
    );
    let text = match &answer_file {
        Some(file) => {
            let text = std::fs::read_to_string(file);
            let _ = std::fs::remove_file(file);
            output?;
            text.map_err(|e| e.to_string())?
        }
        None => String::from_utf8_lossy(&output?).into_owned(),
    };
    if text.trim().is_empty() {
        return Err(format!("{} answered nothing", agent.name()));
    }
    Ok(text)
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
    #[ignore = "asks Claude Code (uses tokens)"]
    fn claude_summarizes_a_real_change() {
        let (_dir, root, mut changes) = crate::review::tests::project();
        crate::review::tests::feature(&mut changes, &root);
        let summary = start(Agent::Claude, &changes.files)
            .recv_timeout(TIMEOUT)
            .unwrap()
            .unwrap();
        println!("{summary:#?}");
        assert_eq!(summary.files.len(), 2);
        assert!(!summary.overall.is_empty());
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
    fn settings_name_the_agent() {
        assert_eq!(Agent::from_setting("off"), Ok(None));
        assert_eq!(Agent::from_setting("claude"), Ok(Some(Agent::Claude)));
        assert_eq!(
            Agent::from_setting("ollama run llama3.2"),
            Ok(Some(Agent::Command(vec![
                "ollama".into(),
                "run".into(),
                "llama3.2".into()
            ])))
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
        let prompt = prompt(std::slice::from_ref(&file));
        assert!(prompt.contains("=== x.txt (modified, +1 -0)\n@@ -8,3 +8,4 @@\n a\n a\n a\n+b\n"));
        let many = vec![file; 2000];
        assert!(prompt.len() < MAX_DIFF + 2000);
        assert!(super::prompt(&many).len() < MAX_DIFF + 2000);
        assert_ne!(key(&many[..1]), key(&many[..2]));
    }
}

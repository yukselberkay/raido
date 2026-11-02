//! An AI's reading of the changes under review, asked for only when you press `s`
//! in the review (it costs tokens): a sentence per file and a short account of the
//! whole change. It runs the AI CLI you have, without tools or a session: Claude
//! Code on its small, quick model, or Codex. Asked again, it's sent only the files
//! whose changes it hasn't read; what it said about the others is kept.
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
}

impl Summary {
    /// What it said about `file`, and whether that was about the changes it has now.
    pub fn note(&self, file: &FileDiff) -> Option<(&str, bool)> {
        let note = self.files.get(&file.path)?;
        Some((note.text.as_str(), note.key == file_key(file)))
    }
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
        let answer = ask(&agent, &prompt).map(|text| {
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
                by: agent.name(),
                sent: keys.len(),
                left_out,
            }
        });
        let _ = sender.send(answer);
    });
    Some(receiver)
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
        let summary = start(Agent::Claude, &changes.files, None)
            .unwrap()
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
        assert!(start(Agent::Claude, std::slice::from_ref(&file), Some(&before)).is_none());
    }
}

//! Interactive documentation (`Ctrl-b /`, `:ask`): ask how to do something in the
//! language you're writing, what a function does, or how this project does a
//! thing, and an AI agent answers in a pane beside the code, the way the official
//! documentation and a good Stack Overflow answer would: the function that does it
//! and its import, how to call it, a short example and what the example prints.
//!
//! It runs the AI CLI you have (`docs_ai`: Claude Code, Codex, Gemini, Antigravity,
//! OpenCode or a local model) in the background, one run per question, told what
//! to be by `INSTRUCTIONS` along with the question, and with the last few
//! questions and answers, so a follow-up is understood. No skill is needed: every
//! agent gets the same instructions, those without skills too. The agent may read
//! the project and, Claude Code, the web, and it tries its examples in a scratch
//! folder of its own, so the output it shows was printed. It changes nothing in
//! the project.
use crate::{
    config::Theme,
    highlight::Syntax,
    markdown::{self, Document},
    session::SavedAnswer,
    summary::{Agent, Usage},
};
use std::{
    fmt::Write as _,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// A question gets this long to be answered.
const TIMEOUT: Duration = Duration::from_secs(300);
/// The earlier answers sent along with a question, and how much of each.
const HISTORY: usize = 3;
const HISTORY_ANSWER: usize = 4_000;
/// How much of a selection goes with the question.
const MAX_SELECTION: usize = 8_000;
/// How much of a question goes: a long paste is cut, as Antigravity and OpenCode
/// take the prompt on their command line.
const MAX_QUESTION: usize = 12_000;
/// The answers kept with the project's session.
const SAVED: usize = 20;
/// Claude Code's model unless `docs_ai` names one: right about APIs, and quick.
const CLAUDE_MODEL: &str = "sonnet";
/// Claude Code runs its examples in its sandbox, which lets them write only in the
/// scratch folder, without asking (`-p` can't ask).
const CLAUDE_SANDBOX: &str = r#"{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true}}"#;
const STOPPED: &str = "Stopped";

/// What the agent is told it is, before the question: short answers, fast.
pub const INSTRUCTIONS: &str = "\
You are the interactive documentation inside the user's code editor: the official \
documentation of the language and its libraries, and a Stack Overflow answer. Answer \
from your own knowledge; skills about other tools don't apply.

Answer only the question asked, as briefly as you can, then stop. No preamble, no \
headings, no summary, no alternatives or extra sections unless asked. Answer in \
Markdown.

For how to do something: the function or API that does it, with its import, in \
one line; a minimal example in the user's language (that of their file) in a fenced \
code block naming the language; then what it prints under **Output**, in a ```text \
block. Give the output you know; run the example in the scratch folder only when \
you aren't sure what it prints.
For a function or other name: its signature and what it returns in a line, then \
the same minimal example and output.
Look at the project only when the question is about it, reading just the files \
needed, and cite them as path:line.

Never change, create or delete files in the project: write only in the scratch \
folder.";

/// Told to the agents that can't run commands in their mode here: Antigravity and
/// Gemini in plan mode (Antigravity's print mode stops at the first command it
/// isn't allowed), and local models, which have no tools. They don't try, and
/// don't pass off what they expect as what was printed.
const NO_COMMANDS: &str = "You can't run commands in this session, so don't try: show \
what the example would print under **Expected output**.";

/// Whether `agent` runs commands here, to show what its examples really print.
fn runs_examples(agent: &Agent) -> bool {
    !matches!(agent, Agent::Gemini(_) | Agent::Agy(_) | Agent::Local(_))
}

/// Where you were in the code when you asked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// From the project folder.
    pub file: String,
    pub language: String,
    /// From 1.
    pub line: usize,
    /// That line.
    pub text: String,
    pub selection: Option<String>,
    /// The name under the cursor.
    pub word: Option<String>,
}

impl Context {
    /// "app.py:42 · Python".
    pub fn label(&self) -> String {
        format!("{}:{} · {}", self.file, self.line, self.language)
    }
}

/// A question and its answer.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    /// The question as shown: what you typed, or "`read_csv`" for the name under
    /// the cursor.
    pub shown: String,
    /// The question as the agent got it.
    pub asked: String,
    /// Where you were: "app.py:42 · Python".
    pub about: Option<String>,
    pub answer: Option<String>,
    pub error: Option<String>,
    /// Who answered: "Claude (sonnet)".
    pub by: String,
    /// The AIs asked before it that couldn't answer, why, and who was asked next.
    pub failed: Vec<String>,
    pub usage: Option<Usage>,
    pub took: Option<Duration>,
}

/// What a question running in the background sends.
enum Update {
    /// What the agent is doing: "reading app.py", "running python3 example.py".
    Progress(String),
    /// The agent couldn't answer (`failed` says why): `by` is asked instead.
    Next {
        by: String,
        failed: String,
    },
    Done(Result<Answer, String>),
}

struct Answer {
    text: String,
    usage: Option<Usage>,
    by: String,
}

/// A question being answered. Dropping it stops the agent.
pub struct Asking {
    pub started: Instant,
    /// What the agent is doing, as it last said.
    pub progress: String,
    /// The spinner's frame, to redraw only when it turns.
    frame: u128,
    updates: Receiver<Update>,
    child: Arc<Mutex<Option<Child>>>,
    stopped: Arc<AtomicBool>,
}

impl Asking {
    /// The spinner, turning while the agent works.
    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[(self.frame % FRAMES.len() as u128) as usize]
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        kill(&self.child);
    }
}

/// The answers drawn: rendered once per change and width.
pub struct Rendered {
    revision: u64,
    width: usize,
    pub document: Document,
    /// The rendered line of each question.
    starts: Vec<usize>,
    /// Each code block: the rendered line it starts on, and its code.
    blocks: Vec<(usize, String)>,
}

/// The interactive documentation pane.
#[derive(Default)]
pub struct DocsView {
    pub entries: Vec<Entry>,
    /// The question being typed, and the cursor in it (in characters).
    pub input: String,
    pub cursor: usize,
    pub scroll: usize,
    /// Rows the answers had when last drawn, for paging.
    pub height: usize,
    /// Where you were in the code, sent with each question.
    pub context: Option<Context>,
    pub asking: Option<Asking>,
    /// Bumped when what's shown changes, to render it again.
    revision: u64,
    /// The question to scroll to the top when next drawn.
    jump: Option<usize>,
    pub rendered: Option<Rendered>,
    /// Which AI answers, being chosen.
    pub choosing: Option<Chooser>,
}

/// An AI the chooser offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// What `docs_ai` names it: "claude:sonnet", "codex", "local:qwen3:8b".
    pub setting: String,
    /// "Claude Code · Sonnet".
    pub label: String,
    /// What it's like here: "the most capable; slower".
    pub note: String,
}

/// Which AI answers the docs' questions: asked the first time you ask when more
/// than one could (`docs_ai` still "auto"), and by `:ask-with`. The pick is saved
/// as `docs_ai`, the rest of "auto" after it for when it can't answer.
#[derive(Default)]
pub struct Chooser {
    pub offers: Vec<Offer>,
    pub selected: usize,
    /// Opened by asking: the question goes once an AI is picked.
    pub asking: bool,
    /// Where the local models are listed from ("Ollama", your model server's name),
    /// while they're on their way.
    pub listing_from: Option<String>,
    /// Why the local models can't be offered.
    pub problem: Option<String>,
    listing: Option<Models>,
}

/// Your local models' names, listed in the background, or why they can't be.
pub type Models = Receiver<Result<Vec<String>, String>>;

/// A CLI the chooser offers: its program, its name, and its models with a note on
/// each (`None`: the model it's set to use).
type Offered = (
    &'static str,
    &'static str,
    &'static [(Option<&'static str>, &'static str)],
);

/// The CLIs "auto" tries that the chooser offers, in its order, before the local
/// models: (program, name, its models as (model, note)). `None`: the CLI's own.
const OFFERED: [Offered; 5] = [
    (
        "claude",
        "Claude Code",
        &[
            (Some("sonnet"), "right about APIs, and quick (Raido's pick)"),
            (
                Some("opus"),
                "the most capable; slower, uses more of your plan",
            ),
            (Some("haiku"), "the quickest; for simple questions"),
        ],
    ),
    (
        "codex",
        "Codex",
        &[(None, "the model in Codex's own settings")],
    ),
    (
        "gemini",
        "Gemini CLI",
        &[(None, "shows the output it expects; runs nothing")],
    ),
    (
        "agy",
        "Antigravity",
        &[(None, "shows the output it expects; runs nothing")],
    ),
    ("opencode", "OpenCode", &[(None, "the model it used last")]),
];

impl Chooser {
    /// The choice among `programs` (installed CLIs "auto" tries); with `local`,
    /// your local models too, listed in the background from `listing_from`.
    pub fn new(programs: &[&str], local: Option<(String, Models)>) -> Self {
        let mut offers = Vec::new();
        for (program, name, models) in OFFERED {
            if !programs.contains(&program) {
                continue;
            }
            for (model, note) in models {
                offers.push(Offer {
                    setting: match model {
                        Some(model) => format!("{program}:{model}"),
                        None => program.to_string(),
                    },
                    label: match model {
                        Some(model) => format!("{name} · {}", capitalized(model)),
                        None => format!("{name} · its own model"),
                    },
                    note: note.to_string(),
                });
            }
        }
        let (listing_from, listing) = match local {
            Some((from, listing)) => (Some(from), Some(listing)),
            None => (None, None),
        };
        Self {
            offers,
            listing_from,
            listing,
            ..Self::default()
        }
    }
    /// Whether to ask before the first question: more than one AI could answer.
    /// `programs` are the CLIs installed; local models count as several, as each
    /// is an AI of its own.
    pub fn worth_asking(programs: usize, local: bool) -> bool {
        programs >= 2 || local
    }
    /// The local models are still on their way.
    pub fn listing(&self) -> bool {
        self.listing.is_some()
    }
    /// Take the local models if they've arrived; true when they did.
    pub fn poll(&mut self) -> bool {
        let Some(listing) = &self.listing else {
            return false;
        };
        let models = match listing.try_recv() {
            Ok(models) => models,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("the listing stopped".into()),
        };
        self.listing = None;
        let from = self.listing_from.take().unwrap_or_else(|| "Ollama".into());
        match models {
            Ok(models) if models.is_empty() => {
                self.problem = Some(format!(
                    "{from} has no models yet · pull one: ollama pull qwen3"
                ));
            }
            Ok(models) => {
                let note = match from.as_str() {
                    "Ollama" => {
                        "on this computer; shows the output it expects, runs nothing".to_string()
                    }
                    _ => "on your model server; shows the output it expects, runs nothing"
                        .to_string(),
                };
                self.offers.extend(models.into_iter().map(|model| Offer {
                    label: format!("{from} · {model}"),
                    setting: format!("local:{model}"),
                    note: note.clone(),
                }));
            }
            Err(error) => self.problem = Some(format!("{from}: {error}")),
        }
        true
    }
    pub fn up(&mut self) {
        let rows = self.offers.len().max(1);
        self.selected = (self.selected + rows - 1) % rows;
    }
    pub fn down(&mut self) {
        let rows = self.offers.len().max(1);
        self.selected = (self.selected + 1) % rows;
    }
}

/// "sonnet" → "Sonnet".
fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl DocsView {
    /// The view of saved questions and answers.
    pub fn with(entries: Vec<Entry>) -> Self {
        let jump = entries.len().checked_sub(1);
        Self {
            entries,
            jump,
            ..Self::default()
        }
    }
    /// The questions answered, to keep with the session: the last few.
    pub fn saved(&self) -> Vec<SavedAnswer> {
        let answered: Vec<&Entry> = self.entries.iter().filter(|e| e.answer.is_some()).collect();
        answered[answered.len().saturating_sub(SAVED)..]
            .iter()
            .map(|e| SavedAnswer {
                question: e.shown.clone(),
                asked: (e.asked != e.shown).then(|| e.asked.clone()),
                about: e.about.clone(),
                answer: e.answer.clone(),
                by: e.by.clone(),
            })
            .collect()
    }
    /// The view of a session's answers.
    pub fn restored(answers: Vec<SavedAnswer>) -> Self {
        Self::with(
            answers
                .into_iter()
                .map(|a| Entry {
                    asked: a.asked.unwrap_or_else(|| a.question.clone()),
                    shown: a.question,
                    about: a.about,
                    answer: a.answer,
                    by: a.by,
                    ..Entry::default()
                })
                .collect(),
        )
    }
    /// Something shown changed: render it again.
    pub fn changed(&mut self) {
        self.revision += 1;
    }
    /// What Enter asks: what you typed, else the name under the cursor or the code
    /// you selected. (shown, asked)
    pub fn question(&self) -> Option<(String, String)> {
        let typed = typed(&self.input);
        if !typed.is_empty() {
            return Some((typed.to_string(), typed.to_string()));
        }
        let context = self.context.as_ref()?;
        if context.selection.is_some() {
            return Some((
                "The selected code".into(),
                "Explain the code I selected: what it does, the functions and APIs it \
                 uses and how to call them, with an example and what it prints."
                    .into(),
            ));
        }
        let word = context.word.as_ref()?;
        Some((
            format!("`{word}`"),
            format!(
                "`{word}`: what it is, its signature, what it returns and how to use it, \
                 with an example and what it prints."
            ),
        ))
    }
    /// Ask the first of `agents`, and the next when one can't answer; the answer
    /// comes in `poll`. Asking while an answer is on its way stops that one.
    pub fn ask(&mut self, shown: String, asked: String, agents: Vec<Agent>, root: &Path) {
        self.stop();
        let agents: Vec<Agent> = agents
            .into_iter()
            .map(|agent| match agent {
                Agent::Claude(None) => Agent::Claude(Some(CLAUDE_MODEL.into())),
                agent => agent,
            })
            .collect();
        let Some(first) = agents.first() else {
            return;
        };
        let scratch = scratch(root);
        let answered: Vec<&Entry> = self.entries.iter().filter(|e| e.answer.is_some()).collect();
        let history = &answered[answered.len().saturating_sub(HISTORY)..];
        let prompt = prompt(&asked, self.context.as_ref(), history, root, &scratch);
        self.entries.push(Entry {
            shown,
            asked,
            about: self.context.as_ref().map(Context::label),
            by: first.name(),
            ..Entry::default()
        });
        self.asking = Some(start(agents, prompt, root.to_path_buf(), scratch));
        self.input.clear();
        self.cursor = 0;
        self.jump = Some(self.entries.len() - 1);
        self.changed();
    }
    /// Take what the agent sent; true when there's something new to draw.
    pub fn poll(&mut self) -> bool {
        let listed = self.choosing.as_mut().is_some_and(Chooser::poll);
        let Some(asking) = &mut self.asking else {
            return listed;
        };
        let mut moved = listed;
        let done = loop {
            match asking.updates.try_recv() {
                Ok(Update::Progress(progress)) => {
                    asking.progress = progress;
                    moved = true;
                }
                Ok(Update::Next { by, failed }) => {
                    if let Some(entry) = self.entries.last_mut() {
                        entry.failed.push(format!("{failed} · asked {by} instead"));
                        let was = std::mem::replace(&mut entry.by, by);
                        asking.progress = format!("{was} couldn't answer");
                    }
                    moved = true;
                }
                Ok(Update::Done(result)) => break Some(result),
                Err(TryRecvError::Empty) => break None,
                Err(TryRecvError::Disconnected) => {
                    break Some(Err("The agent stopped without answering".into()))
                }
            }
        };
        let frame = asking.started.elapsed().as_millis() / 120;
        moved |= frame != asking.frame;
        asking.frame = frame;
        let Some(result) = done else {
            return moved;
        };
        let took = asking.started.elapsed();
        self.asking = None;
        if let Some(entry) = self.entries.last_mut() {
            entry.took = Some(took);
            match result {
                Ok(answer) => {
                    entry.answer = Some(answer.text.trim().to_string());
                    entry.usage = answer.usage;
                    entry.by = answer.by;
                }
                Err(error) => entry.error = Some(error),
            }
        }
        self.jump = Some(self.entries.len() - 1);
        self.changed();
        true
    }
    /// Stop the answer on its way; false when there's none.
    pub fn stop(&mut self) -> bool {
        let Some(asking) = self.asking.take() else {
            return false;
        };
        let took = asking.started.elapsed();
        drop(asking);
        if let Some(entry) = self.entries.last_mut() {
            if entry.answer.is_none() && entry.error.is_none() {
                entry.error = Some(STOPPED.into());
                entry.took = Some(took);
            }
        }
        self.changed();
        true
    }
    /// A new topic: forget the questions asked.
    pub fn clear(&mut self) {
        self.stop();
        self.entries.clear();
        self.scroll = 0;
        self.changed();
    }
    pub fn scroll_by(&mut self, rows: i64) {
        self.scroll = self.scroll.saturating_add_signed(rows as isize);
    }
    /// Render the answers `width` columns wide, unless they are already.
    pub fn layout(&mut self, width: usize, theme: &Theme, syntax: &Syntax) {
        let fresh = self
            .rendered
            .as_ref()
            .is_some_and(|r| r.revision == self.revision && r.width == width);
        if fresh {
            return;
        }
        let (text, starts, asked) = self.transcript();
        // Code wraps: the pane can't scroll sideways.
        let document = markdown::render_wrapped(&text, width, theme, syntax);
        let starts = starts
            .into_iter()
            .map(|line| document.line_for_source(line))
            .collect();
        // The answers' examples, to copy: not the code you asked about.
        let blocks = code_blocks(&text)
            .into_iter()
            .filter(|(line, _)| !asked.contains(line))
            .map(|(line, code)| (document.line_for_source(line), code))
            .collect();
        self.rendered = Some(Rendered {
            revision: self.revision,
            width,
            document,
            starts,
            blocks,
        });
    }
    /// After `layout`: scroll to the question asked or answered last, keep the
    /// scroll on the answers, and remember the room they have.
    pub fn place(&mut self, height: usize) {
        let Some(rendered) = &self.rendered else {
            return;
        };
        if let Some(entry) = self.jump.take() {
            self.scroll = rendered.starts.get(entry).copied().unwrap_or(0);
        }
        let max = rendered.document.lines.len().saturating_sub(height);
        self.scroll = self.scroll.min(max);
        self.height = height;
    }
    /// The code example on screen: the first that starts there, else the one the
    /// screen is in.
    pub fn example(&self) -> Option<&str> {
        let rendered = self.rendered.as_ref()?;
        let end = self.scroll + self.height.max(1);
        rendered
            .blocks
            .iter()
            .find(|(line, _)| (self.scroll..end).contains(line))
            .or_else(|| {
                rendered
                    .blocks
                    .iter()
                    .rfind(|(line, _)| *line < self.scroll)
            })
            .map(|(_, code)| code.as_str())
    }
    /// The questions and answers as Markdown, the source line of each question,
    /// and the source line of each code block a question holds.
    fn transcript(&self) -> (String, Vec<usize>, Vec<usize>) {
        if self.entries.is_empty() {
            return (self.intro(), Vec::new(), Vec::new());
        }
        let mut text = String::new();
        let mut starts = Vec::new();
        let mut asked = Vec::new();
        let line = |text: &str| text.bytes().filter(|b| *b == b'\n').count();
        for (index, entry) in self.entries.iter().enumerate() {
            if index > 0 {
                text.push_str("\n---\n\n");
            }
            starts.push(line(&text));
            // A question of several lines (code pasted in it): its first line as the
            // heading, the rest as it was typed, tabs and spaces kept.
            let (first, rest) = match entry.shown.split_once('\n') {
                Some((first, rest)) => (first, Some(rest.trim_end())),
                None => (entry.shown.as_str(), None),
            };
            let _ = writeln!(text, "## ❯ {}\n", one_line(first));
            if let Some(rest) = rest.filter(|r| !r.trim().is_empty()) {
                let fence = "`".repeat(longest_run(rest, '`').max(2) + 1);
                let language = entry
                    .about
                    .as_deref()
                    .and_then(|a| a.rsplit(" · ").next())
                    .map(str::to_lowercase)
                    .filter(|l| l != "plain text" && !l.contains(char::is_whitespace))
                    .unwrap_or_default();
                asked.push(line(&text));
                let _ = writeln!(text, "{fence}{language}\n{rest}\n{fence}\n");
            }
            if let Some(about) = &entry.about {
                let _ = writeln!(text, "*{}*\n", escape(about));
            }
            for failed in &entry.failed {
                let _ = writeln!(text, "> [!NOTE]\n> {}\n", one_line(failed));
            }
            match (&entry.answer, &entry.error) {
                (Some(answer), _) => {
                    let _ = writeln!(text, "{answer}\n");
                    let mut said = vec![escape(&entry.by)];
                    if let Some(took) = entry.took {
                        said.push(format!("{}s", took.as_secs()));
                    }
                    if let Some(usage) = entry.usage {
                        said.push(escape(&usage.describe()));
                    }
                    let _ = writeln!(text, "*{}*", said.join(" · "));
                }
                (None, Some(error)) => {
                    let _ = writeln!(text, "> [!WARNING]\n> {}", one_line(error));
                }
                (None, None) => {}
            }
        }
        (text, starts, asked)
    }
    /// What the pane says before the first question.
    fn intro(&self) -> String {
        let language = self
            .context
            .as_ref()
            .map(|c| c.language.as_str())
            .filter(|l| !l.is_empty() && *l != "Plain text");
        let mut text = String::from("## Interactive documentation\n\n");
        let _ = writeln!(
            text,
            "Ask how to do something in **{}**: you get the function that does it and \
             its import, how to call it, a short example and what it prints. Ask what a \
             function does, or how this project does something.\n",
            language.unwrap_or("the language you write")
        );
        let examples: &[&str] = match language {
            Some("Python") => &[
                "how do I read a CSV file into a list of dicts?",
                "what does `functools.lru_cache` do?",
            ],
            Some("Rust") => &[
                "how do I read a file line by line?",
                "what does `Option::take` do?",
            ],
            Some("Go") => &[
                "how do I run two functions at once and wait for both?",
                "what does `strings.Fields` do?",
            ],
            Some("JavaScript" | "TypeScript" | "TSX") => &[
                "how do I wait for several promises at once?",
                "what does `Array.prototype.flatMap` do?",
            ],
            _ => &[
                "how do I read a file line by line?",
                "how do I sort a list by a field?",
            ],
        };
        for example in examples {
            let _ = writeln!(text, "- {example}");
        }
        text.push_str("- where does this project read its settings?\n\n");
        let what = match &self.context {
            Some(c) if c.selection.is_some() => Some("the code you selected".to_string()),
            Some(Context {
                word: Some(word), ..
            }) => Some(format!("`{word}`, the name under the cursor")),
            _ => None,
        };
        if let Some(what) = what {
            let _ = writeln!(text, "Enter on an empty line explains {what}.\n");
        }
        text.push_str(
            "The agent reads the project and tries its examples in a scratch folder: \
             it changes nothing here.",
        );
        text
    }
    /// Type `c` in the question.
    pub fn insert(&mut self, c: char) {
        let at = byte(&self.input, self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
    }
    /// A paste, as it was: its lines, tabs and spaces (a terminal sends its line
    /// breaks as \r).
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let kept: String = text
            .chars()
            .filter(|c| matches!(c, '\n' | '\t') || !c.is_control())
            .collect();
        let at = byte(&self.input, self.cursor);
        self.input.insert_str(at, &kept);
        self.cursor += kept.chars().count();
    }
    /// The question has more than one line: ↑↓ move in it.
    pub fn multiline(&self) -> bool {
        self.input.contains('\n')
    }
    /// The cursor's line and column, in characters.
    pub fn line_col(&self) -> (usize, usize) {
        let before: String = self.input.chars().take(self.cursor).collect();
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count());
        (line, col)
    }
    /// To the line `by` lines up or down, at the same column where it has one.
    pub fn move_line(&mut self, by: isize) {
        let (line, col) = self.line_col();
        let lines: Vec<&str> = self.input.split('\n').collect();
        let Some(to) = line.checked_add_signed(by).filter(|l| *l < lines.len()) else {
            return;
        };
        let start: usize = lines[..to].iter().map(|l| l.chars().count() + 1).sum();
        self.cursor = start + col.min(lines[to].chars().count());
    }
    /// To the start of the cursor's line.
    pub fn home(&mut self) {
        let (_, col) = self.line_col();
        self.cursor -= col;
    }
    /// To the end of the cursor's line.
    pub fn end(&mut self) {
        let (line, col) = self.line_col();
        let length = self
            .input
            .split('\n')
            .nth(line)
            .map_or(0, |l| l.chars().count());
        self.cursor += length - col;
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = byte(&self.input, self.cursor);
            self.input.remove(at);
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.input.chars().count() {
            let at = byte(&self.input, self.cursor);
            self.input.remove(at);
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.input.chars().count());
    }
}

/// What you typed, as it goes: without the blank lines before it or the space
/// after it, the first line's indentation kept when there are more (pasted code).
fn typed(input: &str) -> &str {
    let text = input.trim_end();
    let Some(first) = text.find(|c: char| !c.is_whitespace()) else {
        return "";
    };
    let start = text[..first].rfind('\n').map_or(0, |i| i + 1);
    let text = &text[start..];
    if text.contains('\n') {
        text
    } else {
        text.trim_start()
    }
}

/// The most `c` in a row in `text`: a fence around it needs one more.
fn longest_run(text: &str, c: char) -> usize {
    let (mut most, mut now) = (0, 0);
    for d in text.chars() {
        now = if d == c { now + 1 } else { 0 };
        most = most.max(now);
    }
    most
}

/// The byte where character `at` starts.
fn byte(text: &str, at: usize) -> usize {
    text.char_indices().nth(at).map_or(text.len(), |(i, _)| i)
}

/// Where the agent tries its examples: a folder of its own per project, kept, so
/// a follow-up finds the files the last answer made.
pub fn scratch(root: &Path) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    std::env::temp_dir()
        .join("raido-docs")
        .join(format!("{:016x}", hasher.finish()))
}

/// The question as the agent gets it: where the project is, where you are in it,
/// what was asked before, then the question.
pub fn prompt(
    question: &str,
    context: Option<&Context>,
    history: &[&Entry],
    root: &Path,
    scratch: &Path,
) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Project: {} (read its files when the question is about this code base)",
        root.display()
    );
    let _ = writeln!(
        text,
        "Scratch folder, to try examples in: {}",
        scratch.display()
    );
    if let Some(c) = context {
        let _ = writeln!(
            text,
            "\nThe user is in {} ({}), at line {}:\n    {}",
            c.file,
            c.language,
            c.line,
            c.text.trim_end()
        );
        if let Some(selection) = &c.selection {
            let _ = writeln!(
                text,
                "They selected:\n```\n{}\n```",
                cut(selection, MAX_SELECTION).trim_end()
            );
        }
    }
    if !history.is_empty() {
        text.push_str("\nEarlier in this conversation:\n");
        for entry in history {
            let _ = writeln!(text, "Q: {}", entry.asked);
            if let Some(answer) = &entry.answer {
                let _ = writeln!(text, "A: {}\n", cut(answer, HISTORY_ANSWER).trim_end());
            }
        }
    }
    let _ = writeln!(text, "\nQuestion: {}", cut(question, MAX_QUESTION));
    text
}

/// `text` up to `max` bytes, cut at a character, marked when cut.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    format!("{} […]", &text[..at])
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Markdown's own characters as plain text.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '#') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The fenced code blocks in `markdown`: the line each starts on, and its code.
fn code_blocks(markdown: &str) -> Vec<(usize, String)> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, String, String)> = None;
    for (number, line) in markdown.lines().enumerate() {
        let fence = line.trim_start();
        let marker: String = fence
            .chars()
            .take_while(|&c| c == '`' || c == '~')
            .collect();
        let is_fence =
            marker.len() >= 3 && marker.chars().all(|c| c == marker.as_bytes()[0] as char);
        match &mut open {
            Some((start, opened, code)) => {
                if is_fence && marker.starts_with(opened.as_str()) && fence.trim() == marker {
                    blocks.push((*start, std::mem::take(code)));
                    open = None;
                } else {
                    code.push_str(line);
                    code.push('\n');
                }
            }
            None if is_fence => open = Some((number, marker, String::new())),
            None => {}
        }
    }
    blocks
}

/// Start asking `agents`, off the UI thread: the first, then the next while one
/// can't answer (signed out, out of quota, an error), until one does or you stop it.
fn start(agents: Vec<Agent>, prompt: String, root: PathBuf, scratch: PathBuf) -> Asking {
    let (sender, updates) = mpsc::channel();
    let child = Arc::new(Mutex::new(None));
    let stopped = Arc::new(AtomicBool::new(false));
    let (slot, stop) = (child.clone(), stopped.clone());
    thread::spawn(move || {
        let mut answer = Err("No AI to answer with".to_string());
        for (index, agent) in agents.iter().enumerate() {
            answer = run(agent, &prompt, &root, &scratch, &slot, &stop, &sender);
            let next = agents.get(index + 1);
            match (&answer, next) {
                (Err(error), Some(next)) if !stop.load(Ordering::SeqCst) && error != STOPPED => {
                    let failed = crate::summary::said_by(&agent.name(), error.clone());
                    let by = next.name();
                    let _ = sender.send(Update::Next { by, failed });
                }
                _ => break,
            }
        }
        let _ = sender.send(Update::Done(answer));
    });
    Asking {
        started: Instant::now(),
        progress: String::new(),
        frame: 0,
        updates,
        child,
        stopped,
    }
}

/// Run the agent on the question, telling `sender` what it's doing.
fn run(
    agent: &Agent,
    prompt: &str,
    root: &Path,
    scratch: &Path,
    slot: &Mutex<Option<Child>>,
    stopped: &AtomicBool,
    sender: &Sender<Update>,
) -> Result<Answer, String> {
    let whole = match runs_examples(agent) {
        true => format!("{INSTRUCTIONS}\n\n{prompt}"),
        false => format!("{INSTRUCTIONS}\n\n{NO_COMMANDS}\n\n{prompt}"),
    };
    // `local` with a model server: its OpenAI API answers.
    if let (Agent::Local(model), true) = (agent, crate::model_server::configured()) {
        let (text, usage, by) = crate::model_server::chat(model.as_deref(), &whole, TIMEOUT)
            .map_err(|e| format!("{e:#}"))?;
        return answered(text, Some(usage), by);
    }
    std::fs::create_dir_all(scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let program = crate::platform::which(agent.program())
        .ok_or_else(|| format!("{} isn't installed", agent.program()))?;
    let mut command = Command::new(program);
    let mut by = agent.name();
    let answer_file = scratch.join(format!(".answer-{}.md", std::process::id()));
    let input = match agent {
        Agent::Claude(model) => {
            command.args(["-p", "--model", model.as_deref().unwrap_or(CLAUDE_MODEL)]);
            command.args(["--output-format", "stream-json", "--verbose"]);
            // A short answer needs little thought: it comes faster.
            command.args(["--effort", "low"]);
            // Reading, the web, and commands in the sandbox: no edits.
            command.args(["--tools", "Read,Grep,Glob,WebSearch,WebFetch,Bash"]);
            command.args(["--allowedTools", "Read,Grep,Glob,WebSearch,WebFetch"]);
            command.arg("--add-dir").arg(root);
            command.args(["--settings", CLAUDE_SANDBOX]);
            command.args(["--strict-mcp-config", "--no-session-persistence"]);
            command.arg("--append-system-prompt").arg(INSTRUCTIONS);
            command.current_dir(scratch);
            prompt.to_string()
        }
        Agent::Codex(model) => {
            command.args(["exec", "--ephemeral", "--skip-git-repo-check"]);
            command.args(["--color", "never", "--json"]);
            command.args(["-c", "model_reasoning_effort=\"low\""]);
            // It reads anything, and writes only in the scratch folder.
            command.args(["--sandbox", "workspace-write"]);
            command.arg("--cd").arg(scratch);
            if let Some(model) = model {
                command.args(["--model", model]);
            }
            command.arg("-o").arg(&answer_file).arg("-");
            command.current_dir(scratch);
            whole
        }
        Agent::Gemini(model) => {
            // The prompt goes on its input; this line is added after it.
            command.args(["-p", "Answer as asked above.", "-o", "json"]);
            command.args(["--approval-mode", "plan"]);
            if crate::summary::gemini_skips_trust() {
                command.arg("--skip-trust");
            }
            if let Some(model) = model {
                command.args(["--model", model]);
            }
            command.current_dir(root);
            whole
        }
        // Antigravity in plan mode, which changes no files, reading the project too.
        Agent::Agy(model) => {
            command.arg("--add-dir").arg(root);
            command.args(["--effort", "low"]);
            crate::summary::agy_args(&mut command, model.as_deref(), &whole);
            command.current_dir(scratch);
            String::new()
        }
        Agent::OpenCode(model) => {
            crate::summary::opencode_args(&mut command, model.as_deref(), &whole);
            command.current_dir(scratch);
            String::new()
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
            command.current_dir(scratch);
            whole
        }
        Agent::Command(words) => {
            command.args(&words[1..]);
            command.current_dir(root);
            whole
        }
    };
    let output = stream(&mut command, &input, slot, stopped, |line| {
        if let Some(progress) = progress(agent, line) {
            let _ = sender.send(Update::Progress(progress));
        }
    });
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            let _ = std::fs::remove_file(&answer_file);
            return Err(error);
        }
    };
    let failed = || Err(failure(&by, &output.error));
    match agent {
        Agent::Claude(_) => {
            let result = output.stdout.lines().rev().find(|line| {
                serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| v["type"] == "result")
            });
            match result {
                Some(line) => {
                    let (text, usage) = crate::summary::claude_answer(line)?;
                    answered(text, Some(usage), by)
                }
                None => failed(),
            }
        }
        Agent::Codex(_) => {
            let text = std::fs::read_to_string(&answer_file);
            let _ = std::fs::remove_file(&answer_file);
            match text {
                Ok(text) if output.success => {
                    answered(text, crate::summary::codex_usage(&output.stdout), by)
                }
                _ => failed(),
            }
        }
        Agent::Gemini(_) if output.stdout.contains('{') => {
            let (text, usage) = crate::summary::gemini_answer(&output.stdout)?;
            answered(text, Some(usage), by)
        }
        // It says what went wrong in its JSON, and exits 0 either way.
        Agent::Agy(_) if output.stdout.contains('{') => {
            let (text, usage) = crate::summary::agy_answer(&output.stdout)?;
            answered(text, Some(usage), by)
        }
        Agent::OpenCode(_) if output.success => {
            answered(crate::summary::plain(&output.stdout), None, by)
        }
        _ if output.success => answered(output.stdout, None, by),
        _ => failed(),
    }
}

/// Why a run failed, from the first line of its errors: a CLI that isn't signed in
/// says how to sign in, not its stack trace.
fn failure(by: &str, error: &str) -> String {
    let lower = error.to_lowercase();
    let signed_out = ["authenticat", "not logged in", "log in", "login", "sign in"]
        .iter()
        .any(|w| lower.contains(w));
    match error {
        "" => format!("{by} failed"),
        _ if signed_out => format!(
            "{by} isn't signed in · start it once in an AI pane (Ctrl-b a) to sign in ({error})"
        ),
        error => format!("{by}: {error}"),
    }
}

fn answered(text: String, usage: Option<Usage>, by: String) -> Result<Answer, String> {
    if text.trim().is_empty() {
        return Err(format!("{by} answered nothing"));
    }
    Ok(Answer { text, usage, by })
}

/// What a run printed.
struct Output {
    stdout: String,
    success: bool,
    /// The first line of what it said went wrong.
    error: String,
}

/// Run `command` with `input`, handing each line it prints to `line` as it comes;
/// it's stopped when `stopped` is set, or after `TIMEOUT`.
fn stream(
    command: &mut Command,
    input: &str,
    slot: &Mutex<Option<Child>>,
    stopped: &AtomicBool,
    mut line: impl FnMut(&str),
) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    if let Ok(mut slot) = slot.lock() {
        *slot = Some(child);
    }
    if stopped.load(Ordering::SeqCst) {
        kill(slot);
        return Err(STOPPED.into());
    }
    if let Some(mut stdin) = stdin {
        let input = input.to_string();
        thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let (lines_out, lines) = mpsc::channel::<String>();
    if let Some(stdout) = stdout {
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut bytes = Vec::new();
            while reader.read_until(b'\n', &mut bytes).is_ok_and(|n| n > 0) {
                let text = String::from_utf8_lossy(&bytes);
                if lines_out
                    .send(text.trim_end_matches(['\n', '\r']).to_string())
                    .is_err()
                {
                    break;
                }
                bytes.clear();
            }
        });
    }
    let errors = stderr.map(|mut stderr| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });
    let started = Instant::now();
    let mut stdout = String::new();
    let mut take = |text: String, stdout: &mut String| {
        line(&text);
        stdout.push_str(&text);
        stdout.push('\n');
    };
    let status = loop {
        match lines.recv_timeout(Duration::from_millis(50)) {
            Ok(text) => {
                take(text, &mut stdout);
                continue;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Its output closed: it's about to end.
            Err(RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_millis(10)),
        }
        if stopped.load(Ordering::SeqCst) {
            kill(slot);
            return Err(STOPPED.into());
        }
        if started.elapsed() > TIMEOUT {
            kill(slot);
            return Err(format!(
                "No answer after {} minutes",
                TIMEOUT.as_secs() / 60
            ));
        }
        let exited = slot
            .lock()
            .ok()
            .and_then(|mut child| child.as_mut()?.try_wait().ok().flatten());
        if let Some(status) = exited {
            break status;
        }
    };
    while let Ok(text) = lines.recv_timeout(Duration::from_millis(500)) {
        take(text, &mut stdout);
    }
    if let Ok(mut slot) = slot.lock() {
        *slot = None;
    }
    let errors = errors.and_then(|e| e.join().ok()).unwrap_or_default();
    let error = errors
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string();
    Ok(Output {
        stdout,
        success: status.success(),
        error,
    })
}

fn kill(slot: &Mutex<Option<Child>>) {
    if let Ok(mut slot) = slot.lock() {
        if let Some(child) = slot.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *slot = None;
    }
}

/// What the agent is doing, from a line of its output: Claude Code's and Codex's
/// events say which file it reads, what it searches for, what it runs.
fn progress(agent: &Agent, line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let said = match agent {
        Agent::Claude(_) => {
            if value["type"] != "assistant" {
                return None;
            }
            value["message"]["content"]
                .as_array()?
                .iter()
                .rev()
                .find_map(|item| match item["type"].as_str()? {
                    "tool_use" => Some(tool(item["name"].as_str()?, &item["input"])),
                    "thinking" => Some("thinking".into()),
                    _ => None,
                })?
        }
        Agent::Codex(_) => {
            let item = &value["item"];
            match (value["type"].as_str()?, item["type"].as_str()?) {
                ("item.started", "command_execution") => {
                    format!("running {}", shell_command(item["command"].as_str()?))
                }
                ("item.started", "web_search") => "searching the web".into(),
                ("item.completed", "reasoning") => "thinking".into(),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(one_line(&said))
}

/// "reading app.py", "searching the web for pandas read_csv".
fn tool(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| input[key].as_str().unwrap_or_default();
    match name {
        "Read" => format!(
            "reading {}",
            Path::new(field("file_path"))
                .file_name()
                .map_or_else(|| field("file_path").into(), |n| n.to_string_lossy())
        ),
        "Grep" => format!("searching for {}", field("pattern")),
        "Glob" => format!("looking for {}", field("pattern")),
        "WebSearch" => format!("searching the web for {}", field("query")),
        "WebFetch" => format!("reading {}", field("url")),
        "Bash" => format!("running {}", field("command")),
        other => format!("using {other}"),
    }
}

/// A command Codex runs, without the shell it's wrapped in (`bash -lc '…'`).
fn shell_command(command: &str) -> &str {
    let Some((_, rest)) = command
        .split_once(" -lc ")
        .or_else(|| command.split_once(" -c "))
    else {
        return command;
    };
    let rest = rest.trim();
    // One pair of quotes around it, not the ones inside.
    for quote in ['\'', '"'] {
        if let Some(inner) = rest.strip_prefix(quote).and_then(|r| r.strip_suffix(quote)) {
            return inner;
        }
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            file: "src/app.py".into(),
            language: "Python".into(),
            line: 42,
            text: "    rows = load(path)".into(),
            selection: None,
            word: Some("load".into()),
        }
    }

    #[test]
    fn the_agent_is_told_where_you_are_and_what_was_asked_before() {
        let root = Path::new("/work/project");
        let scratch = Path::new("/tmp/raido-docs/1");
        let before = Entry {
            asked: "how do I read a csv file?".into(),
            answer: Some("Use `csv.DictReader`.".into()),
            ..Entry::default()
        };
        let text = prompt(
            "and write one?",
            Some(&context()),
            &[&before],
            root,
            scratch,
        );
        assert!(text.contains("Project: /work/project"), "{text}");
        assert!(text.contains("Scratch folder, to try examples in: /tmp/raido-docs/1"));
        assert!(text.contains("The user is in src/app.py (Python), at line 42:"));
        assert!(text.contains("    rows = load(path)"));
        assert!(text.contains("Q: how do I read a csv file?\nA: Use `csv.DictReader`."));
        assert!(text.trim_end().ends_with("Question: and write one?"));
        // A selection goes along; a long one is cut.
        let mut selected = context();
        selected.selection = Some("x = 1\n".repeat(3_000));
        let text = prompt("what is this?", Some(&selected), &[], root, scratch);
        assert!(text.contains("They selected:\n```\nx = 1\n"));
        assert!(text.contains("[…]") && text.len() < MAX_SELECTION + 1_000);
        assert!(!text.contains("Earlier in this conversation"));
    }

    #[test]
    fn enter_asks_what_you_typed_else_the_name_or_code_at_the_cursor() {
        let mut view = DocsView::default();
        assert_eq!(view.question(), None);
        view.context = Some(context());
        let (shown, asked) = view.question().unwrap();
        assert_eq!(shown, "`load`");
        assert!(asked.starts_with("`load`: what it is, its signature"));
        view.context.as_mut().unwrap().selection = Some("load(path)".into());
        assert_eq!(view.question().unwrap().0, "The selected code");
        for c in "how do I sort by key?".chars() {
            view.insert(c);
        }
        assert_eq!(view.question().unwrap().0, "how do I sort by key?");
        // Editing the question: the cursor moves, characters go in at it.
        view.left();
        view.backspace();
        view.insert('Y');
        view.delete();
        assert_eq!(view.input, "how do I sort by keY");
        // A paste keeps its line breaks.
        view.paste(" in\nPython 3");
        assert_eq!(view.input, "how do I sort by keY in\nPython 3");
    }

    #[test]
    fn answers_render_with_each_question_and_its_examples_found() {
        let mut view = DocsView::with(vec![
            Entry {
                shown: "how do I read a csv file?".into(),
                about: Some("src/app.py:42 · Python".into()),
                answer: Some(
                    "Use `csv.DictReader`:\n\n```python\nimport csv\nprint(1)\n```\n\n\
                     **Output**\n\n```text\n1\n```"
                        .into(),
                ),
                by: "Claude (sonnet)".into(),
                took: Some(Duration::from_secs(9)),
                ..Entry::default()
            },
            Entry {
                shown: "and the `_private` one?".into(),
                error: Some("Stopped".into()),
                ..Entry::default()
            },
        ]);
        let theme = Theme::default();
        let syntax = Syntax::new(&theme);
        view.layout(60, &theme, &syntax);
        view.place(8);
        let rendered = view.rendered.as_ref().unwrap();
        let lines: Vec<String> = rendered
            .document
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let text = lines.join("\n");
        assert!(text.contains("❯ how do I read a csv file?"), "{text}");
        assert!(text.contains("src/app.py:42 · Python"));
        assert!(text.contains("Claude (sonnet) · 9s"));
        assert!(text.contains("Stopped"));
        // Opened on the last question.
        assert_eq!(view.scroll, rendered.starts[1].min(lines.len() - 8));
        assert!(lines[rendered.starts[1]].contains("_private"), "{text}");
        // The examples, by where they show.
        assert_eq!(rendered.blocks.len(), 2);
        assert_eq!(rendered.blocks[0].1, "import csv\nprint(1)\n");
        assert!(lines[rendered.blocks[0].0..]
            .iter()
            .any(|l| l.contains("import csv")));
        view.scroll = 0;
        assert_eq!(view.example(), Some("import csv\nprint(1)\n"));
        view.scroll = rendered.blocks[1].0;
        assert_eq!(view.example(), Some("1\n"));
        // Before any question: what to ask.
        let mut empty = DocsView {
            context: Some(context()),
            ..DocsView::default()
        };
        empty.layout(60, &theme, &syntax);
        let intro: String = empty
            .rendered
            .unwrap()
            .document
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            intro.contains("Python") && intro.contains("lru_cache"),
            "{intro}"
        );
        assert!(intro.contains("load"), "{intro}");
    }

    #[test]
    fn code_blocks_are_found_with_their_own_fences() {
        let text = "a\n```python\nx = 1\n```\n~~~~\n```\nnot a fence\n~~~~\n````\n```\n````\n";
        assert_eq!(
            code_blocks(text),
            [
                (1, "x = 1\n".to_string()),
                (4, "```\nnot a fence\n".to_string()),
                (8, "```\n".to_string())
            ]
        );
    }

    #[test]
    fn what_the_agent_does_shows_while_it_works() {
        let claude = Agent::Claude(None);
        let read = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"Read","input":{"file_path":"/work/project/src/app.py"}}]}}"#;
        assert_eq!(progress(&claude, read).as_deref(), Some("reading app.py"));
        let search = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"WebSearch","input":{"query":"python csv DictReader"}}]}}"#;
        assert_eq!(
            progress(&claude, search).as_deref(),
            Some("searching the web for python csv DictReader")
        );
        let run = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"python3 example.py\necho done"}}]}}"#;
        assert_eq!(
            progress(&claude, run).as_deref(),
            Some("running python3 example.py echo done")
        );
        assert_eq!(progress(&claude, r#"{"type":"result","result":"x"}"#), None);
        assert_eq!(progress(&claude, "not json"), None);
        let codex = Agent::Codex(None);
        let started = r#"{"type":"item.started","item":{"id":"1","type":"command_execution","command":"/bin/zsh -lc 'python3 -c \"print(1)\"'"}}"#;
        assert_eq!(
            progress(&codex, started).as_deref(),
            Some("running python3 -c \"print(1)\"")
        );
        let thinking =
            r#"{"type":"item.completed","item":{"id":"2","type":"reasoning","text":"…"}}"#;
        assert_eq!(progress(&codex, thinking).as_deref(), Some("thinking"));
        assert_eq!(progress(&Agent::Command(vec!["sh".into()]), read), None);
    }

    /// Wait for the answer, as the app's tick does.
    fn answer(view: &mut DocsView) {
        let start = Instant::now();
        while view.asking.is_some() {
            view.poll();
            assert!(start.elapsed() < Duration::from_secs(20), "no answer");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_command_answers_and_a_slow_one_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        // The command gets the instructions and the question on its input.
        let echo = Agent::Command(vec!["sh".into(), "-c".into(), "cat".into()]);
        let mut view = DocsView {
            context: Some(context()),
            ..DocsView::default()
        };
        view.ask(
            "q".into(),
            "how do I sort by key?".into(),
            vec![echo.clone()],
            dir.path(),
        );
        assert!(view.asking.is_some() && view.input.is_empty());
        answer(&mut view);
        let first = &view.entries[0];
        let said = first.answer.as_deref().unwrap();
        assert!(
            said.starts_with("You are the interactive documentation"),
            "{said}"
        );
        assert!(said.contains("Question: how do I sort by key?"));
        assert_eq!(first.about.as_deref(), Some("src/app.py:42 · Python"));
        assert!(first.took.is_some() && first.error.is_none());
        // A follow-up carries the question before it.
        view.ask(
            "q2".into(),
            "and in reverse?".into(),
            vec![echo],
            dir.path(),
        );
        answer(&mut view);
        let said = view.entries[1].answer.as_deref().unwrap();
        assert!(said.contains("Q: how do I sort by key?"), "{said}");
        // A command that fails says why.
        let failing = Agent::Command(vec![
            "sh".into(),
            "-c".into(),
            "echo broken >&2; exit 3".into(),
        ]);
        view.ask("q3".into(), "x".into(), vec![failing], dir.path());
        answer(&mut view);
        assert_eq!(view.entries[2].error.as_deref(), Some("sh: broken"));
        assert!(runs_examples(&Agent::Claude(None)) && runs_examples(&Agent::Codex(None)));
        assert!(!runs_examples(&Agent::Agy(None)) && !runs_examples(&Agent::Local(None)));
        assert!(
            failure("Gemini", "Error authenticating: FatalCancellationError")
                .starts_with("Gemini isn't signed in · start it once in an AI pane")
        );
        assert_eq!(failure("Codex", ""), "Codex failed");
        // Stopping kills the agent at once.
        let slow = Agent::Command(vec!["sleep".into(), "30".into()]);
        view.ask("q4".into(), "x".into(), vec![slow], dir.path());
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        assert!(view.stop());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(view.entries[3].error.as_deref(), Some(STOPPED));
        assert!(!view.stop());
        // A new topic forgets them.
        view.clear();
        assert!(view.entries.is_empty());
    }

    #[test]
    fn a_pasted_snippet_keeps_its_lines_tabs_and_spaces() {
        let mut view = DocsView {
            context: Some(context()),
            ..DocsView::default()
        };
        // A terminal sends a paste's line breaks as \r; other control characters go.
        view.paste("\r\nhow does this work?\rdef f(x):\r\n\tif x:\r\n\t\treturn  x\u{1b}\n\n");
        assert_eq!(
            view.input,
            "\nhow does this work?\ndef f(x):\n\tif x:\n\t\treturn  x\n\n"
        );
        assert!(view.multiline());
        // Blank lines around it go; the lines, tabs and double spaces stay.
        let (shown, asked) = view.question().unwrap();
        assert_eq!(
            shown,
            "how does this work?\ndef f(x):\n\tif x:\n\t\treturn  x"
        );
        assert_eq!(asked, shown);
        assert_eq!(typed("   how?  "), "how?");
        assert_eq!(typed("\n  \n    x = 1\ny = 2\n"), "    x = 1\ny = 2");
        assert_eq!(typed(" \n\t \n"), "");
        // ↑↓ keep the column where the line has one; Home End stay on the line.
        view.input = "ab\ncdef\ng".into();
        view.cursor = 6; // "cdef", after "cde"
        assert_eq!(view.line_col(), (1, 3));
        view.move_line(-1);
        assert_eq!((view.cursor, view.line_col()), (2, (0, 2)));
        view.move_line(-1);
        assert_eq!(view.cursor, 2, "no line above");
        view.move_line(1);
        view.move_line(1);
        assert_eq!(view.line_col(), (2, 1));
        view.move_line(1);
        assert_eq!(view.line_col(), (2, 1), "no line below");
        view.move_line(-1);
        view.home();
        assert_eq!(view.line_col(), (1, 0));
        view.end();
        assert_eq!(view.line_col(), (1, 4));
        view.insert('\t');
        assert_eq!(view.input, "ab\ncdef\t\ng");
        // Asked: the agent gets it as it was typed.
        let question = "explain:\nfor i in x:\n\tprint(i)  # ``` fence";
        let prompt = prompt(question, None, &[], Path::new("/p"), Path::new("/s"));
        assert!(
            prompt.ends_with(&format!("Question: {question}\n")),
            "{prompt}"
        );
        // Shown: its first line as the heading, the rest in a block of the file's
        // language, fenced past the backticks in it; Ctrl-y doesn't copy it.
        view.entries = vec![Entry {
            shown: question.into(),
            asked: question.into(),
            about: Some("src/app.py:42 · Python".into()),
            answer: Some("Prints each.\n\n```python\nprint(1)\n```".into()),
            ..Entry::default()
        }];
        let (text, _, asked) = view.transcript();
        assert!(
            text.starts_with(
                "## ❯ explain:\n\n````python\nfor i in x:\n\tprint(i)  # ``` fence\n````\n"
            ),
            "{text}"
        );
        assert_eq!(asked, [2]);
        view.changed();
        let theme = Theme::default();
        view.layout(60, &theme, &Syntax::new(&theme));
        view.place(100);
        assert_eq!(view.example(), Some("print(1)\n"));
    }

    #[cfg(unix)]
    #[test]
    fn the_next_ai_answers_when_one_cant_and_stopping_asks_none() {
        let dir = tempfile::tempdir().unwrap();
        let command =
            |script: &str| Agent::Command(["sh", "-c", script].map(String::from).to_vec());
        let signed_out = command("echo 'Invalid API key' >&2; exit 1");
        let echo = command("echo answered");
        let mut view = DocsView::default();
        view.ask(
            "q".into(),
            "x".into(),
            vec![signed_out.clone(), signed_out.clone(), echo.clone()],
            dir.path(),
        );
        answer(&mut view);
        let entry = &view.entries[0];
        assert_eq!(entry.answer.as_deref(), Some("answered"));
        assert_eq!(
            entry.failed,
            [
                "sh: Invalid API key · asked sh instead",
                "sh: Invalid API key · asked sh instead"
            ]
        );
        let (text, ..) = view.transcript();
        assert!(
            text.contains("> [!NOTE]\n> sh: Invalid API key · asked sh instead"),
            "{text}"
        );
        // None can: the last one's error, after those before it.
        view.ask(
            "q2".into(),
            "x".into(),
            vec![signed_out.clone(), signed_out],
            dir.path(),
        );
        answer(&mut view);
        assert_eq!(view.entries[1].failed.len(), 1);
        assert_eq!(
            view.entries[1].error.as_deref(),
            Some("sh: Invalid API key")
        );
        // Stopped, the next isn't asked.
        let slow = Agent::Command(vec!["sleep".into(), "30".into()]);
        view.ask("q3".into(), "x".into(), vec![slow, echo], dir.path());
        thread::sleep(Duration::from_millis(100));
        assert!(view.stop());
        thread::sleep(Duration::from_millis(200));
        view.poll();
        assert_eq!(view.entries[2].error.as_deref(), Some(STOPPED));
        assert!(view.entries[2].failed.is_empty() && view.asking.is_none());
    }

    #[test]
    fn the_chooser_offers_each_ai_installed_and_the_local_models() {
        // Claude Code on each of its models, the others on their own.
        let chooser = Chooser::new(&["codex", "claude"], None);
        let settings: Vec<&str> = chooser.offers.iter().map(|o| o.setting.as_str()).collect();
        assert_eq!(
            settings,
            ["claude:sonnet", "claude:opus", "claude:haiku", "codex"]
        );
        assert_eq!(chooser.offers[0].label, "Claude Code · Sonnet");
        assert!(chooser.offers[0].note.contains("Raido's pick"));
        assert_eq!(chooser.offers[3].label, "Codex · its own model");
        // Asked when there's a choice: two CLIs, or local models (each is an AI).
        assert!(!Chooser::worth_asking(1, false) && !Chooser::worth_asking(0, false));
        assert!(Chooser::worth_asking(2, false) && Chooser::worth_asking(0, true));
        // The local models come in the background, after the CLIs.
        let (send, listing) = mpsc::channel();
        let mut chooser = Chooser::new(&["gemini"], Some(("Ollama".into(), listing)));
        assert!(chooser.listing() && !chooser.poll());
        send.send(Ok(vec!["qwen3:8b".into(), "gemma3".into()]))
            .unwrap();
        assert!(chooser.poll() && !chooser.listing());
        let settings: Vec<&str> = chooser.offers.iter().map(|o| o.setting.as_str()).collect();
        assert_eq!(settings, ["gemini", "local:qwen3:8b", "local:gemma3"]);
        assert_eq!(chooser.offers[1].label, "Ollama · qwen3:8b");
        assert_eq!(
            Agent::parse(&chooser.offers[1].setting),
            Agent::Local(Some("qwen3:8b".into()))
        );
        chooser.up();
        assert_eq!(chooser.selected, 2);
        chooser.down();
        assert_eq!(chooser.selected, 0);
        // None, or Ollama not running: it says so.
        let (send, listing) = mpsc::channel();
        let mut chooser = Chooser::new(&[], Some(("gpu-box".into(), listing)));
        send.send(Err("connection refused".into())).unwrap();
        assert!(chooser.poll());
        assert_eq!(
            chooser.problem.as_deref(),
            Some("gpu-box: connection refused")
        );
        assert!(chooser.offers.is_empty());
    }
}

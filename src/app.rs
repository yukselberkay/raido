use crate::{
    clipboard::Clipboard,
    config::{self, Config, Theme},
    copymode,
    editor::Buffer,
    gitgraph::{self, GitView},
    highlight::Syntax,
    language,
    layout::{Axis, Tile},
    lsp::{self, Lsp},
    markdown::{self, Previews},
    quickdiff,
    review::{self, ChangesView, Target},
    search,
    session::{self, SavedBuffer, SavedPane, Session},
    sound,
    terminal::{Launch, TerminalPane},
    textsize,
    workspace::{self, FileTree, GitInfo},
};
use anyhow::{bail, Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

pub enum Pane {
    Editor(usize),
    Preview(Preview),
    Terminal(Box<TerminalPane>),
    Ai,
    /// Branches, commit graph and commit details (`:git`, Ctrl-b G).
    Git(Box<GitView>),
    /// What AI agents changed, to keep or undo (`:changes`, Ctrl-b v).
    Changes(Box<ChangesView>),
}

/// Rendered, read-only Markdown view of a buffer.
pub struct Preview {
    pub buffer: usize,
    pub scroll: usize,
    /// Source row the scroll was last aligned to; a moved cursor re-aligns it.
    pub synced: Option<usize>,
    /// Visible rows, recorded at draw time for paging.
    pub height: usize,
}

impl Preview {
    fn new(buffer: usize) -> Self {
        Self {
            buffer,
            scroll: 0,
            synced: None,
            height: 20,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// Character selection (`v`, or Shift+arrows).
    Visual,
    /// Whole-line selection (`V`).
    VisualLine,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Command,
    Search,
    Open,
}
pub struct Input {
    pub kind: InputKind,
    pub text: String,
}

/// The find / replace bar over an editor (Ctrl-f, Ctrl-h), as in VS Code.
pub struct Find {
    pub buffer: usize,
    pub query: String,
    pub replace: String,
    /// The replace row is shown, and typing goes to it when `in_replace`.
    pub replacing: bool,
    pub in_replace: bool,
    pub whole_word: bool,
    /// The query was filled in for you (selection or word under the cursor); the
    /// first key typed replaces it.
    pub fresh: bool,
    /// Where the cursor was when the bar opened: the search starts there as you type.
    origin: usize,
}

/// Theme chooser: moving previews a theme live, Enter keeps it, Esc restores.
pub struct ThemePicker {
    /// (name, origin: "built in" / "your file" / "omarchy")
    pub themes: Vec<(String, &'static str)>,
    pub shown: Vec<usize>,
    pub selected: usize,
    pub filter: String,
    /// Theme active when the picker opened, restored on Esc.
    pub original: String,
}

/// AI CLIs the launcher offers: (key, program, name, install hint).
pub const AGENTS: [(char, &str, &str, &str); 5] = [
    ('1', "codex", "OpenAI Codex", "npm install -g @openai/codex"),
    (
        '2',
        "claude",
        "Claude Code",
        "see https://code.claude.com/docs",
    ),
    (
        '3',
        "gemini",
        "Gemini CLI",
        "npm install -g @google/gemini-cli",
    ),
    (
        '4',
        "agy",
        "Antigravity CLI",
        "curl -fsSL https://antigravity.google/cli/install.sh | bash",
    ),
    (
        '5',
        "local",
        "Local LLM (Ollama)",
        "curl -fsSL https://ollama.com/install.sh | sh",
    ),
];

/// Insert-mode completion popup.
pub struct Menu {
    pub items: Vec<lsp::Completion>,
    pub shown: Vec<usize>,
    pub selected: usize,
    pub buffer: usize,
    pub row: usize,
    /// Column where the word being completed starts.
    pub start: usize,
    /// It popped up while typing; Enter takes a suggestion only once you've
    /// picked one with ↑↓ (Tab always does).
    pub auto: bool,
    pub picked: bool,
}

/// How to start an AI CLI pane.
struct AgentRun {
    run: crate::local_ai::Run,
    /// Show the pane's saved output: the CLI will not redraw its conversation.
    replay: bool,
    /// The conversation id the pane is pinned to (Claude Code).
    session: Option<String>,
}

/// Flags for a built-in AI CLI: (args, replay saved output, pinned conversation id).
fn agent_flags(
    program: &str,
    resume: bool,
    session: Option<&str>,
    exists: &dyn Fn(&str) -> bool,
    new_id: &dyn Fn() -> String,
) -> (Vec<String>, bool, Option<String>) {
    let words = |w: &[&str]| w.iter().map(|f| f.to_string()).collect::<Vec<_>>();
    match (program, resume, session) {
        ("claude", true, Some(id)) if exists(id) => {
            (words(&["--resume", id]), false, Some(id.to_string()))
        }
        // A session saved before panes had ids: let you pick, never guess.
        ("claude", true, None) => (words(&["--resume"]), true, None),
        // New pane, or a pinned conversation that never got a message.
        ("claude", _, id) => {
            let id = id.map_or_else(new_id, String::from);
            (words(&["--session-id", &id]), true, Some(id))
        }
        ("codex", true, _) => (words(&["resume", "--last"]), false, None),
        ("gemini", true, _) => (words(&["--resume", "latest"]), false, None),
        ("agy", true, _) => (words(&["--continue"]), false, None),
        _ => (Vec::new(), true, None),
    }
}

/// A random (version 4) UUID for a new conversation id, from std's per-process
/// random hasher keys.
fn conversation_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(nanos);
        hasher.write_usize(i);
        hasher.write_u32(std::process::id());
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Whether Claude Code recorded conversation `id` (it does on the first message):
/// `<config>/projects/<project>/<id>.jsonl`, config being `$CLAUDE_CONFIG_DIR` or
/// `~/.claude`.
fn claude_conversation_exists(id: &str) -> bool {
    let Some(Ok(projects)) = claude_home().map(|b| std::fs::read_dir(b.join("projects"))) else {
        return false;
    };
    let file = format!("{id}.jsonl");
    projects
        .flatten()
        .any(|project| project.path().join(&file).is_file())
}

/// Claude Code's own directory: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().join(".claude")))
}

/// The conversation a running Claude Code is in right now. It keeps a registry
/// entry per process, `<home>/sessions/<pid>.json`, whose `sessionId` follows
/// `/resume`, `/clear` and forks, unlike the id the pane was started with.
fn claude_live_conversation(home: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(home.join("sessions").join(format!("{pid}.json"))).ok()?;
    let entry: serde_json::Value = serde_json::from_str(&text).ok()?;
    // The entry must be this process's, not one left by an earlier one.
    if entry.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(pid)) {
        return None;
    }
    let id = entry.get("sessionId")?.as_str()?;
    let well_formed = id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    well_formed.then(|| id.to_string())
}

pub struct App {
    pub root: PathBuf,
    pub config: Config,
    pub theme: Theme,
    pub syntax: Syntax,
    pub previews: Previews,
    pub buffers: Vec<Buffer>,
    pub panes: BTreeMap<usize, Pane>,
    pub tiles: Tile,
    pub focused: usize,
    pub sidebar_focus: bool,
    pub sidebar_visible: bool,
    pub tree: FileTree,
    pub mode: Mode,
    pub prefix: bool,
    pub input: Option<Input>,
    pub message: String,
    pub error: bool,
    /// The F1 field guide, docked beside the panes while you keep working.
    pub help: bool,
    /// The guide has the keyboard: plain arrows and paging keys scroll it.
    pub help_focus: bool,
    pub help_scroll: usize,
    pub zoom: bool,
    pub running: bool,
    pub git: GitInfo,
    pub git_focus: bool,
    pub git_selected: usize,
    pub rectangles: Vec<(usize, Rect)>,
    pub redraw: bool,
    pub count: String,
    pending: Option<char>,
    pub clipboard: Clipboard,
    pub lsp: Lsp,
    /// Hover documentation shown near the cursor until the next key.
    pub hover: Option<String>,
    pub menu: Option<Menu>,
    pub picker: Option<ThemePicker>,
    /// Buffer opened by browsing the sidebar, and its revision then. It is reused
    /// for the next browsed file unless it was opened with Enter or edited.
    peek: Option<(usize, u64)>,
    /// The agent skill was installed or refreshed this session.
    skill_ready: bool,
    /// Screen position of the editor cursor, recorded at draw time for popups.
    pub cursor_at: Option<(u16, u16)>,
    /// Positions to return to with Ctrl-o after go-to-definition.
    jumps: Vec<(PathBuf, usize, usize)>,
    /// Mode to return to when a selection ends (Shift+arrows from insert mode).
    visual_from: Mode,
    next_pane: usize,
    git_receiver: Option<Receiver<GitInfo>>,
    last_git: Instant,
    last_reload: Instant,
    theme_stamp: Option<std::time::SystemTime>,
    completion: Option<(String, usize)>,
    /// Last session JSON written to `.raido/session.json`, to skip unchanged saves.
    session_json: String,
    last_session: Instant,
    /// Autosave this project's session (`:session forget` turns it off until restart).
    session_enabled: bool,
    /// The terminal's answer to a text size change.
    text_size: Option<Receiver<Result<String>>>,
    /// Right after Ctrl-b +, `-` or `0`, those keys repeat without the prefix.
    text_size_repeat: Option<Instant>,
    /// `raido FILE` typed in a terminal pane arrives here.
    pub remote: Option<crate::remote::Server>,
    /// Quick diff gutter marks, by buffer.
    pub diffs: HashMap<usize, quickdiff::State>,
    /// Copy mode in a terminal pane (Ctrl-b [).
    pub copy: Option<copymode::CopyMode>,
    /// Copy view (Ctrl-b Z): only the focused pane's text, edge to edge, so the
    /// terminal's own mouse selection picks up nothing else. Holds when it began,
    /// for the hint shown briefly at the start.
    pub copy_view: Option<Instant>,
    /// The find / replace bar (Ctrl-f, Ctrl-h).
    pub find: Option<Find>,
    /// The replace text and whole-word choice from the last find, for the next one.
    find_last: (String, bool),
    /// Where the explorer's entries, the sidebar's Git section and the guide were
    /// last drawn, for the mouse.
    pub explorer_rows: Rect,
    pub git_area: Rect,
    pub help_area: Rect,
    /// Docked panes (Ctrl-b d), oldest first: still running and still in the
    /// layout, just not drawn, so they come back to the same place and size.
    pub docked: Vec<usize>,
    /// The dock list (Ctrl-b D) is open, with this entry selected.
    pub dock_list: Option<usize>,
    /// Where each docked pane's tab was drawn in the top bar, for clicks.
    pub dock_tabs: Vec<(usize, Rect)>,
    /// Find in files (Ctrl-f in the explorer), shown in place of the file tree.
    pub explorer_search: Option<search::Search>,
    /// The result shown on each row of that list, for clicks.
    pub search_lines: Vec<Option<usize>>,
    /// The word (buffer, row, start column) completions were last asked for while
    /// typing, so each word asks the language server once.
    asked: Option<(usize, usize, usize)>,
    /// The completions on their way were asked for by typing (not Ctrl-Space or a
    /// trigger character like `.`): they're for that word only.
    asked_by_typing: bool,
    /// Ctrl-b y: the mouse is handed back to the terminal (select, ⌘C) with the
    /// layout unchanged, until Ctrl-b y again.
    pub mouse_released: bool,
    /// Resize mode (Ctrl-b r): arrows resize the focused pane until Enter or Esc.
    pub resizing: bool,
    /// Files AI agents changed that you haven't kept or undone yet.
    pub changes: review::Changes,
}

impl App {
    pub fn new(root: PathBuf, initial: Option<PathBuf>, demo: bool) -> Result<Self> {
        let config = Config::load()?;
        // A broken user theme must not stop Raido from starting.
        let (theme, theme_error) = match config::load_theme(&config.theme) {
            Ok(theme) => (theme, None),
            Err(error) => (Theme::default(), Some(format!("{error:#}"))),
        };
        let theme_stamp = config::theme_stamp(&config.theme);
        let tree = FileTree::new(root.clone())?;
        let mut panes = BTreeMap::new();
        panes.insert(0, Pane::Editor(0));
        let lsp = Lsp::new(root.clone(), config.lsp_enabled, config.lsp.clone());
        let mut app = Self {
            syntax: Syntax::new(&theme),
            previews: Previews::default(),
            root: root.clone(),
            config,
            theme,
            buffers: vec![Buffer::empty()],
            panes,
            tiles: Tile::Leaf(0),
            focused: 0,
            sidebar_focus: false,
            sidebar_visible: true,
            tree,
            mode: Mode::Normal,
            prefix: false,
            input: None,
            message: "Welcome to Raido. F1 opens the field guide.".into(),
            error: false,
            help: false,
            help_focus: false,
            help_scroll: 0,
            zoom: false,
            running: true,
            git: GitInfo::default(),
            git_focus: false,
            git_selected: 0,
            rectangles: Vec::new(),
            redraw: true,
            count: String::new(),
            pending: None,
            clipboard: Clipboard::default(),
            lsp,
            hover: None,
            menu: None,
            picker: None,
            peek: None,
            skill_ready: false,
            cursor_at: None,
            jumps: Vec::new(),
            visual_from: Mode::Normal,
            next_pane: 1,
            git_receiver: Some(workspace::refresh_git(root.clone())),
            last_git: Instant::now(),
            last_reload: Instant::now(),
            theme_stamp,
            completion: None,
            session_json: String::new(),
            last_session: Instant::now(),
            session_enabled: true,
            text_size: None,
            text_size_repeat: None,
            remote: None,
            diffs: HashMap::new(),
            copy: None,
            find: None,
            find_last: (String::new(), false),
            explorer_rows: Rect::default(),
            git_area: Rect::default(),
            help_area: Rect::default(),
            docked: Vec::new(),
            dock_list: None,
            dock_tabs: Vec::new(),
            explorer_search: None,
            search_lines: Vec::new(),
            asked: None,
            asked_by_typing: false,
            mouse_released: false,
            resizing: false,
            copy_view: None,
            changes: review::Changes::new(root.clone()),
        };
        if let Some(path) = initial {
            app.open(&path)?;
        }
        if demo {
            let ai = app.add_pane(Pane::Ai, Axis::Horizontal);
            if let Tile::Split { percent, .. } = &mut app.tiles {
                *percent = 63;
            }
            app.focused = ai;
            app.open_terminal(None, Some(Axis::Vertical))?;
            app.focused = 0;
            app.message = "All systems in view.  Code · shell · AI · Git".into();
        }
        if app.config.git_pane {
            let focused = app.focused;
            app.git_pane()?;
            app.focused = focused;
        }
        if let Some(error) = theme_error {
            app.fail(format!("Theme failed, using phosphor: {error}"));
        }
        Ok(app)
    }
    pub fn notice(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.error = false;
        self.redraw = true;
    }
    pub fn fail(&mut self, error: impl std::fmt::Display) {
        self.message = error.to_string();
        self.error = true;
        self.redraw = true;
    }
    pub fn buffer_id(&self) -> Option<usize> {
        if let Some(Pane::Editor(id)) = self.panes.get(&self.focused) {
            Some(*id)
        } else {
            None
        }
    }
    pub fn preview_buffer(&self) -> Option<usize> {
        if let Some(Pane::Preview(p)) = self.panes.get(&self.focused) {
            Some(p.buffer)
        } else {
            None
        }
    }
    pub fn active_buffer(&mut self) -> Option<&mut Buffer> {
        let id = self.buffer_id()?;
        self.buffers.get_mut(id)
    }
    pub fn add_pane(&mut self, pane: Pane, axis: Axis) -> usize {
        let id = self.next_pane;
        self.next_pane += 1;
        self.panes.insert(id, pane);
        self.tiles.split(self.focused, id, axis);
        self.focused = id;
        self.sidebar_focus = false;
        self.zoom = false;
        self.mode = Mode::Normal;
        id
    }
    pub fn open(&mut self, path: &Path) -> Result<()> {
        self.open_file(path, false)
    }
    /// Keep the quick diff of every buffer on screen current; with `refetch`, ask Git
    /// for the index versions again (something may have been staged or committed).
    fn update_diffs(&mut self, refetch: bool) -> bool {
        let shown: BTreeSet<usize> = self
            .panes
            .values()
            .filter_map(|pane| match pane {
                Pane::Editor(id) => Some(*id),
                _ => None,
            })
            .collect();
        let mut changed = false;
        for id in shown {
            let Some(buffer) = self.buffers.get(id) else {
                continue;
            };
            let Some(path) = buffer.path.clone() else {
                changed |= self.diffs.remove(&id).is_some();
                continue;
            };
            let state = self.diffs.entry(id).or_default();
            if state.path.as_ref() != Some(&path) {
                *state = quickdiff::State::new(path);
                state.refetch();
            } else if refetch {
                state.refetch();
            }
            changed |= state.update(buffer.revision, || buffer.text.to_string());
        }
        changed
    }
    /// `raido PATH` from one of Raido's terminals: open the file here instead of
    /// nesting another Raido; a folder of this project is shown in the explorer.
    fn remote_open(&mut self, path: &Path) -> crate::remote::Reply {
        use crate::remote::Reply;
        if path.is_dir() {
            let Ok(dir) = path.canonicalize() else {
                return Reply::Failed(format!("Cannot open {}", path.display()));
            };
            if !dir.starts_with(&self.root) {
                return Reply::Elsewhere;
            }
            self.sidebar_visible = true;
            if dir != self.root {
                self.tree.reveal(&dir);
                self.tree.expanded.insert(dir.clone());
                let _ = self.tree.refresh();
            }
            let name = dir.file_name().unwrap_or_default().to_string_lossy();
            return Reply::Opened(format!(
                "Raido is already open here; {} is in the explorer",
                workspace::printable(&name)
            ));
        }
        match self.open_from_terminal(path) {
            Ok(name) => {
                self.notice(format!("Opened {name} from the terminal"));
                Reply::Opened(format!("Opened {name} in Raido"))
            }
            Err(error) => {
                let error = format!("{error:#}");
                self.fail(error.clone());
                Reply::Failed(error)
            }
        }
    }
    /// Show PATH in a pane of its own beside the terminal (or focus the pane already
    /// showing it). A file that doesn't exist yet opens as a new buffer.
    fn open_from_terminal(&mut self, path: &Path) -> Result<String> {
        let fresh = Buffer::open(path)?;
        let key = fresh.path.clone().context("Missing file name")?;
        let id = match self
            .buffers
            .iter()
            .position(|b| b.path.as_ref() == Some(&key))
        {
            Some(id) => id,
            None => {
                self.buffers.push(fresh);
                self.buffers.len() - 1
            }
        };
        let name = self.buffers[id].name();
        let shown = self.panes.iter().find_map(|(&pane, p)| match p {
            Pane::Editor(b) if *b == id => Some(pane),
            Pane::Preview(p) if p.buffer == id => Some(pane),
            _ => None,
        });
        if let Some(pane) = shown {
            self.focused = pane;
            self.sidebar_focus = false;
            self.mode = Mode::Normal;
            return Ok(name);
        }
        let document = matches!(
            self.panes.get(&self.focused),
            Some(Pane::Editor(_) | Pane::Preview(_))
        );
        if !document && self.panes.len() < 12 {
            let wide = self
                .rectangles
                .iter()
                .find(|(pane, _)| *pane == self.focused)
                .is_none_or(|(_, area)| area.width >= 80);
            let axis = if wide {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
            self.add_pane(Pane::Editor(id), axis);
        }
        self.open(&key)?;
        Ok(name)
    }
    /// Zoom in / out: ask the terminal window for bigger or smaller text. Raido's
    /// layout follows the resize that results.
    fn change_text_size(&mut self, step: textsize::Step) {
        self.text_size = Some(textsize::change(step));
        self.text_size_repeat = Some(Instant::now());
    }
    /// Show the file highlighted in the sidebar without leaving the sidebar.
    /// Keys in the explorer's find-in-files view: typing searches, arrows preview,
    /// Enter opens, Esc goes back to the file tree.
    fn explorer_search_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(search) = self.explorer_search.as_mut() else {
            return Ok(());
        };
        let last = search.hits.len().saturating_sub(1);
        let mut restart = false;
        match key.code {
            KeyCode::Esc => {
                self.explorer_search = None;
                return Ok(());
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                let step: isize = match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -SIDEBAR_PAGE,
                    _ => SIDEBAR_PAGE,
                };
                search.selected = search.selected.saturating_add_signed(step).min(last);
                return self.show_hit(true);
            }
            KeyCode::Enter => return self.show_hit(false),
            KeyCode::Tab => {
                self.git_focus = true;
                return Ok(());
            }
            KeyCode::Backspace => restart = search.query.pop().is_some(),
            KeyCode::Char('u') if ctrl => {
                restart = !search.query.is_empty();
                search.query.clear();
            }
            KeyCode::Char('v') if ctrl => {
                let (text, _) = self.clipboard.paste();
                self.explorer_search_type(&text);
                return Ok(());
            }
            KeyCode::Char(c) if !ctrl => {
                search.query.push(c);
                restart = true;
            }
            _ => {}
        }
        if restart {
            search.start(&self.root);
        }
        Ok(())
    }
    /// Text typed or pasted into the explorer's search (its first line).
    fn explorer_search_type(&mut self, text: &str) {
        let text: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if let Some(search) = self.explorer_search.as_mut() {
            search.query.push_str(&text);
            search.start(&self.root);
        }
    }
    /// Open the selected search result at its line, the match highlighted; `peek`
    /// previews it and keeps the explorer focused.
    fn show_hit(&mut self, peek: bool) -> Result<()> {
        let Some(search) = &self.explorer_search else {
            return Ok(());
        };
        let Some(hit) = search.hits.get(search.selected).cloned() else {
            return Ok(());
        };
        let query = search.query.clone();
        let path = self.root.join(&hit.path);
        if peek {
            self.open_file(&path, true)?;
        } else {
            self.open(&path)?;
            self.sidebar_focus = false;
        }
        let canonical = path.canonicalize().unwrap_or(path);
        if let Some(b) = self
            .buffers
            .iter_mut()
            .find(|b| b.path.as_ref() == Some(&canonical))
        {
            if let Some(line) = hit.line {
                b.row = line.min(b.lines().saturating_sub(1));
                b.col = hit.col;
                b.clamp();
                b.scroll = b.row.saturating_sub(5);
            }
            b.search = query.clone();
            b.search_case = query.chars().any(char::is_uppercase);
            b.search_word = false;
        }
        Ok(())
    }
    fn peek_selected(&mut self) {
        let path = if self.git_focus {
            self.git
                .changes
                .get(self.git_selected)
                .map(|(_, p)| self.root.join(p))
        } else {
            self.tree
                .entries
                .get(self.tree.selected)
                .filter(|e| !e.directory)
                .map(|e| e.path.clone())
        };
        if let Some(path) = path.filter(|p| p.is_file()) {
            if let Err(error) = self.open_file(&path, true) {
                self.notice(format!("Can't preview {}: {error}", path.display()));
            }
        }
    }
    fn open_file(&mut self, path: &Path, peek: bool) -> Result<()> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let document = |p: &Pane| matches!(p, Pane::Editor(_) | Pane::Preview(_));
        // Browsing only fills an existing editor pane; it never creates one.
        if peek
            && !self
                .panes
                .iter()
                .any(|(id, p)| self.tiles.contains(*id) && document(p))
        {
            return Ok(());
        }
        let existing = self
            .buffers
            .iter()
            .position(|b| b.path.as_ref() == Some(&canonical));
        let id = if let Some(id) = existing {
            id
        } else if let Some(slot) = self.peek_slot().filter(|_| peek) {
            // Replace the previous preview instead of piling up buffers.
            let mut fresh = Buffer::open(&path)?;
            fresh.revision = self.buffers[slot].revision + 1;
            self.buffers[slot] = fresh;
            self.lsp.diagnostics.remove(&slot);
            slot
        } else {
            let b = Buffer::open(&path)?;
            self.buffers.push(b);
            self.buffers.len() - 1
        };
        if peek {
            if existing.is_none() {
                self.peek = Some((id, self.buffers[id].revision));
            }
        } else if self.peek.is_some_and(|(p, _)| p == id) {
            self.peek = None;
        }
        // Existing Markdown files open rendered; new or empty ones open for writing.
        let rendered = markdown::is_markdown(&canonical) && self.buffers[id].text.len_chars() > 0;
        let pane = || {
            if rendered {
                Pane::Preview(Preview::new(id))
            } else {
                Pane::Editor(id)
            }
        };
        let document = |p: &Pane| matches!(p, Pane::Editor(_) | Pane::Preview(_));
        if self.panes.get(&self.focused).is_some_and(document) {
            self.panes.insert(self.focused, pane());
        } else if let Some((&pane_id, _)) = self
            .panes
            .iter()
            .find(|(id, p)| self.tiles.contains(**id) && document(p))
        {
            self.focused = pane_id;
            self.panes.insert(pane_id, pane());
        } else {
            self.add_pane(pane(), Axis::Horizontal);
        }
        if peek {
            self.sidebar_focus = true;
            self.notice(format!(
                "Previewing {} · Enter to edit",
                self.buffers[id].name()
            ));
            return Ok(());
        }
        self.tree.reveal(&canonical);
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.notice(format!("Opened {}", self.buffers[id].name()));
        Ok(())
    }
    /// The preview buffer, if it may be replaced: untouched since browsing opened it
    /// and shown in at most one pane.
    fn peek_slot(&self) -> Option<usize> {
        let (id, revision) = self.peek?;
        let buffer = self.buffers.get(id)?;
        let shown = self
            .panes
            .values()
            .filter(|p| {
                matches!(p, Pane::Editor(b) | Pane::Preview(Preview { buffer: b, .. }) if *b == id)
            })
            .count();
        (buffer.revision == revision && !buffer.dirty && shown <= 1).then_some(id)
    }
    pub fn open_terminal(
        &mut self,
        program: Option<(&str, Vec<String>)>,
        split: Option<Axis>,
    ) -> Result<()> {
        self.open_terminal_with(program, &[], split)
    }
    fn open_terminal_with(
        &mut self,
        program: Option<(&str, Vec<String>)>,
        env: &[(String, String)],
        split: Option<Axis>,
    ) -> Result<()> {
        if split.is_some() && self.panes.len() >= 12 {
            bail!("Maximum of 12 panes; close one first");
        }
        let (shell, shell_args) = crate::platform::shell(self.config.shell.as_deref());
        let (program, args) = program.unwrap_or((&shell, shell_args));
        let label = Path::new(program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        let terminal =
            TerminalPane::spawn(program, &args, env, &self.root, label, &self.theme, &[])?;
        if let Some(axis) = split {
            self.add_pane(Pane::Terminal(Box::new(terminal)), axis);
        } else {
            self.panes
                .insert(self.focused, Pane::Terminal(Box::new(terminal)));
        }
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.notice("Terminal active. Ctrl-b then an arrow changes panes; Ctrl-b b sends Ctrl-b.");
        Ok(())
    }
    fn split_editor(&mut self, axis: Axis) -> Result<()> {
        if self.panes.len() >= 12 {
            bail!("Maximum of 12 panes");
        }
        let buffer = self.buffer_id().unwrap_or_else(|| {
            self.buffers.push(Buffer::empty());
            self.buffers.len() - 1
        });
        self.add_pane(Pane::Editor(buffer), axis);
        self.notice("Split created. Ctrl-b + arrows to move; Ctrl-b z to zoom.");
        Ok(())
    }
    fn close(&mut self, force: bool) -> Result<()> {
        if let Some(id) = self.buffer_id() {
            if self.buffers[id].dirty && !force {
                bail!("Unsaved changes. :w to save, :q! to discard this view.");
            }
        }
        let closing = self.focused;
        if self.visible().len() == 1 {
            // The last pane on screen: a docked one takes its place, else Raido quits.
            let Some(next) = self.docked.pop() else {
                self.quit(force)?;
                return Ok(());
            };
            self.tiles.remove(closing);
            self.panes.remove(&closing);
            self.focused = next;
            self.zoom = false;
            self.mode = Mode::Normal;
            self.notice("Pane closed · a docked pane came back");
            return Ok(());
        }
        if self.tiles.remove(closing) {
            self.panes.remove(&closing);
        }
        self.focused = *self.visible().first().context("No remaining pane")?;
        self.zoom = false;
        self.mode = Mode::Normal;
        self.notice("Pane closed");
        Ok(())
    }
    fn quit(&mut self, force: bool) -> Result<()> {
        if !force && self.buffers.iter().any(|b| b.dirty) {
            bail!("Unsaved buffers. :wa to save all, :qa! to discard and exit.");
        }
        if let Err(error) = self.save_session(false) {
            self.fail(format!("Session not saved: {error:#}"));
        }
        self.running = false;
        Ok(())
    }
    /// The workspace as it is now, for `.raido/session.json`.
    pub fn session(&self) -> Session {
        let relative = |path: &Path| {
            path.strip_prefix(&self.root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let buffers = self
            .buffers
            .iter()
            .map(|b| {
                // Keep the text only when the file on disk won't give it back.
                let changed = b.dirty || (b.path.is_none() && b.text.len_chars() > 0);
                SavedBuffer {
                    path: b.path.as_deref().map(relative),
                    text: changed.then(|| b.text.to_string()),
                    row: b.row,
                    col: b.col,
                    scroll: b.scroll,
                }
            })
            .collect();
        let panes = self
            .panes
            .iter()
            .map(|(id, pane)| {
                let saved = match pane {
                    Pane::Editor(buffer) => SavedPane::Editor { buffer: *buffer },
                    Pane::Preview(p) => SavedPane::Preview {
                        buffer: p.buffer,
                        scroll: p.scroll,
                    },
                    Pane::Terminal(t) => {
                        let (transcript, cwd) = t.saved();
                        SavedPane::Terminal {
                            launch: t.launch.clone(),
                            cwd: cwd.map(|cwd| relative(&cwd)),
                            transcript,
                        }
                    }
                    Pane::Ai => SavedPane::Ai,
                    Pane::Git(view) => SavedPane::Git {
                        only: view.only.clone(),
                    },
                    Pane::Changes(_) => SavedPane::Changes,
                };
                (*id, saved)
            })
            .collect();
        let mut session = Session::new(
            self.tiles.clone(),
            self.focused,
            self.zoom,
            self.sidebar_visible,
            buffers,
            panes,
        );
        session.docked = self.docked.clone();
        session
    }
    /// Save the session when it changed. `force` also starts sessions in a project
    /// that has none yet (`:session save`).
    pub fn save_session(&mut self, force: bool) -> Result<()> {
        if !self.config.restore_session || !self.session_enabled {
            return Ok(());
        }
        if !force && !session::wanted(&self.root) {
            return Ok(());
        }
        for pane in self.panes.values_mut() {
            if let Pane::Terminal(terminal) = pane {
                terminal.snapshot();
            }
        }
        if let Some(home) = claude_home() {
            self.follow_conversations(&home);
        }
        let json = serde_json::to_string(&self.session())?;
        if json != self.session_json || force {
            session::save(&self.root, &json)?;
            self.session_json = json;
        }
        Ok(())
    }
    /// Pin each Claude Code pane to the conversation it's in now, so a restored pane
    /// reopens that one even after you switched with /resume or /clear in it.
    fn follow_conversations(&mut self, home: &Path) {
        for pane in self.panes.values_mut() {
            let Pane::Terminal(terminal) = pane else {
                continue;
            };
            let Some(pid) = terminal.pid() else {
                continue;
            };
            if let Launch::Agent { name, session, .. } = &mut terminal.launch {
                if name == "claude" {
                    if let Some(id) = claude_live_conversation(home, pid) {
                        *session = Some(id);
                    }
                }
            }
        }
    }
    /// Reopen this project's saved session, if any. Problems are reported, never fatal.
    pub fn restore_session(&mut self) {
        if !self.config.restore_session {
            return;
        }
        match session::load(&self.root) {
            Ok(Some(saved)) => match self.restore(saved) {
                Ok(note) => self.notice(note),
                Err(error) => self.fail(format!("Session not restored: {error:#}")),
            },
            Ok(None) => {}
            Err(error) => self.fail(format!("Session not restored: {error:#}")),
        }
    }
    fn restore(&mut self, saved: Session) -> Result<String> {
        let mut buffers = Vec::new();
        let mut unsaved = 0;
        for b in saved.buffers {
            let path = b.path.map(|p| self.root.join(p));
            let mut buffer = match &path {
                Some(p) if p.is_file() => Buffer::open(p).unwrap_or_else(|_| Buffer::empty()),
                Some(p) => {
                    let mut buffer = Buffer::empty();
                    buffer.path = Some(p.clone());
                    buffer
                }
                None => Buffer::empty(),
            };
            if let Some(text) = b.text {
                if buffer.text != text.as_str() {
                    buffer.text = ropey::Rope::from_str(&text);
                    buffer.dirty = true;
                    buffer.revision += 1;
                    unsaved += 1;
                }
            }
            buffer.row = b.row;
            buffer.col = b.col;
            buffer.scroll = b.scroll;
            buffer.clamp();
            buffers.push(buffer);
        }
        if buffers.is_empty() {
            buffers.push(Buffer::empty());
        }
        let last = buffers.len() - 1;
        self.buffers = buffers;
        self.panes.clear();
        let mut problems = Vec::new();
        for (id, pane) in saved.panes {
            let pane = match pane {
                SavedPane::Editor { buffer } => Pane::Editor(buffer.min(last)),
                SavedPane::Preview { buffer, scroll } => {
                    let mut preview = Preview::new(buffer.min(last));
                    preview.scroll = scroll;
                    preview.synced = Some(self.buffers[buffer.min(last)].row);
                    Pane::Preview(preview)
                }
                SavedPane::Ai => Pane::Ai,
                SavedPane::Changes => Pane::Changes(Box::default()),
                SavedPane::Git { only } => {
                    let mut view = GitView::new(self.root.clone());
                    if only.is_some() {
                        view.only = only;
                        view.refresh();
                    }
                    Pane::Git(Box::new(view))
                }
                SavedPane::Terminal {
                    launch,
                    cwd,
                    transcript,
                } => match self.restore_terminal(launch, cwd, &transcript) {
                    Ok(terminal) => Pane::Terminal(Box::new(terminal)),
                    Err(error) => {
                        problems.push(format!("{error:#}"));
                        Pane::Ai
                    }
                },
            };
            self.panes.insert(id, pane);
        }
        self.tiles = saved.tiles;
        self.docked = saved.docked;
        self.docked.retain(|id| self.tiles.contains(*id));
        if self.visible().is_empty() {
            self.docked.clear();
        }
        self.next_pane = self.panes.keys().max().map_or(0, |id| id + 1);
        self.focused = if self.visible().contains(&saved.focused) {
            saved.focused
        } else {
            *self.visible().first().context("Session has no panes")?
        };
        self.zoom = saved.zoom;
        self.sidebar_visible = saved.sidebar_visible;
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.session_json = serde_json::to_string(&self.session()).unwrap_or_default();
        let mut note = format!("Session restored · {} panes", self.panes.len());
        if unsaved > 0 {
            note.push_str(&format!(" · {unsaved} unsaved buffer(s)"));
        }
        if let Some(problem) = problems.first() {
            note.push_str(&format!(" · {problem}"));
        }
        Ok(note)
    }
    /// Start a saved terminal again: shells in their last directory below their old
    /// output, AI CLIs resuming their latest conversation where they support it.
    fn restore_terminal(
        &self,
        launch: Launch,
        cwd: Option<PathBuf>,
        transcript: &str,
    ) -> Result<TerminalPane> {
        let cwd = cwd
            .map(|c| self.root.join(c))
            .filter(|c| c.is_dir())
            .unwrap_or_else(|| self.root.clone());
        let mut history = transcript.as_bytes().to_vec();
        if !history.is_empty() {
            history.extend_from_slice(
                b"\x1b[0m\r\n\x1b[2m-- restored from your last session --\x1b[0m\r\n",
            );
        }
        let agent = match &launch {
            Launch::Agent {
                name,
                model,
                session,
            } => self
                .agent_command(name, model.as_deref(), session.as_deref(), true)
                .ok()
                .map(|agent| (agent, name.clone(), model.clone())),
            _ => None,
        };
        let (program, args, env, launch, history) = match agent {
            // A CLI that reopens its conversation redraws it; don't show it twice.
            Some((agent, name, model)) => {
                let (program, args, env) = agent.run;
                let history = if agent.replay { history } else { Vec::new() };
                let launch = Launch::Agent {
                    name,
                    model,
                    session: agent.session,
                };
                (program, args, env, launch, history)
            }
            None => {
                let (shell, args) = crate::platform::shell(self.config.shell.as_deref());
                (shell, args, Vec::new(), Launch::Shell, history)
            }
        };
        let label = Path::new(&program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&program)
            .to_string();
        let mut terminal =
            TerminalPane::spawn(&program, &args, &env, &cwd, &label, &self.theme, &history)?;
        terminal.launch = launch;
        Ok(terminal)
    }
    fn save(&mut self, path: Option<PathBuf>) -> Result<()> {
        let id = self
            .buffer_id()
            .or_else(|| self.preview_buffer())
            .context("Focus an editor pane to save")?;
        // Saving under another name (:w path) writes the text as it is.
        let tidied = if path.is_none() {
            self.tidy_on_save(id)
        } else {
            None
        };
        self.buffers[id].save(path)?;
        self.lsp.saved(id, &self.buffers[id]);
        if let Some(path) = &self.buffers[id].path {
            self.changes.saved(path);
        }
        self.notice(format!(
            "Saved {} · {} lines{}",
            self.buffers[id].name(),
            self.buffers[id].lines(),
            tidied.unwrap_or_default()
        ));
        self.tree.refresh()?;
        self.last_git = Instant::now() - Duration::from_secs(5);
        Ok(())
    }
    /// Like VS Code with Go: before saving a file of a `format_on_save` language, ask
    /// its server to organize the imports, then to format the file, and apply both.
    /// Gives up after two seconds or on an error and saves the text as it is.
    /// Returns a note for the status line, or `None` when the file isn't covered.
    fn tidy_on_save(&mut self, id: usize) -> Option<String> {
        let key = self.buffers[id]
            .path
            .as_deref()
            .and_then(lsp::language_key)?;
        if !self.config.lsp_enabled || !self.config.format_on_save.iter().any(|l| l == key) {
            return None;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        // The server must have the file open before it can work on it.
        for event in self.lsp.sync(&self.buffers, &[id]) {
            self.lsp_event(event);
        }
        // Not every server organizes imports; formatting still runs without it.
        let organized = self.edits_from_server(lsp::Kind::OrganizeImports, id, deadline);
        let formatted = self.edits_from_server(lsp::Kind::Format, id, deadline);
        let program = self.lsp.program(key).unwrap_or("language server");
        Some(match (organized, formatted) {
            (_, Err(error)) => format!(" · not formatted: {error:#}"),
            (Ok(_), Ok(_)) => format!(" · imports organized and formatted by {program}"),
            (Err(_), Ok(_)) => format!(" · formatted by {program}"),
        })
    }

    /// Ask the server for `kind`'s edits to buffer `id` and apply them, handling any
    /// other server messages that arrive meanwhile.
    fn edits_from_server(&mut self, kind: lsp::Kind, id: usize, deadline: Instant) -> Result<()> {
        self.lsp
            .request(kind, id, &self.buffers[id], self.config.tab_width)?;
        loop {
            let (events, _) = self.lsp.poll(&self.buffers);
            let mut answer = None;
            for event in events {
                match event {
                    lsp::Event::Organized { buffer, edits }
                        if buffer == id && kind == lsp::Kind::OrganizeImports =>
                    {
                        answer = Some(Ok(edits));
                    }
                    lsp::Event::Format { buffer, edits }
                        if buffer == id && kind == lsp::Kind::Format =>
                    {
                        answer = Some(Ok(edits));
                    }
                    lsp::Event::Error(text) if answer.is_none() => answer = Some(Err(text)),
                    other => self.lsp_event(other),
                }
            }
            match answer {
                Some(Ok(edits)) => {
                    self.buffers[id].apply_edits(edits);
                    return Ok(());
                }
                Some(Err(text)) => bail!("{text}"),
                None if Instant::now() >= deadline => bail!("the language server took too long"),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }
    pub fn tick(&mut self) {
        let visible: Vec<usize> = self
            .panes
            .values()
            .filter_map(|p| match p {
                Pane::Editor(id) | Pane::Preview(Preview { buffer: id, .. }) => Some(*id),
                _ => None,
            })
            .collect();
        let mut events = self.lsp.sync(&self.buffers, &visible);
        let (replies, changed) = self.lsp.poll(&self.buffers);
        events.extend(replies);
        self.redraw |= changed || !events.is_empty();
        for event in events {
            self.lsp_event(event);
        }
        if let Some(search) = self.explorer_search.as_mut() {
            self.redraw |= search.poll();
        }
        let mut finished = None;
        for (id, pane) in self.panes.iter_mut() {
            match pane {
                Pane::Terminal(terminal) => {
                    self.redraw |= terminal.poll();
                    if terminal.finished() {
                        finished = Some((*id, terminal.title.clone()));
                    }
                }
                Pane::Git(view) => self.redraw |= view.poll(),
                _ => {}
            }
        }
        if self.config.review_changes {
            let busy: Vec<String> = self
                .panes
                .values()
                .filter_map(|p| match p {
                    Pane::Terminal(t) if t.agent_busy() => Some(t.title.clone()),
                    _ => None,
                })
                .collect();
            let agents = self
                .panes
                .values()
                .any(|p| matches!(p, Pane::Terminal(t) if t.is_agent() && t.exited.is_none()));
            let watching = self
                .panes
                .iter()
                .any(|(id, p)| matches!(p, Pane::Changes(_)) && !self.docked.contains(id));
            self.redraw |= self.changes.tick(&busy, agents, watching);
        }
        if let Some((id, title)) = finished {
            sound::play(&self.config.ai_sound);
            let changed = self.changes.by(&title);
            let review = if changed > 0 {
                format!(" · {} to review: Ctrl-b v", review::files(changed))
            } else {
                String::new()
            };
            if self.docked.contains(&id) {
                self.notice(format!(
                    "{title} finished{review} · it's docked: Ctrl-b D brings it back"
                ));
            } else if id != self.focused {
                self.notice(format!(
                    "{title} finished{review} · Ctrl-b arrows to go there"
                ));
            } else if changed > 0 {
                self.notice(format!(
                    "{title} changed {} · Ctrl-b v reviews them: keep or undo each change",
                    review::files(changed)
                ));
            }
        }
        let asks: Vec<_> = self
            .remote
            .as_ref()
            .map(|server| server.requests.try_iter().collect())
            .unwrap_or_default();
        for ask in asks {
            let reply = self.remote_open(&ask.path);
            ask.answer(reply);
            self.redraw = true;
        }
        if let Some(receiver) = &self.text_size {
            match receiver.try_recv() {
                Ok(Ok(status)) => {
                    self.text_size = None;
                    self.notice(status);
                }
                Ok(Err(error)) => {
                    self.text_size = None;
                    self.fail(format!("Text size: {error:#}"));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.text_size = None,
                _ => {}
            }
        }
        let mut git_changed = false;
        if let Some(receiver) = &self.git_receiver {
            match receiver.try_recv() {
                Ok(info) => {
                    git_changed = true;
                    self.git = info;
                    self.git_receiver = None;
                    self.redraw = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.git_receiver = None;
                }
                _ => {}
            }
        }
        self.redraw |= self.update_diffs(git_changed);
        // Copy view's hint goes away after its first moments.
        if self
            .copy_view
            .is_some_and(|since| (2..3).contains(&since.elapsed().as_secs()))
        {
            self.redraw = true;
        }
        if self.git_receiver.is_none() && self.last_git.elapsed() >= Duration::from_secs(4) {
            self.git_receiver = Some(workspace::refresh_git(self.root.clone()));
            self.last_git = Instant::now();
        }
        // Closing the terminal window kills Raido without a clean quit, so keep the
        // session current while you work.
        if self.last_session.elapsed() >= Duration::from_secs(5) {
            self.last_session = Instant::now();
            if let Err(error) = self.save_session(false) {
                self.session_enabled = false;
                self.fail(format!("Session autosave off: {error:#}"));
            }
        }
        if self.last_reload.elapsed() >= Duration::from_secs(2) {
            self.last_reload = Instant::now();
            // Theme files are live: saving the active one re-themes Raido immediately.
            let stamp = config::theme_stamp(&self.config.theme);
            if stamp.is_some() && stamp != self.theme_stamp {
                self.theme_stamp = stamp;
                let name = self.config.theme.clone();
                match self.apply_theme(&name) {
                    Ok(()) => self.notice(format!("Theme {name} reloaded from disk")),
                    Err(error) => self.fail(format!("Theme {name}: {error:#}")),
                }
            }
            // Refresh clean, open buffers so AI/terminal edits appear in the editor.
            for id in self
                .panes
                .values()
                .filter_map(|p| match p {
                    Pane::Editor(id) | Pane::Preview(Preview { buffer: id, .. }) => Some(*id),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
            {
                let b = &mut self.buffers[id];
                if b.dirty {
                    continue;
                }
                if let Some(path) = b.path.clone() {
                    let stamp = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok().map(|time| (time, m.len())));
                    if stamp.is_none() || stamp == b.disk_stamp {
                        continue;
                    }
                    if let Ok(mut fresh) = Buffer::open(&path) {
                        if fresh.text != b.text {
                            fresh.row = b.row;
                            fresh.col = b.col;
                            fresh.scroll = b.scroll;
                            fresh.revision = b.revision + 1;
                            fresh.clamp();
                            *b = fresh;
                            self.redraw = true;
                        } else {
                            b.disk_stamp = fresh.disk_stamp;
                        }
                    }
                }
            }
        }
    }
    pub fn event(&mut self, event: Event) {
        self.redraw = true;
        let result = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
            Event::Mouse(mouse) => self.mouse(mouse),
            Event::Paste(text) => {
                if self.explorer_search.is_some() && self.sidebar_focus && !self.git_focus {
                    self.explorer_search_type(&text);
                    Ok(())
                } else if self.find.is_some() {
                    self.find_type(&text);
                    Ok(())
                } else if let Some(input) = &mut self.input {
                    input.text.extend(text.chars().filter(|c| !c.is_control()));
                    Ok(())
                } else if let Some(Pane::Terminal(terminal)) = self.panes.get_mut(&self.focused) {
                    terminal.paste(&text)
                } else if let Some((start, end)) = self.selection() {
                    // The terminal's paste (⌘V, Ctrl-Shift-V, Windows Terminal's Ctrl-V)
                    // replaces a selection, like Ctrl-v does.
                    let text = text.replace("\r\n", "\n");
                    if let Some(b) = self.active_buffer() {
                        b.replace(start, end, &text);
                        b.end_group();
                    }
                    self.end_visual();
                    Ok(())
                } else if let Some(b) = self.active_buffer() {
                    // Terminal paste inserts at the cursor in any mode.
                    b.insert(&text.replace("\r\n", "\n"));
                    b.end_group();
                    Ok(())
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(error);
        }
        if self.mode != Mode::Insert {
            self.menu = None;
        }
    }
    /// Raido takes the mouse (config `mouse`), except in copy view, which is for
    /// selecting text with it.
    pub fn mouse_wanted(&self) -> bool {
        self.config.mouse && self.copy_view.is_none() && !self.mouse_released
    }
    /// Ctrl-b y (`:mouse`): give the mouse to the terminal for selecting and
    /// copying, the layout as it is; again to take it back for the wheel.
    fn toggle_mouse(&mut self) {
        if !self.config.mouse {
            return self.notice("mouse = false in config.toml: the terminal has the mouse already");
        }
        self.mouse_released = !self.mouse_released;
        self.notice(if self.mouse_released {
            format!(
                "Mouse released: drag to select, {} copies · Ctrl-b y gives the wheel back",
                crate::platform::COPY_KEY
            )
        } else {
            "Mouse back: wheel scrolls, click focuses · Ctrl-b y releases it again".into()
        });
    }
    /// The wheel scrolls what's under the pointer; a click focuses it, puts the
    /// editor cursor there, or opens the explorer entry.
    fn mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        let at = (mouse.column, mouse.row);
        let inside = |r: Rect| at.0 >= r.x && at.0 < r.right() && at.1 >= r.y && at.1 < r.bottom();
        let pane = self.rectangles.iter().copied().find(|(_, r)| inside(*r));
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(&(id, _)) = self.dock_tabs.iter().find(|(_, r)| inside(*r)) {
                if let Some(index) = self.docked.iter().position(|d| *d == id) {
                    self.dock_list = None;
                    self.undock(index);
                }
                return Ok(());
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                let step: isize = if up { -3 } else { 3 };
                if self.help && inside(self.help_area) {
                    self.help_scroll = self.help_scroll.saturating_add_signed(step);
                } else if inside(self.explorer_rows) {
                    match self.explorer_search.as_mut() {
                        Some(search) => {
                            let last = search.hits.len().saturating_sub(1);
                            search.selected = search.selected.saturating_add_signed(step).min(last);
                        }
                        None => self.tree.down(step),
                    }
                } else if let Some((id, rect)) = pane {
                    self.wheel(id, rect, up, at)?;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.input.is_some() || self.picker.is_some() || self.copy.is_some() {
                    return Ok(());
                }
                self.close_find();
                if inside(self.explorer_rows) && self.explorer_search.is_some() {
                    let row = usize::from(at.1 - self.explorer_rows.y);
                    if let Some(Some(hit)) = self.search_lines.get(row).copied() {
                        if let Some(search) = self.explorer_search.as_mut() {
                            search.selected = hit;
                        }
                        self.sidebar_focus = true;
                        self.git_focus = false;
                        return self.show_hit(false);
                    }
                } else if inside(self.explorer_rows) {
                    let index = self.tree.offset + usize::from(at.1 - self.explorer_rows.y);
                    if index < self.tree.entries.len() {
                        self.tree.selected = index;
                        self.sidebar_focus = true;
                        self.git_focus = false;
                        return self.sidebar_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    }
                } else if inside(self.git_area) {
                    self.sidebar_visible = true;
                    self.sidebar_focus = true;
                    self.git_focus = true;
                } else if let Some((id, rect)) = pane {
                    if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
                        self.end_visual();
                    }
                    self.focused = id;
                    self.sidebar_focus = false;
                    self.click_editor(id, rect, at);
                    // A click in the review picks the row (below the pane's own heading).
                    if let Some(Pane::Changes(view)) = self.panes.get_mut(&id) {
                        if at.1 >= rect.y + 2 {
                            let row = view.scroll + usize::from(at.1 - rect.y - 2);
                            if row < view.rows.len() {
                                view.cursor = row;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// The wheel over pane `id` (drawn at `rect`, border included).
    fn wheel(&mut self, id: usize, rect: Rect, up: bool, at: (u16, u16)) -> Result<()> {
        let step: isize = if up { -3 } else { 3 };
        let inner = Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        match self.panes.get_mut(&id) {
            Some(Pane::Terminal(t)) => {
                let column =
                    at.0.saturating_sub(inner.x)
                        .min(inner.width.saturating_sub(1));
                let row =
                    at.1.saturating_sub(inner.y)
                        .min(inner.height.saturating_sub(1));
                t.wheel(up, column, row)?;
            }
            Some(Pane::Editor(buffer)) => {
                // Like Ctrl-e / Ctrl-y in Vim: the view moves, the cursor stays on
                // screen.
                let b = &mut self.buffers[*buffer];
                let height = (inner.height - u16::from(inner.height > 3)).max(1) as usize;
                let last = b.lines().saturating_sub(1);
                b.scroll = b.scroll.saturating_add_signed(step).min(last);
                b.row = b.row.clamp(b.scroll, (b.scroll + height - 1).min(last));
                b.clamp();
            }
            Some(Pane::Preview(p)) => p.scroll = p.scroll.saturating_add_signed(step),
            Some(Pane::Git(view)) => match view.diff.as_mut() {
                Some(diff) => diff.scroll_by(step),
                None => view.step(step),
            },
            Some(Pane::Changes(view)) => {
                view.sync(&self.changes);
                view.move_by(step);
            }
            _ => {}
        }
        Ok(())
    }
    /// A click in an editor pane puts the cursor on that character.
    fn click_editor(&mut self, id: usize, rect: Rect, at: (u16, u16)) {
        let Some(Pane::Editor(buffer)) = self.panes.get(&id) else {
            return;
        };
        let tab = self.config.tab_width.max(1);
        let b = &mut self.buffers[*buffer];
        let gutter = b.lines().to_string().len().max(3) as u16 + 2;
        let (left, top) = (rect.x + 1 + gutter, rect.y + 1);
        if at.0 < left || at.1 < top {
            return;
        }
        let row = b.scroll + usize::from(at.1 - top);
        if row >= b.lines() {
            return;
        }
        // Screen column to character, with tabs as wide as they are drawn.
        let target = b.hscroll + usize::from(at.0 - left);
        let mut x = 0;
        let mut col = 0;
        for c in b.line(row).chars() {
            let width = if c == '\t' {
                tab - x % tab
            } else {
                unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
            };
            if x + width > target {
                break;
            }
            x += width;
            col += 1;
        }
        b.end_group();
        b.row = row;
        b.col = col;
        b.clamp();
    }
    fn input_start(&mut self, kind: InputKind) {
        self.input = Some(Input {
            kind,
            text: String::new(),
        });
        self.pending = None;
        self.count.clear();
    }
    fn key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // F1 opens the guide focused, refocuses an open one, and closes a focused one.
        // Everything else keeps working while it is open.
        if key.code == KeyCode::F(1) {
            if self.help && !self.help_focus {
                self.help_focus = true;
                self.notice("Guide focused · ↑↓ PgUp PgDn scroll · Esc back to work · F1 closes");
            } else {
                self.toggle_help();
            }
            return Ok(());
        }
        // A focused guide scrolls with plain keys (Alt is often taken on macOS); any
        // other key goes back to your work and does its usual job.
        if self.help && self.help_focus && !ctrl {
            let step = match key.code {
                KeyCode::Up | KeyCode::Char('k') => Some(-1),
                KeyCode::Down | KeyCode::Char('j') => Some(1),
                KeyCode::PageUp | KeyCode::Char('b') => Some(-10),
                KeyCode::PageDown | KeyCode::Char(' ') => Some(10),
                KeyCode::Home | KeyCode::Char('g') => Some(isize::MIN / 2),
                KeyCode::End | KeyCode::Char('G') => Some(isize::MAX / 2),
                _ => None,
            };
            if let Some(step) = step {
                self.help_scroll = self.help_scroll.saturating_add_signed(step);
                return Ok(());
            }
            self.help_focus = false;
            if matches!(key.code, KeyCode::Esc | KeyCode::Tab) {
                return Ok(());
            }
        }
        if self.help && key.modifiers.contains(KeyModifiers::ALT) {
            let step = match key.code {
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::PageUp => Some(-10),
                KeyCode::PageDown => Some(10),
                _ => None,
            };
            if let Some(step) = step {
                self.help_scroll = self.help_scroll.saturating_add_signed(step);
                return Ok(());
            }
        }
        if self.copy.is_some() {
            return self.copy_key(key);
        }
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        if self.dock_list.is_some() {
            return self.dock_list_key(key);
        }
        if self.resizing {
            return self.resize_key(key);
        }
        if self.hover.take().is_some() && key.code == KeyCode::Esc {
            return Ok(());
        }
        if self.input.is_some() {
            return self.input_key(key);
        }
        if self.find.is_some() {
            return self.find_key(key);
        }
        if let Some(at) = self.text_size_repeat.take() {
            if let (KeyCode::Char(c), true) = (key.code, at.elapsed() < Duration::from_secs(1)) {
                if let Some(step) = textsize::Step::key(c) {
                    self.change_text_size(step);
                    return Ok(());
                }
            }
        }
        if self.prefix {
            self.prefix = false;
            return self.prefix_key(key);
        }
        if ctrl && key.code == KeyCode::Char('b') {
            self.prefix = true;
            return Ok(());
        }
        if ctrl && key.code == KeyCode::Char('q') {
            return self.quit(false);
        }
        if ctrl && key.code == KeyCode::Char('p') {
            self.input_start(InputKind::Open);
            return Ok(());
        }
        if ctrl && key.code == KeyCode::Char('e') {
            self.toggle_explorer();
            return Ok(());
        }
        if ctrl
            && key.code == KeyCode::Char('s')
            && !matches!(self.panes.get(&self.focused), Some(Pane::Terminal(_)))
        {
            return self.save(None);
        }
        if self.sidebar_focus {
            return self.sidebar_key(key);
        }
        match self.panes.get_mut(&self.focused) {
            Some(Pane::Terminal(terminal)) => {
                if terminal.scroll_key(key) {
                    return Ok(());
                }
                return terminal.key(key);
            }
            Some(Pane::Preview(_)) => return self.preview_key(key),
            Some(Pane::Git(_)) => return self.git_pane_key(key),
            Some(Pane::Changes(_)) => return self.changes_key(key),
            Some(Pane::Ai) => {
                let chosen = match key.code {
                    KeyCode::Char('c') => Some("codex"),
                    KeyCode::Char('a') => Some("claude"),
                    KeyCode::Char('g') => Some("gemini"),
                    KeyCode::Char('y') => Some("agy"),
                    KeyCode::Char('l') => Some("local"),
                    KeyCode::Char(c) => AGENTS.iter().find(|a| a.0 == c).map(|a| a.1),
                    _ => None,
                };
                if let Some(program) = chosen {
                    return self.launch_agent(program, None);
                }
                // Keys 6–9 launch your own [ai.<name>] entries, in name order.
                if let KeyCode::Char(c @ '6'..='9') = key.code {
                    let customs = self.custom_agents();
                    if let Some(name) = customs.get(c as usize - '6' as usize) {
                        return self.launch_agent(&name.clone(), None);
                    }
                }
                if key.code == KeyCode::Char(':') {
                    self.input_start(InputKind::Command);
                }
                return Ok(());
            }
            _ => {}
        }
        if ctrl
            && self.buffer_id().is_some()
            && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('h'))
        {
            self.open_find(key.code == KeyCode::Char('h'));
            return Ok(());
        }
        if self.buffer_id().is_some() {
            if let Some((back, select)) = word_key(key) {
                return self.jump(select, |b| if back { b.word(true) } else { b.word_end() });
            }
            // Cmd+arrows, from terminals that pass Cmd on (kitty, WezTerm, Ghostty):
            // line start / end, top / bottom, as in a Mac text field.
            if key.modifiers.contains(KeyModifiers::SUPER) {
                let select = key.modifiers.contains(KeyModifiers::SHIFT);
                match key.code {
                    KeyCode::Left => return self.jump(select, Buffer::home),
                    KeyCode::Right => return self.jump(select, Buffer::end),
                    KeyCode::Up => return self.jump(select, |b| b.set_index(0)),
                    KeyCode::Down => return self.jump(select, |b| b.set_index(b.text.len_chars())),
                    _ => {}
                }
            }
            if key.code == KeyCode::Backspace
                && key.modifiers.contains(KeyModifiers::ALT)
                && self.mode == Mode::Insert
            {
                if let Some(b) = self.active_buffer() {
                    b.delete_word_back();
                }
                return Ok(());
            }
        }
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return self.visual_key(key);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if shift && is_motion(key.code) && self.buffer_id().is_some() {
            self.start_visual(Mode::Visual);
            return self.visual_key(key);
        }
        if ctrl && self.buffer_id().is_some() {
            match key.code {
                KeyCode::Char('c') => return self.copy_line(false),
                KeyCode::Char('x') => return self.copy_line(true),
                KeyCode::Char('v') => return self.paste(true),
                _ => {}
            }
        }
        if self.mode == Mode::Insert {
            return self.insert_key(key);
        }
        self.normal_key(key)
    }
    /// Ctrl-f / Ctrl-h: open the find bar on the selection (one line of it), else the
    /// word under the cursor, else the last search; Ctrl-h also shows the replace row.
    fn open_find(&mut self, replacing: bool) {
        let Some(id) = self.buffer_id() else {
            return;
        };
        let selected = self.selection().and_then(|(start, end)| {
            let text = self.buffers[id].slice(start, end);
            (!text.is_empty() && !text.contains('\n')).then_some((text, start))
        });
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.end_visual();
        }
        let b = &self.buffers[id];
        let (query, origin) = selected
            .or_else(|| word_at(b))
            .unwrap_or_else(|| (b.search.clone(), b.index()));
        let (replace, whole_word) = self.find_last.clone();
        self.find = Some(Find {
            buffer: id,
            in_replace: replacing && !query.is_empty(),
            fresh: !query.is_empty(),
            query,
            replace,
            replacing,
            whole_word,
            origin,
        });
        self.apply_find();
    }
    /// Search for the bar's query from where it opened, as you type.
    fn apply_find(&mut self) {
        let Some(find) = &self.find else {
            return;
        };
        let b = &mut self.buffers[find.buffer];
        b.search = find.query.clone();
        // Smart case: a capital letter makes the search match case.
        b.search_case = find.query.chars().any(char::is_uppercase);
        b.search_word = find.whole_word;
        if find.query.is_empty() {
            b.set_index(find.origin);
        } else {
            b.find_from(find.origin);
        }
    }
    fn close_find(&mut self) {
        if let Some(find) = self.find.take() {
            self.find_last = (find.replace, find.whole_word);
        }
    }
    /// Text typed or pasted into the find bar's active field.
    fn find_type(&mut self, text: &str) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let text: String = text
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        if find.in_replace {
            find.replace.push_str(&text);
        } else {
            if std::mem::take(&mut find.fresh) {
                find.query.clear();
            }
            find.query.push_str(&text);
            self.apply_find();
        }
    }
    fn find_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-b, Ctrl-s and Ctrl-q still work: close the bar, then act on them.
        if ctrl && matches!(key.code, KeyCode::Char('b' | 's' | 'q')) {
            self.close_find();
            return self.key(key);
        }
        let Some(find) = self.find.as_mut() else {
            return Ok(());
        };
        let id = find.buffer;
        let fresh = std::mem::take(&mut find.fresh);
        match key.code {
            KeyCode::Esc => self.close_find(),
            KeyCode::Enter if find.in_replace => {
                let with = find.replace.clone();
                let b = &mut self.buffers[id];
                if b.replace_match(&with) {
                    let (_, left) = b.match_position();
                    self.notice(format!("Replaced 1 · {left} left · ^A replaces all"));
                }
            }
            KeyCode::Enter | KeyCode::Down => {
                if !self.buffers[id].find(false) {
                    self.notice("No results");
                }
            }
            KeyCode::Up => {
                self.buffers[id].find(true);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                find.replacing = true;
                find.in_replace = !find.in_replace;
            }
            KeyCode::Char('f') if ctrl => find.in_replace = false,
            KeyCode::Char('h') if ctrl => {
                find.replacing = true;
                find.in_replace = true;
            }
            KeyCode::Char('w') if ctrl => {
                find.whole_word = !find.whole_word;
                find.origin = self.buffers[id].index();
                self.apply_find();
            }
            KeyCode::Char('a') if ctrl && find.replacing => {
                let with = find.replace.clone();
                let count = self.buffers[id].replace_all(&with);
                self.notice(match count {
                    0 => "No results".to_string(),
                    1 => "Replaced 1 match".to_string(),
                    n => format!("Replaced {n} matches · u undoes them at once"),
                });
            }
            KeyCode::Char('u') if ctrl => {
                if find.in_replace {
                    find.replace.clear();
                } else {
                    find.query.clear();
                    self.apply_find();
                }
            }
            KeyCode::Char('v') if ctrl => {
                let (text, _) = self.clipboard.paste();
                if let Some(find) = self.find.as_mut() {
                    find.fresh = fresh;
                }
                self.find_type(&text);
            }
            KeyCode::Backspace => {
                if find.in_replace {
                    find.replace.pop();
                } else {
                    if fresh {
                        find.query.clear();
                    } else {
                        find.query.pop();
                    }
                    self.apply_find();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                find.fresh = fresh;
                self.find_type(&c.to_string());
            }
            _ => {}
        }
        Ok(())
    }
    /// Move the cursor with `motion` in any mode, selecting with Shift (Option+←/→,
    /// Cmd+arrows); without Shift it ends a Shift-selection, like the arrows do.
    fn jump(&mut self, select: bool, motion: impl FnOnce(&mut Buffer)) -> Result<()> {
        let selecting = matches!(self.mode, Mode::Visual | Mode::VisualLine);
        if select && !selecting {
            self.start_visual(Mode::Visual);
        } else if !select && selecting && self.visual_from == Mode::Insert {
            self.end_visual();
        }
        if let Some(b) = self.active_buffer() {
            b.end_group();
            motion(b);
        }
        Ok(())
    }
    fn start_visual(&mut self, mode: Mode) {
        let from = if self.mode == Mode::Insert {
            Mode::Insert
        } else {
            Mode::Normal
        };
        if let Some(b) = self.active_buffer() {
            b.end_group();
            b.anchor = Some(b.index());
            self.visual_from = from;
            self.mode = mode;
        }
    }
    fn end_visual(&mut self) {
        if let Some(b) = self.active_buffer() {
            b.anchor = None;
        }
        self.mode = self.visual_from;
    }
    /// The selected char range, if a selection is active in the focused editor.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let b = &self.buffers[self.buffer_id()?];
        match self.mode {
            Mode::Visual => b.selection(self.visual_from != Mode::Insert, false),
            Mode::VisualLine => b.selection(true, true),
            _ => None,
        }
    }
    /// Copy (or cut) the selection; returns false when nothing is selected.
    fn copy_selection(&mut self, cut: bool) -> bool {
        let Some((start, end)) = self.selection() else {
            return false;
        };
        let linewise = self.mode == Mode::VisualLine;
        let id = self.buffer_id().expect("selection implies editor");
        let text = self.buffers[id].slice(start, end);
        let lines = text.matches('\n').count().max(1);
        self.clipboard.copy(text, linewise);
        let b = &mut self.buffers[id];
        if cut {
            b.replace(start, end, "");
        } else {
            let anchor = start;
            b.anchor = None;
            b.row = b.text.char_to_line(anchor);
            b.col = anchor - b.text.line_to_char(b.row);
        }
        self.end_visual();
        let what = if linewise {
            format!("{lines} line{}", if lines == 1 { "" } else { "s" })
        } else {
            "selection".into()
        };
        self.notice(format!(
            "{} {what} · Ctrl-v or p pastes",
            if cut { "Cut" } else { "Copied" }
        ));
        true
    }
    /// Ctrl-c / Ctrl-x without a selection work on the whole cursor line.
    fn copy_line(&mut self, cut: bool) -> Result<()> {
        let id = self.buffer_id().context("Focus an editor")?;
        let b = &mut self.buffers[id];
        let text = if cut {
            b.delete_line()
        } else {
            let start = b.char_index(b.row, 0);
            let end = b.char_index(b.row + 1, 0).max(start + b.line_len(b.row));
            let line = b.slice(start, end);
            if line.ends_with('\n') {
                line
            } else {
                format!("{line}\n")
            }
        };
        self.clipboard.copy(text, true);
        self.notice(if cut {
            "Cut line · Ctrl-v pastes it above the cursor line"
        } else {
            "Copied line · Ctrl-v pastes it above the cursor line, p below"
        });
        Ok(())
    }
    /// Paste the clipboard. Whole lines go above (Ctrl-v, `P`) or below (`p`) the
    /// cursor line; other text goes at the cursor (Ctrl-v, `P`) or after it (`p`).
    fn paste(&mut self, before: bool) -> Result<()> {
        let (text, linewise) = self.clipboard.paste();
        if text.is_empty() {
            self.notice("Clipboard is empty");
            return Ok(());
        }
        let insert = self.mode == Mode::Insert;
        let b = self.active_buffer().context("Focus an editor")?;
        b.end_group();
        if linewise {
            b.paste_lines(&text, before);
        } else {
            if !before && !insert && b.line_len(b.row) > 0 {
                b.move_by(1, 0);
            }
            let index = b.index();
            b.replace(index, index, &text);
        }
        Ok(())
    }
    fn visual_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let from_insert = self.visual_from == Mode::Insert;
        match key.code {
            KeyCode::Esc => self.end_visual(),
            KeyCode::Char('c') if ctrl => {
                self.copy_selection(false);
            }
            KeyCode::Char('x') if ctrl => {
                self.copy_selection(true);
            }
            KeyCode::Char('v') if ctrl => self.replace_selection_with_clipboard()?,
            KeyCode::Char('d' | 'u') if ctrl => self.normal_key(key)?,
            _ if ctrl && !is_motion(key.code) => {}
            KeyCode::Backspace | KeyCode::Delete => self.delete_selection(),
            // Plain arrows end a Shift-selection, like any text field.
            code if from_insert && !shift && is_motion(code) => {
                self.end_visual();
                return self.insert_key(key);
            }
            code if is_motion(code) || (!from_insert && is_vim_motion(code)) => {
                let key = KeyEvent::new(code, key.modifiers - KeyModifiers::SHIFT);
                self.normal_key(key)?;
            }
            // Typing over a Shift-selection replaces it; an opening bracket or quote
            // wraps it instead.
            KeyCode::Char(c) if from_insert => {
                let wrapped = match (self.selection(), self.buffer_id()) {
                    (Some((start, end)), Some(id)) if self.config.auto_pairs => {
                        let b = &self.buffers[id];
                        let rules = language::Rules::new(b, self.config.tab_width);
                        language::surround(&rules, c, &b.slice(start, end))
                            .map(|text| (start, end, text))
                    }
                    _ => None,
                };
                if let Some((start, end, text)) = wrapped {
                    if let Some(b) = self.active_buffer() {
                        b.anchor = None;
                        b.replace(start, end, &text);
                    }
                    self.mode = self.visual_from;
                    return Ok(());
                }
                self.delete_selection();
                if let Some(b) = self.active_buffer() {
                    b.insert(&c.to_string());
                }
            }
            KeyCode::Enter if from_insert => {
                self.delete_selection();
                if let Some(b) = self.active_buffer() {
                    b.newline();
                }
            }
            KeyCode::Char('v') => {
                if self.mode == Mode::Visual {
                    self.end_visual();
                } else {
                    self.mode = Mode::Visual;
                }
            }
            KeyCode::Char('V') => {
                if self.mode == Mode::VisualLine {
                    self.end_visual();
                } else {
                    self.mode = Mode::VisualLine;
                }
            }
            KeyCode::Char('y') => {
                self.copy_selection(false);
            }
            KeyCode::Char('d') | KeyCode::Char('x') => {
                self.copy_selection(true);
            }
            KeyCode::Char('c') | KeyCode::Char('s') => {
                self.copy_selection(true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('p') | KeyCode::Char('P') => self.replace_selection_with_clipboard()?,
            _ => {}
        }
        Ok(())
    }
    fn delete_selection(&mut self) {
        if let Some((start, end)) = self.selection() {
            if let Some(b) = self.active_buffer() {
                b.replace(start, end, "");
            }
        }
        self.end_visual();
    }
    fn replace_selection_with_clipboard(&mut self) -> Result<()> {
        let (text, _) = self.clipboard.paste();
        if let Some((start, end)) = self.selection() {
            let b = self.active_buffer().context("Focus an editor")?;
            b.replace(start, end, &text);
        }
        self.end_visual();
        Ok(())
    }
    /// Ctrl-b [: freeze the focused terminal pane and select from its history.
    fn enter_copy_mode(&mut self) {
        let Some(Pane::Terminal(terminal)) = self.panes.get(&self.focused) else {
            self.notice(
                "Copy mode is for terminal and AI panes; in editors, v / V select and y copies",
            );
            return;
        };
        let (rows, cursor, top) = terminal.history();
        self.copy = Some(copymode::CopyMode::new(self.focused, rows, cursor, top));
        self.sidebar_focus = false;
        self.notice(
            "Copy mode: move with arrows / hjkl, v or V selects, y copies, / ? search, q leaves",
        );
    }
    fn copy_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-b and Ctrl-q still work: leave copy mode, then act on them.
        if ctrl && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('q')) {
            self.copy = None;
            return self.key(key);
        }
        let Some(mode) = self.copy.as_mut() else {
            return Ok(());
        };
        match mode.key(key) {
            copymode::Outcome::Stay => {}
            copymode::Outcome::Exit => self.copy = None,
            copymode::Outcome::NotFound(query) => self.notice(format!("Not found: {query}")),
            copymode::Outcome::Copy(text) => {
                self.copy = None;
                let lines = text.lines().count().max(1);
                let chars = text.chars().count();
                self.clipboard.copy(text, false);
                self.notice(if lines > 1 {
                    format!("Copied {lines} lines")
                } else {
                    format!("Copied {chars} characters")
                });
            }
        }
        Ok(())
    }
    /// Ctrl-b Z: the focused pane alone, without borders, sidebar, line numbers or
    /// status lines, for selecting with the mouse; again to return.
    fn toggle_copy_view(&mut self) {
        self.copy_view = match self.copy_view {
            Some(_) => None,
            None => Some(Instant::now()),
        };
        self.sidebar_focus = false;
    }
    fn prefix_key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(step) = match key.code {
            KeyCode::Char(c) => textsize::Step::key(c),
            _ => None,
        } {
            self.change_text_size(step);
            return Ok(());
        }
        match key.code {
            KeyCode::Char('%') => self.split_editor(Axis::Horizontal)?,
            KeyCode::Char('"') => self.split_editor(Axis::Vertical)?,
            KeyCode::Char('c') => self.open_terminal(None, Some(Axis::Vertical))?,
            KeyCode::Char('C') => self.open_terminal(None, Some(Axis::Horizontal))?,
            KeyCode::Char('a') => self.open_ai(Some(Axis::Horizontal))?,
            KeyCode::Char('A') => self.open_ai(Some(Axis::Vertical))?,
            KeyCode::Char('e') => self.toggle_explorer(),
            KeyCode::Char('s') => {
                self.sidebar_visible = !self.sidebar_visible;
                self.sidebar_focus = false;
            }
            KeyCode::Char('g') => {
                self.sidebar_visible = true;
                self.sidebar_focus = true;
                self.git_focus = true;
            }
            KeyCode::Char('G') => self.git_pane()?,
            KeyCode::Char('v') => self.changes_pane()?,
            KeyCode::Char('z') => {
                self.zoom = !self.zoom;
                self.sidebar_focus = false;
            }
            KeyCode::Char('x') => self.close(false)?,
            KeyCode::Char('q') => self.quit(false)?,
            KeyCode::Char('m') => self.toggle_preview()?,
            KeyCode::Char('t') => self.open_picker(),
            KeyCode::Char('M') => self.split_preview()?,
            KeyCode::Char('?') => self.toggle_help(),
            KeyCode::Char('[') => self.enter_copy_mode(),
            KeyCode::Char('Z') => self.toggle_copy_view(),
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            KeyCode::Char('o') | KeyCode::Tab => self.cycle(),
            KeyCode::Char('b') => {
                if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
                    t.send(&[2])?;
                }
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                if key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.move_pane(key.code)
            }
            KeyCode::Char('H') => self.move_pane(KeyCode::Left),
            KeyCode::Char('L') => self.move_pane(KeyCode::Right),
            KeyCode::Char('K') => self.move_pane(KeyCode::Up),
            KeyCode::Char('J') => self.move_pane(KeyCode::Down),
            KeyCode::Char(' ') => self.flip_split(),
            KeyCode::Char('d') => self.dock(),
            KeyCode::Char('y') => self.toggle_mouse(),
            KeyCode::Char('r') => {
                self.resizing = true;
                self.zoom = false;
                self.notice("Resize: ← → ↑ ↓ or h j k l · Enter or Esc when done");
            }
            KeyCode::Char('D') => self.open_dock_list(),
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    let axis = if matches!(key.code, KeyCode::Left | KeyCode::Right) {
                        Axis::Horizontal
                    } else {
                        Axis::Vertical
                    };
                    let delta = if matches!(key.code, KeyCode::Left | KeyCode::Up) {
                        -5
                    } else {
                        5
                    };
                    self.tiles.resize(self.focused, axis, delta);
                } else {
                    self.move_focus(key.code);
                }
            }
            KeyCode::Char('h') => self.move_focus(KeyCode::Left),
            KeyCode::Char('l') => self.move_focus(KeyCode::Right),
            KeyCode::Char('k') => self.move_focus(KeyCode::Up),
            KeyCode::Char('j') => self.move_focus(KeyCode::Down),
            KeyCode::PageUp | KeyCode::PageDown => {
                if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
                    t.scroll_key(KeyEvent::new(key.code, KeyModifiers::NONE));
                }
            }
            KeyCode::Esc => {}
            _ => self.notice(
                "Ctrl-b: % split · \" stack · c shell below · C beside · a AI · arrows focus · ⇧arrows move · d dock · ? help",
            ),
        }
        Ok(())
    }
    /// Ctrl-e: focus the file explorer, even from the sidebar's Git section;
    /// pressed in the explorer, return to the active pane.
    fn toggle_explorer(&mut self) {
        if self.sidebar_focus && !self.git_focus {
            self.sidebar_focus = false;
        } else {
            self.sidebar_visible = true;
            self.sidebar_focus = true;
            self.git_focus = false;
        }
        self.mode = Mode::Normal;
    }
    /// The panes on screen (not docked), in layout order.
    pub fn visible(&self) -> Vec<usize> {
        let mut ids = Vec::new();
        self.tiles.leaves(&mut ids);
        ids.retain(|id| !self.docked.contains(id));
        ids
    }
    /// A short name for a pane: the file, the program, GIT or AI.
    pub fn pane_name(&self, id: usize) -> String {
        match self.panes.get(&id) {
            Some(Pane::Editor(b)) | Some(Pane::Preview(Preview { buffer: b, .. })) => {
                workspace::printable(&self.buffers[*b].name())
            }
            Some(Pane::Terminal(t)) => t.title.clone(),
            Some(Pane::Git(_)) => "Git".into(),
            Some(Pane::Changes(_)) => "Changes".into(),
            Some(Pane::Ai) => "AI launcher".into(),
            None => "?".into(),
        }
    }
    /// Ctrl-b d: hide the focused pane, still running; it waits in the dock (tabs
    /// in the top bar) and comes back to the same place and size.
    fn dock(&mut self) {
        let id = self.focused;
        if self.visible().len() < 2 {
            return self.notice("The last pane on screen stays · open another first");
        }
        if self.copy.as_ref().is_some_and(|c| c.pane == id) {
            self.copy = None;
        }
        self.close_find();
        // Focus the pane that takes over most of its room.
        let room = self
            .rectangles
            .iter()
            .find(|(p, _)| *p == id)
            .map(|(_, r)| *r);
        let area = self
            .rectangles
            .iter()
            .map(|(_, r)| *r)
            .reduce(|a, b| a.union(b));
        self.docked.push(id);
        self.zoom = false;
        let mut after = Vec::new();
        if let Some(area) = area {
            self.tiles.layout(area, &self.docked, &mut after);
        }
        let heir = room.and_then(|room| {
            after
                .iter()
                .max_by_key(|(_, r)| {
                    let shared = r.intersection(room);
                    u32::from(shared.width) * u32::from(shared.height)
                })
                .map(|(p, _)| *p)
        });
        self.focused = heir
            .or_else(|| self.visible().first().copied())
            .unwrap_or(id);
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        let name = self.pane_name(id);
        self.notice(format!(
            "{name} docked and still running · Ctrl-b D or click its tab to bring it back"
        ));
    }
    /// Bring docked pane number `index` back where it was, and focus it.
    fn undock(&mut self, index: usize) {
        if index >= self.docked.len() {
            return;
        }
        let id = self.docked.remove(index);
        self.focused = id;
        self.zoom = false;
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        self.close_find();
        let name = self.pane_name(id);
        self.notice(format!("{name} is back"));
    }
    /// Ctrl-b D: the docked panes, to bring one back (or close it).
    fn open_dock_list(&mut self) {
        if self.docked.is_empty() {
            self.notice("Nothing docked · Ctrl-b d docks the focused pane");
        } else {
            self.dock_list = Some(self.docked.len() - 1);
        }
    }
    fn dock_list_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(selected) = self.dock_list else {
            return Ok(());
        };
        let len = self.docked.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.dock_list = Some(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.dock_list = Some((selected + 1).min(len.saturating_sub(1)))
            }
            KeyCode::Enter => {
                self.dock_list = None;
                self.undock(selected);
            }
            KeyCode::Char(c @ '1'..='9') => {
                let index = c as usize - '1' as usize;
                if index < len {
                    self.dock_list = None;
                    self.undock(index);
                }
            }
            KeyCode::Char('x') => {
                let id = self.docked.remove(selected);
                let name = self.pane_name(id);
                self.tiles.remove(id);
                self.panes.remove(&id);
                self.dock_list =
                    (!self.docked.is_empty()).then(|| selected.min(self.docked.len() - 1));
                self.notice(format!("{name} closed"));
            }
            _ => self.dock_list = None,
        }
        Ok(())
    }
    fn cycle(&mut self) {
        let ids = self.visible();
        let index = ids.iter().position(|id| *id == self.focused).unwrap_or(0);
        self.focused = ids[(index + 1) % ids.len()];
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
    }
    /// Ctrl-b arrows: the pane on that side of the focused one, like tmux. A pane
    /// counts only when it lies wholly past that edge and shares some of the edge, so
    /// a wide pane below is never "right"; among several, the nearest, then the one
    /// level with the focused pane's middle.
    fn move_focus(&mut self, direction: KeyCode) {
        if let Some(id) = self.neighbor(direction) {
            self.focused = id;
            self.sidebar_focus = false;
            self.mode = Mode::Normal;
        }
    }
    /// The pane next to the focused one on that side (see [`move_focus`]).
    fn neighbor(&self, direction: KeyCode) -> Option<usize> {
        let &(_, current) = self.rectangles.iter().find(|(id, _)| *id == self.focused)?;
        let span = |start: u16, size: u16| (start as i32, start as i32 + size as i32);
        let (left, right) = span(current.x, current.width);
        let (top, bottom) = span(current.y, current.height);
        let mut best = None;
        for &(id, rect) in &self.rectangles {
            if id == self.focused {
                continue;
            }
            let (x0, x1) = span(rect.x, rect.width);
            let (y0, y1) = span(rect.y, rect.height);
            // Distance past the edge, the shared stretch of edge, and how far the
            // candidate's middle is from ours along that edge.
            let (gap, low, high, middle, ours) = match direction {
                KeyCode::Left => (
                    left - x1,
                    y0.max(top),
                    y1.min(bottom),
                    y0 + y1,
                    top + bottom,
                ),
                KeyCode::Right => (
                    x0 - right,
                    y0.max(top),
                    y1.min(bottom),
                    y0 + y1,
                    top + bottom,
                ),
                KeyCode::Up => (top - y1, x0.max(left), x1.min(right), x0 + x1, left + right),
                _ => (
                    y0 - bottom,
                    x0.max(left),
                    x1.min(right),
                    x0 + x1,
                    left + right,
                ),
            };
            if gap < 0 || low >= high {
                continue;
            }
            let score = (gap, (middle - ours).abs());
            if best.is_none_or(|(_, s)| score < s) {
                best = Some((id, score));
            }
        }
        best.map(|(id, _)| id)
    }
    /// Ctrl-b Shift+arrow (or H J K L): move the focused pane that way. With a pane
    /// on that side, the two trade places; at the edge already, it takes the whole
    /// side of the window (a terminal below two editors moves to a full-height
    /// column on the right).
    fn move_pane(&mut self, direction: KeyCode) {
        if self.visible().len() < 2 {
            return self.notice("Only one pane");
        }
        if self.zoom {
            return self.notice("Ctrl-b z first: the pane is maximized");
        }
        if let Some(other) = self.neighbor(direction) {
            self.tiles.swap(self.focused, other);
        } else {
            let axis = match direction {
                KeyCode::Left | KeyCode::Right => Axis::Horizontal,
                _ => Axis::Vertical,
            };
            let last = matches!(direction, KeyCode::Right | KeyCode::Down);
            if !self.tiles.move_to_side(self.focused, axis, last) {
                return self.notice("Already there");
            }
        }
        self.sidebar_focus = false;
        self.notice("Pane moved · Ctrl-b Shift+arrows move it on · Ctrl-b Space flips");
    }
    /// Resize mode (Ctrl-b r): each arrow moves the focused pane's nearest split on
    /// that axis by 5%, without Ctrl-b again; it works where the OS keeps
    /// Ctrl+arrows for itself (macOS switches desktops with them). Any other key
    /// ends it and does its usual job.
    fn resize_key(&mut self, key: KeyEvent) -> Result<()> {
        let (axis, delta) = match key.code {
            KeyCode::Left | KeyCode::Char('h') => (Axis::Horizontal, -5),
            KeyCode::Right | KeyCode::Char('l') => (Axis::Horizontal, 5),
            KeyCode::Up | KeyCode::Char('k') => (Axis::Vertical, -5),
            KeyCode::Down | KeyCode::Char('j') => (Axis::Vertical, 5),
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('r') | KeyCode::Char('q') => {
                self.resizing = false;
                self.notice("Resized");
                return Ok(());
            }
            _ => {
                self.resizing = false;
                return self.key(key);
            }
        };
        if !self.tiles.resize(self.focused, axis, delta) {
            self.notice(match axis {
                Axis::Horizontal => "No side-by-side split here · ↑ ↓ resize stacked ones",
                Axis::Vertical => "No stacked split here · ← → resize side-by-side ones",
            });
        }
        Ok(())
    }
    /// Ctrl-b Space: turn the focused pane's split between side by side and stacked.
    fn flip_split(&mut self) {
        if self.tiles.flip(self.focused) {
            self.zoom = false;
            self.notice("Split flipped");
        } else {
            self.notice("Only one pane");
        }
    }
    fn sidebar_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !self.git_focus {
            if ctrl && key.code == KeyCode::Char('f') {
                if self.explorer_search.is_none() {
                    self.explorer_search = Some(search::Search::new(String::new()));
                }
                return Ok(());
            }
            if self.explorer_search.is_some() {
                return self.explorer_search_key(key);
            }
        }
        if key.code == KeyCode::Tab {
            self.git_focus = !self.git_focus;
            return Ok(());
        }
        // PgUp / PgDn (Fn-↑/↓ on a Mac) move a few entries, Home / End to either end.
        let step = match key.code {
            KeyCode::Up | KeyCode::Char('k') => Some(-1),
            KeyCode::Down | KeyCode::Char('j') => Some(1),
            KeyCode::PageUp => Some(-SIDEBAR_PAGE),
            KeyCode::PageDown => Some(SIDEBAR_PAGE),
            KeyCode::Home => Some(isize::MIN / 2),
            KeyCode::End => Some(isize::MAX / 2),
            _ => None,
        };
        if let Some(step) = step {
            if self.git_focus {
                let last = self.git.changes.len().saturating_sub(1);
                self.git_selected = self.git_selected.saturating_add_signed(step).min(last);
            } else {
                self.tree.down(step);
            }
            self.peek_selected();
            return Ok(());
        }
        if self.git_focus {
            match key.code {
                KeyCode::Enter => {
                    if let Some((_, path)) = self.git.changes.get(self.git_selected) {
                        let path = self.root.join(path);
                        self.open(&path)?;
                    }
                }
                KeyCode::Esc => self.sidebar_focus = false,
                KeyCode::Char(':') => self.input_start(InputKind::Command),
                _ => {}
            }
            return Ok(());
        }
        let path = match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                self.tree.left()?;
                None
            }
            KeyCode::Right | KeyCode::Char('l') => self.tree.right()?,
            KeyCode::Enter => self.tree.activate()?,
            KeyCode::Char(' ') => {
                self.tree.toggle()?;
                None
            }
            KeyCode::Char('c') => {
                self.tree.collapse_all()?;
                None
            }
            KeyCode::Char('E') => {
                self.tree.expand_all()?;
                None
            }
            KeyCode::Char('r') => {
                self.tree.refresh()?;
                self.notice("File tree refreshed");
                None
            }
            KeyCode::Char('n') => {
                self.input = Some(Input {
                    kind: InputKind::Command,
                    text: "e ".into(),
                });
                None
            }
            KeyCode::Esc => {
                self.sidebar_focus = false;
                None
            }
            KeyCode::Char(':') => {
                self.input_start(InputKind::Command);
                None
            }
            _ => None,
        };
        if let Some(path) = path {
            self.open(&path)?;
        }
        Ok(())
    }
    fn insert_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if self.menu.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || ctrl => {
                    self.menu_move(-1);
                    return Ok(());
                }
                KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || ctrl => {
                    self.menu_move(1);
                    return Ok(());
                }
                KeyCode::Tab => {
                    self.menu_accept();
                    return Ok(());
                }
                // Enter takes the suggestion, unless it popped up by itself and none
                // was picked, or the word is already typed out: then it's a new line.
                KeyCode::Enter
                    if !self.menu_typed_out()
                        && self.menu.as_ref().is_some_and(|m| !m.auto || m.picked) =>
                {
                    self.menu_accept();
                    return Ok(());
                }
                KeyCode::Enter => self.menu = None,
                KeyCode::Esc => {
                    self.menu = None;
                    return Ok(());
                }
                _ => {}
            }
        }
        // Ctrl-Space, or Ctrl-n as in Vim: macOS (and Linux input methods like fcitx)
        // can take Ctrl-Space for switching keyboard languages.
        if ctrl && matches!(key.code, KeyCode::Char(' ' | '@' | 'n')) {
            let ready = self.buffer_id().is_some_and(|id| self.lsp.ready(id));
            if ready {
                self.asked_by_typing = false;
                return self.lsp_request(lsp::Kind::Completion);
            }
            if !self.suggest_words(true, false) {
                self.notice("No completions");
            }
            return Ok(());
        }
        let typed = match key.code {
            KeyCode::Char(c) if !ctrl => Some(c),
            _ => None,
        };
        self.insert_edit(key)?;
        if self.menu.is_some() {
            self.menu_filter();
        }
        if let (Some(c), Some(id)) = (typed, self.buffer_id()) {
            // In a string or a comment, suggestions only come when asked (Ctrl-Space).
            if self.in_string_or_comment(id) {
                if self.menu.as_ref().is_some_and(|m| m.auto) {
                    self.menu = None;
                }
            } else if self.menu.is_none() && self.lsp.is_trigger(id, c) {
                self.asked_by_typing = false;
                let _ = self.lsp_request(lsp::Kind::Completion);
            } else if self.menu.is_none() && self.config.autocomplete && is_word(c) {
                self.autocomplete(id);
            }
        }
        Ok(())
    }
    /// The word being typed before the cursor: (start column, text).
    fn typed_word(&self, id: usize) -> (usize, String) {
        let b = &self.buffers[id];
        let line: Vec<char> = b.line(b.row).chars().collect();
        let col = b.col.min(line.len());
        let mut start = col;
        while start > 0 && is_word(line[start - 1]) {
            start -= 1;
        }
        (start, line[start..col].iter().collect())
    }
    /// After two letters of a word: ask the language server once for this word, or
    /// without one, suggest keywords and words from your files.
    /// Whether the cursor of buffer `id` is inside a string or a comment.
    fn in_string_or_comment(&mut self, id: usize) -> bool {
        let b = &self.buffers[id];
        self.syntax.in_string_or_comment(id, b, b.row, b.col)
    }
    fn autocomplete(&mut self, id: usize) {
        let (start, word) = self.typed_word(id);
        if word.chars().count() < 2 || word.starts_with(|c: char| c.is_ascii_digit()) {
            return;
        }
        if self.lsp.ready(id) {
            let here = (id, self.buffers[id].row, start);
            if self.asked != Some(here) {
                self.asked = Some(here);
                self.asked_by_typing = true;
                let _ = self.lsp_request(lsp::Kind::Completion);
            }
            return;
        }
        self.suggest_words(false, true);
    }
    /// Keywords and words from the open files of this language that start with the
    /// word being typed. False when there are none.
    fn suggest_words(&mut self, any_length: bool, auto: bool) -> bool {
        let Some(id) = self.buffer_id() else {
            return false;
        };
        let (start, word) = self.typed_word(id);
        if word.is_empty() && !any_length {
            return false;
        }
        let lang = language::Lang::detect(self.buffers[id].path.as_deref());
        let texts: Vec<String> = self
            .buffers
            .iter()
            .filter(|b| language::Lang::detect(b.path.as_deref()) == lang)
            .map(|b| b.text.slice(..b.text.len_chars().min(400_000)).to_string())
            .collect();
        let items: Vec<lsp::Completion> = language::suggestions(lang, &texts, &word)
            .into_iter()
            .map(|(label, detail)| lsp::Completion::word(&label, detail))
            .collect();
        if items.is_empty() {
            return false;
        }
        self.menu = Some(Menu {
            shown: (0..items.len()).collect(),
            items,
            selected: 0,
            buffer: id,
            row: self.buffers[id].row,
            start,
            auto,
            picked: false,
        });
        self.menu_filter();
        self.menu.is_some()
    }
    /// Whether the selected suggestion is exactly what's already typed.
    fn menu_typed_out(&self) -> bool {
        let Some(menu) = &self.menu else {
            return false;
        };
        let b = &self.buffers[menu.buffer];
        let typed: String = b
            .line(menu.row)
            .chars()
            .skip(menu.start)
            .take(b.col.saturating_sub(menu.start))
            .collect();
        menu.shown
            .get(menu.selected)
            .is_some_and(|&i| menu.items[i].insert == typed)
    }
    fn insert_edit(&mut self, key: KeyEvent) -> Result<()> {
        let width = self.config.tab_width;
        let typing = language::Typing {
            auto_pairs: self.config.auto_pairs,
            smart_indent: self.config.smart_indent,
        };
        let Some(buffer) = self.active_buffer() else {
            return Ok(());
        };
        let rules = language::Rules::new(buffer, width);
        match key.code {
            KeyCode::Esc => {
                buffer.end_group();
                self.mode = Mode::Normal;
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                language::type_char(buffer, &rules, c, typing)
            }
            KeyCode::Enter if typing.smart_indent => language::enter(buffer, &rules),
            KeyCode::Enter => buffer.newline(),
            KeyCode::Tab => language::tab(buffer, &rules),
            KeyCode::BackTab => language::dedent(buffer, &rules),
            KeyCode::Backspace => language::backspace(buffer, &rules, typing),
            KeyCode::Delete => buffer.delete_char(),
            KeyCode::Left => {
                buffer.end_group();
                buffer.move_by(-1, 0);
            }
            KeyCode::Right => {
                buffer.end_group();
                buffer.move_by(1, 0);
            }
            KeyCode::Up => {
                buffer.end_group();
                buffer.move_by(0, -1);
            }
            KeyCode::Down => {
                buffer.end_group();
                buffer.move_by(0, 1);
            }
            KeyCode::Home => buffer.home(),
            KeyCode::End => buffer.end(),
            KeyCode::PageUp => buffer.move_by(0, -20),
            KeyCode::PageDown => buffer.move_by(0, 20),
            _ => {}
        }
        Ok(())
    }
    fn normal_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char(':') {
            self.input_start(InputKind::Command);
            return Ok(());
        }
        if key.code == KeyCode::Char('/') {
            self.input_start(InputKind::Search);
            return Ok(());
        }
        if key.code == KeyCode::Tab {
            self.next_buffer(false);
            return Ok(());
        }
        if key.code == KeyCode::BackTab {
            self.next_buffer(true);
            return Ok(());
        }
        if key.code == KeyCode::Esc {
            self.pending = None;
            self.count.clear();
            return Ok(());
        }
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_digit() && (c != '0' || !self.count.is_empty()) && !ctrl {
                if self.count.len() < 6 {
                    self.count.push(c);
                }
                // Relative numbers show distances; say how to use the one typed.
                if self.config.relative_numbers {
                    let n = &self.count;
                    self.notice(format!(
                        "{n} · j/↓/Enter: {n} lines down · k/↑/-: {n} lines up · G: line {n}"
                    ));
                }
                return Ok(());
            }
        }
        let count = self.count.parse::<usize>().unwrap_or(1).clamp(1, 100_000);
        let has_count = !self.count.is_empty();
        self.count.clear();
        let pending = self.pending.take();
        let Some(id) = self.buffer_id() else {
            return Ok(());
        };
        let smart = self.config.smart_indent;
        let rules = language::Rules::new(&self.buffers[id], self.config.tab_width);
        let b = &mut self.buffers[id];
        if ctrl {
            match key.code {
                KeyCode::Char('r') => b.undo(true),
                KeyCode::Char('o') => return self.jump_back(),
                KeyCode::Char('d') => b.move_by(0, 15),
                KeyCode::Char('u') => b.move_by(0, -15),
                _ => {}
            }
            return Ok(());
        }
        if let Some(pending) = pending {
            match (pending, key.code) {
                ('g', KeyCode::Char('g')) => b.goto(if has_count { count } else { 1 }),
                ('g', KeyCode::Char('d')) => return self.lsp_request(lsp::Kind::Definition),
                (']', KeyCode::Char('d')) => self.next_diagnostic(false),
                ('[', KeyCode::Char('d')) => self.next_diagnostic(true),
                (']', KeyCode::Char('c')) => self.next_change(false),
                ('[', KeyCode::Char('c')) => self.next_change(true),
                ('d', KeyCode::Char('d')) => {
                    let line = b.delete_line();
                    self.clipboard.copy(line, true);
                }
                ('y', KeyCode::Char('y')) => {
                    let line = format!("{}\n", b.line(b.row));
                    self.clipboard.copy(line, true);
                    self.notice("Line copied · p pastes below, P above");
                }
                _ => {}
            }
            return Ok(());
        }
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => b.move_by(-(count as isize), 0),
            KeyCode::Char('l') | KeyCode::Right => b.move_by(count as isize, 0),
            KeyCode::Char('j') | KeyCode::Down => b.move_by(0, count as isize),
            KeyCode::Char('k') | KeyCode::Up => b.move_by(0, -(count as isize)),
            KeyCode::Char('w') => {
                for _ in 0..count {
                    b.word(false);
                }
            }
            KeyCode::Char('b') => {
                for _ in 0..count {
                    b.word(true);
                }
            }
            KeyCode::Char('0') | KeyCode::Home => b.home(),
            KeyCode::Char('$') | KeyCode::End => b.end(),
            KeyCode::Char('^') => {
                b.col = b
                    .line(b.row)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count()
            }
            KeyCode::Char('G') => b.goto(if has_count { count } else { b.lines() }),
            // Vim's relative jumps: 15<Enter> / 15+ down, 15- up, to the first non-blank.
            KeyCode::Enter | KeyCode::Char('+') | KeyCode::Char('-') => {
                let up = key.code == KeyCode::Char('-');
                b.move_by(
                    0,
                    if up {
                        -(count as isize)
                    } else {
                        count as isize
                    },
                );
                b.col = b
                    .line(b.row)
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .count();
            }
            KeyCode::Char('K') => return self.lsp_request(lsp::Kind::Hover),
            KeyCode::Char('g')
            | KeyCode::Char('d')
            | KeyCode::Char('y')
            | KeyCode::Char('[')
            | KeyCode::Char(']') => {
                if let KeyCode::Char(c) = key.code {
                    self.pending = Some(c);
                    if has_count {
                        self.count = count.to_string();
                    }
                }
            }
            KeyCode::Char('i') => {
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('a') => {
                b.move_by(1, 0);
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('A') => {
                b.end();
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('I') => {
                b.home();
                b.end_group();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') if smart => {
                language::open_line(b, &rules, false);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('o') => {
                b.open_line(false);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('O') => {
                b.open_line(true);
                self.mode = Mode::Insert;
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                for _ in 0..count.min(b.line_len(b.row).saturating_sub(b.col)) {
                    b.delete_char();
                }
            }
            KeyCode::Char('D') => b.delete_to_end(),
            KeyCode::Char('C') => {
                b.delete_to_end();
                self.mode = Mode::Insert;
            }
            KeyCode::Char('p') => return self.paste(false),
            KeyCode::Char('P') => return self.paste(true),
            KeyCode::Char('v') => self.start_visual(Mode::Visual),
            KeyCode::Char('V') => self.start_visual(Mode::VisualLine),
            KeyCode::Char('u') => b.undo(false),
            KeyCode::Char('n') => {
                if !b.find(false) {
                    self.notice("No match");
                }
            }
            KeyCode::Char('N') => {
                if !b.find(true) {
                    self.notice("No match");
                }
            }
            KeyCode::PageUp => b.move_by(0, -20),
            KeyCode::PageDown => b.move_by(0, 20),
            _ => {}
        }
        Ok(())
    }
    fn lsp_request(&mut self, kind: lsp::Kind) -> Result<()> {
        let id = self.buffer_id().context("Focus an editor")?;
        self.lsp
            .request(kind, id, &self.buffers[id], self.config.tab_width)?;
        if kind == lsp::Kind::Format {
            self.notice("Formatting…");
        }
        Ok(())
    }
    fn lsp_event(&mut self, event: lsp::Event) {
        match event {
            lsp::Event::Notice(text) => self.notice(text),
            lsp::Event::Error(text) => self.fail(text),
            lsp::Event::Hover(text) => self.hover = Some(text),
            lsp::Event::Definition {
                path,
                line,
                character,
                utf32,
            } => {
                let Some(id) = self.buffer_id() else {
                    return;
                };
                let b = &self.buffers[id];
                let here = (b.path.clone(), b.row, b.col);
                if let Err(error) = self.open(&path) {
                    return self.fail(error);
                }
                if let (Some(path), row, col) = here {
                    self.jumps.push((path, row, col));
                }
                if let Some(b) = self.active_buffer() {
                    b.row = line.min(b.lines() - 1);
                    b.col = lsp::decode(&b.line(b.row), character, utf32);
                    b.scroll = b.row.saturating_sub(5);
                }
                self.notice("Jumped to definition · Ctrl-o returns");
            }
            lsp::Event::Completion { buffer, row, items } => {
                let Some(b) = self.buffers.get(buffer) else {
                    return;
                };
                if self.mode != Mode::Insert || self.buffer_id() != Some(buffer) || b.row != row {
                    return;
                }
                // The word before the cursor now.
                let line: Vec<char> = b.line(row).chars().collect();
                let col = b.col.min(line.len());
                let mut start = col;
                while start > 0 && (line[start - 1].is_alphanumeric() || line[start - 1] == '_') {
                    start -= 1;
                }
                // Asked for by typing a word: only for that word, while it's still being
                // typed (a late answer after a space or a bracket is for nothing on
                // screen any more), and never inside a string or a comment.
                let typing = self.asked_by_typing;
                if typing
                    && (self.asked != Some((buffer, row, start))
                        || start == col
                        || self.in_string_or_comment(buffer))
                {
                    return;
                }
                if items.is_empty() {
                    // Nothing from the server: words from your files, quietly when
                    // it was asked for while typing.
                    if !self.suggest_words(false, typing) && !typing {
                        self.notice("No completions");
                    }
                    return;
                }
                self.menu = Some(Menu {
                    shown: (0..items.len()).collect(),
                    items,
                    selected: 0,
                    buffer,
                    row,
                    start,
                    auto: typing,
                    picked: false,
                });
                self.menu_filter();
            }
            // Asked for on save, which applies them itself; a late answer is dropped.
            lsp::Event::Organized { .. } => {}
            lsp::Event::Format { buffer, edits } => {
                let count = edits.len();
                if let Some(b) = self.buffers.get_mut(buffer) {
                    b.apply_edits(edits);
                }
                self.notice(if count == 0 {
                    "Already formatted".to_string()
                } else {
                    format!(
                        "Formatted · {count} edit{}",
                        if count == 1 { "" } else { "s" }
                    )
                });
            }
        }
    }
    /// Narrow the completion menu to what has been typed since it opened.
    fn menu_filter(&mut self) {
        let focused = self.buffer_id();
        let Some(menu) = &mut self.menu else {
            return;
        };
        let b = &self.buffers[menu.buffer];
        if focused != Some(menu.buffer) || b.row != menu.row || b.col < menu.start {
            self.menu = None;
            return;
        }
        let prefix: String = b
            .line(b.row)
            .chars()
            .skip(menu.start)
            .take(b.col - menu.start)
            .collect();
        if prefix.chars().any(|c| !(c.is_alphanumeric() || c == '_')) {
            self.menu = None;
            return;
        }
        menu.shown = (0..menu.items.len())
            .filter(|&i| menu.items[i].matches(&prefix))
            .collect();
        menu.selected = 0;
        // Nothing left to complete: no suggestions, or only the word already typed.
        let done = match menu.shown[..] {
            [] => true,
            [only] => {
                let item = &menu.items[only];
                !prefix.is_empty() && (item.label == prefix || item.insert == prefix)
            }
            _ => false,
        };
        if done {
            self.menu = None;
        }
    }
    fn menu_move(&mut self, delta: isize) {
        if let Some(menu) = &mut self.menu {
            menu.picked = true;
            let len = menu.shown.len() as isize;
            menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
        }
    }
    fn menu_accept(&mut self) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let item = &menu.items[menu.shown[menu.selected]];
        let b = &mut self.buffers[menu.buffer];
        let start_col = item.start.unwrap_or(menu.start).min(b.col);
        let start = b.char_index(menu.row, start_col);
        let end = b.index();
        b.replace(start, end, &item.insert);
    }
    fn next_diagnostic(&mut self, backwards: bool) {
        let Some(id) = self.buffer_id() else {
            return;
        };
        let rows: Vec<usize> = self
            .lsp
            .diagnostics
            .get(&id)
            .map(|d| d.iter().map(|d| d.row).collect())
            .unwrap_or_default();
        let b = &mut self.buffers[id];
        let target = if backwards {
            rows.iter().rev().find(|&&r| r < b.row).or(rows.last())
        } else {
            rows.iter().find(|&&r| r > b.row).or(rows.first())
        };
        match target {
            Some(&row) => {
                b.row = row;
                b.col = 0;
                b.clamp();
            }
            None => self.notice("No diagnostics in this file"),
        }
    }
    /// `]c` / `[c`: the next / previous block of lines that differ from Git.
    fn next_change(&mut self, backwards: bool) {
        let Some(id) = self.buffer_id() else {
            return;
        };
        let marks = self
            .diffs
            .get(&id)
            .map(|d| d.marks.as_slice())
            .unwrap_or_default();
        let starts: Vec<usize> = (0..marks.len())
            .filter(|&row| marks[row].is_some() && (row == 0 || marks[row - 1] != marks[row]))
            .collect();
        let b = &mut self.buffers[id];
        let target = if backwards {
            starts.iter().rev().find(|&&r| r < b.row).or(starts.last())
        } else {
            starts.iter().find(|&&r| r > b.row).or(starts.first())
        };
        match target {
            Some(&row) => {
                b.row = row;
                b.col = 0;
                b.clamp();
            }
            None => self.notice("No changes from Git in this file"),
        }
    }
    fn jump_back(&mut self) -> Result<()> {
        let (path, row, col) = self.jumps.pop().context("No earlier position")?;
        self.open(&path)?;
        if let Some(b) = self.active_buffer() {
            b.row = row;
            b.col = col;
            b.clamp();
        }
        Ok(())
    }
    fn markdown_buffer(&self, id: usize) -> Result<()> {
        if self.buffers[id]
            .path
            .as_deref()
            .is_some_and(markdown::is_markdown)
        {
            Ok(())
        } else {
            bail!("Preview renders Markdown files (.md, .markdown)")
        }
    }
    /// Switch the focused pane between rendered Markdown and its source, keeping place.
    fn toggle_preview(&mut self) -> Result<()> {
        match self.panes.get(&self.focused) {
            Some(Pane::Editor(id)) => {
                let id = *id;
                self.markdown_buffer(id)?;
                self.buffers[id].end_group();
                self.panes
                    .insert(self.focused, Pane::Preview(Preview::new(id)));
                self.mode = Mode::Normal;
                self.notice("Markdown preview · e edits the source · [ ] jump between headings");
            }
            Some(Pane::Preview(p)) => {
                let id = p.buffer;
                if let Some(doc) = self.previews.get(id) {
                    let b = &mut self.buffers[id];
                    b.row = doc.source_for_line(p.scroll + 1).min(b.lines() - 1);
                    b.col = 0;
                    b.scroll = b.row.saturating_sub(2);
                }
                self.panes.insert(self.focused, Pane::Editor(id));
                self.notice("Editing Markdown source · Ctrl-b m or :preview renders it");
            }
            _ => bail!("Focus a Markdown file to preview it"),
        }
        Ok(())
    }
    /// Live preview beside the source; focus stays in the editor.
    fn split_preview(&mut self) -> Result<()> {
        let id = match self.panes.get(&self.focused) {
            Some(Pane::Editor(id)) => *id,
            Some(Pane::Preview(p)) => {
                let id = p.buffer;
                self.panes.insert(self.focused, Pane::Editor(id));
                id
            }
            _ => bail!("Focus a Markdown file to preview it"),
        };
        self.markdown_buffer(id)?;
        if self.panes.len() >= 12 {
            bail!("Maximum of 12 panes");
        }
        let editor = self.focused;
        self.add_pane(Pane::Preview(Preview::new(id)), Axis::Horizontal);
        self.focused = editor;
        self.notice("Live preview: it follows your cursor as you edit");
        Ok(())
    }
    /// Open the Git pane docked full-width along the bottom; when it is open, focus
    /// it, and when it is already focused, maximize or restore it.
    fn git_pane(&mut self) -> Result<()> {
        let existing = self
            .panes
            .iter()
            .find(|(_, p)| matches!(p, Pane::Git(_)))
            .map(|(id, _)| *id);
        match existing {
            Some(id) if id == self.focused && !self.sidebar_focus => {
                self.zoom = !self.zoom;
            }
            Some(id) => {
                self.focused = id;
                self.sidebar_focus = false;
                self.zoom = false;
            }
            None => {
                if self.panes.len() >= 12 {
                    bail!("Maximum of 12 panes");
                }
                let id = self.next_pane;
                self.next_pane += 1;
                self.panes
                    .insert(id, Pane::Git(Box::new(GitView::new(self.root.clone()))));
                let above = std::mem::replace(&mut self.tiles, Tile::Leaf(id));
                self.tiles = Tile::Split {
                    axis: Axis::Vertical,
                    percent: 60,
                    first: Box::new(above),
                    second: Box::new(Tile::Leaf(id)),
                };
                self.focused = id;
                self.sidebar_focus = false;
                self.zoom = false;
                self.notice("Git: Tab branches/commits · Enter show branch · Ctrl-b G maximize");
            }
        }
        self.mode = Mode::Normal;
        Ok(())
    }
    fn git_pane_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Pane::Git(view)) = self.panes.get_mut(&self.focused) else {
            return Ok(());
        };
        // A commit's diff is open: scroll it; Esc, q or Enter goes back to the graph.
        if let Some(diff) = view.diff.as_mut() {
            let page = diff.height.max(2) as isize;
            match key.code {
                KeyCode::Char('d') if ctrl => diff.scroll_by(page / 2),
                KeyCode::Char('u') if ctrl => diff.scroll_by(-page / 2),
                _ if ctrl => {}
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter | KeyCode::Backspace => {
                    view.diff = None
                }
                KeyCode::Char('j') | KeyCode::Down => diff.scroll_by(1),
                KeyCode::Char('k') | KeyCode::Up => diff.scroll_by(-1),
                KeyCode::PageDown | KeyCode::Char(' ') => diff.scroll_by(page - 1),
                KeyCode::PageUp => diff.scroll_by(1 - page),
                KeyCode::Char('g') | KeyCode::Home => diff.scroll = 0,
                KeyCode::Char('G') | KeyCode::End => diff.scroll_by(isize::MAX / 2),
                KeyCode::Char('n') | KeyCode::Char(']') => diff.next_file(false),
                KeyCode::Char('N') | KeyCode::Char('[') => diff.next_file(true),
                KeyCode::Char('l') | KeyCode::Right => diff.hscroll += 8,
                KeyCode::Char('h') | KeyCode::Left => diff.hscroll = diff.hscroll.saturating_sub(8),
                KeyCode::Char('y') => {
                    let hash = diff.hash.clone();
                    self.clipboard.copy(hash.clone(), false);
                    self.notice(format!("Copied {hash}"));
                }
                KeyCode::Char(':') => self.input_start(InputKind::Command),
                _ => {}
            }
            return Ok(());
        }
        let page = view.height.max(2) as isize;
        match key.code {
            KeyCode::Char('d') if ctrl => view.step(page / 2),
            KeyCode::Char('u') if ctrl => view.step(-page / 2),
            _ if ctrl => {}
            KeyCode::Tab | KeyCode::BackTab => {
                view.focus = match view.focus {
                    gitgraph::Focus::Commits => gitgraph::Focus::Branches,
                    gitgraph::Focus::Branches => gitgraph::Focus::Commits,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => view.step(1),
            KeyCode::Char('k') | KeyCode::Up => view.step(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => view.step(page - 1),
            KeyCode::PageUp => view.step(1 - page),
            KeyCode::Char('g') | KeyCode::Home => view.step(isize::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => view.step(isize::MAX / 2),
            KeyCode::Enter => match view.focus {
                gitgraph::Focus::Branches => view.toggle_only(),
                gitgraph::Focus::Commits => view.open_diff(),
            },
            KeyCode::Char('d') => view.details = !view.details,
            KeyCode::Char('a') | KeyCode::Esc => view.show_all(),
            KeyCode::Char('r') => view.refresh(),
            KeyCode::Char('y') => {
                if let Some(hash) = view.selected().map(|c| c.hash.clone()) {
                    self.clipboard.copy(hash.clone(), false);
                    self.notice(format!("Copied {hash}"));
                }
            }
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            _ => {}
        }
        Ok(())
    }
    /// Ctrl-b v / `:changes`: the review of what AI agents changed. Opens it beside
    /// the focused pane (below a narrow one), focuses it, or maximizes it when focused.
    fn changes_pane(&mut self) -> Result<()> {
        if !self.config.review_changes {
            bail!("Change review is off (review_changes = false in config.toml)");
        }
        self.changes.request();
        let existing = self
            .panes
            .iter()
            .find(|(_, p)| matches!(p, Pane::Changes(_)))
            .map(|(id, _)| *id);
        match existing {
            Some(id) if id == self.focused && !self.sidebar_focus => self.zoom = !self.zoom,
            Some(id) => {
                if let Some(index) = self.docked.iter().position(|d| *d == id) {
                    self.undock(index);
                }
                self.focused = id;
                self.sidebar_focus = false;
                self.zoom = false;
            }
            None => {
                if self.panes.len() >= 12 {
                    bail!("Maximum of 12 panes");
                }
                let wide = self
                    .rectangles
                    .iter()
                    .find(|(id, _)| *id == self.focused)
                    .is_some_and(|(_, r)| r.width >= 120);
                let axis = if wide {
                    Axis::Horizontal
                } else {
                    Axis::Vertical
                };
                self.add_pane(Pane::Changes(Box::default()), axis);
                self.notice(match self.changes.files.len() {
                    0 => "Changes: what AI agents change shows up here · Ctrl-b v maximizes".into(),
                    n => format!(
                        "Changes: {} changed by AI agents · Ctrl-b v maximizes",
                        review::files(n)
                    ),
                });
            }
        }
        self.mode = Mode::Normal;
        Ok(())
    }
    fn changes_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Pane::Changes(view)) = self.panes.get_mut(&self.focused) else {
            return Ok(());
        };
        view.sync(&self.changes);
        // An undo waits for y (or Enter); any other key leaves everything as it is.
        if let Some((target, _)) = view.confirm.take() {
            if !ctrl && matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                return self.undo_changes(&target);
            }
            self.notice("Nothing undone");
            return Ok(());
        }
        let page = view.height.max(2) as isize;
        let whole = matches!(key.code, KeyCode::Char('Y') | KeyCode::Char('X'));
        match key.code {
            KeyCode::Char('d') if ctrl => view.move_by(page / 2),
            KeyCode::Char('u') if ctrl => view.move_by(-page / 2),
            _ if ctrl => {}
            KeyCode::Char('j') | KeyCode::Down => view.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => view.move_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => view.move_by(page - 1),
            KeyCode::PageUp => view.move_by(1 - page),
            KeyCode::Char('g') | KeyCode::Home => view.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => view.move_by(isize::MAX / 2),
            KeyCode::Char('n') => view.jump(false, false),
            KeyCode::Char('N') => view.jump(false, true),
            KeyCode::Char(']') => view.jump(true, false),
            KeyCode::Char('[') => view.jump(true, true),
            KeyCode::Char('l') | KeyCode::Right => view.hscroll += 8,
            KeyCode::Char('h') | KeyCode::Left => view.hscroll = view.hscroll.saturating_sub(8),
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(target) = view.target(&self.changes, whole) {
                    self.keep_changes(&target)?;
                }
            }
            KeyCode::Char('A') => self.keep_changes(&Target::All)?,
            KeyCode::Char('x') | KeyCode::Char('X') => match view.target(&self.changes, whole) {
                // One hunk is quick to redo by hand; a whole file asks first.
                Some(target @ Target::Hunk(..)) => self.undo_changes(&target)?,
                Some(target) => self.ask_undo(target),
                None => {}
            },
            KeyCode::Char('U') => self.ask_undo(Target::All),
            KeyCode::Enter | KeyCode::Char('o') => {
                if let Some((path, line)) = view.location(&self.changes) {
                    if !path.exists() {
                        bail!("That file was deleted · x undoes the deletion");
                    }
                    self.open(&path)?;
                    if let Some(b) = self.active_buffer() {
                        b.goto(line);
                    }
                }
            }
            KeyCode::Char('r') => self.changes.request(),
            KeyCode::Char('q') => self.close(false)?,
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            _ => {}
        }
        Ok(())
    }
    /// Ask before undoing a whole file or everything: the agent's version is gone after.
    fn ask_undo(&mut self, target: Target) {
        let question = match &target {
            Target::All if self.changes.files.is_empty() => {
                self.notice("No agent changes to undo");
                return;
            }
            Target::All => format!(
                "Undo every agent change in {}? y undoes · any other key keeps them",
                review::files(self.changes.files.len())
            ),
            Target::File(path) | Target::Hunk(path, _) => {
                let name = self
                    .changes
                    .files
                    .iter()
                    .find(|f| &f.path == path)
                    .map_or(path.clone(), |f| f.name.clone());
                format!("Undo every agent change to {name}? y undoes · any other key keeps it")
            }
        };
        if let Some(view) = self.panes.values_mut().find_map(|p| match p {
            Pane::Changes(view) => Some(view),
            _ => None,
        }) {
            view.confirm = Some((target, question.clone()));
            self.notice(question);
        }
    }
    fn keep_changes(&mut self, target: &Target) -> Result<()> {
        let note = self.changes.keep(target).map_err(anyhow::Error::msg)?;
        self.notice(note);
        Ok(())
    }
    fn undo_changes(&mut self, target: &Target) -> Result<()> {
        let count = self.changes.files.len();
        let name = match target {
            Target::File(path) | Target::Hunk(path, _) => self
                .changes
                .files
                .iter()
                .find(|f| &f.path == path)
                .map_or(path.clone(), |f| f.name.clone()),
            Target::All => String::new(),
        };
        let written = self.changes.undo(target).map_err(anyhow::Error::msg)?;
        // Open buffers of those files show their text again on the next tick.
        self.last_reload = Instant::now() - Duration::from_secs(5);
        let unsaved: Vec<String> = self
            .buffers
            .iter()
            .filter(|b| b.dirty && b.path.as_ref().is_some_and(|p| written.contains(p)))
            .map(|b| b.name())
            .collect();
        let mut note = match target {
            Target::Hunk(..) => format!("Undid that change in {name}"),
            Target::File(_) => format!("Undid the agent's changes to {name}"),
            Target::All => format!("Undid the agent changes to {}", review::files(count)),
        };
        if let Some(name) = unsaved.first() {
            note.push_str(&format!(
                " · {name} has unsaved edits in Raido; :reload shows the file"
            ));
        }
        self.notice(note);
        Ok(())
    }
    fn preview_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(Pane::Preview(p)) = self.panes.get_mut(&self.focused) else {
            return Ok(());
        };
        let page = p.height.max(2);
        let headings = self
            .previews
            .get(p.buffer)
            .map(|d| d.headings.clone())
            .unwrap_or_default();
        match key.code {
            KeyCode::Char('d') if ctrl => p.scroll += page / 2,
            KeyCode::Char('u') if ctrl => p.scroll = p.scroll.saturating_sub(page / 2),
            KeyCode::Char('f') if ctrl => p.scroll += page,
            KeyCode::Char('b') if ctrl => p.scroll = p.scroll.saturating_sub(page),
            _ if ctrl => {}
            KeyCode::Char('j') | KeyCode::Down => p.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::PageDown => p.scroll += page.saturating_sub(2),
            KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(page.saturating_sub(2)),
            KeyCode::Char('g') | KeyCode::Home => p.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => p.scroll = usize::MAX / 2,
            KeyCode::Char(']') => {
                if let Some(&line) = headings.iter().find(|&&h| h > p.scroll) {
                    p.scroll = line;
                }
            }
            KeyCode::Char('[') => {
                if let Some(&line) = headings.iter().rev().find(|&&h| h < p.scroll) {
                    p.scroll = line;
                }
            }
            KeyCode::Char('e') | KeyCode::Char('i') | KeyCode::Enter => self.toggle_preview()?,
            KeyCode::Char('/') => {
                self.toggle_preview()?;
                self.input_start(InputKind::Search);
            }
            KeyCode::Char(':') => self.input_start(InputKind::Command),
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_preview()?;
                self.next_buffer(key.code == KeyCode::BackTab);
            }
            _ => {}
        }
        Ok(())
    }
    /// Default placement for AI panes, from `ai_split` in config.toml.
    fn ai_axis(&self) -> Axis {
        match self.config.ai_split.as_str() {
            "below" | "stacked" | "down" => Axis::Vertical,
            _ => Axis::Horizontal,
        }
    }
    /// Open the AI launcher pane beside (Horizontal) or below (Vertical) the focus.
    fn open_ai(&mut self, axis: Option<Axis>) -> Result<()> {
        if self.panes.len() >= 12 {
            bail!("Maximum of 12 panes");
        }
        let axis = axis.unwrap_or_else(|| self.ai_axis());
        self.add_pane(Pane::Ai, axis);
        self.notice(
            "AI launcher: 1 Codex · 2 Claude · 3 Gemini · 4 Antigravity · 5 Local (Ollama)",
        );
        Ok(())
    }
    /// Run an AI CLI in the focused AI pane (`split: None`) or in a new split.
    fn launch_agent(&mut self, program: &str, split: Option<Axis>) -> Result<()> {
        self.launch(program, None, split)
    }
    /// Your own `[ai.<name>]` launchers that are not built-in names, in name order.
    pub fn custom_agents(&self) -> Vec<String> {
        self.config
            .ai
            .keys()
            .filter(|name| !AGENTS.iter().any(|a| a.1 == name.as_str()))
            .cloned()
            .collect()
    }
    fn launch(&mut self, program: &str, model: Option<&str>, split: Option<Axis>) -> Result<()> {
        // Agents find Raido's docs as a skill; install or refresh it once per session.
        if self.config.ai_skill && !self.skill_ready {
            self.skill_ready = crate::skill::install().is_ok();
        }
        let agent = self.agent_command(program, model, None, false)?;
        let (command, args, env) = agent.run;
        self.open_terminal_with(Some((&command, args)), &env, split)?;
        if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
            t.launch = Launch::Agent {
                name: program.into(),
                model: model.map(String::from),
                session: agent.session,
            };
        }
        Ok(())
    }
    /// What to run for an AI CLI. Claude Code panes get their own conversation id
    /// (`--session-id`), so a restored pane reopens exactly its own conversation
    /// with `--resume <id>`; the other built-in CLIs can only reopen their latest
    /// conversation in this project.
    fn agent_command(
        &self,
        program: &str,
        model: Option<&str>,
        session: Option<&str>,
        resume: bool,
    ) -> Result<AgentRun> {
        use crate::local_ai;
        let plain = |run| AgentRun {
            run,
            replay: true,
            session: None,
        };
        if program == "local" {
            return Ok(plain(local_ai::local(
                self.config.ai.get("local"),
                model,
                local_ai::ollama_models,
                crate::lsp::on_path,
            )?));
        }
        // An [ai.<name>] entry, which may also override a built-in like `codex`.
        if let Some(launcher) = self.config.ai.get(program).cloned() {
            if !crate::lsp::on_path(&launcher.program) {
                bail!("`{}` from [ai.{program}] is not on PATH", launcher.program);
            }
            return Ok(plain(local_ai::command(&launcher, model)?));
        }
        let (_, _, name, hint) = AGENTS.iter().find(|a| a.1 == program).copied().unwrap_or((
            ' ',
            program,
            program,
            "see its documentation",
        ));
        if !crate::lsp::on_path(program) {
            bail!("{name} is not installed (`{program}` not on PATH) · install: {hint}");
        }
        let (args, replay, session) = agent_flags(
            program,
            resume,
            session,
            &claude_conversation_exists,
            &conversation_id,
        );
        Ok(AgentRun {
            run: (program.into(), args, Vec::new()),
            replay,
            session,
        })
    }
    /// `:font`, `:font + | - | reset` (zoom), `:font size N`, `:font family NAME`, `:font list`.
    fn font_command(&mut self, arg: &str) -> Result<()> {
        use crate::fonts;
        if let Some(step) = textsize::Step::parse(arg) {
            self.change_text_size(step);
            return Ok(());
        }
        let term = fonts::detect();
        let (verb, rest) = arg
            .split_once(char::is_whitespace)
            .map(|(a, b)| (a, b.trim()))
            .unwrap_or((arg, ""));
        let message = match verb {
            "" => fonts::summary(term),
            "size" => fonts::set_size(term, rest.parse().context("Usage: :font size 12")?)?,
            "family" | "face" => fonts::set_family(term, rest)?,
            "list" => {
                let all = fonts::list()?;
                let shown: Vec<&str> = all.iter().take(12).map(String::as_str).collect();
                format!(
                    "{} monospace fonts: {}{} · :font family \"Name\"",
                    all.len(),
                    shown.join(", "),
                    if all.len() > shown.len() { ", …" } else { "" }
                )
            }
            size if size.parse::<f32>().is_ok() => {
                fonts::set_size(term, size.parse().expect("checked"))?
            }
            _ => bail!(
                "Use :font, :font + / - / reset, :font size 12, :font family \"Name\", :font list"
            ),
        };
        self.notice(message);
        Ok(())
    }
    fn toggle_help(&mut self) {
        self.help = !self.help;
        self.help_focus = self.help;
        self.help_scroll = 0;
        if self.help {
            self.notice(
                "Field guide · ↑↓ PgUp PgDn scroll · Esc back to work (it stays open) · F1 closes",
            );
        }
    }
    pub fn open_picker(&mut self) {
        let themes: Vec<(String, &'static str)> = config::available_themes()
            .into_iter()
            .map(|name| {
                let origin = config::theme_origin(&name);
                (name, origin)
            })
            .collect();
        let original = self.config.theme.clone();
        let selected = themes.iter().position(|(n, _)| *n == original).unwrap_or(0);
        self.picker = Some(ThemePicker {
            shown: (0..themes.len()).collect(),
            themes,
            selected,
            filter: String::new(),
            original,
        });
        self.notice(format!(
            "Themes: ↑↓ preview · type to filter · Enter keeps · Esc cancels · add your own in {}",
            config::themes_dir().display()
        ));
    }
    fn picker_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(picker) = &mut self.picker else {
            return Ok(());
        };
        let len = picker.shown.len();
        let mut refilter = false;
        match key.code {
            KeyCode::Esc => {
                let original = picker.original.clone();
                self.picker = None;
                self.apply_theme(&original)?;
                self.notice(format!("Theme unchanged: {original}"));
                return Ok(());
            }
            KeyCode::Enter => {
                let chosen = picker
                    .shown
                    .get(picker.selected)
                    .map(|&i| picker.themes[i].0.clone());
                self.picker = None;
                if let Some(name) = chosen {
                    self.command(&format!("theme {name}"))?;
                }
                return Ok(());
            }
            KeyCode::Up | KeyCode::BackTab if len > 0 => {
                picker.selected = (picker.selected + len - 1) % len;
            }
            KeyCode::Down | KeyCode::Tab if len > 0 => {
                picker.selected = (picker.selected + 1) % len;
            }
            KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
            KeyCode::PageDown if len > 0 => picker.selected = (picker.selected + 10).min(len - 1),
            KeyCode::Home => picker.selected = 0,
            KeyCode::End => picker.selected = len.saturating_sub(1),
            KeyCode::Backspace => {
                picker.filter.pop();
                refilter = true;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                picker.filter.push(c);
                refilter = true;
            }
            _ => return Ok(()),
        }
        if refilter {
            let filter = picker.filter.to_lowercase();
            picker.shown = (0..picker.themes.len())
                .filter(|&i| picker.themes[i].0.to_lowercase().contains(&filter))
                .collect();
            picker.selected = 0;
        }
        // Live preview of the highlighted theme; not saved until Enter.
        let preview = picker
            .shown
            .get(picker.selected)
            .map(|&i| picker.themes[i].0.clone());
        if let Some(name) = preview {
            if name != self.config.theme {
                if let Err(error) = self.apply_theme(&name) {
                    self.fail(format!("{name}: {error:#}"));
                }
            }
        }
        Ok(())
    }
    fn apply_theme(&mut self, name: &str) -> Result<()> {
        let theme = config::load_theme(name)?;
        self.syntax = Syntax::new(&theme);
        self.previews.clear();
        for pane in self.panes.values_mut() {
            if let Pane::Terminal(terminal) = pane {
                terminal.set_theme(&theme);
            }
        }
        self.theme = theme;
        self.config.theme = name.into();
        self.theme_stamp = config::theme_stamp(name);
        self.redraw = true;
        Ok(())
    }
    fn next_buffer(&mut self, backwards: bool) {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            self.end_visual();
        }
        if let Some(id) = self.buffer_id() {
            let id = if backwards {
                (id + self.buffers.len() - 1) % self.buffers.len()
            } else {
                (id + 1) % self.buffers.len()
            };
            self.panes.insert(self.focused, Pane::Editor(id));
        }
    }
    fn input_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code != KeyCode::Tab {
            self.completion = None;
        }
        match key.code {
            KeyCode::Esc => {
                self.input = None;
            }
            KeyCode::Backspace => {
                self.input.as_mut().expect("input active").text.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.as_mut().expect("input active").text.push(c)
            }
            KeyCode::Enter => {
                let input = self.input.take().expect("input active");
                match input.kind {
                    InputKind::Command => self.command(input.text.trim())?,
                    InputKind::Search => {
                        if let Some(b) = self.active_buffer() {
                            b.search = input.text;
                            b.search_case = true;
                            b.search_word = false;
                            if !b.find(false) {
                                self.notice("No match");
                            }
                        }
                    }
                    InputKind::Open => {
                        let path = self.root.join(&input.text);
                        if !input.text.is_empty() && path.is_dir() {
                            self.show_folder(&path)?;
                        } else if !input.text.is_empty() {
                            self.open(Path::new(&input.text))?;
                        }
                    }
                }
            }
            KeyCode::Tab => {
                let text = self.input.as_ref().expect("input active").text.clone();
                let kind = self.input.as_ref().expect("input active").kind;
                if kind == InputKind::Command && text.starts_with("theme ") {
                    // Repeated Tab cycles through every theme matching what was typed.
                    let (prefix, index) = match self.completion.take() {
                        Some((prefix, index)) => (prefix, index + 1),
                        None => (text["theme ".len()..].trim().to_string(), 0),
                    };
                    let matches: Vec<String> = config::available_themes()
                        .into_iter()
                        .filter(|n| n.starts_with(&prefix))
                        .collect();
                    if !matches.is_empty() {
                        let name = &matches[index % matches.len()];
                        self.input.as_mut().expect("input active").text = format!("theme {name}");
                        self.completion = Some((prefix, index));
                    }
                    return Ok(());
                }
                if kind == InputKind::Open {
                    self.complete_open(&text);
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// Tab in Ctrl-p completes the path like a shell: one match is filled in (a
    /// folder with `/`, so the next Tab goes inside it); several fill in what they
    /// share and are listed, and Tab again steps through them.
    fn complete_open(&mut self, text: &str) {
        let (typed, step) = match self.completion.take() {
            Some((typed, index)) => (typed, Some(index + 1)),
            None => (text.to_string(), None),
        };
        let found = workspace::complete_path(&self.root, &typed);
        let mut fill = |value: &str| {
            if let Some(input) = self.input.as_mut() {
                input.text = value.to_string();
            }
        };
        match (found.len(), step) {
            (0, _) => self.notice(format!("Nothing matches {}", workspace::printable(&typed))),
            (1, _) => fill(&found[0]),
            (n, Some(index)) => {
                fill(&found[index % n]);
                self.completion = Some((typed, index));
            }
            (n, None) => {
                let shared = found.iter().skip(1).fold(found[0].clone(), |shared, next| {
                    shared
                        .chars()
                        .zip(next.chars())
                        .take_while(|(a, b)| a == b)
                        .map(|(a, _)| a)
                        .collect()
                });
                if shared.chars().count() > typed.chars().count() {
                    fill(&shared);
                } else {
                    fill(&found[0]);
                    self.completion = Some((typed.clone(), 0));
                }
                let folder = typed.rfind('/').map_or(0, |at| at + 1);
                let names: Vec<&str> = found.iter().map(|f| &f[folder..]).take(12).collect();
                let more = if n > 12 { "  …" } else { "" };
                self.notice(format!(
                    "{n} matches · Tab steps through: {}{more}",
                    names.join("  ")
                ));
            }
        }
    }
    /// Enter on a folder in Ctrl-p: show it, opened, in the explorer.
    fn show_folder(&mut self, path: &Path) -> Result<()> {
        let dir = path.canonicalize()?;
        if !dir.starts_with(&self.root) {
            bail!("{} is outside this project", path.display());
        }
        self.sidebar_visible = true;
        self.sidebar_focus = true;
        self.git_focus = false;
        if dir != self.root {
            self.tree.expanded.insert(dir.clone());
            self.tree.reveal(&dir);
        }
        self.notice("Folder opened in the explorer");
        Ok(())
    }
    pub fn command(&mut self, command: &str) -> Result<()> {
        // :+15 / :-15 move relative to the cursor line.
        if let Some(offset) = command
            .strip_prefix('+')
            .or_else(|| command.strip_prefix('-'))
            .and_then(|n| n.parse::<isize>().ok())
        {
            let delta = if command.starts_with('-') {
                -offset
            } else {
                offset
            };
            if let Some(b) = self.active_buffer() {
                b.move_by(0, delta);
            }
            return Ok(());
        }
        if let Ok(line) = command.parse::<usize>() {
            if let Some(b) = self.active_buffer() {
                b.goto(line);
            }
            return Ok(());
        }
        let (verb, arg) = command
            .split_once(char::is_whitespace)
            .map(|(a, b)| (a, b.trim()))
            .unwrap_or((command, ""));
        match verb {
            "w" | "write" => self.save((!arg.is_empty()).then(|| self.root.join(arg)))?,
            "wa" => {
                for id in 0..self.buffers.len() {
                    if self.buffers[id].dirty {
                        self.tidy_on_save(id);
                        self.buffers[id].save(None)?;
                        self.lsp.saved(id, &self.buffers[id]);
                        if let Some(path) = &self.buffers[id].path {
                            self.changes.saved(path);
                        }
                    }
                }
                self.notice("Saved all modified buffers");
            }
            "wq" => {
                self.save(None)?;
                self.close(false)?;
            }
            "q" => self.close(false)?,
            "q!" => self.close(true)?,
            "qa" => self.quit(false)?,
            "qa!" => self.quit(true)?,
            "e" | "edit" => {
                if arg.is_empty() {
                    bail!("Usage: :e path (creates a new file buffer if absent)");
                }
                self.open(Path::new(arg))?;
            }
            "new" => {
                self.buffers.push(Buffer::empty());
                self.add_pane(Pane::Editor(self.buffers.len() - 1), Axis::Horizontal);
            }
            "reload" => {
                let id = self.buffer_id().context("Focus an editor")?;
                if self.buffers[id].dirty {
                    bail!("Unsaved changes; save to another name first");
                }
                let path = self.buffers[id].path.clone().context("Unnamed buffer")?;
                let mut fresh = Buffer::open(&path)?;
                fresh.revision = self.buffers[id].revision + 1;
                self.buffers[id] = fresh;
            }
            "preview" | "md" => match arg {
                "" => self.toggle_preview()?,
                "split" | "vs" => self.split_preview()?,
                _ => bail!("Use :preview or :preview split"),
            },
            "format" | "fmt" => self.lsp_request(lsp::Kind::Format)?,
            "font" | "fonts" => self.font_command(arg)?,
            "docs" | "manual" => {
                crate::skill::write_source()?;
                self.open(&crate::skill::manual())?;
                self.notice("Raido manual · [ ] jump between headings · F1 for the shortcut guide");
            }
            "skill" => match arg {
                "" | "install" => {
                    let places = crate::skill::install()?;
                    self.skill_ready = true;
                    self.notice(format!(
                        "Raido skill ready in {} agent folder(s) · ask your AI agent \"how do I … in Raido?\"",
                        places.len()
                    ));
                }
                "remove" => {
                    let removed = crate::skill::remove()?;
                    self.notice(format!(
                        "Removed the Raido skill from {removed} place(s) · set ai_skill = false to keep it off"
                    ));
                }
                _ => bail!("Use :skill, :skill install or :skill remove"),
            },
            "lsp" => match arg {
                "" => self.notice(self.lsp.report()),
                "restart" => {
                    self.lsp.restart();
                    self.notice("Language servers restarting");
                }
                _ => bail!("Use :lsp or :lsp restart"),
            },
            "vsplit" | "vs" => self.split_editor(Axis::Horizontal)?,
            "split" | "sp" => self.split_editor(Axis::Vertical)?,
            // :term [below|right]
            "term" | "terminal" => {
                let axis = match arg {
                    "" | "below" | "stacked" | "down" => Axis::Vertical,
                    "right" | "beside" | "side" => Axis::Horizontal,
                    _ => bail!("Use :term, :term below or :term right"),
                };
                self.open_terminal(None, Some(axis))?
            }
            "copymode" | "copy-mode" => self.enter_copy_mode(),
            "copyview" | "copy-view" => self.toggle_copy_view(),
            "collapse" => {
                self.tree.collapse_all()?;
                self.notice("Explorer folders collapsed");
            }
            "expand" => {
                self.tree.expand_all()?;
                self.notice("Explorer: selected folder expanded, with everything inside");
            }
            // :zoom in / out / reset changes the text size; bare :zoom maximizes the pane.
            // :move left|right|up|down, :flip
            "move" => {
                let direction = match arg {
                    "left" => KeyCode::Left,
                    "right" => KeyCode::Right,
                    "up" => KeyCode::Up,
                    "down" => KeyCode::Down,
                    _ => bail!("Use :move left, right, up or down"),
                };
                self.move_pane(direction);
            }
            "flip" => self.flip_split(),
            "dock" => self.dock(),
            "mouse" => match arg {
                "" => self.toggle_mouse(),
                "off" | "release" if !self.mouse_released => self.toggle_mouse(),
                "on" if self.mouse_released => self.toggle_mouse(),
                "on" | "off" | "release" => {}
                _ => bail!("Use :mouse, :mouse off (release it) or :mouse on"),
            },
            // :undock (the dock list), :undock 2, :undock claude
            "undock" => match arg {
                "" => self.open_dock_list(),
                _ => {
                    let index = arg
                        .parse::<usize>()
                        .ok()
                        .and_then(|n| n.checked_sub(1))
                        .or_else(|| self.docked.iter().position(|d| self.pane_name(*d) == arg));
                    match index.filter(|i| *i < self.docked.len()) {
                        Some(i) => self.undock(i),
                        None => bail!("Nothing docked as {arg} · :undock lists them"),
                    }
                }
            },
            "noh" | "nohlsearch" => {
                if let Some(b) = self.active_buffer() {
                    b.search.clear();
                }
            }
            "sound" => {
                sound::play(&self.config.ai_sound);
                self.notice(format!(
                    "ai_sound = \"{}\" plays when an AI pane finishes a prompt",
                    self.config.ai_sound
                ));
            }
            "zoom" => match (arg, textsize::Step::parse(arg)) {
                ("", _) => {
                    self.zoom = !self.zoom;
                    self.sidebar_focus = false;
                }
                (_, Some(step)) => self.change_text_size(step),
                _ => bail!(
                    "Use :zoom in, :zoom out or :zoom reset (text size); :zoom maximizes the pane"
                ),
            },
            "session" => match arg {
                "" => self.notice(if !self.config.restore_session {
                    "Sessions are off (restore_session = false)".to_string()
                } else if !self.session_enabled {
                    "Session saving is off until restart; :session save turns it back on".into()
                } else if session::wanted(&self.root) {
                    format!(
                        "Session saves to {} and reopens next time",
                        session::file(&self.root).display()
                    )
                } else {
                    "No session here yet; :session save keeps this workspace for next time".into()
                }),
                "save" => {
                    self.session_enabled = true;
                    self.save_session(true)?;
                    self.notice(format!(
                        "Session saved to {}",
                        session::file(&self.root).display()
                    ));
                }
                "forget" | "clear" => {
                    session::forget(&self.root)?;
                    self.session_enabled = false;
                    self.session_json.clear();
                    self.notice(
                        "Session deleted; Raido will start fresh here (:session save undoes)",
                    );
                }
                _ => bail!("Use :session, :session save or :session forget"),
            },
            // :changes [keep|undo]: the review of what AI agents changed.
            "changes" | "review" => match arg {
                "" => self.changes_pane()?,
                "keep" => self.keep_changes(&Target::All)?,
                "undo" => {
                    self.changes_pane()?;
                    self.ask_undo(Target::All);
                }
                _ => bail!("Use :changes, :changes keep (all) or :changes undo (all)"),
            },
            "git" | "graph" => match arg {
                "" => self.git_pane()?,
                "all" => {
                    self.git_pane()?;
                    if let Some(Pane::Git(view)) = self.panes.get_mut(&self.focused) {
                        view.show_all();
                    }
                }
                "side" | "sidebar" => {
                    self.sidebar_visible = true;
                    self.sidebar_focus = true;
                    self.git_focus = true;
                }
                _ => bail!("Use :git (open / focus / zoom the Git pane), :git all or :git side"),
            },
            // :ai [codex|claude|gemini|agy|local|<[ai.name]>] [model] [right|below]
            "ai" => {
                let mut program = None;
                let mut axis = None;
                let mut model = None;
                for word in arg.split_whitespace() {
                    match word {
                        "right" | "beside" | "side" => axis = Some(Axis::Horizontal),
                        "below" | "stacked" | "down" => axis = Some(Axis::Vertical),
                        name if program.is_none()
                            && (AGENTS.iter().any(|a| a.1 == name)
                                || self.config.ai.contains_key(name)) =>
                        {
                            program = Some(name)
                        }
                        name if program.is_some() && model.is_none() => model = Some(name),
                        _ => bail!(
                            "Use :ai [codex|claude|gemini|agy|local|<your [ai.name]>] [model] [right|below]"
                        ),
                    }
                }
                match program {
                    Some(program) => {
                        let axis = axis.unwrap_or_else(|| self.ai_axis());
                        self.launch(program, model, Some(axis))?;
                    }
                    None => self.open_ai(axis)?,
                }
            }
            "theme" if !arg.is_empty() => {
                self.apply_theme(arg)?;
                self.theme_stamp = config::theme_stamp(arg);
                let origin = match (arg, config::theme_file(arg)) {
                    (crate::omarchy::FOLLOW, _) => format!(
                        " · follows Omarchy ({})",
                        crate::omarchy::current_name().unwrap_or_else(|| "current theme".into())
                    ),
                    (_, Some(path)) => format!(" · {}", path.display()),
                    (_, None) => " · built in".into(),
                };
                // The choice is remembered for the next launch.
                match config::save_theme(arg) {
                    Ok(_) => self.notice(format!("Theme: {arg}{origin} · saved as default")),
                    Err(error) => self.fail(format!("Theme: {arg} (not saved: {error:#})")),
                }
            }
            "theme" | "themes" => self.open_picker(),
            "set" => match arg {
                "rnu" | "relativenumber" => self.config.relative_numbers = true,
                "nornu" | "norelativenumber" => self.config.relative_numbers = false,
                _ => bail!("Supported: :set rnu / :set nornu"),
            },
            "files" => {
                self.sidebar_visible = true;
                self.sidebar_focus = true;
                self.git_focus = false;
            }
            "help" | "h" => self.toggle_help(),
            "bn" => self.next_buffer(false),
            "bp" => self.next_buffer(true),
            "run" => {
                let plugin = self.config.plugins.get(arg).cloned().with_context(|| {
                    format!(
                        "Unknown plugin: {arg}. Configure it in {}/config.toml",
                        config::config_dir().display()
                    )
                })?;
                self.open_terminal(Some((&plugin.program, plugin.args)), Some(Axis::Vertical))?;
                if let Some(Pane::Terminal(t)) = self.panes.get_mut(&self.focused) {
                    t.launch = Launch::Command { name: arg.into() };
                }
            }
            "" => {}
            _ => bail!("Unknown command: {verb}. F1 for help"),
        }
        Ok(())
    }
}

/// Entries PgUp / PgDn move in the sidebar's lists (files, Git changes, search
/// results): a short hop that keeps your place, not a whole screenful.
const SIDEBAR_PAGE: isize = 5;

/// The word (letters, digits, `_`) under or just before the cursor, and where it
/// starts (a char index).
fn word_at(b: &Buffer) -> Option<(String, usize)> {
    let line: Vec<char> = b.line(b.row).chars().collect();
    let word = |i: usize| {
        line.get(i)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
    };
    let mut at = b.col.min(line.len());
    if !word(at) && at > 0 && word(at - 1) {
        at -= 1;
    }
    if !word(at) {
        return None;
    }
    let start = (0..=at).rev().take_while(|&i| word(i)).last()?;
    let end = (at..line.len()).take_while(|&i| word(i)).last()? + 1;
    let text = line[start..end].iter().collect();
    Some((text, b.text.line_to_char(b.row) + start))
}

/// Letters, digits and `_`: what identifiers are made of.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Word-wise arrows: Option+←/→ on a Mac (Terminal.app and Ghostty send them as
/// Esc b / Esc f, which arrive as Alt+b / Alt+f), Alt+←/→, and Ctrl+←/→ on Linux
/// and Windows. Returns (backwards, extend the selection).
fn word_key(key: KeyEvent) -> Option<(bool, bool)> {
    let m = key.modifiers;
    let shift = m.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Left | KeyCode::Right
            if m.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            Some((key.code == KeyCode::Left, shift))
        }
        KeyCode::Char(c @ ('b' | 'f' | 'B' | 'F'))
            if m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL) =>
        {
            Some((
                c.eq_ignore_ascii_case(&'b'),
                shift || c.is_ascii_uppercase(),
            ))
        }
        _ => None,
    }
}

/// Keys that move the cursor in every mode (and extend a selection with Shift).
fn is_motion(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
    )
}

/// Normal-mode motions that also extend a `v`/`V` selection.
fn is_vim_motion(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char('h' | 'j' | 'k' | 'l' | 'w' | 'b' | '0' | '$' | '^' | 'G' | 'g' | '1'..='9')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.event(Event::Key(KeyEvent::new(code, modifiers)));
    }
    #[test]
    fn keyboard_edit_save_search_jump_and_dirty_quit_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path.clone()), false).unwrap();
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        app.event(Event::Paste("λ ".into()));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit(false).is_err());
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "λ first\nsecond\nthird\n"
        );
        app.command("3").unwrap();
        assert_eq!(app.active_buffer().unwrap().row, 2);
        app.active_buffer().unwrap().search = "second".into();
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.active_buffer().unwrap().row, 1);
        assert!(app.quit(false).is_ok());
    }
    #[test]
    fn option_and_cmd_arrows_move_by_word_and_line_like_a_mac() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.js");
        std::fs::write(&path, "let count = value;\nnext line\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        let col = |app: &App| (app.buffers[id].row, app.buffers[id].col);
        let alt = KeyModifiers::ALT;
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        // Terminal.app sends Option+→ / ← as Esc f / Esc b: Alt+f / Alt+b.
        press(&mut app, KeyCode::Char('f'), alt);
        assert_eq!(col(&app), (0, 3), "end of let");
        press(&mut app, KeyCode::Right, alt);
        assert_eq!(col(&app), (0, 9), "end of count");
        press(&mut app, KeyCode::Char('b'), alt);
        assert_eq!(col(&app), (0, 4));
        assert_eq!(app.mode, Mode::Insert, "nothing typed");
        // Shift+Option selects a word; typing replaces it.
        press(&mut app, KeyCode::Right, alt | KeyModifiers::SHIFT);
        assert_eq!(app.selection(), Some((4, 9)));
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(app.buffers[id]
            .text
            .to_string()
            .starts_with("let n = value;"));
        // Option+Backspace deletes the word before the cursor.
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        press(&mut app, KeyCode::Backspace, alt);
        assert!(app.buffers[id]
            .text
            .to_string()
            .starts_with("let n = value\n"));
        press(&mut app, KeyCode::Backspace, alt);
        assert!(app.buffers[id].text.to_string().starts_with("let n = \n"));
        // Cmd+arrows, where the terminal passes Cmd on.
        let cmd = KeyModifiers::SUPER;
        press(&mut app, KeyCode::Left, cmd);
        assert_eq!(col(&app), (0, 0));
        press(&mut app, KeyCode::Right, cmd | KeyModifiers::SHIFT);
        assert_eq!(app.selection(), Some((0, 8)));
        press(&mut app, KeyCode::Down, cmd);
        assert_eq!(app.mode, Mode::Insert, "a plain one ends the selection");
        assert_eq!(col(&app), (2, 0));
        press(&mut app, KeyCode::Up, cmd);
        assert_eq!(col(&app), (0, 0));
        // Ctrl+arrows (Linux, Windows) move by word too.
        press(&mut app, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(col(&app), (0, 3));
    }
    #[test]
    fn ctrl_p_tab_completes_folders_and_what_is_inside_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for file in ["src/main.rs", "src/model/user.rs", "scripts/run.sh", ".env"] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x\n").unwrap();
        }
        let mut app = App::new(root.clone(), None, false).unwrap();
        let text = |app: &App| app.input.as_ref().unwrap().text.clone();
        let typed = |app: &mut App, keys: &str| {
            for c in keys.chars() {
                let code = match c {
                    '\t' => KeyCode::Tab,
                    '\n' => KeyCode::Enter,
                    c => KeyCode::Char(c),
                };
                press(app, code, KeyModifiers::NONE);
            }
        };
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        // Two folders start with s: listed, and Tab steps through them.
        typed(&mut app, "s\t");
        assert_eq!(text(&app), "scripts/");
        assert!(app.message.contains("2 matches"), "{}", app.message);
        typed(&mut app, "\t");
        assert_eq!(text(&app), "src/");
        // Into the folder: what they share is filled in, then one is left.
        typed(&mut app, "mo\t");
        assert_eq!(text(&app), "src/model/");
        typed(&mut app, "\t");
        assert_eq!(text(&app), "src/model/user.rs");
        typed(&mut app, "\n");
        assert!(app.buffers[app.buffer_id().unwrap()]
            .name()
            .contains("user.rs"));
        // Case is ignored when nothing matches exactly; hidden names need a dot.
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        typed(&mut app, "src/MA\t");
        assert_eq!(text(&app), "src/main.rs");
        assert!(workspace::complete_path(&root, "")
            .iter()
            .all(|p| !p.starts_with('.')));
        assert_eq!(workspace::complete_path(&root, ".e"), vec![".env"]);
        assert!(workspace::complete_path(&root, "nothing/").is_empty());
        // Enter on a folder shows it, opened, in the explorer.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        typed(&mut app, "src/model\n");
        let model = root.join("src/model");
        assert!(app.sidebar_focus && !app.git_focus);
        assert!(app.tree.expanded.contains(&model));
        assert_eq!(app.tree.entries[app.tree.selected].path, model);
    }
    #[test]
    fn typing_python_indents_after_a_colon_and_closes_brackets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.py");
        std::fs::write(&path, "").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        let keys = |app: &mut App, text: &str| {
            for c in text.chars() {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                press(app, code, KeyModifiers::NONE);
            }
        };
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        keys(&mut app, "def testfunc():\nprint(\"hi\")");
        assert_eq!(
            app.buffers[id].text.to_string(),
            "def testfunc():\n    print(\"hi\")"
        );
        // Select a word with Shift+Option and type [ to wrap it.
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        press(
            &mut app,
            KeyCode::Left,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert!(app.selection().is_some());
        keys(&mut app, "[");
        assert!(app.buffers[id]
            .text
            .to_string()
            .ends_with("print(\"[hi]\")"));
        // o in normal mode indents like Enter; the switches turn it off.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.buffers[id].row = 0;
        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        assert_eq!(app.buffers[id].line(1), "    ");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.config.auto_pairs = false;
        app.config.smart_indent = false;
        press(&mut app, KeyCode::Char('A'), KeyModifiers::NONE);
        keys(&mut app, "(\n");
        assert!(app.buffers[id].text.to_string().contains("    (\n    \n"));
    }
    #[test]
    fn page_keys_move_through_the_explorer_a_few_entries_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..80 {
            std::fs::write(dir.path().join(format!("file{n:02}.txt")), "").unwrap();
        }
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.sidebar_focus = true;
        screen(&mut app, 100, 30);
        let start = app.tree.selected;
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.tree.selected, start + SIDEBAR_PAGE as usize);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.tree.selected, start + 6 * SIDEBAR_PAGE as usize);
        // The list scrolls along: the selected file is on screen.
        let name = app.tree.entries[app.tree.selected]
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            screen(&mut app, 100, 30).join("\n").contains(&name),
            "{name}"
        );
        for _ in 0..6 {
            press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        }
        assert_eq!(app.tree.selected, start);
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.tree.selected, app.tree.entries.len() - 1);
        press(&mut app, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.tree.selected, 0);
    }

    #[test]
    fn ctrl_f_in_the_explorer_finds_names_and_text_in_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.py"),
            "import os\n\ndef main():\n    total = count(1)\n",
        )
        .unwrap();
        std::fs::write(root.join("counter.txt"), "nothing\n").unwrap();
        let mut app = App::new(root.clone(), Some(root.join("counter.txt")), false).unwrap();
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        for c in "count".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        let start = Instant::now();
        while app.explorer_search.as_ref().unwrap().running
            && start.elapsed() < Duration::from_secs(10)
        {
            app.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        let shown = screen(&mut app, 120, 30).join("\n");
        assert!(
            shown.contains("SEARCH") && shown.contains("2 results in 2 files"),
            "{shown}"
        );
        assert!(shown.contains("total = count(1)"));
        // Down previews the line; Enter opens it with the cursor on the match.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(app.sidebar_focus, "previewing keeps the explorer focused");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.sidebar_focus);
        let b = &app.buffers[app.buffer_id().unwrap()];
        assert!(b.name().contains("main.py"));
        assert_eq!((b.row, b.col, b.search.as_str()), (3, 12, "count"));
        // Back in the explorer, the search is still there; Esc returns to the tree.
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(screen(&mut app, 120, 30).join("\n").contains("SEARCH"));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.explorer_search.is_none());
        assert!(screen(&mut app, 120, 30).join("\n").contains("EXPLORER"));
    }
    #[test]
    fn suggestions_pop_up_while_typing_without_a_language_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.js");
        std::fs::write(&path, "const totalCount = 1;\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        let keys = |app: &mut App, text: &str| {
            for c in text.chars() {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                press(app, code, KeyModifiers::NONE);
            }
        };
        let labels = |app: &App| -> Vec<String> {
            app.menu
                .as_ref()
                .map(|m| m.shown.iter().map(|&i| m.items[i].label.clone()).collect())
                .unwrap_or_default()
        };
        press(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        keys(&mut app, "t");
        assert!(app.menu.is_none(), "one letter is not enough");
        keys(&mut app, "o");
        assert_eq!(labels(&app), vec!["totalCount"]);
        let shown = screen(&mut app, 100, 20).join("\n");
        assert!(shown.contains("totalCount"), "{shown}");
        // Enter is a new line until a suggestion is picked; Tab takes it.
        keys(&mut app, "\nto");
        assert!(app.buffers[id].text.to_string().contains("\nto\nto"));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.buffers[id].text.to_string().ends_with("\ntotalCount"));
        // Keywords too; ↓ picks one, and then Enter takes it.
        keys(&mut app, "\nfun");
        assert_eq!(labels(&app), vec!["function"]);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.buffers[id].text.to_string().ends_with("\nfunction"));
        // autocomplete = false: nothing pops up, Ctrl-Space (or Ctrl-n) still asks.
        app.config.autocomplete = false;
        keys(&mut app, " tot");
        assert!(app.menu.is_none());
        press(&mut app, KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(labels(&app), vec!["totalCount"]);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.menu.is_none());
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(labels(&app), vec!["totalCount"]);
    }
    #[test]
    fn suggestions_stay_out_of_strings_and_comments_and_close_when_typed_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.go");
        std::fs::write(
            &path,
            "package main\n\nfunc greeting() string {\n\treturn \"\"\n}\n",
        )
        .unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let keys = |app: &mut App, text: &str| {
            for c in text.chars() {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                press(app, code, KeyModifiers::NONE);
            }
        };
        press(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        keys(&mut app, "x := gre");
        assert!(app.menu.is_some(), "suggests greeting");
        // The whole word typed: nothing left to suggest, the list goes away.
        keys(&mut app, "eting");
        assert!(app.menu.is_none(), "closed once typed out");
        // Inside a string or a comment it doesn't pop up by itself.
        keys(&mut app, "\ny := \"gre");
        assert!(app.menu.is_none(), "not in a string");
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        keys(&mut app, " // gre");
        assert!(app.menu.is_none(), "not in a comment");
        // Asked for, it comes anyway.
        press(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(app.menu.is_some(), "Ctrl-n still asks");
    }
    #[test]
    fn suggestions_stay_inside_the_pane_being_typed_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.py");
        let words = "print_report_for_every_customer = 1\nprinted_values = 2\nprint_header = 3\n";
        std::fs::write(&path, format!("{words}    \n")).unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let left = app.focused;
        app.split_editor(Axis::Horizontal).unwrap();
        let right = app.focused;
        let other = dir.path().join("other.txt");
        std::fs::write(&other, "another file\n").unwrap();
        app.open(&other).unwrap();
        app.focused = left;
        let id = app.buffer_id().unwrap();
        // The cursor sits far right in the narrow left pane.
        app.buffers[id].row = 3;
        app.buffers[id].col = 4;
        press(&mut app, KeyCode::Char('A'), KeyModifiers::NONE);
        let (width, height) = (90, 20);
        let before = screen(&mut app, width, height);
        let pane = |app: &App, id| app.rectangles.iter().find(|(i, _)| *i == id).unwrap().1;
        let (l, r) = (pane(&app, left), pane(&app, right));
        for c in "x = 1 + pr".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        let menu = app.menu.as_ref().expect("suggestions popped up");
        assert!(menu.auto && menu.shown.len() >= 3);
        let count = menu.shown.len();
        let after = screen(&mut app, width, height);
        let columns = |row: &str, from: u16, to: u16| -> String {
            row.chars()
                .skip(from as usize)
                .take((to - from) as usize)
                .collect()
        };
        // The other pane looks exactly as it did; the list is inside the left one.
        for y in 0..height as usize {
            assert_eq!(
                columns(&before[y], r.x, r.right()),
                columns(&after[y], r.x, r.right()),
                "row {y}"
            );
        }
        let shown = after.join("\n");
        assert!(shown.contains("print_"), "{shown}");
        let title = format!("╭ 1/{count}");
        assert!(
            after
                .iter()
                .any(|row| columns(row, l.x, l.right()).contains(&title)),
            "{shown}"
        );
    }
    #[test]
    fn find_and_replace_like_vscode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.js");
        std::fs::write(
            &path,
            "let count = 1;\nlet counter = count + 2;\nprint(count, Count);\n",
        )
        .unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        let ctrl = |app: &mut App, c| press(app, KeyCode::Char(c), KeyModifiers::CONTROL);
        let typed = |app: &mut App, text: &str| {
            for c in text.chars() {
                press(app, KeyCode::Char(c), KeyModifiers::NONE);
            }
        };
        let at = |app: &App| app.buffers[id].match_position();
        // Ctrl-f takes the word under the cursor; lower case ignores case.
        app.buffers[id].col = 5;
        ctrl(&mut app, 'f');
        assert_eq!(app.find.as_ref().unwrap().query, "count");
        assert_eq!(at(&app), (1, 5), "count, counter, count, count, Count");
        // Ctrl-w: whole words only, so "counter" drops out.
        ctrl(&mut app, 'w');
        assert_eq!(at(&app), (1, 4));
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(at(&app), (2, 4));
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(at(&app), (1, 4));
        let shown = screen(&mut app, 100, 20).join("\n");
        assert!(shown.contains("1 of 4"), "{shown}");
        // Ctrl-h: replace one with Enter, then the rest with Ctrl-a, one undo step.
        ctrl(&mut app, 'h');
        typed(&mut app, "total");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.buffers[id]
            .text
            .to_string()
            .starts_with("let total = 1;"));
        assert_eq!(at(&app), (1, 3), "on to the next one");
        assert!(screen(&mut app, 100, 20)
            .join("\n")
            .contains("Find & replace"));
        ctrl(&mut app, 'a');
        assert_eq!(
            app.buffers[id].text.to_string(),
            "let total = 1;\nlet counter = total + 2;\nprint(total, total);\n"
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.find.is_none());
        press(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(
            app.buffers[id].text.to_string(),
            "let total = 1;\nlet counter = count + 2;\nprint(count, Count);\n"
        );
        // Typing replaces the filled-in word; a capital letter matches case. The
        // replace text and whole-word choice carry over.
        app.buffers[id].row = 1;
        app.buffers[id].col = 6;
        ctrl(&mut app, 'f');
        assert_eq!(app.find.as_ref().unwrap().query, "counter");
        assert_eq!(app.find.as_ref().unwrap().replace, "total");
        typed(&mut app, "Count");
        assert_eq!(app.find.as_ref().unwrap().query, "Count");
        assert_eq!(at(&app), (1, 1));
        assert_eq!((app.buffers[id].row, app.buffers[id].col), (2, 13));
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        typed(&mut app, "zzz");
        assert!(screen(&mut app, 100, 20).join("\n").contains("No results"));
        // Ctrl-b closes the bar and still works; a selection fills the bar in.
        ctrl(&mut app, 'b');
        assert!(app.find.is_none() && app.prefix);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.buffers[id].row = 0;
        app.buffers[id].col = 0;
        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        typed(&mut app, "ll");
        ctrl(&mut app, 'f');
        assert_eq!(app.find.as_ref().unwrap().query, "let");
        assert_eq!(app.mode, Mode::Normal);
        // :noh clears the highlights; n keeps searching after Esc otherwise.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(app.buffers[id].row, 1);
        app.command("noh").unwrap();
        assert!(app.buffers[id].search.is_empty());
    }
    #[test]
    fn the_terminals_paste_replaces_a_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "keep old text\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        // Shift-select "old" in insert mode, then paste with the terminal (⌘V).
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        app.buffers[id].col = 5;
        for _ in 0..3 {
            press(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        }
        app.event(Event::Paste("new".into()));
        assert_eq!(app.buffers[id].text.to_string(), "keep new text\n");
        assert_eq!(app.mode, Mode::Insert, "back to typing");
        // v in normal mode too; no selection, it inserts at the cursor.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.buffers[id].col = 0;
        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        }
        app.event(Event::Paste("save".into()));
        assert_eq!(app.buffers[id].text.to_string(), "save new text\n");
        app.event(Event::Paste("!".into()));
        assert!(app.buffers[id].text.to_string().contains('!'));
    }
    #[test]
    fn copy_cut_paste_with_selection_lines_and_shift_arrows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.txt");
        std::fs::write(&path, "alpha beta\ngamma\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let text = |app: &mut App| app.active_buffer().unwrap().text.to_string();
        let none = KeyModifiers::NONE;
        // v + motion + y copies inclusively; P pastes at the cursor.
        press(&mut app, KeyCode::Char('v'), none);
        press(&mut app, KeyCode::Char('l'), none);
        press(&mut app, KeyCode::Char('l'), none);
        press(&mut app, KeyCode::Char('y'), none);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.clipboard.paste(), ("alp".into(), false));
        press(&mut app, KeyCode::Char('P'), none);
        assert_eq!(text(&mut app), "alpalpha beta\ngamma\n");
        // Ctrl-c without a selection copies the line; Ctrl-v pastes it above.
        press(&mut app, KeyCode::Char('u'), none);
        press(&mut app, KeyCode::Char('j'), none);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(text(&mut app), "alpha beta\ngamma\ngamma\n");
        // V + d cuts whole lines; p pastes them below.
        press(&mut app, KeyCode::Char('V'), none);
        press(&mut app, KeyCode::Char('d'), none);
        assert_eq!(text(&mut app), "alpha beta\ngamma\n");
        press(&mut app, KeyCode::Char('k'), none);
        press(&mut app, KeyCode::Char('p'), none);
        assert_eq!(text(&mut app), "alpha beta\ngamma\ngamma\n");
        // Shift+arrows in insert mode select exclusively; typing replaces the selection.
        press(&mut app, KeyCode::Char('g'), none);
        press(&mut app, KeyCode::Char('g'), none);
        press(&mut app, KeyCode::Char('i'), none);
        for _ in 0..5 {
            press(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        }
        assert_eq!(app.mode, Mode::Visual);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(app.mode, Mode::Insert);
        assert_eq!(app.clipboard.paste(), ("alpha".into(), false));
        press(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert_eq!(text(&mut app), "Zbeta\ngamma\ngamma\n");
        // Bracketed paste works without entering insert mode first.
        press(&mut app, KeyCode::Esc, none);
        app.event(Event::Paste("!".into()));
        assert!(text(&mut app).starts_with("!Zbeta") || text(&mut app).starts_with("Z!beta"));
    }
    #[cfg(unix)]
    #[test]
    fn lsp_diagnostics_completion_menu_and_format_through_the_app() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {\n    x;\n}\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let mut servers = BTreeMap::new();
        servers.insert(
            "rust".to_string(),
            crate::config::CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), crate::lsp::FAKE_SERVER.into()],
            },
        );
        app.lsp = Lsp::new(dir.path().into(), true, servers);
        let settle = |app: &mut App, done: &dyn Fn(&App) -> bool| {
            for _ in 0..300 {
                app.tick();
                if done(app) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("timed out: {}", app.message);
        };
        let id = app.buffer_id().unwrap();
        settle(&mut app, &|a| a.lsp.diagnostics.contains_key(&id));
        // `.` is a trigger character: the menu opens, typing filters, Tab accepts.
        app.command("2").unwrap();
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('.'), KeyModifiers::NONE);
        settle(&mut app, &|a| a.menu.is_some());
        press(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.menu.is_none());
        assert_eq!(app.active_buffer().unwrap().line(1), "    x;.println!(x)");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.command("format").unwrap();
        settle(&mut app, &|a| a.message.starts_with("Formatted"));
        assert_eq!(app.active_buffer().unwrap().line(0), "pub fn main() {");
        // Two letters of a word ask the server by themselves, once per word.
        press(&mut app, KeyCode::Char('o'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
        settle(&mut app, &|a| a.menu.is_some());
        let menu = app.menu.as_ref().unwrap();
        assert!(menu.auto && menu.shown.len() == 2);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.active_buffer().unwrap().line(2).trim(), "print");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.command("lsp").unwrap();
        assert!(app.message.contains("python3 ready"), "{}", app.message);
    }
    #[test]
    fn theme_picker_previews_live_filters_and_restores_on_escape() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.command("theme nord").unwrap();
        // Ctrl-b t opens the picker on the current theme.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.themes[picker.shown[picker.selected]].0, "nord");
        assert!(picker.themes.iter().any(|(n, _)| n == "vscode-dark-2026"));
        // Moving previews the next theme immediately.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_ne!(app.config.theme, "nord");
        // Esc restores the original.
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.picker.is_none());
        assert_eq!(app.config.theme, "nord");
        // Typing filters; Enter keeps the highlighted theme.
        app.command("themes").unwrap();
        for c in "drac".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.picker.as_ref().unwrap().shown.len(), 1);
        assert_eq!(app.config.theme, "dracula");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.picker.is_none());
        assert_eq!(app.theme.background, "#282a36");
        assert!(app.message.contains("saved as default"), "{}", app.message);
        // It renders at small and large sizes.
        app.command("themes").unwrap();
        for (width, height) in [(160, 45), (60, 16)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(screen.contains("THEMES"));
        }
    }
    #[test]
    fn browsing_the_explorer_previews_files_in_one_reused_buffer() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.path().join(name), format!("{name}\n")).unwrap();
        }
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let shown = |app: &App| match app.panes.get(&0) {
            Some(Pane::Editor(id)) => app.buffers[*id].name(),
            _ => String::new(),
        };
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(app.sidebar_focus);
        // Ctrl-e lands in the explorer even after visiting the Git section.
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.git_focus);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(app.sidebar_focus && !app.git_focus);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(
            !app.sidebar_focus,
            "Ctrl-e in the explorer returns to the pane"
        );
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
        assert!(app.sidebar_focus && app.git_focus);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(app.sidebar_focus && !app.git_focus);
        let start = app.buffers.len();
        // Each arrow press shows the highlighted file; focus stays in the sidebar.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let first = shown(&app);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        let second = shown(&app);
        assert_ne!(first, second);
        assert!(second.ends_with(".txt"));
        assert!(app.sidebar_focus);
        // Previews reuse one buffer instead of opening one per file.
        assert_eq!(app.buffers.len(), start + 1);
        // Enter keeps the file open and focuses it; the next preview gets a new buffer.
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.sidebar_focus);
        assert_eq!(shown(&app), second);
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(shown(&app), first);
        assert_eq!(app.buffers.len(), start + 2);
        assert!(app.buffers.iter().any(|b| b.name() == second));
        // An edited preview is kept, not replaced.
        let Some(Pane::Editor(id)) = app.panes.get(&0) else {
            panic!("editor pane")
        };
        let id = *id;
        app.buffers[id].insert("edit ");
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(app.buffers[id].dirty);
        assert_eq!(app.buffers[id].name(), first);
    }
    #[test]
    fn field_guide_stays_open_while_commands_and_edits_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.help && app.help_focus);
        // Focused, the guide scrolls with plain keys — no Alt needed on macOS.
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.help_scroll, 2);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(app.help_scroll, 1);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.help_scroll, 0);
        assert_eq!(app.active_buffer().unwrap().line(0), "hello", "no edits");
        // A command typed with the guide open runs, and the guide stays.
        press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
        for c in "theme nord".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.config.theme, "nord");
        assert!(app.help);
        // Editing works too.
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        press(&mut app, KeyCode::Char('!'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.active_buffer().unwrap().line(0), "hello!");
        assert!(app.help);
        // Alt-arrows scroll the guide; it renders docked beside the panes.
        press(&mut app, KeyCode::Down, KeyModifiers::ALT);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("FIELD GUIDE"));
        assert!(screen.contains("a.txt"), "editor pane still visible");
        assert!(screen.contains("◉ RAIDO"), "the logo stays in the header");
        assert!(!screen.contains("ALL SYSTEMS IN VIEW"), "no tagline");
        assert_eq!(app.help_scroll, 1);
        assert!(!app.help_focus, "typing went back to work");
        // Esc leaves a focused guide open; F1 refocuses it, then closes it.
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.help && app.help_focus);
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(app.help_scroll, 11);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.help && !app.help_focus);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(!app.help);
    }
    #[test]
    fn relative_jumps_follow_the_numbers_shown_with_rnu() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lines.txt");
        let text: String = (1..=40).map(|i| format!("  line {i}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        app.command("set rnu").unwrap();
        app.command("20").unwrap();
        let row = |app: &mut App| app.active_buffer().unwrap().row;
        assert_eq!(row(&mut app), 19);
        // 15 then Enter: fifteen lines down, on the first non-blank.
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('5'), KeyModifiers::NONE);
        assert!(app.message.contains("15 lines down"), "{}", app.message);
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(row(&mut app), 34);
        assert_eq!(app.active_buffer().unwrap().col, 2);
        // 15 then -: fifteen lines up. 3k / 3j work too.
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('5'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('-'), KeyModifiers::NONE);
        assert_eq!(row(&mut app), 19);
        press(&mut app, KeyCode::Char('3'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(row(&mut app), 16);
        // :+N / :-N in the command line; :N stays absolute.
        app.command("+10").unwrap();
        assert_eq!(row(&mut app), 26);
        app.command("-6").unwrap();
        assert_eq!(row(&mut app), 20);
        app.command("5").unwrap();
        assert_eq!(row(&mut app), 4);
    }
    #[test]
    fn ai_panes_open_beside_or_below_and_missing_clis_explain_install() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let rects = |app: &mut App| {
            app.rectangles.clear();
            app.tiles
                .rectangles(Rect::new(0, 0, 120, 40), &mut app.rectangles);
            app.rectangles.clone()
        };
        app.command("ai below").unwrap();
        let r = rects(&mut app);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1.x, r[1].1.x, "stacked panes share a column");
        app.close(false).unwrap();
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        let r = rects(&mut app);
        assert_eq!(r[0].1.y, r[1].1.y, "side-by-side panes share a row");
        app.close(false).unwrap();
        // The config default applies when no placement is given.
        app.config.ai_split = "below".into();
        app.command("ai").unwrap();
        let r = rects(&mut app);
        assert_eq!(r[0].1.x, r[1].1.x);
        // A CLI that is not installed explains how to get it.
        assert!(matches!(app.panes.get(&app.focused), Some(Pane::Ai)));
        let error = app.launch_agent("raido-no-such-agent", None).unwrap_err();
        assert!(error.to_string().contains("not installed"));
        assert!(app.command("ai nonsense").is_err());
    }
    #[test]
    fn field_guide_fits_small_screens_and_scrolls_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        for (width, height) in [(80u16, 24u16), (60, 20), (120, 30)] {
            let mut seen = String::new();
            app.help_scroll = 0;
            let mut last = usize::MAX;
            while last != app.help_scroll {
                last = app.help_scroll;
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
                let screen: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                seen.push_str(&screen);
                press(&mut app, KeyCode::Down, KeyModifiers::ALT);
            }
            // Every section, down to the last row, is reachable at this size.
            for heading in [
                "THEMES",
                "WORKSPACE",
                "EDITOR",
                "MARKDOWN",
                "COMMANDS",
                "rnu",
            ] {
                assert!(seen.contains(heading), "{heading} at {width}x{height}");
            }
        }
    }
    #[cfg(unix)]
    #[test]
    fn your_own_ai_launchers_from_config_run_in_ai_panes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        // Exactly what a user writes in config.toml.
        let config: crate::config::Config = toml::from_str(
            r#"
[ai.local]
program = "sh"
args = ["-c", "echo local {model}; sleep 5"]
model = "tiny"

[ai.llamacpp]
program = "sh"
args = ["-c", "echo $OPENAI_BASE_URL; sleep 5"]
env = { OPENAI_BASE_URL = "http://localhost:8080/v1" }
"#,
        )
        .unwrap();
        app.config.ai = config.ai;
        assert_eq!(app.custom_agents(), vec!["llamacpp"]);
        let terminals = |app: &App| {
            app.panes
                .values()
                .filter(|p| matches!(p, Pane::Terminal(_)))
                .count()
        };
        // :ai local uses [ai.local], with a model override.
        app.command("ai local big below").unwrap();
        assert_eq!(terminals(&app), 1);
        // Launcher key 6 runs the first custom entry in the AI pane itself.
        app.command("ai").unwrap();
        assert!(matches!(app.panes.get(&app.focused), Some(Pane::Ai)));
        press(&mut app, KeyCode::Char('6'), KeyModifiers::NONE);
        assert_eq!(terminals(&app), 2);
        assert!(matches!(app.panes.get(&app.focused), Some(Pane::Terminal(t)) if t.title == "sh"));
        // Unknown names and missing programs explain themselves.
        assert!(app.command("ai nosuch").is_err());
        app.config.ai.get_mut("llamacpp").unwrap().program = "raido-no-such-llm".into();
        let error = app.command("ai llamacpp").unwrap_err().to_string();
        assert!(error.contains("not on PATH"), "{error}");
    }
    #[test]
    fn pane_shortcuts_focus_and_collapse_without_losing_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('%'), KeyModifiers::NONE);
        assert_eq!(app.panes.len(), 2);
        app.tiles
            .rectangles(Rect::new(0, 0, 120, 40), &mut app.rectangles);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.focused, 0);
        app.close(false).unwrap();
        assert_eq!(app.panes.len(), 1);
        assert!(app.tiles.contains(app.focused));
    }
    #[test]
    fn rendering_contains_editor_tree_git_and_ai_at_multiple_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() { println!(\"hello\"); }\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        app.add_pane(Pane::Ai, Axis::Horizontal);
        for (width, height) in [(160, 45), (80, 24), (45, 12), (20, 8)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(rendered.contains("RAIDO"));
            if width == 160 {
                for needle in [
                    "EXPLORER",
                    "GIT / TREE",
                    "main.rs",
                    "OpenAI Codex",
                    "Claude Code",
                ] {
                    assert!(rendered.contains(needle), "Missing {needle}");
                }
            }
        }
    }
    #[test]
    fn arrows_move_only_to_panes_on_that_side() {
        // Two panes on top, one full-width pane below.
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let top_left = app.focused;
        app.split_editor(Axis::Vertical).unwrap();
        let below = app.focused;
        app.focused = top_left;
        app.split_editor(Axis::Horizontal).unwrap();
        let top_right = app.focused;
        // A wide window: the old centre-to-centre rule picked the pane below for Right.
        app.tiles
            .rectangles(Rect::new(0, 0, 200, 30), &mut app.rectangles);
        let mut go = |from, key| {
            app.focused = from;
            press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(&mut app, key, KeyModifiers::NONE);
            app.focused
        };
        assert_eq!(
            go(top_left, KeyCode::Right),
            top_right,
            "not the wide pane below"
        );
        assert_eq!(go(top_left, KeyCode::Down), below);
        assert_eq!(go(top_right, KeyCode::Left), top_left);
        assert_eq!(go(top_right, KeyCode::Down), below);
        assert_eq!(
            go(top_right, KeyCode::Right),
            top_right,
            "nothing further right"
        );
        assert_eq!(go(below, KeyCode::Right), below);
        assert_eq!(go(below, KeyCode::Left), below);
        assert!([top_left, top_right].contains(&go(below, KeyCode::Up)));
    }
    #[cfg(unix)]
    #[test]
    fn docked_panes_keep_running_and_come_back_to_the_same_place() {
        // Two panes on top (a, b), a shell below across the whole width (c).
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let a = app.focused;
        app.open_terminal(
            Some((
                "/bin/sh",
                vec!["-c".into(), "read x; echo got-$x; exec cat".into()],
            )),
            Some(Axis::Vertical),
        )
        .unwrap();
        let c = app.focused;
        app.focused = a;
        app.split_editor(Axis::Horizontal).unwrap();
        let b = app.focused;
        let prefix = |app: &mut App, code| {
            press(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(app, code, KeyModifiers::SHIFT);
        };
        let rect = |app: &App, id| app.rectangles.iter().find(|(i, _)| *i == id).map(|r| r.1);
        screen(&mut app, 120, 30);
        let (before_b, before_c) = (rect(&app, b).unwrap(), rect(&app, c).unwrap());
        // Dock the shell: the top panes take its room, and focus goes there.
        app.focused = c;
        prefix(&mut app, KeyCode::Char('d'));
        assert_eq!(app.docked, vec![c]);
        assert_ne!(app.focused, c);
        let shown = screen(&mut app, 120, 30).join("\n");
        assert!(rect(&app, c).is_none());
        assert!(
            rect(&app, a).unwrap().height > before_c.y - 2,
            "full height now"
        );
        assert!(
            shown.contains(" dock ") && shown.contains(" 1 sh "),
            "{shown}"
        );
        // It keeps running while docked.
        if let Some(Pane::Terminal(t)) = app.panes.get_mut(&c) {
            t.send(b"hi\n").unwrap();
        }
        let mut got = false;
        for _ in 0..200 {
            app.tick();
            if let Some(Pane::Terminal(t)) = app.panes.get(&c) {
                got = t.transcript().contains("got-hi");
            }
            if got {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(got, "the docked shell answered");
        // Dock b too, then bring both back from the list and a tab click.
        app.focused = b;
        app.command("dock").unwrap();
        assert_eq!(app.visible(), vec![a]);
        prefix(&mut app, KeyCode::Char('d'));
        assert!(app.message.contains("last pane"), "the last one stays");
        prefix(&mut app, KeyCode::Char('D'));
        assert_eq!(app.dock_list, Some(1), "the newest is selected");
        press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(app.focused, c);
        screen(&mut app, 120, 30);
        assert_eq!(rect(&app, c).unwrap().y, before_c.y, "back below");
        let (id, tab) = app.dock_tabs[0];
        assert_eq!(id, b);
        app.event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: tab.x + 1,
            row: tab.y,
            modifiers: KeyModifiers::NONE,
        }));
        screen(&mut app, 120, 30);
        assert_eq!(rect(&app, b), Some(before_b), "same place and size");
        assert_eq!(rect(&app, c), Some(before_c));
        assert!(app.docked.is_empty());
        // Docked panes are saved with the session; x in the list closes one.
        app.focused = c;
        app.command("dock").unwrap();
        assert_eq!(app.session().docked, vec![c]);
        app.command("undock").unwrap();
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(!app.panes.contains_key(&c) && app.docked.is_empty());
        assert_eq!(app.visible().len(), 2);
    }
    #[test]
    fn ctrl_b_y_hands_the_mouse_to_the_terminal_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let toggle = |app: &mut App| {
            press(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(app, KeyCode::Char('y'), KeyModifiers::NONE);
        };
        assert!(app.mouse_wanted());
        toggle(&mut app);
        assert!(!app.mouse_wanted(), "released for the terminal's selection");
        let shown = screen(&mut app, 120, 20).join("\n");
        assert!(shown.contains("MOUSE RELEASED"), "{shown}");
        // The copy key this OS's terminals use: ⌘C on a Mac, Ctrl-Shift-C elsewhere.
        assert!(shown.contains(crate::platform::COPY_KEY), "{shown}");
        assert!(shown.contains("EXPLORER"), "the layout stays as it is");
        toggle(&mut app);
        assert!(app.mouse_wanted());
        assert!(!screen(&mut app, 120, 20)
            .join("\n")
            .contains("MOUSE RELEASED"));
        // :mouse off / on say what they mean, whatever the state.
        app.command("mouse off").unwrap();
        app.command("mouse off").unwrap();
        assert!(!app.mouse_wanted());
        app.command("mouse on").unwrap();
        assert!(app.mouse_wanted());
        assert!(app.command("mouse sideways").is_err());
        app.config.mouse = false;
        toggle(&mut app);
        assert!(app.message.contains("mouse = false"));
    }
    #[test]
    fn resize_mode_resizes_with_plain_arrows() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.split_editor(Axis::Horizontal).unwrap();
        let percent = |app: &App| match &app.tiles {
            Tile::Split { percent, .. } => *percent,
            Tile::Leaf(_) => unreachable!(),
        };
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(app.resizing);
        assert!(screen(&mut app, 100, 20).join("\n").contains("RESIZE"));
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(percent(&app), 60);
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(percent(&app), 55);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert!(app.message.contains("No stacked split"));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.resizing);
        // Any other key ends it and does its usual job.
        app.command("noh").unwrap();
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char(':'), KeyModifiers::NONE);
        assert!(!app.resizing && app.input.is_some());
    }
    #[test]
    fn the_active_pane_carries_the_raido_mark() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.split_editor(Axis::Horizontal).unwrap();
        let marks = |app: &mut App| -> Vec<String> {
            screen(app, 120, 20)
                .iter()
                .flat_map(|row| {
                    row.match_indices("◉ ")
                        .map(|(at, _)| row[at..].chars().take(12).collect::<String>())
                })
                .filter(|s| !s.starts_with("◉ RAIDO"))
                .collect()
        };
        let focused = app.focused + 1;
        let shown = marks(&mut app);
        assert_eq!(shown.len(), 1, "only the focused pane: {shown:?}");
        assert!(shown[0].starts_with(&format!("◉ {focused} ")), "{shown:?}");
        // In the accent color.
        let backend = ratatui::backend::TestBackend::new(120, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let accent = crate::config::color(&app.theme.accent);
        let dot = (0..20)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .filter(|&(x, y)| y > 0 && buffer[(x, y)].symbol() == "◉")
            .collect::<Vec<_>>();
        assert_eq!(dot.len(), 1);
        assert_eq!(buffer[dot[0]].fg, accent);
        // Focus moves to the explorer: so does the mark.
        press(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        let shown = marks(&mut app);
        assert_eq!(shown.len(), 1);
        assert!(shown[0].starts_with("◉ EXPLORER"), "{shown:?}");
    }
    #[test]
    fn a_maximized_pane_says_other_panes_are_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let zoom = |app: &mut App| {
            press(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(app, KeyCode::Char('z'), KeyModifiers::NONE);
        };
        zoom(&mut app);
        assert!(
            !screen(&mut app, 100, 20).join("\n").contains("MAXIMIZED"),
            "nothing hidden"
        );
        zoom(&mut app);
        app.split_editor(Axis::Horizontal).unwrap();
        app.split_editor(Axis::Vertical).unwrap();
        zoom(&mut app);
        assert!(app.zoom);
        let shown = screen(&mut app, 100, 20).join("\n");
        assert!(
            shown.contains("MAXIMIZED · 2 more panes · Ctrl-b z restores"),
            "{shown}"
        );
        // Narrow: a shorter badge still fits on the border.
        assert!(screen(&mut app, 50, 12).join("\n").contains("+2 panes"));
        zoom(&mut app);
        assert!(!screen(&mut app, 100, 20).join("\n").contains("MAXIMIZED"));
    }
    #[test]
    fn panes_move_swap_and_flip_after_they_are_made() {
        // Two panes on top (a, b), one full-width pane below (c).
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let a = app.focused;
        app.split_editor(Axis::Vertical).unwrap();
        let c = app.focused;
        app.focused = a;
        app.split_editor(Axis::Horizontal).unwrap();
        let b = app.focused;
        app.focused = c;
        let area = Rect::new(0, 0, 120, 40);
        let at = |app: &mut App, id| {
            app.rectangles.clear();
            app.tiles.rectangles(area, &mut app.rectangles);
            app.rectangles.iter().find(|(i, _)| *i == id).unwrap().1
        };
        let prefix = |app: &mut App, code, modifiers| {
            press(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(app, code, modifiers);
        };
        // Nothing to its right: the bottom pane becomes a full-height right column.
        at(&mut app, c);
        prefix(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        let (ra, rb, rc) = (at(&mut app, a), at(&mut app, b), at(&mut app, c));
        assert_eq!(rc.height, area.height, "full height");
        assert!(rc.x > rb.x && rb.x > ra.x, "a | b | c");
        assert_eq!(app.focused, c, "the moved pane keeps focus");
        prefix(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert!(app.message.contains("Already there"));
        // With a pane on that side, they trade places (H J K L work too).
        prefix(&mut app, KeyCode::Char('H'), KeyModifiers::SHIFT);
        assert!(at(&mut app, c).x < at(&mut app, b).x, "a | c | b");
        // Ctrl-b Space: c's split with a turns from side by side to stacked.
        prefix(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        let (ra, rc) = (at(&mut app, a), at(&mut app, c));
        assert_eq!(ra.x, rc.x);
        assert!(rc.y > ra.y, "a above c");
        app.command("move up").unwrap();
        assert!(at(&mut app, c).y < at(&mut app, a).y, "c above a");
        assert!(app.command("move sideways").is_err());
        assert_eq!(app.panes.len(), 3);
    }
    #[test]
    fn terminals_open_below_or_beside_the_active_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.config.shell = Some("sh".into());
        let editor = app.focused;
        let area = Rect::new(0, 0, 120, 40);
        // Ctrl-b C: a shell to the right, full height.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('C'), KeyModifiers::SHIFT);
        assert_eq!(app.panes.len(), 2);
        let beside = app.focused;
        assert!(matches!(app.panes.get(&beside), Some(Pane::Terminal(_))));
        app.rectangles.clear();
        app.tiles.rectangles(area, &mut app.rectangles);
        let rect = |app: &App, id| app.rectangles.iter().find(|(i, _)| *i == id).unwrap().1;
        assert!(rect(&app, beside).x > rect(&app, editor).x);
        assert_eq!(rect(&app, beside).height, area.height);
        // Ctrl-b c: a shell below the focused one, same column.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
        let below = app.focused;
        app.rectangles.clear();
        app.tiles.rectangles(area, &mut app.rectangles);
        assert!(rect(&app, below).y > rect(&app, beside).y);
        assert_eq!(rect(&app, below).x, rect(&app, beside).x);
        // :term right does what Ctrl-b C does.
        app.command("term right").unwrap();
        let right = app.focused;
        app.rectangles.clear();
        app.tiles.rectangles(area, &mut app.rectangles);
        assert!(rect(&app, right).x > rect(&app, below).x);
        assert_eq!(rect(&app, right).y, rect(&app, below).y);
        assert!(app.command("term sideways").is_err());
        assert_eq!(app.panes.len(), 4);
    }
    #[test]
    fn missing_ai_binary_reports_error_without_losing_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        assert!(app
            .open_terminal(
                Some(("/nonexistent/raido-test-cli", Vec::new())),
                Some(Axis::Horizontal)
            )
            .is_err());
        assert_eq!(app.panes.len(), 1);
    }
    #[test]
    fn switching_all_themes_rebuilds_syntax_and_preserves_unsaved_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() { let number = 42; }\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        press(&mut app, KeyCode::Char('i'), KeyModifiers::NONE);
        app.event(Event::Paste("// unsaved\n".into()));
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let original = app.buffers[id].text.to_string();
        assert!(app.buffers[id].dirty);
        for (name, _) in config::BUNDLED_THEMES {
            app.syntax.prepare(id, &app.buffers[id], 10);
            assert!(!app.syntax.cache.is_empty());
            app.command(&format!("theme {name}")).unwrap();
            assert!(app.syntax.cache.is_empty());
            assert_eq!(app.config.theme, *name);
            let backend = ratatui::backend::TestBackend::new(100, 30);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
            assert_eq!(
                terminal.backend().buffer()[(99, 10)].bg,
                config::color(&app.theme.background)
            );
            assert_eq!(app.buffers[id].text.to_string(), original);
            assert!(app.buffers[id].dirty);
        }
        assert!(app.command("theme ../../bad").is_err());
        assert_eq!(app.config.theme, "github-dark");
    }
    #[test]
    fn git_pane_shows_every_branch_and_maximizes() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com"])
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&["checkout", "-q", "-b", "feature"]);
        git(&["commit", "-q", "--allow-empty", "-m", "feature work"]);
        git(&["checkout", "-q", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "main work"]);
        git(&["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
        git(&["tag", "v1.0"]);
        git(&["checkout", "-q", "-b", "topic", "HEAD~1"]);
        git(&["commit", "-q", "--allow-empty", "-m", "unmerged idea"]);
        git(&["checkout", "-q", "main"]);
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.command("git").unwrap();
        let git_id = app.focused;
        assert!(matches!(app.panes.get(&git_id), Some(Pane::Git(_))));
        for _ in 0..200 {
            app.tick();
            if let Some(Pane::Git(view)) = app.panes.get(&git_id) {
                if view.data.commits.len() == 5 && view.detail.is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                let row: String = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect();
                row.trim_end().to_string() + "\n"
            })
            .collect();
        for expected in [
            "feature",
            "topic",
            "merge feature",
            "unmerged idea",
            "v1.0",
            "◉ main",
        ] {
            assert!(screen.contains(expected), "missing {expected}");
        }
        // Selecting a branch jumps to its newest commit; Enter shows only it.
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        let Some(Pane::Git(view)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        let topic = view
            .data
            .branches
            .iter()
            .position(|b| b.name == "topic")
            .unwrap();
        for _ in 0..topic {
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        let Some(Pane::Git(view)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        assert_eq!(view.selected().unwrap().subject, "unmerged idea");
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let Some(Pane::Git(view)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        assert_eq!(view.only.as_deref(), Some("topic"));
        // Ctrl-b G maximizes and restores the focused Git pane, and refocuses it.
        let prefix_g = |app: &mut App| {
            press(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
            press(app, KeyCode::Char('G'), KeyModifiers::SHIFT);
        };
        prefix_g(&mut app);
        assert!(app.zoom);
        // Maximized, the selected commit's details have room.
        for _ in 0..200 {
            app.tick();
            if let Some(Pane::Git(view)) = app.panes.get(&git_id) {
                if view.data.commits.len() == 4 && view.detail.is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            screen.contains("Ada <ada@example.com>  20"),
            "commit details shown"
        );
        prefix_g(&mut app);
        assert!(!app.zoom);
        app.focused = 0;
        prefix_g(&mut app);
        assert_eq!(app.focused, git_id);
        assert_eq!(app.panes.len(), 2, "one Git pane, reused");
    }
    #[cfg(unix)]
    #[test]
    fn sessions_bring_back_panes_unsaved_text_and_terminal_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "saved line\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path.clone()), false).unwrap();
        press(&mut app, KeyCode::Char('A'), KeyModifiers::SHIFT);
        for c in " + unsaved".chars() {
            press(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.open_terminal(
            Some((
                "/bin/sh",
                vec!["-c".into(), "cd sub; echo shell-marker; exec sh".into()],
            )),
            Some(Axis::Vertical),
        )
        .unwrap();
        let shell = app.focused;
        for _ in 0..200 {
            app.tick();
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            if t.transcript().contains("shell-marker") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        app.focused = 0;
        app.save_session(false).unwrap();
        let mut leaves = Vec::new();
        app.tiles.leaves(&mut leaves);
        drop(app);

        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.restore_session();
        assert!(
            app.message.starts_with("Session restored"),
            "{}",
            app.message
        );
        let mut restored = Vec::new();
        app.tiles.leaves(&mut restored);
        assert_eq!(restored, leaves);
        assert_eq!(app.focused, 0);
        let buffer = app.active_buffer().unwrap();
        assert_eq!(buffer.line(0), "saved line + unsaved");
        assert!(buffer.dirty, "unsaved text stays unsaved");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved line\n");
        let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
            panic!("terminal pane restored")
        };
        assert!(t.transcript().contains("shell-marker"));
        assert!(t.transcript().contains("restored from your last session"));
        // The shell came back in the directory it had moved to.
        let mut cwd = None;
        for _ in 0..100 {
            cwd = t.cwd();
            if cwd.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(cwd) = cwd {
            assert!(cwd.ends_with("sub"), "{}", cwd.display());
        }
        let ignore = std::fs::read_to_string(dir.path().join(".raido/.gitignore")).unwrap();
        assert!(ignore.contains('*'));
    }
    #[cfg(unix)]
    #[test]
    fn claude_panes_follow_the_conversation_they_switched_to() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("claude-home");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        app.open_terminal(
            Some(("/bin/sh", vec!["-c".into(), "exec sleep 30".into()])),
            Some(Axis::Horizontal),
        )
        .unwrap();
        let pane = app.focused;
        let started = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let switched = "6e0449c2-4ca3-4fde-a7c1-23fda501b633";
        let Some(Pane::Terminal(t)) = app.panes.get_mut(&pane) else {
            unreachable!()
        };
        t.launch = Launch::Agent {
            name: "claude".into(),
            model: None,
            session: Some(started.into()),
        };
        let pid = t.pid().unwrap();
        let pinned = |app: &App| match app.session().panes.into_iter().find(|(id, _)| *id == pane) {
            Some((
                _,
                SavedPane::Terminal {
                    launch: Launch::Agent { session, .. },
                    ..
                },
            )) => session,
            _ => None,
        };
        let entry = |pid: u32, id: &str| {
            std::fs::write(
                home.join("sessions").join(format!("{pid}.json")),
                format!(r#"{{"pid": {pid}, "sessionId": "{id}", "kind": "interactive"}}"#),
            )
            .unwrap();
        };
        // No registry entry yet: the pane keeps the id it started with.
        app.follow_conversations(&home);
        assert_eq!(pinned(&app).as_deref(), Some(started));
        // You /resume'd another conversation in the pane: Claude's registry says so.
        entry(pid, switched);
        app.follow_conversations(&home);
        assert_eq!(pinned(&app).as_deref(), Some(switched));
        // ...and that is what the pane reopens next time.
        let (args, _, _) = agent_flags(
            "claude",
            true,
            pinned(&app).as_deref(),
            &|_| true,
            &String::new,
        );
        assert_eq!(args, ["--resume", switched]);
        // An entry left by another process, or a malformed id, is ignored.
        std::fs::write(
            home.join("sessions").join(format!("{pid}.json")),
            format!(r#"{{"pid": {}, "sessionId": "{started}"}}"#, pid + 1),
        )
        .unwrap();
        app.follow_conversations(&home);
        assert_eq!(pinned(&app).as_deref(), Some(switched));
        entry(pid, "not-an-id");
        app.follow_conversations(&home);
        assert_eq!(pinned(&app).as_deref(), Some(switched));
    }
    #[test]
    fn claude_panes_reopen_exactly_their_own_conversation() {
        let new_id = || "11111111-2222-4333-8444-555555555555".to_string();
        let yes = |_: &str| true;
        let no = |_: &str| false;
        let id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        // A new Claude pane is pinned to a fresh conversation id.
        let (args, replay, session) = agent_flags("claude", false, None, &no, &new_id);
        assert_eq!(args, ["--session-id", &new_id()]);
        assert!(replay);
        assert_eq!(session.as_deref(), Some(new_id().as_str()));
        // Restored: that exact conversation, never "the latest in this folder".
        let (args, replay, session) = agent_flags("claude", true, Some(id), &yes, &new_id);
        assert_eq!(args, ["--resume", id]);
        assert!(!replay, "Claude redraws the conversation itself");
        assert_eq!(session.as_deref(), Some(id));
        // Pinned but never used: start it under the same id, below the old output.
        let (args, replay, _) = agent_flags("claude", true, Some(id), &no, &new_id);
        assert_eq!(args, ["--session-id", id]);
        assert!(replay);
        // Saved before ids existed: ask, with the old output shown.
        let (args, replay, session) = agent_flags("claude", true, None, &no, &new_id);
        assert_eq!(args, ["--resume"]);
        assert!(replay && session.is_none());
        assert!(!args.contains(&"--continue".to_string()));
        // CLIs that can't be pinned reopen their latest conversation here.
        assert_eq!(
            agent_flags("codex", true, None, &no, &new_id).0,
            ["resume", "--last"]
        );
        assert!(agent_flags("codex", false, None, &no, &new_id).0.is_empty());
        let a = conversation_id();
        let b = conversation_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4", "version 4 UUID");
    }
    #[test]
    fn explorer_marks_changed_files_and_their_folders() {
        use crate::workspace::Mark;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Raido",
                    "-c",
                    "user.email=raido@example.com",
                    "-C",
                ])
                .arg(root)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "a").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("src/lib.rs"), "b").unwrap();
        std::fs::write(root.join("notes.md"), "").unwrap();
        let mut app = App::new(root.into(), None, false).unwrap();
        app.tree.expanded.insert(root.join("src"));
        app.tree.refresh().unwrap();
        let start = Instant::now();
        while app.git.changes.len() < 2 && start.elapsed() < Duration::from_secs(10) {
            app.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |name: &str| {
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect::<String>()
                })
                .position(|line| line.contains(name) && line.contains("│"))
                .map(|y| y as u16)
                .unwrap_or_else(|| panic!("{name} not in the explorer"))
        };
        // The badge sits at the right edge of the explorer, in the mark's color.
        let badge = |name: &str| {
            let y = row(name);
            let edge = (1..buffer.area.width)
                .find(|&x| buffer[(x, y)].symbol() == "│")
                .unwrap();
            let cell = &buffer[(edge - 2, y)];
            (cell.symbol().to_string(), cell.fg)
        };
        let theme = &app.theme;
        assert_eq!(
            badge("lib.rs"),
            ("M".into(), crate::ui::mark_color(Mark::Modified, theme))
        );
        assert_eq!(
            badge("notes.md"),
            ("U".into(), crate::ui::mark_color(Mark::Untracked, theme))
        );
        assert_eq!(badge("src").0, "●");
        assert_eq!(badge(".git").0, " ", "ignored paths are only dimmed");
    }
    #[test]
    fn raido_typed_in_a_terminal_opens_the_file_here() {
        use crate::remote::Reply;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("blob.bin"), [0u8, 1, 2]).unwrap();
        let mut app = App::new(root.clone(), None, false).unwrap();
        // Typed from a non-editor pane (the AI launcher stands in for a shell).
        let launcher = app.add_pane(Pane::Ai, Axis::Horizontal);
        let panes = app.panes.len();
        let reply = app.remote_open(&root.join("src/main.rs"));
        assert_eq!(reply, Reply::Opened("Opened main.rs in Raido".into()));
        assert_eq!(app.panes.len(), panes + 1, "a pane of its own");
        let Some(Pane::Editor(id)) = app.panes.get(&app.focused) else {
            panic!("the file is focused in an editor");
        };
        assert_eq!(app.buffers[*id].line(0), "fn main() {}");
        let editor = app.focused;
        // Again from the terminal: the pane showing it comes forward, nothing new.
        app.focused = launcher;
        app.remote_open(&root.join("src/main.rs"));
        assert_eq!((app.focused, app.panes.len()), (editor, panes + 1));
        // A file that doesn't exist yet opens as a new buffer, saved on :w.
        app.focused = launcher;
        app.remote_open(&root.join("xyz.txt"));
        assert_eq!(app.panes.len(), panes + 2);
        assert_eq!(
            app.active_buffer().unwrap().path,
            Some(root.join("xyz.txt"))
        );
        assert!(!root.join("xyz.txt").exists());
        // Folders of this project show in the explorer; others get their own Raido.
        assert!(matches!(
            app.remote_open(&root.join("src")),
            Reply::Opened(_)
        ));
        assert!(app.tree.expanded.contains(&root.join("src")));
        let other = tempfile::tempdir().unwrap();
        assert_eq!(app.remote_open(other.path()), Reply::Elsewhere);
        assert!(matches!(
            app.remote_open(&root.join("blob.bin")),
            Reply::Failed(_)
        ));
    }
    #[test]
    fn edited_lines_are_marked_until_staged() {
        use crate::quickdiff::Change;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Raido",
                    "-c",
                    "user.email=raido@example.com",
                    "-C",
                ])
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
        };
        let path = root.join("list.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let mut app = App::new(root.clone(), Some(path.clone()), false).unwrap();
        let id = app.buffer_id().unwrap();
        let wait = |app: &mut App, done: &dyn Fn(&App) -> bool| {
            let start = Instant::now();
            while !done(app) && start.elapsed() < Duration::from_secs(10) {
                app.tick();
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let marks = move |app: &App| {
            app.diffs
                .get(&id)
                .map(|d| d.marks.clone())
                .unwrap_or_default()
        };
        wait(&mut app, &|app| {
            app.diffs.get(&id).is_some_and(|d| d.computed_base())
        });
        assert!(
            marks(&app).iter().all(Option::is_none),
            "a clean file has no marks"
        );
        // Unsaved edits: change "two", add a line after "three", remove "five".
        let b = &mut app.buffers[id];
        let at = |b: &Buffer, row: usize, col: usize| b.text.line_to_char(row) + col;
        b.replace(at(b, 1, 0), at(b, 1, 3), "TWO");
        b.replace(at(b, 3, 0), at(b, 3, 0), "new\n");
        b.replace(at(b, 4, 4), at(b, 5, 4), "");
        assert_eq!(b.text.to_string(), "one\nTWO\nthree\nnew\nfour\n");
        app.tick();
        assert_eq!(
            marks(&app),
            [
                None,
                Some(Change::Modified),
                None,
                Some(Change::Added),
                Some(Change::Deleted),
            ]
        );
        // The gutter shows them; ]c walks from change to change.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let cell = |line: &str| {
            let y = (0..buffer.area.height)
                .find(|&y| {
                    let row: String = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol().to_string())
                        .collect();
                    row.contains(line)
                })
                .unwrap();
            let x = (0..buffer.area.width)
                .find(|&x| ["▌", "▁"].contains(&buffer[(x, y)].symbol()))
                .unwrap_or_else(|| panic!("no marker beside {line}"));
            (buffer[(x, y)].symbol().to_string(), buffer[(x, y)].fg)
        };
        let theme = &app.theme;
        assert_eq!(
            cell("TWO"),
            ("▌".into(), crate::ui::change_color(Change::Modified, theme))
        );
        assert_eq!(
            cell("new"),
            ("▌".into(), crate::ui::change_color(Change::Added, theme))
        );
        assert_eq!(
            cell("four"),
            ("▁".into(), crate::ui::change_color(Change::Deleted, theme))
        );
        app.buffers[id].row = 0;
        app.sidebar_focus = false;
        press(&mut app, KeyCode::Char(']'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(app.buffers[id].row, 1);
        press(&mut app, KeyCode::Char(']'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(app.buffers[id].row, 3);
        // Saved and staged: Git's index matches, so the marks go away.
        app.save(None).unwrap();
        git(&["add", "list.txt"]);
        app.update_diffs(true);
        wait(&mut app, &|app| marks(app).iter().all(Option::is_none));
        assert!(
            marks(&app).iter().all(Option::is_none),
            "staged changes are clean"
        );
    }
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn copy_view_shows_only_the_focused_panes_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "first line\nsecond line\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let normal = screen(&mut app, 100, 24).join("\n");
        assert!(normal.contains("EXPLORER") && normal.contains("RAIDO"));
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert!(app.copy_view.is_some());
        let bare = screen(&mut app, 100, 24);
        // Text from the first column, no numbers, borders, sidebar or status lines.
        assert!(bare[0].starts_with("first line"), "{:?}", bare[0]);
        assert!(bare[1].starts_with("second line"));
        let all = bare.join("\n");
        for chrome in ["EXPLORER", "GIT / TREE", "RAIDO", "NORMAL", "│", "╭"] {
            assert!(!all.contains(chrome), "{chrome} shows in copy view");
        }
        // Ctrl-b Z again returns.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert!(app.copy_view.is_none());
        assert!(screen(&mut app, 100, 24).join("\n").contains("EXPLORER"));
    }

    #[cfg(unix)]
    #[test]
    fn a_finished_ai_prompt_is_announced() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let editor = app.focused;
        // A fake agent: waits for a prompt, shows a spinner for 2 s, answers.
        let script = "read prompt; i=0; while [ $i -lt 20 ]; do printf .; sleep 0.1; \
                      i=$((i+1)); done; echo answer; exec sleep 30";
        app.open_terminal(
            Some(("/bin/sh", vec!["-c".into(), script.into()])),
            Some(Axis::Horizontal),
        )
        .unwrap();
        let agent = app.focused;
        let Some(Pane::Terminal(t)) = app.panes.get_mut(&agent) else {
            unreachable!()
        };
        t.launch = Launch::Agent {
            name: "fake".into(),
            model: None,
            session: None,
        };
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.focused = editor;
        let start = Instant::now();
        while !app.message.contains("finished") && start.elapsed() < Duration::from_secs(10) {
            app.tick();
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(app.message.contains("finished"), "{}", app.message);
        assert!(
            start.elapsed() >= Duration::from_secs(4),
            "not before it went quiet"
        );
    }

    #[cfg(unix)]
    #[test]
    fn what_an_agent_changes_is_reviewed_kept_and_undone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com"])
                .args(["-c", "commit.gpgsign=false", "-C"])
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("app.py"), "print(1)\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        let mut app = App::new(root.clone(), None, false).unwrap();
        let editor = app.focused;
        // A fake agent: takes a prompt, thinks, edits two files, works a while.
        let script = "read prompt; sleep 1; printf 'print(2)\\n' > app.py; \
                      printf 'note\\n' > notes.md; i=0; while [ $i -lt 20 ]; do \
                      printf .; sleep 0.1; i=$((i+1)); done; echo done; exec sleep 30";
        app.open_terminal(
            Some(("/bin/sh", vec!["-c".into(), script.into()])),
            Some(Axis::Horizontal),
        )
        .unwrap();
        let agent = app.focused;
        let Some(Pane::Terminal(t)) = app.panes.get_mut(&agent) else {
            unreachable!()
        };
        t.launch = Launch::Agent {
            name: "fake".into(),
            model: None,
            session: None,
        };
        let title = t.title.clone();
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.focused = editor;
        let start = Instant::now();
        while !app.message.contains("finished") && start.elapsed() < Duration::from_secs(15) {
            app.tick();
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(app.message.contains("2 files to review"), "{}", app.message);
        assert_eq!(app.changes.by(&title), 2);
        // The agent's pane says so too.
        let shown = screen(&mut app, 140, 30).join("\n");
        assert!(shown.contains("✎ 2 files"), "{shown}");

        // Ctrl-b v opens the review: files, hunks, the agent's lines.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('v'), KeyModifiers::NONE);
        assert!(matches!(
            app.panes.get(&app.focused),
            Some(Pane::Changes(_))
        ));
        let shown = screen(&mut app, 140, 40).join("\n");
        for expected in [
            "CHANGES · 2 files",
            "app.py",
            "notes.md",
            "new file",
            "print(2)",
        ] {
            assert!(shown.contains(expected), "{expected}: {shown}");
        }
        // x on app.py's change puts it back; Y keeps all of notes.md.
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            std::fs::read_to_string(root.join("app.py")).unwrap(),
            "print(1)\n"
        );
        assert!(app.message.contains("Undid"), "{}", app.message);
        // Undoing a whole file asks first; any key but y keeps it.
        press(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert!(
            app.message.contains("Undo every agent change to notes.md"),
            "{}",
            app.message
        );
        let shown = screen(&mut app, 140, 40).join("\n");
        assert!(shown.contains("y undo it"), "{shown}");
        press(&mut app, KeyCode::Char('n'), KeyModifiers::NONE);
        assert!(root.join("notes.md").exists());
        press(&mut app, KeyCode::Char('Y'), KeyModifiers::NONE);
        assert!(app.changes.files.is_empty());
        assert!(root.join("notes.md").exists());
        let shown = screen(&mut app, 140, 40).join("\n");
        assert!(shown.contains("nothing to review"), "{shown}");
    }

    #[cfg(unix)]
    #[test]
    fn mouse_wheel_scrolls_and_clicks_focus_what_is_under_the_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.txt");
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, format!("\tx = 1\n{text}")).unwrap();
        std::fs::write(dir.path().join("other.txt"), "other\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path), false).unwrap();
        let id = app.buffer_id().unwrap();
        let editor = app.focused;
        let mouse = |app: &mut App, kind, column, row| {
            app.event(Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }));
        };
        let script = "seq 1 200; echo done; exec cat".to_string();
        app.open_terminal(
            Some(("/bin/sh", vec!["-c".into(), script])),
            Some(Axis::Horizontal),
        )
        .unwrap();
        let shell = app.focused;
        for _ in 0..300 {
            app.tick();
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            if t.transcript().contains("done") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        screen(&mut app, 120, 30);
        let rect = |app: &App, id| app.rectangles.iter().find(|(i, _)| *i == id).unwrap().1;
        let (e, t) = (rect(&app, editor), rect(&app, shell));
        // The wheel over the editor scrolls it, though the shell has focus; the
        // cursor stays on screen.
        mouse(&mut app, MouseEventKind::ScrollDown, e.x + 10, e.y + 5);
        mouse(&mut app, MouseEventKind::ScrollDown, e.x + 10, e.y + 5);
        assert_eq!(app.buffers[id].scroll, 6);
        assert_eq!(app.buffers[id].row, 6);
        assert_eq!(app.focused, shell, "the wheel doesn't move focus");
        mouse(&mut app, MouseEventKind::ScrollUp, e.x + 10, e.y + 5);
        assert_eq!(app.buffers[id].scroll, 3);
        // Over the shell: its history, three lines at a time.
        let back = |app: &App| {
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            t.parser.lock().unwrap().screen().scrollback()
        };
        mouse(&mut app, MouseEventKind::ScrollUp, t.x + 5, t.y + 5);
        assert_eq!(back(&app), 3);
        mouse(&mut app, MouseEventKind::ScrollDown, t.x + 5, t.y + 5);
        assert_eq!(back(&app), 0);
        // A program that tracks the mouse gets the wheel (and PgUp) itself.
        if let Some(Pane::Terminal(t)) = app.panes.get_mut(&shell) {
            t.parser.lock().unwrap().process(b"\x1b[?1000h\x1b[?1006h");
        }
        mouse(&mut app, MouseEventKind::ScrollUp, t.x + 5, t.y + 3);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(back(&app), 0, "nothing scrolled on Raido's side");
        let mut echoed = false;
        for _ in 0..200 {
            app.tick();
            let Some(Pane::Terminal(term)) = app.panes.get(&shell) else {
                unreachable!()
            };
            if term.transcript().contains("[<64;5;3M") {
                echoed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            echoed,
            "the program received the wheel at its own row and column"
        );
        // A click focuses the editor and puts the cursor on the character (the
        // tab on line 1 is drawn four columns wide).
        app.buffers[id].scroll = 0;
        screen(&mut app, 120, 30);
        let gutter = 5;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            e.x + 1 + gutter + 4,
            e.y + 1,
        );
        assert_eq!(app.focused, editor);
        assert_eq!(
            (app.buffers[id].row, app.buffers[id].col),
            (0, 1),
            "on the x"
        );
        // A click on an explorer entry opens it.
        let rows = app.explorer_rows;
        let other = app
            .tree
            .entries
            .iter()
            .position(|e| e.path.ends_with("other.txt"))
            .unwrap();
        let y = rows.y + (other - app.tree.offset) as u16;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 3,
            y,
        );
        assert!(app.buffers[app.buffer_id().unwrap()]
            .name()
            .contains("other.txt"));
        // Copy view hands the mouse back to the terminal for selecting text.
        assert!(app.mouse_wanted());
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('Z'), KeyModifiers::SHIFT);
        assert!(!app.mouse_wanted());
    }

    #[cfg(unix)]
    #[test]
    fn page_keys_scroll_terminal_history_in_half_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let script = "seq 1 200; echo done; exec sleep 30".to_string();
        app.open_terminal(
            Some(("/bin/sh", vec!["-c".into(), script])),
            Some(Axis::Vertical),
        )
        .unwrap();
        let shell = app.focused;
        let back = |app: &App| {
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            t.parser.lock().unwrap().screen().scrollback()
        };
        screen(&mut app, 120, 30);
        for _ in 0..300 {
            app.tick();
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            if t.transcript().contains("done") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let rows = {
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            usize::from(t.parser.lock().unwrap().screen().size().0)
        };
        // Arrows belong to the program until the pane is scrolled back.
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(back(&app), 0);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(back(&app), rows / 2, "half a page, not a whole one");
        assert!(screen(&mut app, 120, 30).join("\n").contains("End returns"));
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(back(&app), rows / 2 + 1, "arrows step one line");
        press(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(back(&app), 1);
        press(&mut app, KeyCode::Home, KeyModifiers::NONE);
        assert!(back(&app) > 150, "Home goes to the oldest line");
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(back(&app), 0);
        // Ctrl-b PgUp does the same; typing returns to the prompt.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(back(&app), rows / 2);
        press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(back(&app), 0);
    }

    #[cfg(unix)]
    #[test]
    fn copy_mode_selects_and_copies_from_a_terminal_pane() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(dir.path().into(), None, false).unwrap();
        let long = format!("long-{}", "x".repeat(150));
        let script = format!("printf 'alpha\\n{long}\\nomega\\n'; exec sleep 30");
        app.open_terminal(
            Some(("/bin/sh", vec!["-c".into(), script])),
            Some(Axis::Vertical),
        )
        .unwrap();
        let shell = app.focused;
        for _ in 0..300 {
            app.tick();
            let Some(Pane::Terminal(t)) = app.panes.get(&shell) else {
                unreachable!()
            };
            if t.transcript().contains("omega") {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        screen(&mut app, 120, 30);
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('['), KeyModifiers::NONE);
        assert!(app.copy.is_some(), "Ctrl-b [ enters copy mode");
        let shown = screen(&mut app, 120, 30).join("\n");
        assert!(shown.contains("COPY"));
        let keys = |app: &mut App, keys: &str| {
            for c in keys.chars() {
                let code = if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                };
                press(app, code, KeyModifiers::NONE);
            }
        };
        // From the long line (wrapped by the terminal) down to "omega", whole lines.
        keys(&mut app, "?long\nV/omega\ny");
        assert!(app.copy.is_none(), "y copies and leaves");
        let (copied, _) = app.clipboard.paste();
        assert_eq!(copied, format!("{long}\nomega"));
        assert!(app.message.contains("Copied 2 lines"));
        // q leaves without copying; editors explain where their copy is.
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('['), KeyModifiers::NONE);
        keys(&mut app, "q");
        assert!(app.copy.is_none());
        app.focused = 0;
        press(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('['), KeyModifiers::NONE);
        assert!(app.copy.is_none() && app.message.contains("v / V select"));
    }
    #[cfg(unix)]
    #[test]
    fn go_files_are_organized_and_formatted_on_save() {
        if crate::platform::which("python3").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.go");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let mut app = App::new(dir.path().into(), Some(path.clone()), false).unwrap();
        let mut servers = BTreeMap::new();
        servers.insert(
            "go".to_string(),
            config::CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), lsp::FAKE_SERVER.into()],
            },
        );
        app.lsp = Lsp::new(dir.path().into(), true, servers);
        app.config.lsp_enabled = true;
        app.config.format_on_save = vec!["go".into()];
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        // The scripted server adds an import after line 1, then formats `fn` as `pub fn`.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "pub fn main() {}\nuse std::fmt;\n"
        );
        assert!(
            app.message
                .contains("imports organized and formatted by python3"),
            "{}",
            app.message
        );
        // Languages not listed save exactly as typed.
        app.config.format_on_save.clear();
        let id = app.buffer_id().unwrap();
        app.buffers[id].replace(0, 0, "x");
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("xpub fn"));
    }

    /// The real thing, when gopls is installed: goimports and gofmt on save.
    #[cfg(unix)]
    #[test]
    fn gopls_tidies_go_code_on_save() {
        if crate::platform::which("gopls").is_none() || crate::platform::which("go").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("go.mod"), "module example.com/tidy\n\ngo 1.21\n").unwrap();
        let path = root.join("main.go");
        let messy = "package main\n\nimport (\n\t\"os\"\n\t\"fmt\"\n)\n\nfunc main() {\nx:=strings.ToUpper(\"hi\")\n    fmt.Println(x)\n}\n";
        std::fs::write(&path, messy).unwrap();
        let mut app = App::new(root.clone(), Some(path.clone()), false).unwrap();
        app.lsp = Lsp::new(root.clone(), true, BTreeMap::new());
        app.config.lsp_enabled = true;
        app.config.format_on_save = vec!["go".into()];
        // Let gopls start and load the module (the first save gives it two seconds).
        let start = Instant::now();
        while app.lsp.status(
            app.buffer_id().unwrap(),
            &app.buffers[app.buffer_id().unwrap()],
        ) != Some("gopls".into())
            && start.elapsed() < Duration::from_secs(20)
        {
            app.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
        press(&mut app, KeyCode::Char('s'), KeyModifiers::CONTROL);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            saved,
            "package main\n\nimport (\n\t\"fmt\"\n\t\"strings\"\n)\n\nfunc main() {\n\tx := strings.ToUpper(\"hi\")\n\tfmt.Println(x)\n}\n",
            "{}",
            app.message
        );
    }
    #[cfg(unix)]
    #[test]
    fn enter_on_a_commit_shows_its_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Ada",
                    "-c",
                    "user.email=ada@example.com",
                    "-C",
                ])
                .arg(&root)
                .args(args)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        std::fs::write(
            root.join("main.go"),
            "package main\n\nfunc main() {\n\tprintln(1)\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "keep\ndrop\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(
            root.join("main.go"),
            "package main\n\nfunc main() {\n\tprintln(2)\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("notes.txt"), "keep\n").unwrap();
        git(&["commit", "-qam", "second: print two"]);
        let mut app = App::new(root.clone(), None, false).unwrap();
        app.git_pane().unwrap();
        let git_id = app.focused;
        let view = |app: &App| match app.panes.get(&git_id) {
            Some(Pane::Git(view)) => view.data.commits.len(),
            _ => 0,
        };
        for _ in 0..300 {
            app.tick();
            if view(&app) == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // The newest commit is selected; Enter opens its diff (maximized for room).
        app.zoom = true;
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        for _ in 0..300 {
            app.tick();
            if let Some(Pane::Git(v)) = app.panes.get(&git_id) {
                if v.diff.as_ref().is_some_and(|d| !d.loading) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let lines = screen(&mut app, 120, 40);
        let all = lines.join("\n");
        assert!(all.contains("second: print two"), "the message");
        assert!(
            all.contains("▌ main.go") && all.contains("▌ notes.txt"),
            "file headings"
        );
        assert!(all.contains("+    println(2)"), "added line, tab expanded");
        assert!(all.contains("-    println(1)"), "removed line");
        assert!(all.contains("-drop"));
        assert!(all.contains("2 files · +1 −2"), "totals in the title");
        assert!(!lines.iter().any(|l| l.trim_matches(['│', ' ']) == "---"));
        // Numbers: line 4 changed in both versions.
        let added = lines
            .iter()
            .find(|l| l.contains("+    println(2)"))
            .unwrap();
        assert!(added.contains("   4 +"), "{added}");
        // Esc goes back to the graph (n / N between files is covered in gitgraph).
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let Some(Pane::Git(v)) = app.panes.get(&git_id) else {
            unreachable!()
        };
        assert!(v.diff.is_none());
        assert!(screen(&mut app, 120, 40)
            .join("\n")
            .contains("second: print two"));
    }
}

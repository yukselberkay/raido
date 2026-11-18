//! A small Language Server Protocol client: diagnostics, hover, go-to-definition,
//! completion and formatting for Rust, Go, Python, C and C++, TypeScript and
//! JavaScript, shell scripts, YAML
//! and JSON (checked against the schemas published for them), and Dockerfiles and
//! Docker Compose files.
//!
//! Each server runs as a child process speaking JSON-RPC over stdio. Reader and
//! writer threads keep the UI loop non-blocking; `Lsp::poll` drains replies on
//! every tick. Documents are synced in full (simple and accepted by every server).
use crate::{config::CommandPlugin, editor::Buffer};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// Debounce for sending edits to the server while typing.
const SYNC_DELAY: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Hover,
    Definition,
    Completion,
    Format,
    /// The `source.organizeImports` code action (gopls: goimports).
    OrganizeImports,
    /// `completionItem/resolve` for an accepted completion: the import it needs.
    Resolve,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub row: usize,
    /// Char columns on `row`; `end` is clamped to the line for multi-line ranges.
    pub start: usize,
    pub end: usize,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub label: String,
    pub detail: String,
    pub insert: String,
    /// Start column of the server's replacement range, when it gave one.
    pub start: Option<usize>,
    filter: String,
    /// Edits that come with it (`additionalTextEdits`): the import it needs, as char
    /// ranges of the buffer it was asked for in.
    pub extra: Vec<(usize, usize, String)>,
    /// The item as the server sent it, when its import comes only once it's asked
    /// for (`completionItem/resolve`, TypeScript's server).
    pub resolve: Option<Value>,
}

impl Completion {
    /// A suggestion that isn't from a server: a keyword, or a word from your files.
    pub fn word(label: &str, detail: &str) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            insert: label.into(),
            start: None,
            filter: label.into(),
            extra: Vec::new(),
            resolve: None,
        }
    }
    pub fn matches(&self, prefix: &str) -> bool {
        let prefix = prefix.to_lowercase();
        self.filter.to_lowercase().starts_with(&prefix)
            || (prefix.len() > 1 && self.filter.to_lowercase().contains(&prefix))
    }
}

pub enum Event {
    Hover(String),
    Definition {
        path: PathBuf,
        line: usize,
        character: usize,
        utf32: bool,
    },
    Completion {
        buffer: usize,
        row: usize,
        items: Vec<Completion>,
    },
    Format {
        buffer: usize,
        edits: Vec<(usize, usize, String)>,
    },
    /// Edits that organize the imports of `buffer`.
    Organized {
        buffer: usize,
        edits: Vec<(usize, usize, String)>,
    },
    /// The import an accepted completion needs, once the server said which.
    Imports {
        buffer: usize,
        edits: Vec<(usize, usize, String)>,
    },
    Notice(String),
    Error(String),
}

struct Language {
    key: &'static str,
    id: &'static str,
}

/// The language key (`go`, `rust`, `python`, `c`, `shell`, `docker`) of a buffer
/// Raido has a server for.
pub fn language_key(buffer: &Buffer) -> Option<&'static str> {
    language_of(buffer).map(|l| l.key)
}

fn language_of(buffer: &Buffer) -> Option<Language> {
    language(
        buffer.path.as_deref()?,
        &crate::language::first_line(buffer),
    )
}

/// The language of the file at `path` that starts with `first_line`.
fn language(path: &Path, first_line: &str) -> Option<Language> {
    // sh, bash and ksh scripts, by their name or `#!` line; zsh and fish are
    // languages of their own to the Bash server and ShellCheck.
    if let Some(shell) = crate::language::shell(Some(path), first_line) {
        return shell.checked().then_some(Language {
            key: "shell",
            id: "shellscript",
        });
    }
    // JSON files without an extension, and ones that allow comments (JSON with
    // Comments, as VS Code reads them).
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(name.as_str(), ".babelrc" | ".swcrc" | ".jshintrc") {
        return Some(Language {
            key: "json",
            id: "jsonc",
        });
    }
    // One server for both: Docker's own.
    if crate::language::is_dockerfile(path) {
        return Some(Language {
            key: "docker",
            id: "dockerfile",
        });
    }
    if crate::language::is_compose(path) {
        return Some(Language {
            key: "docker",
            id: "dockercompose",
        });
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let (key, id) = match ext.as_str() {
        "rs" => ("rust", "rust"),
        "go" => ("go", "go"),
        "py" | "pyi" => ("python", "python"),
        "c" | "h" => ("c", "c"),
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" => ("c", "cpp"),
        // One server for both, told which as VS Code tells it.
        "ts" | "mts" | "cts" => ("typescript", "typescript"),
        "tsx" => ("typescript", "typescriptreact"),
        "js" | "mjs" | "cjs" => ("typescript", "javascript"),
        "jsx" => ("typescript", "javascriptreact"),
        "yaml" | "yml" => ("yaml", "yaml"),
        "json" if json_with_comments(path, &name) => ("json", "jsonc"),
        "json" => ("json", "json"),
        "jsonc" | "code-workspace" => ("json", "jsonc"),
        _ => return None,
    };
    Some(Language { key, id })
}

/// JSON files that allow comments and trailing commas, as VS Code reads them: the
/// TypeScript and JavaScript project files, Babel's and ESLint's, dev containers,
/// and VS Code's own settings.
fn json_with_comments(path: &Path, name: &str) -> bool {
    let tool = ["tsconfig", "jsconfig"]
        .iter()
        .any(|kind| name == format!("{kind}.json") || name.starts_with(&format!("{kind}.")));
    tool || matches!(
        name,
        ".eslintrc.json"
            | ".babelrc.json"
            | "devcontainer.json"
            | ".devcontainer.json"
            | "typedoc.json"
            | "tslint.json"
            | "api-extractor.json"
    ) || path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|folder| folder == ".vscode")
}

/// Default servers per language, first one found on PATH wins.
fn defaults(key: &str) -> &'static [(&'static str, &'static [&'static str])] {
    match key {
        "rust" => &[("rust-analyzer", &[])],
        "go" => &[("gopls", &[])],
        "python" => &[
            ("pyright-langserver", &["--stdio"]),
            ("basedpyright-langserver", &["--stdio"]),
            ("pylsp", &[]),
            ("jedi-language-server", &[]),
        ],
        "c" => &[("clangd", &[])],
        "typescript" => &[("typescript-language-server", &["--stdio"])],
        "shell" => &[("bash-language-server", &["start"])],
        "yaml" => &[("yaml-language-server", &["--stdio"])],
        // Arch's package also calls it vscode-json-languageserver.
        "json" => &[
            ("vscode-json-language-server", &["--stdio"]),
            ("vscode-json-languageserver", &["--stdio"]),
        ],
        "docker" => &[("docker-language-server", &["start", "--stdio"])],
        _ => &[],
    }
}

fn install_hint(key: &str) -> &'static str {
    match key {
        "rust" => "rustup component add rust-analyzer",
        "go" => "go install golang.org/x/tools/gopls@latest",
        "python" => "npm install -g pyright  (or: pip install python-lsp-server)",
        "c" => "install clangd (Arch: pacman -S clang · Debian/Ubuntu: apt install clangd · macOS: xcode-select --install)",
        "typescript" => "npm install -g typescript-language-server typescript@6  (it drives TypeScript 6's tsserver; TypeScript 7 has none)",
        "yaml" => "npm install -g yaml-language-server  (or: brew / pacman install yaml-language-server)",
        "json" => "npm install -g vscode-langservers-extracted  (or: brew install vscode-langservers-extracted · pacman -S vscode-json-languageserver)",
        "shell" => "npm install -g bash-language-server, with shellcheck and shfmt for its warnings and :format (brew / apt / dnf / pacman install shellcheck shfmt)",
        "docker" => "go install github.com/docker/docker-language-server/cmd/docker-language-server@latest  (or a release binary from github.com/docker/docker-language-server)",
        _ => "see the README",
    }
}

/// Why a server stopped. rustup installs a `rust-analyzer` proxy that fails until the
/// component is added; say how to add it rather than only echoing rustup.
fn stopped(program: &str, key: &str, detail: &str) -> String {
    if detail.contains("Unknown binary") || detail.contains("is not installed") {
        format!(
            "{program} is not installed for this toolchain · install: {} · then :lsp restart",
            install_hint(key)
        )
    } else {
        format!("{program} stopped: {detail} · :lsp restart retries")
    }
}

pub(crate) fn on_path(program: &str) -> bool {
    crate::platform::which(program).is_some()
}

/// Folders a server installed the usual way can be in without being on PATH: `go
/// install`'s for gopls, shfmt and Docker's server; for clangd, Homebrew's LLVM and Apple's Command Line Tools
/// on macOS, and the LLVM installer's on Windows. The install scripts rely on these.
fn off_path(program: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match program {
        "gopls" | "shfmt" | "docker-language-server" => {
            dirs.extend(std::env::var_os("GOBIN").map(PathBuf::from));
            if let Some(gopath) = std::env::var_os("GOPATH") {
                dirs.extend(std::env::split_paths(&gopath).next().map(|p| p.join("bin")));
            }
            dirs.extend(
                directories::BaseDirs::new().map(|d| d.home_dir().join("go").join("bin")),
            );
        }
        "clangd" if cfg!(target_os = "macos") => dirs.extend(
            [
                "/opt/homebrew/opt/llvm/bin",
                "/usr/local/opt/llvm/bin",
                "/Library/Developer/CommandLineTools/usr/bin",
                "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin",
            ]
            .map(PathBuf::from),
        ),
        "clangd" if cfg!(windows) => dirs.extend(
            std::env::var_os("ProgramFiles").map(|p| PathBuf::from(p).join("LLVM").join("bin")),
        ),
        _ => {}
    }
    dirs
}

/// How to start `program`: its name when it's on PATH, else its full path in one of
/// the [`off_path`] folders.
fn locate(program: &str) -> Option<String> {
    locate_in(program, &off_path(program))
}

fn locate_in(program: &str, dirs: &[PathBuf]) -> Option<String> {
    if on_path(program) {
        return Some(program.to_string());
    }
    dirs.iter().find_map(|dir| {
        let found = crate::platform::which(&dir.join(program).to_string_lossy())?;
        Some(found.to_string_lossy().into_owned())
    })
}

/// A file's indentation as servers take it: the size of a level, and whether it's
/// spaces (else tabs).
fn indentation(buffer: &Buffer, tab_width: usize) -> (usize, bool) {
    match crate::language::Rules::new(buffer, tab_width).unit.as_str() {
        "\t" => (tab_width, false),
        unit => (unit.len(), true),
    }
}

/// The part of `settings` a server asks for by name (`yaml`, `http.proxy`), or all
/// of them; null for one Raido doesn't set.
fn section(settings: &Value, name: Option<&str>) -> Value {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return settings.clone();
    };
    name.split('.')
        .try_fold(settings, |value, part| value.get(part))
        .cloned()
        .unwrap_or(Value::Null)
}

/// A Kubernetes manifest: a YAML file with top-level `apiVersion:` and `kind:` keys
/// (a kustomization has a schema of its own, by its name).
fn is_kubernetes(path: &Path, buffer: &Buffer) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.starts_with("kustomization.") {
        return false;
    }
    let (mut version, mut kind) = (false, false);
    for line in buffer.text.lines().take(400) {
        let line = line.chars().take(20).collect::<String>();
        version |= line.starts_with("apiVersion:");
        kind |= line.starts_with("kind:");
        if version && kind {
            return true;
        }
    }
    false
}

/// The schemas Raido knows the JSON server needs most, for when SchemaStore's
/// catalog can't be had.
fn known_json_schemas() -> Value {
    json!([
        { "fileMatch": ["package.json"], "url": "https://json.schemastore.org/package.json" },
        { "fileMatch": ["tsconfig.json", "tsconfig.*.json"], "url": "https://json.schemastore.org/tsconfig.json" },
        { "fileMatch": ["jsconfig.json", "jsconfig.*.json"], "url": "https://json.schemastore.org/jsconfig.json" },
    ])
}

/// Where SchemaStore lists the schemas published for each kind of file.
const SCHEMASTORE: &str = "https://www.schemastore.org/api/json/catalog.json";

/// SchemaStore's catalog as the JSON server takes it, kept in `cache` for a week and
/// fetched with curl when older (an old one serves while the new can't be had).
fn schemastore_catalog(cache: &Path) -> Option<Value> {
    const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);
    let file = cache.join("schemastore-catalog.json");
    let fresh = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < WEEK);
    if !fresh && std::fs::create_dir_all(cache).is_ok() {
        let part = cache.join("schemastore-catalog.json.part");
        let fetched = Command::new("curl")
            .args(["-fsSL", "--max-time", "30", "-o"])
            .arg(&part)
            .arg(SCHEMASTORE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if fetched {
            let _ = std::fs::rename(&part, &file);
        }
    }
    let catalog: Value = serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok()?;
    let schemas = catalog_schemas(&catalog);
    (!schemas.as_array().is_some_and(Vec::is_empty)).then_some(schemas)
}

/// The catalog's schemas that name the files they're for, as the JSON server's
/// `json.schemas`: `{ fileMatch, url }`.
fn catalog_schemas(catalog: &Value) -> Value {
    Value::Array(
        catalog["schemas"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|schema| {
                let url = schema["url"].as_str()?;
                let files = schema["fileMatch"].as_array().filter(|f| !f.is_empty())?;
                Some(json!({ "fileMatch": files, "url": url }))
            })
            .collect(),
    )
}

pub fn uri(path: &Path) -> String {
    file_uri(&path.to_string_lossy(), cfg!(windows))
}

/// `/a b/c.rs` → `file:///a%20b/c.rs`; on Windows `C:\x\y.rs` → `file:///C:/x/y.rs`.
fn file_uri(path: &str, windows: bool) -> String {
    let path = if windows {
        let forward = path.replace('\\', "/");
        if forward.starts_with('/') {
            forward
        } else {
            format!("/{forward}")
        }
    } else {
        path.to_string()
    };
    let mut out = String::from("file://");
    for (i, byte) in path.bytes().enumerate() {
        // Keep the drive colon readable: file:///C:/…
        let drive_colon = windows && i == 2 && byte == b':';
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) || drive_colon {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    path_from_file_uri(uri, cfg!(windows))
}

fn path_from_file_uri(uri: &str, windows: bool) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&encoded[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    let mut path = String::from_utf8(out).ok()?;
    if windows {
        // `/C:/x/y.rs` → `C:\x\y.rs`
        if path.as_bytes().get(2) == Some(&b':') {
            path.remove(0);
        }
        path = path.replace('/', "\\");
    }
    Some(PathBuf::from(path))
}

/// Char column → LSP `character` (UTF-16 units unless the server chose UTF-32).
pub fn encode(line: &str, col: usize, utf32: bool) -> usize {
    let chars = line.chars().take(col);
    if utf32 {
        chars.count()
    } else {
        chars.map(char::len_utf16).sum()
    }
}

/// LSP `character` → char column.
pub fn decode(line: &str, character: usize, utf32: bool) -> usize {
    if utf32 {
        return character.min(line.chars().count());
    }
    let mut units = 0;
    for (i, c) in line.chars().enumerate() {
        if units >= character {
            return i;
        }
        units += c.len_utf16();
    }
    line.chars().count()
}

enum Incoming {
    Message(Value),
    Closed(String),
}

struct Client {
    program: String,
    child: Child,
    /// Registrations for file changes (Pyright makes them): while any is held,
    /// Raido reports the changes it notices.
    watching: HashSet<String>,
    /// For Python: the environment the server runs on and what changes in it.
    python: Option<Python>,
    send: mpsc::Sender<Vec<u8>>,
    receive: mpsc::Receiver<Incoming>,
    next_id: u64,
    ready: bool,
    queue: Vec<Value>,
    utf32: bool,
    triggers: Vec<String>,
    /// The server fills in a completion's details when asked (`resolveProvider`).
    resolves: bool,
    pending: HashMap<u64, (Kind, usize)>,
}

struct Python {
    /// The virtual environment, or none: the Python on PATH.
    venv: Option<PathBuf>,
    news: mpsc::Receiver<crate::pyenv::News>,
    /// The watcher behind `news` stops once this is dropped with the client.
    _alive: Arc<()>,
}

impl Client {
    /// Start `program` (found as `command`); a Python server on `venv`, as if it
    /// were activated.
    fn spawn(
        program: &str,
        command: &str,
        args: &[String],
        root: &Path,
        venv: Option<&Path>,
    ) -> Result<Self> {
        let mut command = Command::new(crate::platform::resolve(command));
        command
            .args(args)
            .current_dir(crate::platform::startable(root))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(venv) = venv {
            command
                .envs(crate::pyenv::variables(venv))
                .env_remove("PYTHONHOME");
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Start {program}"))?;
        let mut stdin = child.stdin.take().context("server stdin")?;
        let stdout = child.stdout.take().context("server stdout")?;
        let stderr = child.stderr.take().context("server stderr")?;
        let (send, outbox) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            for bytes in outbox {
                if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        // Keep the last stderr line to explain a crash.
        let last_error = Arc::new(Mutex::new(String::new()));
        let sink = last_error.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    if let Ok(mut last) = sink.lock() {
                        *last = line.trim().chars().take(200).collect();
                    }
                }
            }
        });
        let (inbox, receive) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_message(&mut reader) {
                if inbox.send(Incoming::Message(message)).is_err() {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(50));
            let reason = last_error.lock().map(|s| s.clone()).unwrap_or_default();
            let _ = inbox.send(Incoming::Closed(reason));
        });
        Ok(Self {
            program: program.into(),
            child,
            watching: HashSet::new(),
            python: None,
            send,
            receive,
            next_id: 1,
            ready: false,
            queue: Vec::new(),
            utf32: false,
            triggers: Vec::new(),
            resolves: false,
            pending: HashMap::new(),
        })
    }
    fn write(&self, message: &Value) {
        let body = message.to_string();
        let _ = self
            .send
            .send(format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes());
    }
    /// Send now if initialized, otherwise after the handshake.
    fn post(&mut self, message: Value) {
        if self.ready {
            self.write(&message);
        } else {
            self.queue.push(message);
        }
    }
    fn notify(&mut self, method: &str, params: Value) {
        self.post(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
    fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.post(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_message(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

struct Doc {
    key: &'static str,
    uri: String,
    version: i64,
    revision: u64,
    last_sent: Instant,
}

pub struct Lsp {
    root: PathBuf,
    enabled: bool,
    overrides: BTreeMap<String, CommandPlugin>,
    clients: HashMap<&'static str, Client>,
    /// Languages whose server is missing or crashed, with the reason. Not retried
    /// until `:lsp restart`.
    failed: HashMap<&'static str, String>,
    docs: HashMap<usize, Doc>,
    pub diagnostics: HashMap<usize, Vec<Diagnostic>>,
    announced: HashSet<&'static str>,
    /// YAML and JSON files are checked against the schemas published for them
    /// (`lsp_schemas`), which their servers download; off, only their syntax is.
    pub schemas: bool,
    /// Spaces in an indentation level for a file that doesn't show its own.
    pub tab_width: usize,
    /// Kubernetes manifests opened (by their `apiVersion:` and `kind:` lines), as the
    /// YAML server matches files: it checks them against Kubernetes' own schemas.
    kubernetes: BTreeSet<String>,
    /// SchemaStore's catalog, for the JSON server.
    catalog: Catalog,
}

/// Which schema each kind of JSON file has, from SchemaStore's catalog: on its way,
/// then here. The YAML server fetches the catalog itself; the JSON server is told.
struct Catalog {
    schemas: Option<Value>,
    coming: Option<mpsc::Receiver<Option<Value>>>,
    /// Where it's kept between runs (none in tests: they never go online).
    cache: Option<PathBuf>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            schemas: None,
            coming: None,
            cache: if cfg!(test) {
                None
            } else {
                directories::BaseDirs::new().map(|d| d.cache_dir().join("raido"))
            },
        }
    }
}

impl Lsp {
    pub fn new(root: PathBuf, enabled: bool, overrides: BTreeMap<String, CommandPlugin>) -> Self {
        Self {
            root,
            enabled,
            overrides,
            clients: HashMap::new(),
            failed: HashMap::new(),
            docs: HashMap::new(),
            diagnostics: HashMap::new(),
            announced: HashSet::new(),
            schemas: true,
            tab_width: 4,
            kubernetes: BTreeSet::new(),
            catalog: Catalog::default(),
        }
    }

    /// What the server for `key` is told its settings are: the YAML server asks
    /// (`workspace/configuration`), the JSON server is told
    /// (`workspace/didChangeConfiguration`).
    fn settings(&self, key: &str) -> Value {
        match key {
            "yaml" => {
                let mut schemas = json!({});
                if self.schemas && !self.kubernetes.is_empty() {
                    schemas["kubernetes"] = json!(self.kubernetes);
                }
                json!({ "yaml": {
                    "validate": true,
                    "hover": true,
                    "completion": true,
                    "format": { "enable": true },
                    "schemaStore": { "enable": self.schemas },
                    "kubernetesCRDStore": { "enable": self.schemas },
                    "schemas": schemas,
                } })
            }
            "json" => {
                let schemas = match &self.catalog.schemas {
                    _ if !self.schemas => json!([]),
                    Some(schemas) => schemas.clone(),
                    None => known_json_schemas(),
                };
                json!({ "json": { "validate": { "enable": true }, "schemas": schemas } })
            }
            _ => Value::Null,
        }
    }

    /// The answer to a server's `workspace/configuration` request: per item, the
    /// part of its settings it names, or for `formattingOptions` (the TypeScript
    /// server's) the indentation of the file it names.
    fn configuration(&self, key: &str, message: &Value, buffers: &[Buffer]) -> Value {
        let settings = self.settings(key);
        let items = message.pointer("/params/items").and_then(Value::as_array);
        Value::Array(
            items
                .into_iter()
                .flatten()
                .map(|item| match item["section"].as_str() {
                    Some("formattingOptions") => {
                        let buffer = item["scopeUri"].as_str().and_then(|uri| {
                            let (&id, _) = self.docs.iter().find(|(_, d)| d.uri == uri)?;
                            buffers.get(id)
                        });
                        buffer.map_or(Value::Null, |b| {
                            let (size, spaces) = indentation(b, self.tab_width);
                            json!({ "tabSize": size, "insertSpaces": spaces })
                        })
                    }
                    name => section(&settings, name),
                })
                .collect(),
        )
    }

    /// Tell the server for `key` that its settings changed: the YAML server asks for
    /// them again, the JSON server gets them.
    fn tell_settings(&mut self, key: &'static str) {
        let settings = match key {
            "json" => self.settings(key),
            _ => Value::Null,
        };
        if let Some(client) = self.clients.get_mut(key) {
            client.notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            );
        }
    }

    /// Fetch SchemaStore's catalog in the background, once, for the JSON server.
    fn fetch_catalog(&mut self) {
        if !self.schemas || self.catalog.coming.is_some() || self.catalog.schemas.is_some() {
            return;
        }
        let (send, coming) = mpsc::channel();
        let cache = self.catalog.cache.clone();
        thread::spawn(move || {
            let _ = send.send(cache.and_then(|dir| schemastore_catalog(&dir)));
        });
        self.catalog.coming = Some(coming);
    }

    /// Stop every server; they restart when their files are next shown.
    pub fn restart(&mut self) {
        self.clients.clear();
        self.failed.clear();
        self.docs.clear();
        self.diagnostics.clear();
        self.announced.clear();
    }

    /// The server for `key`: yours from `[lsp.<key>]`, else the first default the
    /// project's virtual environment has (`uv add --dev pyright`), else the first on
    /// PATH. The name it goes by, the command that starts it, and its arguments.
    fn server(&self, key: &str, venv: Option<&Path>) -> Option<(String, String, Vec<String>)> {
        if let Some(custom) = self.overrides.get(key) {
            return Some((
                custom.program.clone(),
                custom.program.clone(),
                custom.args.clone(),
            ));
        }
        let args = |args: &[&str]| args.iter().map(|a| a.to_string()).collect();
        let local = venv.map(crate::pyenv::bin);
        defaults(key)
            .iter()
            .find_map(|(program, a)| {
                let found =
                    crate::platform::which(&local.as_ref()?.join(program).to_string_lossy())?;
                Some((
                    program.to_string(),
                    found.to_string_lossy().into_owned(),
                    args(a),
                ))
            })
            .or_else(|| {
                defaults(key)
                    .iter()
                    .find_map(|(program, a)| Some((program.to_string(), locate(program)?, args(a))))
            })
    }

    /// The virtual environment a Python server for files in `folder` runs on.
    fn venv(&self, key: &str, folder: &Path) -> Option<PathBuf> {
        (key == "python")
            .then(|| crate::pyenv::find(&self.root, folder))
            .flatten()
    }

    /// `venv` as `:lsp` names it: relative to the project when inside it.
    fn venv_name(&self, venv: Option<&Path>) -> String {
        match venv {
            Some(venv) => venv
                .strip_prefix(&self.root)
                .unwrap_or(venv)
                .display()
                .to_string(),
            None => "the Python on PATH (no .venv)".into(),
        }
    }

    /// Stop the server for `key`; it starts again, with its files, on the next sync.
    fn stop(&mut self, key: &'static str) {
        self.clients.remove(key);
        self.docs.retain(|_, doc| doc.key != key);
    }

    /// Start a server for `key` if needed, for the file at `path`; returns a message
    /// the first time it fails.
    fn ensure(&mut self, key: &'static str, path: &Path) -> Option<String> {
        if self.clients.contains_key(key) || self.failed.contains_key(key) {
            return None;
        }
        let folder = path.parent().unwrap_or(&self.root).to_path_buf();
        let venv = self.venv(key, &folder);
        let Some((program, command, args)) = self.server(key, venv.as_deref()) else {
            let reason = format!(
                "No {key} language server found · install: {}",
                install_hint(key)
            );
            self.failed.insert(key, reason.clone());
            return Some(reason);
        };
        if key == "json" {
            self.fetch_catalog();
        }
        match Client::spawn(&program, &command, &args, &self.root, venv.as_deref()) {
            Ok(mut client) => {
                let root = uri(&self.root);
                let name = self
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "workspace".into());
                let mut params = json!({
                    "processId": std::process::id(),
                    "clientInfo": { "name": "raido", "version": crate::VERSION },
                    "rootUri": root,
                    "rootPath": self.root,
                    "workspaceFolders": [{ "uri": root, "name": name }],
                    "capabilities": {
                        "general": { "positionEncodings": ["utf-32", "utf-16"] },
                        "textDocument": {
                            "synchronization": { "didSave": true },
                            "publishDiagnostics": {},
                            "hover": { "contentFormat": ["markdown", "plaintext"] },
                            "definition": { "linkSupport": true },
                            "completion": { "completionItem": { "snippetSupport": false } },
                            "formatting": {},
                            "codeAction": { "codeActionLiteralSupport": { "codeActionKind": {
                                "valueSet": ["source", "source.organizeImports"]
                            } } }
                        },
                        "workspace": { "workspaceFolders": true }
                    }
                });
                if key == "shell" {
                    // The Bash server runs ShellCheck and shfmt by name; say where they
                    // are when go install left shfmt off PATH.
                    let mut options = json!({});
                    if let Some(shellcheck) = locate("shellcheck") {
                        options["shellcheckPath"] = json!(shellcheck);
                    }
                    if let Some(shfmt) = locate("shfmt") {
                        options["shfmt"] = json!({ "path": shfmt });
                    }
                    params["initializationOptions"] = options;
                }
                if key == "json" {
                    // For JSON with Comments; .json files Raido lays out itself.
                    params["initializationOptions"] = json!({ "provideFormatter": true });
                }
                if key == "docker" {
                    // Docker's server reports usage to Docker unless told not to.
                    params["initializationOptions"] = json!({ "telemetry": "off" });
                }
                if key == "python" {
                    // Pyright then registers for file changes, which Raido reports;
                    // other servers watch files themselves unless told otherwise.
                    params["capabilities"]["workspace"]["didChangeWatchedFiles"] =
                        json!({ "dynamicRegistration": true });
                    let alive = Arc::new(());
                    client.python = Some(Python {
                        news: crate::pyenv::watch(
                            self.root.clone(),
                            folder,
                            venv.clone(),
                            Arc::downgrade(&alive),
                        ),
                        venv,
                        _alive: alive,
                    });
                }
                // The handshake request bypasses the queue that waits for it.
                client.write(
                    &json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": params }),
                );
                self.clients.insert(key, client);
                None
            }
            Err(error) => {
                let reason = format!("{error:#} · install: {}", install_hint(key));
                self.failed.insert(key, reason.clone());
                Some(reason)
            }
        }
    }

    /// Open newly shown documents and send pending edits. Call every tick with the
    /// buffers that are visible in editor or preview panes.
    pub fn sync(&mut self, buffers: &[Buffer], visible: &[usize]) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.enabled {
            return events;
        }
        let mut kubernetes = false;
        for &id in visible {
            let Some(buffer) = buffers.get(id) else {
                continue;
            };
            let Some(path) = buffer.path.as_deref() else {
                continue;
            };
            let Some(lang) = language(path, &crate::language::first_line(buffer)) else {
                continue;
            };
            if let Some(reason) = self.ensure(lang.key, path) {
                if self.announced.insert(lang.key) {
                    events.push(Event::Error(reason));
                }
                continue;
            }
            let uri = uri(path);
            if self.docs.get(&id).is_some_and(|d| d.uri != uri) {
                self.docs.remove(&id);
            }
            if lang.key == "yaml"
                && self.schemas
                && !self.docs.contains_key(&id)
                && is_kubernetes(path, buffer)
            {
                // As the YAML server matches it: the file's URI path.
                let pattern = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
                kubernetes |= self.kubernetes.insert(pattern);
            }
            let Some(client) = self.clients.get_mut(lang.key) else {
                continue;
            };
            match self.docs.get_mut(&id) {
                None => {
                    client.notify(
                        "textDocument/didOpen",
                        json!({ "textDocument": {
                            "uri": uri, "languageId": lang.id, "version": 1,
                            "text": buffer.text.to_string()
                        }}),
                    );
                    self.docs.insert(
                        id,
                        Doc {
                            key: lang.key,
                            uri,
                            version: 1,
                            revision: buffer.revision,
                            last_sent: Instant::now(),
                        },
                    );
                }
                Some(doc)
                    if doc.revision != buffer.revision && doc.last_sent.elapsed() >= SYNC_DELAY =>
                {
                    Self::send_change(client, doc, buffer);
                }
                _ => {}
            }
        }
        if kubernetes {
            self.tell_settings("yaml");
        }
        events
    }

    fn send_change(client: &mut Client, doc: &mut Doc, buffer: &Buffer) {
        doc.version += 1;
        doc.revision = buffer.revision;
        doc.last_sent = Instant::now();
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": doc.uri, "version": doc.version },
                "contentChanges": [{ "text": buffer.text.to_string() }]
            }),
        );
    }

    /// Push any unsent edit now, so a request sees the current text.
    fn flush(&mut self, id: usize, buffer: &Buffer) -> Option<&mut Client> {
        let doc = self.docs.get_mut(&id)?;
        let client = self.clients.get_mut(doc.key)?;
        if doc.revision != buffer.revision {
            Self::send_change(client, doc, buffer);
        }
        Some(client)
    }

    /// Ask for the details of the completion just accepted in buffer `id`: the
    /// import it needs comes back as `Event::Imports`.
    pub fn resolve(&mut self, id: usize, buffer: &Buffer, item: Value) {
        if let Some(client) = self.flush(id, buffer) {
            let request = client.request("completionItem/resolve", item);
            client.pending.insert(request, (Kind::Resolve, id));
        }
    }

    pub fn saved(&mut self, id: usize, buffer: &Buffer) {
        let Some(uri) = self.docs.get(&id).map(|d| d.uri.clone()) else {
            return;
        };
        if let Some(client) = self.flush(id, buffer) {
            client.notify(
                "textDocument/didSave",
                json!({ "textDocument": { "uri": uri } }),
            );
        }
    }

    /// Whether a running server has this buffer open, so it can answer completions.
    pub fn ready(&self, id: usize) -> bool {
        self.enabled
            && self
                .docs
                .get(&id)
                .and_then(|d| self.clients.get(d.key))
                .is_some_and(|client| client.ready)
    }

    /// Whether typing `c` should pop up completions (a server trigger character).
    pub fn is_trigger(&self, id: usize, c: char) -> bool {
        self.docs
            .get(&id)
            .and_then(|d| self.clients.get(d.key))
            .is_some_and(|client| client.ready && client.triggers.iter().any(|t| t.starts_with(c)))
    }

    pub fn request(
        &mut self,
        kind: Kind,
        id: usize,
        buffer: &Buffer,
        tab_width: usize,
    ) -> Result<()> {
        if !self.enabled {
            bail!("Language servers are disabled (lsp_enabled = false)");
        }
        let lang = language_of(buffer).context(
            "No language server for this file type (Rust, Go, Python, C, C++, TypeScript, JavaScript, shell, YAML, JSON, Docker)",
        )?;
        if let Some(reason) = self.failed.get(lang.key) {
            bail!("{reason}");
        }
        let Some(uri) = self.docs.get(&id).map(|d| d.uri.clone()) else {
            bail!("Language server is starting…");
        };
        let line = buffer.line(buffer.row);
        let client = self
            .flush(id, buffer)
            .context("Language server is not running")?;
        let position =
            json!({ "line": buffer.row, "character": encode(&line, buffer.col, client.utf32) });
        let document = json!({ "uri": uri });
        let request = match kind {
            // Asked for with the item itself: `Lsp::resolve`.
            Kind::Resolve => bail!("A completion is resolved with Lsp::resolve"),
            Kind::Hover => client.request(
                "textDocument/hover",
                json!({ "textDocument": document, "position": position }),
            ),
            Kind::Definition => client.request(
                "textDocument/definition",
                json!({ "textDocument": document, "position": position }),
            ),
            Kind::Completion => client.request(
                "textDocument/completion",
                json!({ "textDocument": document, "position": position, "context": { "triggerKind": 1 } }),
            ),
            Kind::Format => {
                // The file's own indentation: tabs stay tabs (shfmt, clang-format).
                let (size, spaces) = indentation(buffer, tab_width);
                client.request(
                    "textDocument/formatting",
                    json!({ "textDocument": document, "options": { "tabSize": size, "insertSpaces": spaces } }),
                )
            }
            Kind::OrganizeImports => client.request(
                "textDocument/codeAction",
                json!({
                    "textDocument": document,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": buffer.lines(), "character": 0 }
                    },
                    "context": { "diagnostics": [], "only": ["source.organizeImports"] }
                }),
            ),
        };
        client.pending.insert(request, (kind, id));
        Ok(())
    }

    /// The program serving language `key`, once started.
    pub fn program(&self, key: &str) -> Option<&str> {
        self.clients.get(key).map(|c| c.program.as_str())
    }

    /// One line for the editor status: server name and state.
    pub fn status(&self, id: usize, buffer: &Buffer) -> Option<String> {
        let lang = language_of(buffer)?;
        if !self.enabled {
            return None;
        }
        if self.failed.contains_key(lang.key) {
            return Some("no LSP".into());
        }
        let client = self.clients.get(lang.key)?;
        Some(if client.ready && self.docs.contains_key(&id) {
            client.program.clone()
        } else {
            format!("{} starting…", client.program)
        })
    }

    /// A summary of every language's server for `:lsp`.
    pub fn report(&self) -> String {
        let mut parts = Vec::new();
        for key in [
            "rust",
            "go",
            "python",
            "c",
            "typescript",
            "shell",
            "yaml",
            "json",
            "docker",
        ] {
            let state = if let Some(client) = self.clients.get(key) {
                let on = match &client.python {
                    Some(python) => format!(" on {}", self.venv_name(python.venv.as_deref())),
                    None => String::new(),
                };
                format!(
                    "{} {}{on}",
                    client.program,
                    if client.ready { "ready" } else { "starting" }
                )
            } else if self.failed.contains_key(key) {
                "unavailable".into()
            } else if let Some((program, ..)) =
                self.server(key, self.venv(key, &self.root).as_deref())
            {
                format!("{program} (starts on first file)")
            } else {
                format!("not installed — {}", install_hint(key))
            };
            // The Bash server warns through ShellCheck and formats through shfmt,
            // quietly doing without them when they're missing.
            let helpers = if key == "shell" {
                let missing: Vec<&str> = ["shellcheck", "shfmt"]
                    .into_iter()
                    .filter(|tool| locate(tool).is_none())
                    .collect();
                match missing.as_slice() {
                    [] => String::new(),
                    tools => format!(" ({} not installed)", tools.join(", ")),
                }
            } else {
                String::new()
            };
            parts.push(format!(
                "{}: {state}{helpers}",
                if key == "c" { "c/c++" } else { key }
            ));
        }
        parts.join(" · ")
    }

    /// Drain server messages; buffers are needed to convert positions.
    pub fn poll(&mut self, buffers: &[Buffer]) -> (Vec<Event>, bool) {
        let mut events = Vec::new();
        let mut changed = false;
        if let Some(catalog) = self
            .catalog
            .coming
            .as_ref()
            .and_then(|coming| coming.try_recv().ok())
        {
            self.catalog.coming = None;
            if let Some(schemas) = catalog {
                self.catalog.schemas = Some(schemas);
                self.tell_settings("json");
            }
        }
        let keys: Vec<&'static str> = self.clients.keys().copied().collect();
        for key in keys {
            while let Some(client) = self.clients.get_mut(key) {
                let message = match client.receive.try_recv() {
                    Ok(Incoming::Message(message)) => message,
                    Ok(Incoming::Closed(reason)) => {
                        let program = client.program.clone();
                        let detail = if reason.is_empty() {
                            "exited".to_string()
                        } else {
                            reason
                        };
                        let text = stopped(&program, key, &detail);
                        self.clients.remove(key);
                        self.docs.retain(|_, d| d.key != key);
                        self.failed.insert(key, text.clone());
                        events.push(Event::Error(text));
                        changed = true;
                        break;
                    }
                    Err(_) => break,
                };
                changed |= self.handle(key, message, buffers, &mut events);
            }
            if let Some(notice) = self.follow_python(key) {
                events.push(Event::Notice(notice));
                changed = true;
            }
        }
        (events, changed)
    }

    /// Pass on what changed in the Python environment and the project: to a server
    /// watching files, the files; otherwise a new environment or packages restart
    /// the server. Returns a note when it restarts.
    fn follow_python(&mut self, key: &'static str) -> Option<String> {
        let client = self.clients.get_mut(key)?;
        let python = client.python.as_ref()?;
        let mut restart = None;
        for news in python.news.try_iter().collect::<Vec<_>>() {
            match news {
                crate::pyenv::News::Environment(venv) => restart = Some(Some(venv)),
                crate::pyenv::News::Files { changes, packages } => {
                    if !client.watching.is_empty() {
                        let changes: Vec<Value> = changes
                            .iter()
                            .map(
                                |(path, change)| json!({ "uri": uri(path), "type": *change as u8 }),
                            )
                            .collect();
                        client.notify(
                            "workspace/didChangeWatchedFiles",
                            json!({ "changes": changes }),
                        );
                    } else if packages {
                        restart.get_or_insert(None);
                    }
                }
            }
        }
        let program = client.program.clone();
        let restart = restart?;
        self.stop(key);
        Some(match restart {
            Some(venv) => format!(
                "Python environment changed · {program} restarts on {}",
                self.venv_name(venv.as_deref())
            ),
            None => format!("Python packages changed · {program} restarts"),
        })
    }

    fn handle(
        &mut self,
        key: &'static str,
        message: Value,
        buffers: &[Buffer],
        events: &mut Vec<Event>,
    ) -> bool {
        // The settings a server asks for, and the ones the JSON server is told once
        // it has started.
        let asks = message["method"] == "workspace/configuration";
        let started = message.get("method").is_none() && message["id"] == 0;
        let settings = (started && key == "json").then(|| self.settings(key));
        let answer = asks.then(|| self.configuration(key, &message, buffers));
        let Some(client) = self.clients.get_mut(key) else {
            return false;
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            // A request from the server: answer so it never waits on us.
            (Some(method), Some(id)) => {
                let params = &message["params"];
                let ids = |list: &str, of: &str| -> Vec<String> {
                    params[list]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|r| r["method"] == of)
                        .filter_map(|r| r["id"].as_str().map(String::from))
                        .collect()
                };
                const WATCH: &str = "workspace/didChangeWatchedFiles";
                match method {
                    "client/registerCapability" => {
                        client.watching.extend(ids("registrations", WATCH));
                    }
                    // The protocol spells it "unregisterations".
                    "client/unregisterCapability" => {
                        for id in ids("unregisterations", WATCH) {
                            client.watching.remove(&id);
                        }
                    }
                    _ => {}
                }
                let result = if method == "workspace/configuration" {
                    answer.unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                client.write(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                false
            }
            (Some("textDocument/publishDiagnostics"), None) => {
                let params = &message["params"];
                let uri = params["uri"].as_str().unwrap_or_default();
                let utf32 = client.utf32;
                let Some((&buffer_id, _)) = self.docs.iter().find(|(_, d)| d.uri == uri) else {
                    return false;
                };
                let Some(buffer) = buffers.get(buffer_id) else {
                    return false;
                };
                let mut list: Vec<Diagnostic> = params["diagnostics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|d| {
                        let row = d
                            .pointer("/range/start/line")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as usize;
                        let line = buffer.line(row);
                        let start = decode(
                            &line,
                            d.pointer("/range/start/character")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as usize,
                            utf32,
                        );
                        let end_row = d
                            .pointer("/range/end/line")
                            .and_then(Value::as_u64)
                            .unwrap_or(row as u64) as usize;
                        let end = if end_row == row {
                            decode(
                                &line,
                                d.pointer("/range/end/character")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0) as usize,
                                utf32,
                            )
                        } else {
                            line.chars().count()
                        };
                        Diagnostic {
                            row,
                            start,
                            end: end.max(start),
                            severity: d["severity"].as_u64().unwrap_or(1).clamp(1, 4) as u8,
                            message: d["message"]
                                .as_str()
                                .unwrap_or_default()
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .to_string(),
                        }
                    })
                    .collect();
                list.sort_by_key(|d| (d.row, d.severity, d.start));
                self.diagnostics.insert(buffer_id, list);
                true
            }
            (None, Some(id)) => {
                let id = id.as_u64().unwrap_or(u64::MAX);
                if id == 0 && !client.ready {
                    let caps = &message["result"]["capabilities"];
                    client.utf32 = caps["positionEncoding"].as_str() == Some("utf-32");
                    client.triggers = caps
                        .pointer("/completionProvider/triggerCharacters")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect();
                    client.resolves = caps
                        .pointer("/completionProvider/resolveProvider")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    client.ready = true;
                    client
                        .write(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
                    if let Some(settings) = settings {
                        client.write(&json!({ "jsonrpc": "2.0",
                            "method": "workspace/didChangeConfiguration",
                            "params": { "settings": settings } }));
                    }
                    for queued in std::mem::take(&mut client.queue) {
                        client.write(&queued);
                    }
                    return true;
                }
                let Some((kind, buffer_id)) = client.pending.remove(&id) else {
                    return false;
                };
                if let Some(error) = message.get("error") {
                    let text = error["message"].as_str().unwrap_or("request failed");
                    events.push(Event::Error(format!("{}: {text}", client.program)));
                    return false;
                }
                let (utf32, resolves) = (client.utf32, client.resolves);
                if let Some(buffer) = buffers.get(buffer_id) {
                    events.extend(reply(
                        kind,
                        buffer_id,
                        buffer,
                        &message["result"],
                        utf32,
                        resolves,
                    ));
                }
                false
            }
            _ => false,
        }
    }
}

fn reply(
    kind: Kind,
    id: usize,
    buffer: &Buffer,
    result: &Value,
    utf32: bool,
    resolves: bool,
) -> Option<Event> {
    match kind {
        Kind::Hover => {
            let text = hover_text(&result["contents"]);
            Some(if text.trim().is_empty() {
                Event::Notice("No hover information here".into())
            } else {
                Event::Hover(text)
            })
        }
        Kind::Definition => {
            let first = match result {
                Value::Array(list) => list.first(),
                Value::Null => None,
                single => Some(single),
            };
            let Some(location) = first else {
                return Some(Event::Notice("No definition found".into()));
            };
            let uri = location["uri"]
                .as_str()
                .or(location["targetUri"].as_str())?;
            let range = if location.get("targetSelectionRange").is_some() {
                &location["targetSelectionRange"]
            } else {
                &location["range"]
            };
            Some(Event::Definition {
                path: path_from_uri(uri)?,
                line: range
                    .pointer("/start/line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize,
                character: range
                    .pointer("/start/character")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize,
                utf32,
            })
        }
        Kind::Completion => {
            let items = result
                .as_array()
                .or_else(|| result["items"].as_array())
                .cloned()
                .unwrap_or_default();
            let line = buffer.line(buffer.row);
            let mut items: Vec<(String, Completion)> = items
                .iter()
                .filter_map(|item| {
                    // clangd pads labels with a space or `•` (include insertion marker).
                    let label = item["label"]
                        .as_str()?
                        .trim_start_matches([' ', '•'])
                        .to_string();
                    let edit = item.get("textEdit");
                    let new_text = edit.and_then(|e| e["newText"].as_str());
                    let range = edit.and_then(|e| e.get("range").or_else(|| e.get("replace")));
                    let start = range
                        .filter(|r| {
                            r.pointer("/start/line").and_then(Value::as_u64)
                                == Some(buffer.row as u64)
                        })
                        .and_then(|r| r.pointer("/start/character").and_then(Value::as_u64))
                        .map(|c| decode(&line, c as usize, utf32));
                    let mut insert = new_text
                        .or(item["insertText"].as_str())
                        .unwrap_or(&label)
                        .to_string();
                    if item["insertTextFormat"].as_u64() == Some(2) {
                        insert = strip_snippet(&insert);
                    }
                    let sort = item["sortText"].as_str().unwrap_or(&label).to_string();
                    // The import it needs: with it (gopls, rust-analyzer, clangd), or
                    // once it's asked for (TypeScript's server).
                    let extra = text_edits(&item["additionalTextEdits"], buffer, utf32);
                    let resolve = (extra.is_empty() && resolves && item.get("data").is_some())
                        .then(|| item.clone());
                    Some((
                        sort,
                        Completion {
                            extra,
                            resolve,
                            filter: item["filterText"].as_str().unwrap_or(&label).to_string(),
                            detail: item["detail"]
                                .as_str()
                                .unwrap_or_default()
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .to_string(),
                            label,
                            insert,
                            start,
                        },
                    ))
                })
                .collect();
            // The ones for the word being typed first, then the server's order, before
            // keeping 300: a long list (TypeScript's globals) would otherwise leave
            // out what's typed, its auto-imports last of all.
            let word: String = line
                .chars()
                .take(buffer.col)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            items.sort_by(|a, b| (!a.1.matches(&word), &a.0).cmp(&(!b.1.matches(&word), &b.0)));
            items.truncate(300);
            Some(Event::Completion {
                buffer: id,
                row: buffer.row,
                items: items.into_iter().map(|(_, c)| c).collect(),
            })
        }
        Kind::Format => Some(Event::Format {
            buffer: id,
            edits: text_edits(result, buffer, utf32),
        }),
        Kind::Resolve => {
            let edits = text_edits(&result["additionalTextEdits"], buffer, utf32);
            (!edits.is_empty()).then_some(Event::Imports { buffer: id, edits })
        }
        Kind::OrganizeImports => {
            // Code actions carry a workspace edit; keep the edits for this file.
            let this = buffer.path.as_deref();
            let mine = |uri: &Value| {
                let path = uri.as_str().and_then(path_from_uri);
                path.is_some() && path.as_deref() == this
                    || path
                        .and_then(|p| p.canonicalize().ok())
                        .is_some_and(|p| Some(p.as_path()) == this)
            };
            let mut edits = Vec::new();
            for action in result.as_array().into_iter().flatten() {
                let edit = &action["edit"];
                for change in edit["documentChanges"].as_array().into_iter().flatten() {
                    if mine(&change["textDocument"]["uri"]) {
                        edits.extend(text_edits(&change["edits"], buffer, utf32));
                    }
                }
                for (uri, list) in edit["changes"].as_object().into_iter().flatten() {
                    if mine(&Value::String(uri.clone())) {
                        edits.extend(text_edits(list, buffer, utf32));
                    }
                }
            }
            Some(Event::Organized { buffer: id, edits })
        }
    }
}

/// LSP `TextEdit`s as char ranges of `buffer`, with their new text.
fn text_edits(list: &Value, buffer: &Buffer, utf32: bool) -> Vec<(usize, usize, String)> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|edit| {
            let point = |which: &str| {
                let line = edit.pointer(&format!("/range/{which}/line"))?.as_u64()? as usize;
                let character = edit
                    .pointer(&format!("/range/{which}/character"))?
                    .as_u64()? as usize;
                if line >= buffer.lines() {
                    return Some(buffer.text.len_chars());
                }
                Some(buffer.char_index(line, decode(&buffer.line(line), character, utf32)))
            };
            Some((
                point("start")?,
                point("end")?,
                edit["newText"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn hover_text(contents: &Value) -> String {
    match contents {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(hover_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(map) => {
            let value = map.get("value").and_then(Value::as_str).unwrap_or_default();
            match map.get("language").and_then(Value::as_str) {
                Some(language) => format!("```{language}\n{value}\n```"),
                None => value.to_string(),
            }
        }
        _ => String::new(),
    }
}

/// `foo(${1:x}, $2)$0` → `foo(x, )`.
fn strip_snippet(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.extend(chars.next()),
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
                if chars.peek() == Some(&':') {
                    chars.next();
                }
                let mut depth = 1;
                for c in chars.by_ref() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    out.push(c);
                }
            }
            '$' if chars.peek().is_some_and(|c| c.is_ascii_digit()) => {
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// A scripted server (Python) exercises the whole protocol path end to end.
#[cfg(all(test, unix))]
pub(crate) const FAKE_SERVER: &str = r#"
import json, os, sys
opened = None
def send(msg):
    body = json.dumps(msg)
    sys.stdout.write("Content-Length: %d\r\n\r\n%s" % (len(body.encode()), body)); sys.stdout.flush()
def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        if line.lower().startswith(b"content-length:"): length = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(length))
while True:
    msg = read(); method = msg.get("method"); id = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {"positionEncoding": "utf-16", "completionProvider": {"triggerCharacters": ["."]}}}})
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": 98, "method": "client/registerCapability", "params": {"registrations": [
            {"id": "files", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": [{"globPattern": "**"}]}}]}})
    elif method == "workspace/didChangeWatchedFiles":
        seen = " ".join(sorted("%d:%s" % (c["type"], c["uri"].rsplit("/", 1)[1]) for c in msg["params"]["changes"]))
        venv = os.environ.get("VIRTUAL_ENV", "none").rsplit("/", 1)[-1]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": opened, "diagnostics": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 1, "message": seen + " venv=" + venv}]}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        uri = msg["params"]["textDocument"]["uri"]
        opened = opened or uri
        send({"jsonrpc": "2.0", "method": "workspace/configuration", "id": 99, "params": {"items": [{}]}})
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": uri, "diagnostics": [
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7}}, "severity": 1, "message": "bad name"}]}})
    elif method == "textDocument/hover":
        send({"jsonrpc": "2.0", "id": id, "result": {"contents": {"kind": "markdown", "value": "fn **main**()"}}})
    elif method == "textDocument/definition":
        uri = msg["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": id, "result": [{"uri": uri, "range": {"start": {"line": 1, "character": 4}, "end": {"line": 1, "character": 5}}}]})
    elif method == "textDocument/completion":
        send({"jsonrpc": "2.0", "id": id, "result": {"items": [
            {"label": "println!", "insertText": "println!(${1:x})", "insertTextFormat": 2, "sortText": "2"},
            {"label": "print", "sortText": "1", "detail": "fn print()"}]}})
    elif method == "textDocument/codeAction":
        uri = msg["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "id": id, "result": [{"title": "Organize Imports", "kind": "source.organizeImports",
            "edit": {"documentChanges": [{"textDocument": {"uri": uri, "version": 1}, "edits": [
                {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 0}}, "newText": "use std::fmt;\n"}]}]}}]})
    elif method == "textDocument/formatting":
        send({"jsonrpc": "2.0", "id": id, "result": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": "pub fn"}]})
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_off_path_is_found_where_it_was_installed() {
        let dir = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let name = "raido-test-language-server";
        let file = dir.path().join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        });
        std::fs::write(&file, "").unwrap();
        assert_eq!(locate_in(name, &[empty.path().into()]), None);
        assert_eq!(
            locate_in(name, &[empty.path().into(), dir.path().into()]),
            Some(file.to_string_lossy().into_owned())
        );
        // go install puts gopls in ~/go/bin, which often isn't on PATH.
        assert!(off_path("gopls").iter().any(|d| d.ends_with("go/bin")));
        assert!(off_path("rust-analyzer").is_empty());
    }
    #[test]
    fn positions_convert_between_chars_and_utf16() {
        let line = "a😀b";
        assert_eq!(encode(line, 2, false), 3);
        assert_eq!(encode(line, 2, true), 2);
        assert_eq!(decode(line, 3, false), 2);
        assert_eq!(decode(line, 99, false), 3);
        assert_eq!(decode(line, 2, true), 2);
    }

    #[test]
    fn windows_paths_become_drive_letter_uris_and_back() {
        let uri = file_uri("C:\\Users\\me\\My Code\\main.rs", true);
        assert_eq!(uri, "file:///C:/Users/me/My%20Code/main.rs");
        assert_eq!(
            path_from_file_uri(&uri, true).unwrap(),
            PathBuf::from("C:\\Users\\me\\My Code\\main.rs")
        );
        // Servers may percent-encode the colon.
        assert_eq!(
            path_from_file_uri("file:///c%3A/src/a.rs", true).unwrap(),
            PathBuf::from("c:\\src\\a.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn uris_round_trip_paths_with_spaces() {
        let path = Path::new("/home/me/Telegram Desktop/main.rs");
        let uri = uri(path);
        assert_eq!(uri, "file:///home/me/Telegram%20Desktop/main.rs");
        assert_eq!(path_from_uri(&uri).unwrap(), path);
    }

    #[test]
    fn snippets_and_hover_contents_are_simplified() {
        assert_eq!(strip_snippet("foo(${1:x}, $2)$0"), "foo(x, )");
        assert_eq!(strip_snippet("vec![${1}]"), "vec![]");
        assert_eq!(
            hover_text(&json!({ "kind": "markdown", "value": "**x**" })),
            "**x**"
        );
        assert_eq!(
            hover_text(&json!([{ "language": "rust", "value": "fn f()" }, "doc"])),
            "```rust\nfn f()\n```\n\ndoc"
        );
    }

    #[test]
    fn languages_map_to_servers() {
        let language = |name: &str| language(Path::new(name), "");
        assert_eq!(language("a.rs").unwrap().key, "rust");
        assert_eq!(language("a.hpp").unwrap().id, "cpp");
        assert_eq!(language("a.py").unwrap().key, "python");
        assert!(language("a.md").is_none());
        // Docker's server takes Dockerfiles and Compose files, by their names.
        for (name, id) in [
            ("Dockerfile", "dockerfile"),
            ("Dockerfile.dev", "dockerfile"),
            ("api.dockerfile", "dockerfile"),
            ("Containerfile", "dockerfile"),
            ("compose.yaml", "dockercompose"),
            ("docker-compose.prod.yml", "dockercompose"),
        ] {
            let lang = language(name).unwrap();
            assert_eq!((lang.key, lang.id), ("docker", id), "{name}");
        }
        assert!(language("Dockerfile.dockerignore").is_none());
        // TypeScript's server takes JavaScript too, each by VS Code's name for it.
        for (name, id) in [
            ("a.ts", "typescript"),
            ("a.d.ts", "typescript"),
            ("a.mts", "typescript"),
            ("App.tsx", "typescriptreact"),
            ("a.js", "javascript"),
            ("a.mjs", "javascript"),
            ("a.cjs", "javascript"),
            ("App.jsx", "javascriptreact"),
        ] {
            let lang = language(name).unwrap();
            assert_eq!((lang.key, lang.id), ("typescript", id), "{name}");
        }
        // YAML to Red Hat's server (Compose files to Docker's); JSON to VS Code's, as
        // JSON with Comments where VS Code reads it so.
        assert_eq!(
            language("values.yaml").map(|l| (l.key, l.id)),
            Some(("yaml", "yaml"))
        );
        assert_eq!(language("ci.yml").map(|l| l.key), Some("yaml"));
        assert_eq!(language("compose.yaml").map(|l| l.key), Some("docker"));
        for (name, id) in [
            ("package.json", "json"),
            ("tsconfig.json", "jsonc"),
            ("tsconfig.build.json", "jsonc"),
            ("jsconfig.json", "jsonc"),
            (".vscode/settings.json", "jsonc"),
            ("a.jsonc", "jsonc"),
            ("app.code-workspace", "jsonc"),
            (".babelrc", "jsonc"),
            ("data.json", "json"),
        ] {
            let lang = language(name).unwrap();
            assert_eq!((lang.key, lang.id), ("json", id), "{name}");
        }
        // The Bash server takes sh, bash and ksh scripts, by name or by `#!` line.
        for name in [
            "deploy.sh",
            "a.bash",
            "a.ksh",
            ".bashrc",
            ".profile",
            "PKGBUILD",
        ] {
            let lang = language(name).unwrap();
            assert_eq!((lang.key, lang.id), ("shell", "shellscript"), "{name}");
        }
        let script = |name: &str, first: &str| language_at(name, first).map(|l| l.id);
        assert_eq!(script("deploy", "#!/usr/bin/env bash"), Some("shellscript"));
        assert_eq!(script("run", "#!/bin/sh -e"), Some("shellscript"));
        // zsh and fish aren't theirs, even with a .sh name; nor other #! lines.
        for (name, first) in [
            ("a.zsh", ""),
            (".zshrc", ""),
            ("config.fish", ""),
            ("deploy", "#!/usr/bin/env zsh"),
            ("tool.sh", "#!/bin/zsh"),
            ("deploy", "#!/usr/bin/env python3"),
            ("notes.txt", "#!/bin/bash"),
        ] {
            assert_eq!(script(name, first), None, "{name} {first}");
        }
        assert!(off_path("docker-language-server")
            .iter()
            .any(|d| d.ends_with("go/bin")));
    }

    #[test]
    fn completions_for_the_typed_word_survive_a_long_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.ts");
        std::fs::write(&path, "helpe").unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        (buffer.row, buffer.col) = (0, 5);
        // 400 globals first in the server's order, the auto-import last.
        let mut items: Vec<Value> = (0..400)
            .map(|i| json!({ "label": format!("global{i}"), "sortText": "11" }))
            .collect();
        items.push(json!({ "label": "helper", "sortText": "16" }));
        let Some(Event::Completion { items, .. }) = reply(
            Kind::Completion,
            0,
            &buffer,
            &json!({ "items": items }),
            false,
            false,
        ) else {
            panic!()
        };
        assert_eq!(items.len(), 300);
        assert_eq!(items[0].label, "helper");
    }

    #[test]
    fn servers_get_the_settings_they_ask_for_by_section() {
        let settings = json!({ "yaml": { "validate": true, "schemaStore": { "enable": false } } });
        assert_eq!(section(&settings, Some("yaml.validate")), json!(true));
        assert_eq!(
            section(&settings, Some("yaml"))["schemaStore"]["enable"],
            json!(false)
        );
        assert_eq!(section(&settings, Some("http")), Value::Null);
        assert_eq!(section(&settings, Some("[yaml]")), Value::Null);
        assert_eq!(section(&settings, None), settings);
        // Offline: no SchemaStore, no Kubernetes, no schemas for JSON.
        let mut lsp = Lsp::new(PathBuf::from("/p"), true, BTreeMap::new());
        lsp.kubernetes.insert("/p/deploy.yaml".into());
        assert_eq!(
            lsp.settings("yaml")["yaml"]["schemas"]["kubernetes"],
            json!(["/p/deploy.yaml"])
        );
        assert_eq!(
            lsp.settings("json")["json"]["schemas"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        lsp.schemas = false;
        assert_eq!(
            lsp.settings("yaml")["yaml"]["schemaStore"]["enable"],
            json!(false)
        );
        assert_eq!(lsp.settings("yaml")["yaml"]["schemas"], json!({}));
        assert_eq!(lsp.settings("json")["json"]["schemas"], json!([]));
        assert_eq!(
            lsp.settings("json")["json"]["validate"]["enable"],
            json!(true)
        );
    }

    #[test]
    fn kubernetes_manifests_are_known_by_their_apiversion_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            (path.clone(), Buffer::open(&path).unwrap())
        };
        let (path, b) = open(
            "deploy.yaml",
            "# web\napiVersion: apps/v1\nkind: Deployment\n",
        );
        assert!(is_kubernetes(&path, &b));
        let (path, b) = open("multi.yaml", "---\nkind: Service\napiVersion: v1\n");
        assert!(is_kubernetes(&path, &b));
        // A chart has no kind; nested keys aren't the manifest's; kustomize's own.
        for (name, text) in [
            ("Chart.yaml", "apiVersion: v2\nname: web\n"),
            ("values.yaml", "app:\n  apiVersion: v1\n  kind: x\n"),
            (
                "kustomization.yaml",
                "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\n",
            ),
        ] {
            let (path, b) = open(name, text);
            assert!(!is_kubernetes(&path, &b), "{name}");
        }
    }

    #[test]
    fn schemastores_catalog_is_kept_a_week_and_read_for_the_json_server() {
        let catalog = json!({ "schemas": [
            { "name": "package.json", "fileMatch": ["package.json"], "url": "https://json.schemastore.org/package.json" },
            { "name": "no files", "url": "https://example.com/a.json" },
            { "name": "workflows", "fileMatch": ["**/.github/workflows/*.yml"], "url": "https://json.schemastore.org/github-workflow.json" },
        ] });
        let schemas = catalog_schemas(&catalog);
        assert_eq!(
            schemas,
            json!([
                { "fileMatch": ["package.json"], "url": "https://json.schemastore.org/package.json" },
                { "fileMatch": ["**/.github/workflows/*.yml"], "url": "https://json.schemastore.org/github-workflow.json" },
            ])
        );
        // A fresh copy in the cache is read without going online.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schemastore-catalog.json"),
            catalog.to_string(),
        )
        .unwrap();
        assert_eq!(schemastore_catalog(dir.path()), Some(schemas));
    }

    /// The YAML server asks for its settings, and again when told they changed: a
    /// Kubernetes manifest opened is in them. The JSON server is told its settings,
    /// SchemaStore's catalog once it's here, and nothing to download when offline.
    #[cfg(unix)]
    #[test]
    fn yaml_and_json_servers_get_their_schemas() {
        if !on_path("python3") {
            return;
        }
        const SERVER: &str = r#"
import json, sys
config, opened, asked = None, None, 50
def send(msg):
    body = json.dumps(msg)
    sys.stdout.write("Content-Length: %d\r\n\r\n%s" % (len(body.encode()), body)); sys.stdout.flush()
def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        if line.lower().startswith(b"content-length:"): length = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(length))
def ask():
    global asked
    asked += 1
    send({"jsonrpc": "2.0", "id": asked, "method": "workspace/configuration", "params": {"items": [{"section": "yaml"}, {"section": "http"}]}})
def tell():
    if config is None or opened is None: return
    if "yaml" in config:
        said = "store=%s k8s=%s http=%s" % (config["yaml"]["schemaStore"]["enable"], ",".join(config["yaml"]["schemas"].get("kubernetes", [])), config.get("http"))
    else:
        said = "schemas=%d validate=%s" % (len(config["json"]["schemas"]), config["json"]["validate"]["enable"])
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": opened, "diagnostics": [
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 2, "message": said}]}})
while True:
    msg = read(); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif method == "initialized" and sys.argv[1] == "yaml":
        ask()
    elif method == "workspace/didChangeConfiguration":
        if sys.argv[1] == "yaml": ask()
        else:
            config = msg["params"]["settings"]; tell()
    elif method is None and "result" in msg:
        config = {"yaml": msg["result"][0], "http": msg["result"][1]}; tell()
    elif method == "textDocument/didOpen":
        opened = msg["params"]["textDocument"]["uri"]; tell()
"#;
        let dir = tempfile::tempdir().unwrap();
        let deploy = dir.path().join("deploy.yaml");
        let package = dir.path().join("package.json");
        std::fs::write(&deploy, "apiVersion: v1\nkind: Service\n").unwrap();
        std::fs::write(&package, "{}\n").unwrap();
        let buffers = vec![
            Buffer::open(&deploy).unwrap(),
            Buffer::open(&package).unwrap(),
        ];
        let start = |schemas: bool, cache: Option<PathBuf>| {
            let mut overrides = BTreeMap::new();
            for key in ["yaml", "json"] {
                overrides.insert(
                    key.to_string(),
                    CommandPlugin {
                        program: "python3".into(),
                        args: vec!["-c".into(), SERVER.into(), key.into()],
                    },
                );
            }
            let mut lsp = Lsp::new(dir.path().into(), true, overrides);
            lsp.schemas = schemas;
            lsp.catalog.cache = cache;
            lsp.sync(&buffers, &[0, 1]);
            lsp
        };
        let said = |lsp: &mut Lsp, id: usize, done: &dyn Fn(&str) -> bool| -> String {
            for _ in 0..300 {
                lsp.poll(&buffers);
                if let Some(message) = lsp.diagnostics.get(&id).map(|d| d[0].message.clone()) {
                    if done(&message) {
                        return message;
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("{:?}", lsp.diagnostics.get(&id));
        };
        // SchemaStore's catalog from the cache: one schema.
        let cache = tempfile::tempdir().unwrap();
        let catalog = json!({ "schemas": [{ "fileMatch": ["package.json"], "url": "https://json.schemastore.org/package.json" }] });
        std::fs::write(
            cache.path().join("schemastore-catalog.json"),
            catalog.to_string(),
        )
        .unwrap();
        let mut lsp = start(true, Some(cache.path().into()));
        let yaml = said(&mut lsp, 0, &|m| m.contains("deploy.yaml"));
        assert!(yaml.starts_with("store=True k8s=/"), "{yaml}");
        assert!(yaml.ends_with("deploy.yaml http=None"), "{yaml}");
        said(&mut lsp, 1, &|m| m == "schemas=1 validate=True");
        // Offline: nothing to download, and no Kubernetes schema.
        let mut lsp = start(false, None);
        said(&mut lsp, 0, &|m| m == "store=False k8s= http=None");
        said(&mut lsp, 1, &|m| m == "schemas=0 validate=True");
    }

    /// The TypeScript server asks for each file's indentation to format with.
    #[cfg(unix)]
    #[test]
    fn the_typescript_server_is_told_each_files_indentation() {
        if !on_path("python3") {
            return;
        }
        const SERVER: &str = r#"
import json, sys
asked = {}
def send(msg):
    body = json.dumps(msg)
    sys.stdout.write("Content-Length: %d\r\n\r\n%s" % (len(body.encode()), body)); sys.stdout.flush()
def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        if line.lower().startswith(b"content-length:"): length = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(length))
while True:
    msg = read(); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        uri = msg["params"]["textDocument"]["uri"]
        asked[100 + len(asked)] = uri
        send({"jsonrpc": "2.0", "id": 99 + len(asked), "method": "workspace/configuration", "params": {"items": [{"scopeUri": uri, "section": "formattingOptions"}]}})
    elif method is None and "result" in msg:
        o = msg["result"][0]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": asked[msg["id"]], "diagnostics": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 2, "message": "%s %s" % (o["tabSize"], o["insertSpaces"])}]}})
"#;
        let dir = tempfile::tempdir().unwrap();
        let tabs = dir.path().join("tabs.ts");
        let two = dir.path().join("two.js");
        std::fs::write(&tabs, "function f() {\n\treturn 1;\n}\n").unwrap();
        std::fs::write(&two, "function f() {\n  return 1;\n}\n").unwrap();
        let buffers = vec![Buffer::open(&tabs).unwrap(), Buffer::open(&two).unwrap()];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "typescript".to_string(),
            CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), SERVER.into()],
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        lsp.tab_width = 8;
        lsp.sync(&buffers, &[0, 1]);
        for _ in 0..300 {
            lsp.poll(&buffers);
            if lsp.diagnostics.len() == 2 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let said = |id: usize| lsp.diagnostics.get(&id).map(|d| d[0].message.clone());
        assert_eq!(said(0).as_deref(), Some("8 False"));
        assert_eq!(said(1).as_deref(), Some("2 True"));
    }

    /// The real TypeScript server: a type error, completion and formatting.
    #[test]
    #[ignore = "talks to the real server: RAIDO_TS_LS=<path> cargo test real_typescript -- --ignored (typescript installed beside it)"]
    fn real_typescript_server_checks_completes_and_formats() {
        let program = std::env::var("RAIDO_TS_LS").unwrap_or("typescript-language-server".into());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.ts");
        std::fs::write(
            &path,
            "const count: number = \"three\";\nconst word = \"hi\";\nword.toU\nfunction f(){return 1}\n",
        )
        .unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        (buffer.row, buffer.col) = (2, 8);
        let buffers = vec![buffer];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "typescript".to_string(),
            CommandPlugin {
                program,
                args: vec!["--stdio".into()],
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        lsp.sync(&buffers, &[0]);
        let ask = |lsp: &mut Lsp, kind: Kind| -> Event {
            let mut asked = false;
            for _ in 0..1500 {
                if !asked {
                    asked = lsp.request(kind, 0, &buffers[0], 4).is_ok();
                }
                let (events, _) = lsp.poll(&buffers);
                if let Some(event) = events
                    .into_iter()
                    .find(|e| matches!(e, Event::Completion { .. } | Event::Format { .. }))
                {
                    return event;
                }
                thread::sleep(Duration::from_millis(20));
            }
            panic!("no answer to {kind:?}");
        };
        let Event::Completion { items, .. } = ask(&mut lsp, Kind::Completion) else {
            panic!("completion")
        };
        assert!(
            items.iter().any(|i| i.label == "toUpperCase"),
            "{:?}",
            items.iter().map(|i| &i.label).take(20).collect::<Vec<_>>()
        );
        for _ in 0..1500 {
            lsp.poll(&buffers);
            if lsp.diagnostics.get(&0).is_some_and(|d| !d.is_empty()) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let found: Vec<String> = lsp.diagnostics[&0]
            .iter()
            .map(|d| d.message.clone())
            .collect();
        assert!(
            found
                .iter()
                .any(|m| m.contains("not assignable to type 'number'")),
            "{found:?}"
        );
        let Event::Format { edits, .. } = ask(&mut lsp, Kind::Format) else {
            panic!("format")
        };
        assert!(!edits.is_empty());
    }

    fn language_at(name: &str, first_line: &str) -> Option<Language> {
        language(Path::new(name), first_line)
    }

    /// The real Bash language server on a script known by its `#!` line: ShellCheck's
    /// warnings, completion of its variables, hover docs and shfmt's formatting.
    #[test]
    #[ignore = "talks to the real bash-language-server: RAIDO_BASH_LS=<path> cargo test real_bash -- --ignored (with shellcheck and shfmt on PATH)"]
    fn real_bash_language_server_warns_completes_and_formats() {
        let program = std::env::var("RAIDO_BASH_LS").unwrap_or("bash-language-server".into());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy");
        std::fs::write(
            &path,
            "#!/usr/bin/env bash\nname=world\nif [ $name = x ]; then\necho \"hi $nam\"\nfi\n",
        )
        .unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        (buffer.row, buffer.col) = (3, 12);
        let buffers = vec![buffer];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "shell".to_string(),
            CommandPlugin {
                program,
                args: vec!["start".into()],
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        lsp.sync(&buffers, &[0]);
        let ask = |lsp: &mut Lsp, kind: Kind| -> Event {
            let mut asked = false;
            for _ in 0..1000 {
                if !asked {
                    asked = lsp.request(kind, 0, &buffers[0], 4).is_ok();
                }
                let (events, _) = lsp.poll(&buffers);
                if let Some(event) = events.into_iter().find(|e| !matches!(e, Event::Notice(_))) {
                    return event;
                }
                thread::sleep(Duration::from_millis(20));
            }
            panic!("no answer to {kind:?}");
        };
        let Event::Completion { items, .. } = ask(&mut lsp, Kind::Completion) else {
            panic!("completion")
        };
        assert!(
            items.iter().any(|i| i.label == "name"),
            "{:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        if on_path("shellcheck") {
            for _ in 0..500 {
                lsp.poll(&buffers);
                if lsp.diagnostics.get(&0).is_some_and(|d| !d.is_empty()) {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let found: Vec<String> = lsp.diagnostics[&0]
                .iter()
                .map(|d| d.message.clone())
                .collect();
            // A typo: ShellCheck's SC2154.
            assert!(
                found
                    .iter()
                    .any(|m| m.contains("nam is referenced but not assigned")),
                "{found:?}"
            );
        }
        if locate("shfmt").is_some() {
            let Event::Format { edits, .. } = ask(&mut lsp, Kind::Format) else {
                panic!("format")
            };
            assert!(
                edits.iter().any(|(_, _, text)| text.contains("    echo")),
                "{edits:?}"
            );
        }
    }

    /// The real YAML and JSON servers, online: a GitHub workflow, a Kubernetes
    /// manifest and a package.json, each with a mistake only its schema knows of.
    #[test]
    #[ignore = "talks to the real servers and downloads schemas: RAIDO_YAML_LS=<path> RAIDO_JSON_LS=<path> cargo test real_yaml_and_json -- --ignored"]
    fn real_yaml_and_json_servers_check_files_against_their_schemas() {
        let server = |var: &str, name: &str| std::env::var(var).unwrap_or(name.into());
        // Not `.tmp…`: SchemaStore's `**/.github/workflows/*.yml` skips dot folders.
        let dir = tempfile::Builder::new().prefix("raido").tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workflow = dir.path().join(".github/workflows/ci.yml");
        std::fs::create_dir_all(workflow.parent().unwrap()).unwrap();
        std::fs::write(
            &workflow,
            "on: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    stepz:\n      - run: make\n",
        )
        .unwrap();
        let deploy = dir.path().join("deploy.yaml");
        std::fs::write(&deploy, "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: three\n").unwrap();
        let package = dir.path().join("package.json");
        std::fs::write(&package, "{\n  \"name\": 5\n}\n").unwrap();
        let buffers = vec![
            Buffer::open(&workflow).unwrap(),
            Buffer::open(&deploy).unwrap(),
            Buffer::open(&package).unwrap(),
        ];
        let mut overrides = BTreeMap::new();
        for (key, var, name) in [
            ("yaml", "RAIDO_YAML_LS", "yaml-language-server"),
            ("json", "RAIDO_JSON_LS", "vscode-json-language-server"),
        ] {
            overrides.insert(
                key.to_string(),
                CommandPlugin {
                    program: server(var, name),
                    args: vec!["--stdio".into()],
                },
            );
        }
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        lsp.catalog.cache = Some(cache.path().into());
        lsp.sync(&buffers, &[0, 1, 2]);
        let mut said: HashMap<usize, Vec<String>> = HashMap::new();
        for _ in 0..1500 {
            lsp.poll(&buffers);
            lsp.sync(&buffers, &[0, 1, 2]);
            for id in 0..3 {
                if let Some(list) = lsp.diagnostics.get(&id).filter(|l| !l.is_empty()) {
                    said.insert(id, list.iter().map(|d| d.message.clone()).collect());
                }
            }
            if said.len() == 3 {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let any = |id: usize, text: &str| {
            said.get(&id)
                .is_some_and(|l| l.iter().any(|m| m.contains(text)))
        };
        assert!(any(0, "stepz"), "workflow: {:?}", said.get(&0));
        assert!(any(1, "Incorrect type"), "manifest: {:?}", said.get(&1));
        assert!(any(2, "Incorrect type"), "package.json: {:?}", said.get(&2));
    }

    /// Formatting goes by the file's own indentation: shfmt keeps a script's tabs.
    #[cfg(unix)]
    #[test]
    fn formatting_asks_for_the_files_own_indentation() {
        if !on_path("python3") {
            return;
        }
        const SERVER: &str = r#"
import json, sys
def send(msg):
    body = json.dumps(msg)
    sys.stdout.write("Content-Length: %d\r\n\r\n%s" % (len(body.encode()), body)); sys.stdout.flush()
def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        if line.lower().startswith(b"content-length:"): length = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(length))
while True:
    msg = read(); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/formatting":
        o = msg["params"]["options"]
        said = "%s %s" % (o["tabSize"], o["insertSpaces"])
        send({"jsonrpc": "2.0", "id": msg["id"], "result": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": said}]})
"#;
        let dir = tempfile::tempdir().unwrap();
        let tabs = dir.path().join("tabs.sh");
        let two = dir.path().join("two.sh");
        std::fs::write(&tabs, "if x; then\n\techo\nfi\n").unwrap();
        std::fs::write(&two, "if x; then\n  echo\nfi\n").unwrap();
        let buffers = vec![Buffer::open(&tabs).unwrap(), Buffer::open(&two).unwrap()];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "shell".to_string(),
            CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), SERVER.into()],
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        lsp.sync(&buffers, &[0, 1]);
        for (id, expected) in [(0, "4 False"), (1, "2 True")] {
            let mut said = None;
            for _ in 0..300 {
                if said.is_none() && lsp.request(Kind::Format, id, &buffers[id], 4).is_ok() {
                    said = Some(String::new());
                }
                let (events, _) = lsp.poll(&buffers);
                if let Some(Event::Format { edits, .. }) = events.into_iter().next() {
                    assert_eq!(edits[0].2, expected, "{id}");
                    said = Some(edits[0].2.clone());
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(said.as_deref(), Some(expected), "{id}");
        }
    }

    /// Docker's server is told the language of each file, and to keep its usage
    /// reports to itself: it sends them to Docker unless told not to.
    #[cfg(unix)]
    #[test]
    fn docker_files_go_to_one_server_that_sends_no_telemetry() {
        if !on_path("python3") {
            return;
        }
        const SERVER: &str = r#"
import json, sys
options = {}
def send(msg):
    body = json.dumps(msg)
    sys.stdout.write("Content-Length: %d\r\n\r\n%s" % (len(body.encode()), body)); sys.stdout.flush()
def read():
    length = 0
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        line = line.strip()
        if not line: break
        if line.lower().startswith(b"content-length:"): length = int(line.split(b":")[1])
    return json.loads(sys.stdin.buffer.read(length))
while True:
    msg = read(); method = msg.get("method")
    if method == "initialize":
        options = msg["params"].get("initializationOptions") or {}
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        doc = msg["params"]["textDocument"]
        said = "%s telemetry=%s" % (doc["languageId"], options.get("telemetry"))
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": doc["uri"], "diagnostics": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "severity": 2, "message": said}]}})
"#;
        let dir = tempfile::tempdir().unwrap();
        let dockerfile = dir.path().join("Dockerfile");
        let compose = dir.path().join("compose.yaml");
        std::fs::write(&dockerfile, "FROM alpine\n").unwrap();
        std::fs::write(&compose, "services: {}\n").unwrap();
        let buffers = vec![
            Buffer::open(&dockerfile).unwrap(),
            Buffer::open(&compose).unwrap(),
        ];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "docker".to_string(),
            CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), SERVER.into()],
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        assert!(lsp.sync(&buffers, &[0, 1]).is_empty());
        for _ in 0..300 {
            lsp.poll(&buffers);
            if lsp.diagnostics.len() == 2 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let said = |id: usize| lsp.diagnostics.get(&id).map(|d| d[0].message.clone());
        assert_eq!(said(0).as_deref(), Some("dockerfile telemetry=off"));
        assert_eq!(said(1).as_deref(), Some("dockercompose telemetry=off"));
        assert_eq!(lsp.clients.len(), 1);
    }

    // Windows may resolve `python3` to a Store placeholder; the protocol is the same.
    #[cfg(unix)]
    #[test]
    fn talks_to_a_server_end_to_end() {
        if !on_path("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, "fn main() {\n    x;\n}\n").unwrap();
        let buffers = vec![Buffer::open(&path).unwrap()];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "rust".to_string(),
            CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), FAKE_SERVER.into()],
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        assert!(lsp.sync(&buffers, &[0]).is_empty());
        let wait = |lsp: &mut Lsp, done: &dyn Fn(&Lsp, &[Event]) -> bool| -> Vec<Event> {
            let mut all = Vec::new();
            for _ in 0..200 {
                let (events, _) = lsp.poll(&buffers);
                all.extend(events);
                if done(lsp, &all) {
                    return all;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("server timed out");
        };
        wait(&mut lsp, &|lsp, _| lsp.diagnostics.contains_key(&0));
        let diagnostic = &lsp.diagnostics[&0][0];
        assert_eq!(
            (diagnostic.row, diagnostic.start, diagnostic.end),
            (0, 3, 7)
        );
        assert_eq!(lsp.status(0, &buffers[0]).as_deref(), Some("python3"));
        assert!(lsp.is_trigger(0, '.'));

        lsp.request(Kind::Hover, 0, &buffers[0], 4).unwrap();
        let events = wait(&mut lsp, &|_, e| !e.is_empty());
        assert!(matches!(&events[0], Event::Hover(text) if text == "fn **main**()"));

        lsp.request(Kind::Definition, 0, &buffers[0], 4).unwrap();
        let events = wait(&mut lsp, &|_, e| !e.is_empty());
        // Buffers resolve symlinks (macOS temp dirs live under /private/var).
        let real = path.canonicalize().unwrap();
        assert!(
            matches!(&events[0], Event::Definition { path: p, line: 1, character: 4, .. } if p == &real)
        );

        lsp.request(Kind::Completion, 0, &buffers[0], 4).unwrap();
        let events = wait(&mut lsp, &|_, e| !e.is_empty());
        let Event::Completion { items, .. } = &events[0] else {
            panic!()
        };
        assert_eq!(items[0].label, "print");
        assert_eq!(items[1].insert, "println!(x)");
        assert!(items[1].matches("prin"));

        lsp.request(Kind::Format, 0, &buffers[0], 4).unwrap();
        let events = wait(&mut lsp, &|_, e| !e.is_empty());
        let Event::Format { edits, .. } = &events[0] else {
            panic!()
        };
        assert_eq!(edits, &vec![(0, 2, "pub fn".to_string())]);
        // Organize imports: the code action's workspace edit, for this file.
        lsp.request(Kind::OrganizeImports, 0, &buffers[0], 4)
            .unwrap();
        let events = wait(&mut lsp, &|_, e| !e.is_empty());
        let Event::Organized { edits, buffer } = &events[0] else {
            panic!("{:?}", events.len())
        };
        assert_eq!(*buffer, 0);
        // Line 1 of "fn main() {\n    x;\n}\n" starts at char 12.
        assert_eq!(edits, &vec![(12, 12, "use std::fmt;\n".to_string())]);
    }

    /// Like Pyright: the server runs on the project's `.venv`, hears about packages
    /// installed and modules made while it runs, and starts again on a new venv.
    #[cfg(unix)]
    #[test]
    fn a_python_server_runs_on_the_venv_and_hears_about_new_files() {
        if !on_path("python3") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let site = root.join(".venv/lib/python3.14/site-packages");
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(root.join(".venv/pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let path = root.join("main.py");
        std::fs::write(&path, "import six\n").unwrap();
        let buffers = vec![Buffer::open(&path).unwrap()];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "python".to_string(),
            CommandPlugin {
                program: "python3".into(),
                args: vec!["-c".into(), FAKE_SERVER.into()],
            },
        );
        let mut lsp = Lsp::new(root.clone(), true, overrides);
        let mut notices = Vec::new();
        let mut wait = |lsp: &mut Lsp, done: &dyn Fn(&Lsp) -> bool| {
            for _ in 0..800 {
                lsp.sync(&buffers, &[0]);
                let (events, _) = lsp.poll(&buffers);
                notices.extend(events.into_iter().filter_map(|e| match e {
                    Event::Notice(text) => Some(text),
                    _ => None,
                }));
                if done(lsp) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("server timed out: {:?}", lsp.diagnostics.get(&0));
        };
        let says = |text: &'static str| {
            move |lsp: &Lsp| {
                lsp.diagnostics
                    .get(&0)
                    .is_some_and(|d| d.iter().any(|d| d.message.contains(text)))
            }
        };
        wait(&mut lsp, &|lsp| lsp.ready(0));
        assert!(
            lsp.report().contains("python3 ready on .venv"),
            "{}",
            lsp.report()
        );
        // `uv add six`, and a module an AI wrote.
        std::fs::write(site.join("six.py"), "").unwrap();
        std::fs::write(root.join("helper.py"), "").unwrap();
        wait(&mut lsp, &says("1:helper.py 1:six.py venv=.venv"));
        std::fs::remove_file(root.join("helper.py")).unwrap();
        wait(&mut lsp, &says("3:helper.py venv=.venv"));
        // The venv goes (`rm -rf .venv`): the server starts again without it.
        std::fs::remove_file(root.join(".venv/pyvenv.cfg")).unwrap();
        wait(&mut lsp, &|lsp| {
            lsp.report().contains("ready on the Python on PATH")
        });
        assert_eq!(
            notices,
            ["Python environment changed · python3 restarts on the Python on PATH (no .venv)"]
        );
    }

    #[test]
    fn a_rustup_proxy_without_its_component_says_how_to_add_it() {
        let detail = "error: Unknown binary 'rust-analyzer' in official toolchain 'stable-aarch64-apple-darwin'.";
        let message = stopped("rust-analyzer", "rust", detail);
        assert!(
            message.contains("rustup component add rust-analyzer"),
            "{message}"
        );
        assert!(message.contains(":lsp restart"));
        assert_eq!(
            stopped("gopls", "go", "crashed"),
            "gopls stopped: crashed · :lsp restart retries"
        );
    }

    #[test]
    fn missing_server_is_reported_once_with_install_hint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.go");
        std::fs::write(&path, "package main\n").unwrap();
        let buffers = vec![Buffer::open(&path).unwrap()];
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "go".to_string(),
            CommandPlugin {
                program: "/nonexistent/gopls".into(),
                args: Vec::new(),
            },
        );
        let mut lsp = Lsp::new(dir.path().into(), true, overrides);
        let events = lsp.sync(&buffers, &[0]);
        assert!(matches!(&events[..], [Event::Error(text)] if text.contains("gopls@latest")));
        assert!(lsp.sync(&buffers, &[0]).is_empty());
        assert!(lsp.request(Kind::Hover, 0, &buffers[0], 4).is_err());
    }
}

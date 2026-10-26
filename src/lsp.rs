//! A small Language Server Protocol client: diagnostics, hover, go-to-definition,
//! completion and formatting for Rust, Go, Python, C and C++.
//!
//! Each server runs as a child process speaking JSON-RPC over stdio. Reader and
//! writer threads keep the UI loop non-blocking; `Lsp::poll` drains replies on
//! every tick. Documents are synced in full (simple and accepted by every server).
use crate::{config::CommandPlugin, editor::Buffer};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
    Notice(String),
    Error(String),
}

struct Language {
    key: &'static str,
    id: &'static str,
}

/// The language key (`go`, `rust`, `python`, `c`) of a file Raido has a server for.
pub fn language_key(path: &Path) -> Option<&'static str> {
    language(path).map(|l| l.key)
}

fn language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let (key, id) = match ext.as_str() {
        "rs" => ("rust", "rust"),
        "go" => ("go", "go"),
        "py" | "pyi" => ("python", "python"),
        "c" | "h" => ("c", "c"),
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" => ("c", "cpp"),
        _ => return None,
    };
    Some(Language { key, id })
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
        _ => &[],
    }
}

fn install_hint(key: &str) -> &'static str {
    match key {
        "rust" => "rustup component add rust-analyzer",
        "go" => "go install golang.org/x/tools/gopls@latest",
        "python" => "npm install -g pyright  (or: pip install python-lsp-server)",
        "c" => "install clangd (Arch: pacman -S clang · Debian/Ubuntu: apt install clangd · macOS: xcode-select --install)",
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
    send: mpsc::Sender<Vec<u8>>,
    receive: mpsc::Receiver<Incoming>,
    next_id: u64,
    ready: bool,
    queue: Vec<Value>,
    utf32: bool,
    triggers: Vec<String>,
    pending: HashMap<u64, (Kind, usize)>,
}

impl Client {
    fn spawn(program: &str, args: &[String], root: &Path) -> Result<Self> {
        let mut child = Command::new(crate::platform::resolve(program))
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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
            send,
            receive,
            next_id: 1,
            ready: false,
            queue: Vec::new(),
            utf32: false,
            triggers: Vec::new(),
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
        }
    }

    /// Stop every server; they restart when their files are next shown.
    pub fn restart(&mut self) {
        self.clients.clear();
        self.failed.clear();
        self.docs.clear();
        self.diagnostics.clear();
        self.announced.clear();
    }

    fn server(&self, key: &str) -> Option<(String, Vec<String>)> {
        if let Some(custom) = self.overrides.get(key) {
            return Some((custom.program.clone(), custom.args.clone()));
        }
        defaults(key)
            .iter()
            .find(|(program, _)| on_path(program))
            .map(|(program, args)| {
                (
                    program.to_string(),
                    args.iter().map(|a| a.to_string()).collect(),
                )
            })
    }

    /// Start a server for `key` if needed; returns a message the first time it fails.
    fn ensure(&mut self, key: &'static str) -> Option<String> {
        if self.clients.contains_key(key) || self.failed.contains_key(key) {
            return None;
        }
        let Some((program, args)) = self.server(key) else {
            let reason = format!(
                "No {key} language server found · install: {}",
                install_hint(key)
            );
            self.failed.insert(key, reason.clone());
            return Some(reason);
        };
        match Client::spawn(&program, &args, &self.root) {
            Ok(client) => {
                let root = uri(&self.root);
                let name = self
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "workspace".into());
                let params = json!({
                    "processId": std::process::id(),
                    "clientInfo": { "name": "raido", "version": env!("CARGO_PKG_VERSION") },
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
        for &id in visible {
            let Some(buffer) = buffers.get(id) else {
                continue;
            };
            let Some(path) = buffer.path.as_deref() else {
                continue;
            };
            let Some(lang) = language(path) else {
                continue;
            };
            if let Some(reason) = self.ensure(lang.key) {
                if self.announced.insert(lang.key) {
                    events.push(Event::Error(reason));
                }
                continue;
            }
            let uri = uri(path);
            if self.docs.get(&id).is_some_and(|d| d.uri != uri) {
                self.docs.remove(&id);
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
        let lang = buffer
            .path
            .as_deref()
            .and_then(language)
            .context("No language server for this file type (Rust, Go, Python, C, C++)")?;
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
            Kind::Format => client.request(
                "textDocument/formatting",
                json!({ "textDocument": document, "options": { "tabSize": tab_width, "insertSpaces": true } }),
            ),
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
        let lang = language(buffer.path.as_deref()?)?;
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
        for key in ["rust", "go", "python", "c"] {
            let state = if let Some(client) = self.clients.get(key) {
                format!(
                    "{} {}",
                    client.program,
                    if client.ready { "ready" } else { "starting" }
                )
            } else if self.failed.contains_key(key) {
                "unavailable".into()
            } else if let Some((program, _)) = self.server(key) {
                format!("{program} (starts on first file)")
            } else {
                format!("not installed — {}", install_hint(key))
            };
            parts.push(format!(
                "{}: {state}",
                if key == "c" { "c/c++" } else { key }
            ));
        }
        parts.join(" · ")
    }

    /// Drain server messages; buffers are needed to convert positions.
    pub fn poll(&mut self, buffers: &[Buffer]) -> (Vec<Event>, bool) {
        let mut events = Vec::new();
        let mut changed = false;
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
        }
        (events, changed)
    }

    fn handle(
        &mut self,
        key: &'static str,
        message: Value,
        buffers: &[Buffer],
        events: &mut Vec<Event>,
    ) -> bool {
        let Some(client) = self.clients.get_mut(key) else {
            return false;
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match (method, id) {
            // A request from the server: answer so it never waits on us.
            (Some(method), Some(id)) => {
                let result = if method == "workspace/configuration" {
                    let count = message
                        .pointer("/params/items")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; count])
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
                    client.ready = true;
                    client
                        .write(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
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
                let utf32 = client.utf32;
                if let Some(buffer) = buffers.get(buffer_id) {
                    events.extend(reply(kind, buffer_id, buffer, &message["result"], utf32));
                }
                false
            }
            _ => false,
        }
    }
}

fn reply(kind: Kind, id: usize, buffer: &Buffer, result: &Value, utf32: bool) -> Option<Event> {
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
                    Some((
                        sort,
                        Completion {
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
            items.sort_by(|a, b| a.0.cmp(&b.0));
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
    msg = read(); method = msg.get("method"); id = msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {"positionEncoding": "utf-16", "completionProvider": {"triggerCharacters": ["."]}}}})
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        uri = msg["params"]["textDocument"]["uri"]
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
        assert_eq!(language(Path::new("a.rs")).unwrap().key, "rust");
        assert_eq!(language(Path::new("a.hpp")).unwrap().id, "cpp");
        assert_eq!(language(Path::new("a.py")).unwrap().key, "python");
        assert!(language(Path::new("a.md")).is_none());
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

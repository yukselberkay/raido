//! Claude Code's IDE connection, as its VS Code extension has it: Claude Code in a
//! Raido pane knows the file you're on and the lines you selected, and can ask
//! Raido for open files and problems, or to open a file.
//!
//! Raido listens on 127.0.0.1 for Claude's WebSocket, writes the port and a fresh
//! secret to Claude's `ide/<port>.lock`, and gives its panes the port
//! (`CLAUDE_CODE_SSE_PORT`). Messages are MCP's JSON-RPC. The protocol is Claude's
//! own and not published; this follows what its VS Code extension does.

use crate::{
    app::{App, Pane},
    lsp, websocket,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU16, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

pub const PORT_VAR: &str = "CLAUDE_CODE_SSE_PORT";
pub const ENABLE_VAR: &str = "ENABLE_IDE_INTEGRATION";
/// A moved cursor is reported once it has stayed put this long.
const SETTLE: Duration = Duration::from_millis(100);

static PORT: AtomicU16 = AtomicU16::new(0);

/// The port Claude Code in this Raido's panes connects to.
pub fn port() -> Option<u16> {
    match PORT.load(Ordering::Relaxed) {
        0 => None,
        port => Some(port),
    }
}

/// Where you are in an editor: the file, and the selection or the cursor (lines
/// and characters counted from 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spot {
    pub buffer: usize,
    pub path: PathBuf,
    pub start: (usize, usize),
    pub end: (usize, usize),
    /// The selected chars, when something is selected.
    pub chars: Option<(usize, usize)>,
}

impl Spot {
    /// The focused editor's spot; none for other panes and unsaved buffers.
    pub fn of(app: &App) -> Option<Spot> {
        let id = app.buffer_id()?;
        let b = &app.buffers[id];
        let path = b.path.clone()?;
        let at = |char: usize| {
            let line = b.text.char_to_line(char);
            (line, char - b.text.line_to_char(line))
        };
        Some(match app.selection() {
            Some((start, end)) if end > start => {
                let mut last = at(end);
                // Whole lines end at the start of the next one: the end of the last.
                if last.1 == 0 && last.0 > at(start).0 {
                    last = (last.0 - 1, b.line_len(last.0 - 1));
                }
                Spot {
                    buffer: id,
                    path,
                    start: at(start),
                    end: last,
                    chars: Some((start, end)),
                }
            }
            _ => {
                let here = (b.row, b.col.min(b.line_len(b.row)));
                Spot {
                    buffer: id,
                    path,
                    start: here,
                    end: here,
                    chars: None,
                }
            }
        })
    }
    /// The selected text, from the buffer as it is now.
    pub fn text(&self, app: &App) -> String {
        match (self.chars, app.buffers.get(self.buffer)) {
            (Some((start, end)), Some(b)) => {
                let length = b.text.len_chars();
                b.slice(start.min(length), end.min(length))
            }
            _ => String::new(),
        }
    }
    fn json(&self, app: &App) -> Value {
        let position =
            |(line, character): (usize, usize)| json!({"line": line, "character": character});
        json!({
            "text": self.text(app),
            "filePath": self.path.to_string_lossy(),
            "fileUrl": lsp::uri(&self.path),
            "selection": {
                "start": position(self.start),
                "end": position(self.end),
                "isEmpty": self.chars.is_none(),
            },
        })
    }
}

struct Client {
    id: usize,
    stream: Arc<Mutex<TcpStream>>,
    /// Past MCP's `initialize`: ready to hear where you are.
    ready: bool,
}

enum Incoming {
    Connected(Client),
    Message(usize, Value),
    Closed(usize),
}

pub struct Server {
    incoming: Receiver<Incoming>,
    clients: Vec<Client>,
    lock: PathBuf,
    port: u16,
    /// The spot Claude was last told about, and a newer one waiting to settle.
    sent: Option<Spot>,
    pending: Option<(Spot, Instant)>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock);
        let _ = PORT.compare_exchange(self.port, 0, Ordering::Relaxed, Ordering::Relaxed);
        for client in &self.clients {
            if let Ok(stream) = client.stream.lock() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// Claude Code's folder of IDE lock files: `$CLAUDE_CONFIG_DIR/ide`, else
/// `~/.claude/ide`.
fn lock_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => directories::BaseDirs::new()?.home_dir().join(".claude"),
    };
    Some(base.join("ide"))
}

/// Listen for Claude Code in this project, when it's installed.
pub fn listen(root: &Path) -> Result<Server> {
    let dir = lock_dir().context("No home folder")?;
    let installed = dir.parent().is_some_and(Path::exists) || lsp::on_path("claude");
    if !installed {
        bail!("Claude Code isn't installed");
    }
    listen_in(&dir, root)
}

/// Listen on a free port of 127.0.0.1, announced with a fresh secret in `dir`.
fn listen_in(dir: &Path, root: &Path) -> Result<Server> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = crate::remote::secret();
    let lock = dir.join(format!("{port}.lock"));
    let content = json!({
        "pid": std::process::id(),
        "workspaceFolders": [root.to_string_lossy()],
        "ideName": "Raido",
        "transport": "ws",
        "authToken": token,
    });
    write_private(dir, &lock, &content.to_string())?;
    let (sender, incoming) = mpsc::channel();
    std::thread::spawn(move || {
        for (id, stream) in listener.incoming().enumerate() {
            let Ok(stream) = stream else { continue };
            let (token, sender) = (token.clone(), sender.clone());
            std::thread::spawn(move || serve(id, stream, &token, &sender));
        }
    });
    PORT.store(port, Ordering::Relaxed);
    Ok(Server {
        incoming,
        clients: Vec::new(),
        lock,
        port,
        sent: None,
        pending: None,
    })
}

/// Write `content` to `path` for your eyes only: the folder 0700, the file 0600,
/// put in place whole.
fn write_private(dir: &Path, path: &Path, content: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("Cannot create {}", dir.display()))?;
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        options.mode(0o600);
    }
    options.open(&temporary)?.write_all(content.as_bytes())?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Two secrets compared in the same time whatever their difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |all, (x, y)| all | (x ^ y))
            == 0
}

/// One connection: the handshake (with the secret), then its messages to the main
/// loop until it closes.
fn serve(id: usize, mut stream: TcpStream, token: &str, sender: &Sender<Incoming>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(headers) = websocket::read_request(&mut stream) else {
        return;
    };
    let allowed = headers
        .get("x-claude-code-ide-authorization")
        .is_some_and(|given| same(given, token))
        && headers
            .get("upgrade")
            .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    let Some(key) = headers.get("sec-websocket-key").filter(|_| allowed) else {
        let _ = stream.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    };
    let protocol = headers
        .get("sec-websocket-protocol")
        .and_then(|p| p.split(',').next())
        .map(str::trim);
    if stream
        .write_all(websocket::accept(key, protocol).as_bytes())
        .is_err()
    {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let writer = Arc::new(Mutex::new(writer));
    let client = Client {
        id,
        stream: writer.clone(),
        ready: false,
    };
    if sender.send(Incoming::Connected(client)).is_err() {
        return;
    }
    loop {
        let message = websocket::read_message(&mut stream, |payload| {
            if let Ok(mut out) = writer.lock() {
                let _ = websocket::write_frame(&mut *out, websocket::PONG, payload);
            }
        });
        match message {
            Ok(websocket::Message::Text(text)) => {
                let Ok(value) = serde_json::from_str(&text) else {
                    continue;
                };
                if sender.send(Incoming::Message(id, value)).is_err() {
                    break;
                }
            }
            Ok(websocket::Message::Binary) => {}
            Ok(websocket::Message::Close) | Err(_) => break,
        }
    }
    if let Ok(mut out) = writer.lock() {
        let _ = websocket::write_frame(&mut *out, websocket::CLOSE, &[]);
    }
    let _ = sender.send(Incoming::Closed(id));
}

impl Server {
    pub fn port(&self) -> u16 {
        self.port
    }
    /// How many Claude Codes are connected.
    pub fn connected(&self) -> usize {
        self.clients.len()
    }
    /// Messages since the last call, and how many Claudes connected meanwhile.
    fn poll(&mut self) -> (Vec<(usize, Value)>, usize) {
        let mut messages = Vec::new();
        let mut joined = 0;
        for incoming in self.incoming.try_iter().collect::<Vec<_>>() {
            match incoming {
                Incoming::Connected(client) => {
                    self.clients.push(client);
                    joined += 1;
                }
                Incoming::Message(id, value) => {
                    let method = value["method"].as_str().unwrap_or_default();
                    if matches!(method, "initialize" | "notifications/initialized") {
                        // Initialized, the newcomer hears where you are.
                        for client in self.clients.iter_mut().filter(|c| c.id == id && !c.ready) {
                            client.ready = true;
                            self.sent = None;
                        }
                    }
                    messages.push((id, value));
                }
                Incoming::Closed(id) => self.clients.retain(|c| c.id != id),
            }
        }
        (messages, joined)
    }
    fn send(&mut self, id: usize, message: &Value) {
        let text = message.to_string();
        let failed = self.clients.iter().filter(|c| c.id == id).any(|client| {
            client
                .stream
                .lock()
                .map(|mut out| websocket::write_text(&mut *out, &text).is_err())
                .unwrap_or(true)
        });
        if failed {
            self.clients.retain(|c| c.id != id);
        }
    }
    /// Send to every initialized Claude.
    fn broadcast(&mut self, message: &Value) {
        let ids: Vec<usize> = self
            .clients
            .iter()
            .filter(|c| c.ready)
            .map(|c| c.id)
            .collect();
        for id in ids {
            self.send(id, message);
        }
    }
    /// Whether to tell Claude about `spot` now: once it's new and has settled.
    fn due(&mut self, spot: Option<&Spot>, now: Instant) -> bool {
        let Some(spot) = spot.filter(|_| self.clients.iter().any(|c| c.ready)) else {
            return false;
        };
        if self.sent.as_ref() == Some(spot) {
            self.pending = None;
            return false;
        }
        match &self.pending {
            Some((waiting, since)) if waiting == spot => {
                if now.duration_since(*since) < SETTLE {
                    return false;
                }
                self.sent = Some(spot.clone());
                self.pending = None;
                true
            }
            _ => {
                self.pending = Some((spot.clone(), now));
                false
            }
        }
    }
}

/// Answer Claude's requests and tell it where you are; run from the main loop.
pub fn tick(app: &mut App) {
    // The focused editor's spot; any other pane (Claude's, as you type there) keeps
    // the last one, as VS Code keeps its last editor active.
    if let Some(spot) = Spot::of(app) {
        app.editor_spot = Some(spot);
    }
    let Some(server) = app.ide.as_mut() else {
        return;
    };
    let (messages, joined) = server.poll();
    if joined > 0 {
        app.notice("Claude Code connected: it sees the file you're on and the lines you select");
    }
    for (client, message) in messages {
        if let Some(reply) = answer(app, &message) {
            if let Some(server) = app.ide.as_mut() {
                server.send(client, &reply);
            }
        }
    }
    let spot = app.editor_spot.clone();
    let due = app
        .ide
        .as_mut()
        .is_some_and(|server| server.due(spot.as_ref(), Instant::now()));
    if let (true, Some(spot)) = (due, spot) {
        let message =
            json!({"jsonrpc": "2.0", "method": "selection_changed", "params": spot.json(app)});
        if let Some(server) = app.ide.as_mut() {
            server.broadcast(&message);
        }
    }
}

/// The reply to a request; notifications get none.
fn answer(app: &mut App, message: &Value) -> Option<Value> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str)?;
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": {
                "logging": {},
                "prompts": {"listChanged": true},
                "tools": {"listChanged": true},
            },
            "serverInfo": {"name": "raido", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "prompts/list" => Ok(json!({"prompts": []})),
        "resources/list" => Ok(json!({"resources": []})),
        "tools/call" => call(app, &params),
        _ => Err((-32601, format!("Unknown method {method}"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

/// The tools Raido offers, as VS Code's extension names them. Claude shows its
/// proposed edits in its own pane (no `openDiff`); Raido's review (Ctrl-b v) has
/// them afterwards.
fn tools() -> Value {
    let nothing = json!({"type": "object", "properties": {}});
    let file = |what: &str| {
        json!({
            "type": "object",
            "properties": {"filePath": {"type": "string", "description": what}},
            "required": ["filePath"],
        })
    };
    json!([
        {
            "name": "openFile",
            "description": "Open a file in the editor, the cursor on startText if given",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filePath": {"type": "string", "description": "Path to the file to open"},
                    "preview": {"type": "boolean"},
                    "startText": {"type": "string", "description": "Text to put the cursor on"},
                    "endText": {"type": "string"},
                    "selectToEndOfLine": {"type": "boolean"},
                    "makeFrontmost": {"type": "boolean"},
                },
                "required": ["filePath"],
            },
        },
        {"name": "getCurrentSelection", "description": "Get the selection (or cursor) in the editor", "inputSchema": nothing},
        {"name": "getLatestSelection", "description": "Get the latest selection in the editor, even while another pane is focused", "inputSchema": nothing},
        {"name": "getOpenEditors", "description": "Get the files open in the editor", "inputSchema": nothing},
        {"name": "getWorkspaceFolders", "description": "Get the project folder open in the editor", "inputSchema": nothing},
        {
            "name": "getDiagnostics",
            "description": "Get errors and warnings from the editor's language servers",
            "inputSchema": {
                "type": "object",
                "properties": {"uri": {"type": "string", "description": "A file's URI; every file when left out"}},
            },
        },
        {"name": "checkDocumentDirty", "description": "Check if a document has unsaved changes", "inputSchema": file("Path to the file to check")},
        {"name": "saveDocument", "description": "Save a document's unsaved changes", "inputSchema": file("Path to the file to save")},
        {"name": "closeAllDiffTabs", "description": "Close all diff tabs in the editor", "inputSchema": nothing},
    ])
}

type Failure = (i64, String);

fn call(app: &mut App, params: &Value) -> Result<Value, Failure> {
    let name = params["name"].as_str().unwrap_or_default();
    let args = &params["arguments"];
    let text = match name {
        "getCurrentSelection" | "getLatestSelection" => match app.editor_spot.clone() {
            Some(spot) => {
                let mut value = spot.json(app);
                value["success"] = json!(true);
                value.to_string()
            }
            None => json!({"success": false, "message": "No active editor found"}).to_string(),
        },
        "getOpenEditors" => {
            let active = app.editor_spot.as_ref().map(|s| s.buffer);
            let tabs: Vec<Value> = app
                .buffers
                .iter()
                .enumerate()
                .filter_map(|(id, b)| {
                    Some(json!({
                        "uri": lsp::uri(b.path.as_ref()?),
                        "isActive": active == Some(id),
                        "label": b.name(),
                        "languageId": app.syntax.language(b).to_lowercase(),
                        "isDirty": b.dirty,
                    }))
                })
                .collect();
            json!({"tabs": tabs}).to_string()
        }
        "getWorkspaceFolders" => {
            let name = app.root.file_name().unwrap_or_default().to_string_lossy();
            json!({
                "success": true,
                "folders": [{"name": name, "uri": lsp::uri(&app.root), "path": app.root.to_string_lossy()}],
                "rootPath": app.root.to_string_lossy(),
            })
            .to_string()
        }
        "getDiagnostics" => diagnostics(app, args["uri"].as_str()).to_string(),
        "openFile" => open_file(app, args)?,
        "checkDocumentDirty" | "saveDocument" => {
            let path = args["filePath"].as_str().unwrap_or_default();
            let Some(id) = find(app, path) else {
                return Ok(content(
                    json!({"success": false, "message": format!("Document not open: {path}")})
                        .to_string(),
                ));
            };
            if name == "checkDocumentDirty" {
                json!({"success": true, "filePath": path, "isDirty": app.buffers[id].dirty, "isUntitled": false})
                    .to_string()
            } else {
                app.buffers[id]
                    .save(None)
                    .map_err(|e| (-32000, format!("{e:#}")))?;
                app.lsp.saved(id, &app.buffers[id]);
                if let Some(saved) = app.buffers[id].path.clone() {
                    app.changes.saved(&saved);
                }
                json!({"success": true, "filePath": path, "saved": true, "message": "Document saved successfully"})
                    .to_string()
            }
        }
        "close_tab" => "TAB_CLOSED".into(),
        "closeAllDiffTabs" => "CLOSED_0_DIFF_TABS".into(),
        _ => return Err((-32602, format!("Unknown tool {name}"))),
    };
    Ok(content(text))
}

fn content(text: String) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn absolute(app: &App, path: &str) -> PathBuf {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        app.root.join(path)
    };
    path.canonicalize().unwrap_or(path)
}

/// The buffer holding the file at `path`.
fn find(app: &App, path: &str) -> Option<usize> {
    let path = absolute(app, path);
    app.buffers
        .iter()
        .position(|b| b.path.as_ref() == Some(&path))
}

fn diagnostics(app: &App, uri: Option<&str>) -> Value {
    let mut files: Vec<(String, Value)> = app
        .lsp
        .diagnostics
        .iter()
        .filter_map(|(id, list)| {
            let file = lsp::uri(app.buffers.get(*id)?.path.as_ref()?);
            if uri.is_some_and(|wanted| wanted != file) || list.is_empty() {
                return None;
            }
            let list: Vec<Value> = list
                .iter()
                .map(|d| {
                    json!({
                        "message": d.message,
                        "severity": match d.severity {
                            1 => "Error",
                            2 => "Warning",
                            3 => "Information",
                            _ => "Hint",
                        },
                        "range": {
                            "start": {"line": d.row, "character": d.start},
                            "end": {"line": d.row, "character": d.end},
                        },
                    })
                })
                .collect();
            Some((file.clone(), json!({"uri": file, "diagnostics": list})))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Array(files.into_iter().map(|(_, file)| file).collect())
}

/// Open a file Claude names; the cursor goes on `startText`. Typing in Claude's
/// pane, you stay there.
fn open_file(app: &mut App, args: &Value) -> Result<String, Failure> {
    let path = args["filePath"]
        .as_str()
        .ok_or((-32602, "filePath is required".to_string()))?;
    let path = absolute(app, path);
    let back = app.focused;
    let stay = !matches!(
        app.panes.get(&back),
        Some(Pane::Editor(_)) | Some(Pane::Preview(_))
    );
    app.open(&path).map_err(|e| (-32000, format!("{e:#}")))?;
    let opened = app.buffer_id();
    if let (Some(id), Some(start)) = (opened, args["startText"].as_str()) {
        let b = &mut app.buffers[id];
        let text = b.text.to_string();
        if let Some(byte) = text.find(start).filter(|_| !start.is_empty()) {
            let char = text[..byte].chars().count();
            b.row = b.text.char_to_line(char);
            b.col = char - b.text.line_to_char(b.row);
        }
    }
    if stay && app.panes.contains_key(&back) {
        app.focused = back;
    }
    app.notice(format!(
        "Claude Code opened {}",
        crate::workspace::printable(&path.file_name().unwrap_or_default().to_string_lossy())
    ));
    Ok(if args["makeFrontmost"].as_bool() == Some(false) {
        let lines = opened.map_or(0, |id| app.buffers[id].lines());
        json!({"success": true, "filePath": path.to_string_lossy(), "lineCount": lines}).to_string()
    } else {
        format!("Opened file: {}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// A WebSocket client as Claude's: the handshake with a secret, masked frames.
    struct Claude(TcpStream);

    impl Claude {
        fn connect(port: u16, token: &str) -> std::io::Result<Self> {
            let mut stream = TcpStream::connect(("127.0.0.1", port))?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            write!(
                stream,
                "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mcp\r\nx-claude-code-ide-authorization: {token}\r\n\r\n"
            )?;
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte)?;
                head.push(byte[0]);
            }
            let head = String::from_utf8_lossy(&head);
            if !head.starts_with("HTTP/1.1 101") {
                return Err(std::io::Error::other(
                    head.lines().next().unwrap_or("").to_string(),
                ));
            }
            assert!(head.contains("Sec-WebSocket-Protocol: mcp"));
            Ok(Self(stream))
        }
        fn send(&mut self, message: Value) {
            let payload = message.to_string().into_bytes();
            let mask = [7, 1, 9, 3];
            let mut frame = vec![0x81];
            if payload.len() < 126 {
                frame.push(0x80 | payload.len() as u8);
            } else {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            frame.extend_from_slice(&mask);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.0.write_all(&frame).unwrap();
        }
        fn receive(&mut self) -> Value {
            match websocket::read_message(&mut self.0, |_| {}).unwrap() {
                websocket::Message::Text(text) => serde_json::from_str(&text).unwrap(),
                _ => panic!("not text"),
            }
        }
    }

    /// Run Raido's main-loop side until `done`.
    fn until(app: &mut App, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out");
            tick(app);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Tick until Claude has a message.
    fn reply(app: &mut App, claude: &mut Claude) -> Value {
        claude.0.set_nonblocking(true).expect("nonblocking");
        let start = Instant::now();
        loop {
            tick(app);
            let mut byte = [0];
            match claude.0.peek(&mut byte) {
                Ok(_) => break,
                Err(_) => {
                    assert!(start.elapsed() < Duration::from_secs(5), "no reply");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        claude.0.set_nonblocking(false).unwrap();
        claude.receive()
    }

    #[test]
    fn claude_connects_with_the_secret_and_hears_where_you_are() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("main.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hi\");\n}\n").unwrap();
        let mut app = App::new(root.clone(), Some(path.clone()), false).unwrap();
        let locks = root.join("ide");
        let server = listen_in(&locks, &root).unwrap();
        let port = server.port();
        app.ide = Some(server);
        // The lock file tells Claude the port, the project and the secret, for
        // your eyes only.
        let lock_path = locks.join(format!("{port}.lock"));
        let lock: Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(lock["ideName"], "Raido");
        assert_eq!(lock["transport"], "ws");
        assert_eq!(lock["workspaceFolders"][0], root.to_string_lossy().as_ref());
        let token = lock["authToken"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&lock_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(super::port(), Some(port));
        // Without the secret: turned away.
        let wrong = Claude::connect(port, "0123456789abcdef0123456789abcdef");
        assert!(wrong.is_err_and(|e| e.to_string().contains("401")));
        let mut claude = Claude::connect(port, &token).unwrap();
        until(&mut app, |app| app.ide.as_ref().unwrap().connected() == 1);
        assert!(app.message.contains("Claude Code connected"));
        claude.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}}}));
        let init = reply(&mut app, &mut claude);
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "raido");
        claude.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        // The newcomer hears where you are: the cursor at the start of main.rs.
        let note = reply(&mut app, &mut claude);
        assert_eq!(note["method"], "selection_changed");
        assert_eq!(note["params"]["filePath"], path.to_string_lossy().as_ref());
        assert_eq!(note["params"]["selection"]["isEmpty"], true);
        assert_eq!(note["params"]["selection"]["start"]["line"], 0);
        // Select line 2 (V): Claude hears the lines and the text.
        let id = app.buffer_id().unwrap();
        app.buffers[id].row = 1;
        app.event(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('V'),
                crossterm::event::KeyModifiers::SHIFT,
            ),
        ));
        let note = reply(&mut app, &mut claude);
        let selection = &note["params"]["selection"];
        assert_eq!(note["params"]["text"], "    println!(\"hi\");\n");
        assert_eq!(
            (
                selection["start"]["line"].as_u64(),
                selection["end"]["line"].as_u64()
            ),
            (Some(1), Some(1))
        );
        assert_eq!(selection["end"]["character"], 19, "the end of that line");
        assert_eq!(selection["isEmpty"], false);
        // Focus leaves the editor (for Claude's pane): the selection still counts.
        let editor = app.focused;
        app.focused = usize::MAX;
        claude.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "getLatestSelection", "arguments": {}}}));
        let latest = reply(&mut app, &mut claude);
        let latest: Value =
            serde_json::from_str(latest["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(latest["success"], true);
        assert_eq!(latest["text"], "    println!(\"hi\");\n");
        app.focused = editor;
        // Tools: the list, open editors, the project, unsaved changes.
        claude.send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}));
        let list = reply(&mut app, &mut claude);
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"getDiagnostics") && names.contains(&"openFile"));
        assert!(
            !names.contains(&"openDiff"),
            "edits stay in Claude's own pane"
        );
        let call = |claude: &mut Claude, app: &mut App, id: u64, name: &str, args: Value| {
            claude.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}}));
            let answer = reply(app, claude);
            let text = answer["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string();
            serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
        };
        let editors = call(&mut claude, &mut app, 4, "getOpenEditors", json!({}));
        assert_eq!(editors["tabs"][0]["label"], "main.rs");
        assert_eq!(editors["tabs"][0]["isActive"], true);
        assert_eq!(editors["tabs"][0]["languageId"], "rust");
        let folders = call(&mut claude, &mut app, 5, "getWorkspaceFolders", json!({}));
        assert_eq!(folders["rootPath"], root.to_string_lossy().as_ref());
        let file = path.to_string_lossy().to_string();
        app.buffers[id].insert("// ");
        let dirty = call(
            &mut claude,
            &mut app,
            6,
            "checkDocumentDirty",
            json!({"filePath": file}),
        );
        assert_eq!(dirty["isDirty"], true);
        let saved = call(
            &mut claude,
            &mut app,
            7,
            "saveDocument",
            json!({"filePath": file}),
        );
        assert_eq!(saved["saved"], true);
        assert!(std::fs::read_to_string(&path).unwrap().contains("// "));
        // Opening a file from Claude's pane keeps you typing there.
        let other = root.join("lib.rs");
        std::fs::write(&other, "pub fn add() {}\n").unwrap();
        let shell = app.add_pane(Pane::Ai, crate::layout::Axis::Horizontal);
        app.focused = shell;
        let opened = call(
            &mut claude,
            &mut app,
            8,
            "openFile",
            json!({"filePath": other.to_string_lossy(), "startText": "add"}),
        );
        assert!(opened.as_str().unwrap().starts_with("Opened file:"));
        assert_eq!(app.focused, shell, "focus stays in Claude's pane");
        let b = app
            .buffers
            .iter()
            .find(|b| b.path.as_ref() == Some(&other))
            .unwrap();
        assert_eq!((b.row, b.col), (0, 7), "the cursor on `add`");
        let unknown = {
            claude.send(json!({"jsonrpc": "2.0", "id": 9, "method": "nope"}));
            reply(&mut app, &mut claude)
        };
        assert_eq!(unknown["error"]["code"], -32601);
        // Raido gone: the lock file goes with it.
        app.ide = None;
        assert!(!lock_path.exists());
        assert_eq!(super::port(), None);
    }
}

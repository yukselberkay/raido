//! Your own model server: Ollama or vLLM on another machine (or this one). The AI
//! launcher's Local LLM and OpenCode run on its models, and the review's `local`
//! explanations come from it, while the agents and your code stay here.
//!
//! ```toml
//! [model_server]
//! url = "http://gpu-box:11434"   # Ollama; vLLM: "http://gpu-box:8000"
//! kind = "auto"                  # or "ollama", "vllm" (any OpenAI-API server)
//! tunnel = "gpu-box"             # reach it through SSH; it needn't listen on the network
//! key_env = "VLLM_API_KEY"       # the variable holding its API key, if it has one
//! ```
//!
//! Both speak the OpenAI API (`/v1/models`, `/v1/chat/completions`), which Raido
//! reaches with `curl`; `ollama run` chats with an Ollama server through
//! `OLLAMA_HOST`, and OpenCode gets the server's address in its config.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

/// `[model_server]` in config.toml.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// Where the server listens, as seen from here, or from the tunnel's host.
    pub url: String,
    pub kind: Kind,
    /// SSH destination (`gpu-box`, `me@10.0.0.5`) to forward the server's port
    /// through; empty for none.
    pub tunnel: String,
    /// Environment variable that holds the server's API key; empty for none.
    pub key_env: String,
}

/// What runs on the server.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Ask the server: Ollama answers `/api/version`.
    #[default]
    Auto,
    Ollama,
    /// vLLM, or any server with the OpenAI API (llama.cpp's, LM Studio, SGLang).
    #[serde(alias = "openai")]
    Vllm,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Ollama => "Ollama",
            _ => "vLLM",
        }
    }
}

/// A model the server has.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    /// Its context length, when the server says (vLLM's `max_model_len`).
    pub context: Option<u64>,
}

/// The server, ready to use.
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// How to reach it from here (a tunnel's local end, if any), without `/v1`.
    pub root: String,
    pub kind: Kind,
    /// Its name in messages: the tunnel's host, else the URL's.
    pub name: String,
    /// The API key read from `key_env`.
    pub key: Option<String>,
}

impl Endpoint {
    /// The OpenAI API's base: `http://host:port/v1`.
    pub fn api(&self) -> String {
        format!("{}/v1", self.root)
    }
}

/// A URL taken apart: what Raido needs from it.
#[derive(Debug, PartialEq)]
struct Url {
    /// `scheme://host:port` with any path before `/v1`, no trailing slash.
    root: String,
    host: String,
    port: u16,
}

fn parse_url(url: &str) -> Result<Url> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix("/v1").unwrap_or(url);
    let (scheme, rest) = url
        .split_once("://")
        .filter(|(s, _)| matches!(*s, "http" | "https"))
        .with_context(|| {
            format!("model_server url must start with http:// or https:// (got \"{url}\")")
        })?;
    let authority = rest.split('/').next().unwrap_or_default();
    let (host, port) = match authority.strip_prefix('[') {
        // [::1]:8000
        Some(v6) => {
            let (host, after) = v6.split_once(']').context("unclosed [ in url")?;
            (host.to_string(), after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port)),
            None => (authority.to_string(), None),
        },
    };
    if host.is_empty() {
        bail!("model_server url has no host (got \"{url}\")");
    }
    let port = match port {
        Some(port) => port
            .parse()
            .with_context(|| format!("bad port in model_server url \"{url}\""))?,
        None if scheme == "https" => 443,
        None => 80,
    };
    Ok(Url {
        root: url.to_string(),
        host,
        port,
    })
}

/// The settings, read cheaply from anywhere.
static SETTINGS: RwLock<Option<Settings>> = RwLock::new(None);
/// What using the server set up: the tunnel, the kind it said it is, its models.
static LIVE: Mutex<Live> = Mutex::new(Live {
    tunnel: None,
    kind: None,
    models: Vec::new(),
});

struct Live {
    tunnel: Option<Tunnel>,
    kind: Option<Kind>,
    models: Vec<Model>,
}

/// Use these settings (from config.toml), or none. Closes a tunnel already open.
pub fn configure(settings: Option<Settings>) {
    let settings = settings.filter(|s| !s.url.trim().is_empty());
    *SETTINGS.write().unwrap_or_else(|e| e.into_inner()) = settings;
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.tunnel = None;
    live.kind = None;
    live.models.clear();
}

/// Whether a model server is set.
pub fn configured() -> bool {
    settings().is_some()
}

fn settings() -> Option<Settings> {
    SETTINGS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The server's name for messages ("gpu-box"), without connecting.
pub fn name() -> Option<String> {
    let settings = settings()?;
    Some(display_name(&settings))
}

fn display_name(settings: &Settings) -> String {
    if !settings.tunnel.trim().is_empty() {
        let host = settings.tunnel.trim();
        return host.rsplit('@').next().unwrap_or(host).to_string();
    }
    parse_url(&settings.url).map_or_else(|_| settings.url.clone(), |u| u.host)
}

/// The server, ready to use: the tunnel opened and the kind known when needed.
/// None when no server is set. May take a few seconds the first time.
pub fn endpoint() -> Result<Option<Endpoint>> {
    let Some(settings) = settings() else {
        return Ok(None);
    };
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    resolve(&settings, &mut live).map(Some)
}

/// The server `settings` name, ready to use, with what using it set up in `live`.
fn resolve(settings: &Settings, live: &mut Live) -> Result<Endpoint> {
    let url = parse_url(&settings.url)?;
    let tunneled = !settings.tunnel.trim().is_empty();
    if tunneled && settings.url.trim().starts_with("https://") {
        bail!(
            "tunnel carries plain http: set url to the server's http address as seen from {} (e.g. http://localhost:11434)",
            settings.tunnel.trim()
        );
    }
    let name = display_name(settings);
    let root = if settings.tunnel.trim().is_empty() {
        url.root.clone()
    } else {
        // Reopen a tunnel whose ssh has ended (a dropped connection), on its old
        // port when it's free, so the panes started on it reach the server again.
        let mut before = None;
        if live.tunnel.as_mut().is_some_and(|t| !t.alive()) {
            before = live.tunnel.take().map(|t| t.local);
        }
        if live.tunnel.is_none() {
            live.tunnel = Some(Tunnel::open(
                settings.tunnel.trim(),
                &url.host,
                url.port,
                before,
            )?);
        }
        let local = live.tunnel.as_ref().map_or(0, |t| t.local);
        // The same path on the tunnel's local end; https would fail its certificate.
        let path = url
            .root
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
            .unwrap_or_default();
        format!("http://127.0.0.1:{local}{path}")
    };
    let key = Some(settings.key_env.trim())
        .filter(|k| !k.is_empty())
        .and_then(|k| std::env::var(k).ok())
        .filter(|k| !k.is_empty());
    let kind = match (settings.kind, live.kind) {
        (Kind::Auto, Some(known)) => known,
        (Kind::Auto, None) => {
            let kind =
                probe(&root, key.as_deref()).with_context(|| format!("Can't reach {name}"))?;
            live.kind = Some(kind);
            kind
        }
        (kind, _) => kind,
    };
    Ok(Endpoint {
        root,
        kind,
        name,
        key,
    })
}

/// Ollama answers `/api/version`; anything else is taken for the OpenAI API.
fn probe(root: &str, key: Option<&str>) -> Result<Kind> {
    let reply = http(
        &format!("{root}/api/version"),
        None,
        key,
        Duration::from_secs(5),
    )?;
    // A proxy in front, or a server still starting, can't say yet (5xx): asked
    // again next time rather than taken for vLLM for as long as Raido runs.
    if reply.status == 0 || reply.status >= 500 {
        bail!("/api/version answered {}", reply.status);
    }
    Ok(
        if reply.status == 200 && reply.body.contains("\"version\"") {
            Kind::Ollama
        } else {
            Kind::Vllm
        },
    )
}

/// The server's models, as `/v1/models` lists them (remembered for OpenCode).
pub fn models() -> Result<Vec<Model>> {
    let endpoint = endpoint()?.context("No model_server in config.toml")?;
    let models = list(&endpoint)?;
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).models = models.clone();
    Ok(models)
}

/// The ids of the server's models, as `models` lists them.
pub fn model_ids() -> Result<Vec<String>> {
    Ok(models()?.into_iter().map(|m| m.id).collect())
}

/// The models listed last, without asking again.
pub fn known_models() -> Vec<Model> {
    LIVE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .models
        .clone()
}

fn list(endpoint: &Endpoint) -> Result<Vec<Model>> {
    let reply = http(
        &format!("{}/models", endpoint.api()),
        None,
        endpoint.key.as_deref(),
        Duration::from_secs(10),
    )?;
    reply.ok(endpoint)?;
    parse_models(&reply.body)
}

fn parse_models(body: &str) -> Result<Vec<Model>> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("The server's model list isn't JSON")?;
    let data = json
        .get("data")
        .and_then(|d| d.as_array())
        .context("The server's model list has no \"data\"")?;
    Ok(data
        .iter()
        .filter_map(|m| {
            Some(Model {
                id: m.get("id")?.as_str()?.to_string(),
                context: m.get("max_model_len").and_then(|n| n.as_u64()),
            })
        })
        .collect())
}

/// Ask the server's `model` (the first it has when none) to answer `prompt`: the
/// answer, tokens used, and who answered ("gpu-box (qwen3:8b)").
pub fn chat(
    model: Option<&str>,
    prompt: &str,
    timeout: Duration,
) -> Result<(String, crate::summary::Usage, String)> {
    let endpoint = endpoint()?.context("No model_server in config.toml")?;
    chat_on(&endpoint, model, prompt, timeout)
}

fn chat_on(
    endpoint: &Endpoint,
    model: Option<&str>,
    prompt: &str,
    timeout: Duration,
) -> Result<(String, crate::summary::Usage, String)> {
    let model = match model {
        Some(model) => model.to_string(),
        None => list(endpoint)?
            .into_iter()
            .next()
            .map(|m| m.id)
            .with_context(|| format!("{} has no models", endpoint.name))?,
    };
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    })
    .to_string();
    let reply = http(
        &format!("{}/chat/completions", endpoint.api()),
        Some(&body),
        endpoint.key.as_deref(),
        timeout,
    )?;
    reply.ok(endpoint)?;
    let (text, usage) = parse_chat(&reply.body)?;
    Ok((text, usage, format!("{} ({model})", endpoint.name)))
}

fn parse_chat(body: &str) -> Result<(String, crate::summary::Usage)> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("The server's answer isn't JSON")?;
    let text = json
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .context("The server's answer has no message")?;
    // Reasoning models may think aloud first.
    let text = match text.find("</think>") {
        Some(end) if text.trim_start().starts_with("<think>") => &text[end + "</think>".len()..],
        _ => text,
    };
    let tokens = |key: &str| {
        json.pointer(&format!("/usage/{key}"))
            .and_then(|n| n.as_u64())
    };
    let usage = crate::summary::Usage {
        input: tokens("prompt_tokens").unwrap_or(0),
        output: tokens("completion_tokens").unwrap_or(0),
        cost: None,
    };
    Ok((text.trim().to_string(), usage))
}

/// An HTTP reply: status and body.
#[derive(Debug)]
struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    /// An error that says what the server said, unless it answered 2xx.
    fn ok(&self, endpoint: &Endpoint) -> Result<()> {
        if (200..300).contains(&self.status) {
            return Ok(());
        }
        let said = serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|j| {
                j.pointer("/error/message")
                    .or_else(|| j.get("error"))
                    .or_else(|| j.get("detail"))
                    .map(|m| m.as_str().map_or_else(|| m.to_string(), String::from))
            })
            .unwrap_or_else(|| self.body.chars().take(200).collect());
        let hint = match self.status {
            401 | 403 => " · set key_env in [model_server] to the variable holding its API key",
            _ => "",
        };
        bail!("{} answered {}: {said}{hint}", endpoint.name, self.status)
    }
}

/// GET `url`, or POST `body` as JSON, through curl. The API key goes in a header
/// read from a private file, never on the command line where `ps` shows it.
fn http(url: &str, body: Option<&str>, key: Option<&str>, timeout: Duration) -> Result<Reply> {
    let curl = crate::platform::which("curl")
        .context("curl isn't installed; Raido talks to the model server with it")?;
    let mut command = Command::new(curl);
    command.args(["-sS", "--max-time", &timeout.as_secs().max(1).to_string()]);
    command.args(["-w", "\n%{http_code}"]);
    let header = match key {
        Some(key) => Some(HeaderFile::new(&format!("Authorization: Bearer {key}"))?),
        None => None,
    };
    if let Some(header) = &header {
        command.arg("-H").arg(format!("@{}", header.path.display()));
    }
    if body.is_some() {
        command.args([
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
        ]);
    }
    command.arg(url);
    let output = crate::review::run_for(
        &mut command,
        body.map(str::as_bytes),
        timeout + Duration::from_secs(2),
        "curl",
    )
    .map_err(|e| anyhow::anyhow!("{}", e.trim()))?;
    drop(header);
    let text = String::from_utf8_lossy(&output);
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", &text));
    Ok(Reply {
        status: status.trim().parse().unwrap_or(0),
        body: body.to_string(),
    })
}

/// A header in a file only you can read, removed when dropped.
struct HeaderFile {
    path: std::path::PathBuf,
}

impl HeaderFile {
    fn new(line: &str) -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let path = std::env::temp_dir().join(format!(
            "{}{n}-{stamp:x}",
            header_prefix(std::process::id())
        ));
        // A file of its own, made now: never one (or a link) someone else put there
        // first in a shared /tmp, which would get the key.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&path)
            .with_context(|| format!("Create {}", path.display()))?;
        let header = Self { path };
        writeln!(file, "{line}")?;
        Ok(header)
    }
}

/// How the key's header files of Raido `pid` are named, in the temp folder.
fn header_prefix(pid: u32) -> String {
    format!("raido-header-{pid}-")
}

impl Drop for HeaderFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `ssh -N -L` forwarding a free local port to the server, for as long as Raido
/// runs.
struct Tunnel {
    child: Child,
    local: u16,
}

/// How long ssh may take to log in and start forwarding.
const TUNNEL_WAIT: Duration = Duration::from_secs(15);

impl Tunnel {
    /// Forward to `host:port` through `destination`, on local port `again` when
    /// it's free, else on any free one.
    fn open(destination: &str, host: &str, port: u16, again: Option<u16>) -> Result<Self> {
        let local = again
            .and_then(|p| TcpListener::bind(("127.0.0.1", p)).ok())
            .map_or_else(|| TcpListener::bind("127.0.0.1:0"), Ok)?
            .local_addr()?
            .port();
        let ssh = crate::platform::which("ssh").context("ssh isn't installed; tunnel needs it")?;
        let mut child = Command::new(ssh)
            .args(tunnel_args(destination, local, host, port))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("Start ssh")?;
        let start = Instant::now();
        loop {
            if TcpStream::connect(("127.0.0.1", local)).is_ok() {
                // What ssh says from now on (each forward the server refuses) is read
                // and dropped, so a full pipe never stops the tunnel.
                if let Some(mut stderr) = child.stderr.take() {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                    });
                }
                return Ok(Self { child, local });
            }
            if let Some(status) = child.try_wait()? {
                let mut said = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut stderr, &mut said);
                }
                bail!(
                    "ssh {destination} ended ({status}): {} · tunnel needs a login without a password prompt (an SSH key or agent)",
                    said.trim()
                );
            }
            if start.elapsed() > TUNNEL_WAIT {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "ssh {destination} didn't open the tunnel within {}s",
                    TUNNEL_WAIT.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// ssh's arguments: no shell, no password prompt, fail rather than run without the
/// forward, and notice a dead connection.
fn tunnel_args(destination: &str, local: u16, host: &str, port: u16) -> Vec<String> {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    [
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=30",
        "-L",
    ]
    .into_iter()
    .map(String::from)
    .chain([
        format!("127.0.0.1:{local}:{host}:{port}"),
        destination.to_string(),
    ])
    .collect()
}

/// Close the tunnel (Raido is quitting).
pub fn shutdown() {
    LIVE.lock().unwrap_or_else(|e| e.into_inner()).tunnel = None;
}

/// Closes the tunnel when Raido ends, however it ends.
pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        shutdown();
    }
}

/// OpenCode's arguments and environment to run on the server: its config, given
/// in `OPENCODE_CONFIG_CONTENT`, points OpenCode at the server and lists its
/// `models`, starting on `chosen` when one is. OpenCode 2 (`v2`, which has
/// `--standalone`) reads it in a server of its own; OpenCode 1 has its older
/// config format.
pub fn opencode(
    endpoint: &Endpoint,
    chosen: Option<&str>,
    models: &[Model],
    v2: bool,
) -> (Vec<String>, Vec<(String, String)>) {
    let config = opencode_config(endpoint, chosen, models, v2);
    let args = if v2 {
        vec!["--standalone".to_string()]
    } else {
        Vec::new()
    };
    (
        args,
        vec![("OPENCODE_CONFIG_CONTENT".into(), config.to_string())],
    )
}

/// Whether a model reads images, from its name: vLLM doesn't say, and a model
/// told it can gets images it would refuse. Vision families (`Qwen2.5-VL`,
/// `llava`, `llama3.2-vision`, `gemma3`, `pixtral`, `InternVL`…); Gemma 3's 1B and
/// 270M are text only. Ollama answers for itself: [`ollama_sees_images`].
pub fn sees_images(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let name = id.rsplit('/').next().unwrap_or(&id);
    const FAMILIES: [&str; 22] = [
        "vision",
        "llava",
        "pixtral",
        "idefics",
        "molmo",
        "minicpm-v",
        "moondream",
        "paligemma",
        "smolvlm",
        "internvl",
        "mllama",
        "multimodal",
        "llama4",
        "llama-4",
        "mistral-small3.1",
        "mistral-small-3.1",
        "mistral-small3.2",
        "mistral-small-3.2",
        "glm-4.1v",
        "glm-4.5v",
        "glm-4.6v",
        "ovis",
    ];
    let gemma3 = (name.contains("gemma3") || name.contains("gemma-3"))
        && !name.contains("gemma3n")
        && !name.contains("gemma-3n")
        && !["1b", "270m"].iter().any(|size| {
            name.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|part| part == *size)
        });
    // "-VL" as a word or a suffix: Qwen2.5-VL, qwen2.5vl, Kimi-VL, DeepSeek-VL2.
    let vl = name
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
        .any(|part| part.trim_end_matches(char::is_numeric).ends_with("vl"));
    gemma3 || vl || FAMILIES.iter().any(|family| name.contains(family))
}

/// Whether Ollama at `root` says `model` reads images (its `vision` capability);
/// none when it can't be asked.
pub fn ollama_sees_images(root: &str, key: Option<&str>, model: &str) -> Option<bool> {
    let body = serde_json::json!({ "model": model }).to_string();
    let reply = http(
        &format!("{root}/api/show"),
        Some(&body),
        key,
        Duration::from_secs(3),
    )
    .ok()?;
    if reply.status != 200 {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(&reply.body).ok()?;
    let capabilities = json.get("capabilities")?.as_array()?;
    Some(capabilities.iter().any(|c| c == "vision"))
}

/// A model's key in OpenCode's config: its id without `/` or `#`, which OpenCode
/// reads as provider and variant separators.
fn opencode_key(id: &str) -> String {
    id.chars()
        .map(|c| if c == '/' || c == '#' { '-' } else { c })
        .collect()
}

fn opencode_config(
    endpoint: &Endpoint,
    chosen: Option<&str>,
    models: &[Model],
    v2: bool,
) -> serde_json::Value {
    use serde_json::json;
    let name = format!("{} ({})", endpoint.name, endpoint.kind.label());
    let mut config = json!({});
    if !v2 {
        // OpenCode 1: a provider of its own for the server, either kind.
        let mut options = json!({ "baseURL": endpoint.api() });
        if let Some(key) = &endpoint.key {
            options["apiKey"] = json!(key);
        }
        let listed: serde_json::Map<String, serde_json::Value> = models
            .iter()
            .map(|m| {
                let mut entry = json!({ "name": m.id });
                // Images (screenshots, `Ctrl-b I`) go only to models that see them.
                if sees_images(&m.id) {
                    entry["attachment"] = json!(true);
                    entry["modalities"] = json!({ "input": ["text", "image"], "output": ["text"] });
                }
                (m.id.clone(), entry)
            })
            .collect();
        config["provider"] = json!({ "server": {
            "npm": "@ai-sdk/openai-compatible",
            "name": name,
            "options": options,
            "models": listed,
        }});
        if let Some(model) = chosen {
            config["model"] = json!(format!("server/{model}"));
        }
        return config;
    }
    let mut settings = json!({ "baseURL": endpoint.api() });
    if let Some(key) = &endpoint.key {
        settings["apiKey"] = json!(key);
    }
    match endpoint.kind {
        // OpenCode's own Ollama provider, at the server: it finds the models and
        // which of them can use tools.
        Kind::Ollama => {
            config["providers"] = json!({ "ollama": { "settings": settings } });
            if let Some(model) = chosen {
                config["model"] = json!(format!("ollama/{model}"));
            }
        }
        // OpenCode's vLLM discovery leaves tools off, and an agent needs them: list
        // the models with tools on (the server needs --enable-auto-tool-choice).
        _ => {
            let listed: serde_json::Map<String, serde_json::Value> = models
                .iter()
                .map(|m| {
                    // Images (screenshots, `Ctrl-b I`) go only to models that see them.
                    let input = if sees_images(&m.id) {
                        json!(["text", "image"])
                    } else {
                        json!(["text"])
                    };
                    let mut entry = json!({
                        "modelID": m.id,
                        "capabilities": { "tools": true, "input": input, "output": ["text"] },
                    });
                    if let Some(context) = m.context {
                        entry["limit"] = json!({ "context": context, "output": (context / 4).clamp(1024, 16384) });
                    }
                    (opencode_key(&m.id), entry)
                })
                .collect();
            config["providers"] = json!({ "server": {
                "name": name,
                "package": "@opencode/ai/providers/openai-compatible",
                "settings": settings,
                "models": listed,
            }});
            if let Some(model) = chosen {
                config["model"] = json!(format!("server/{}", opencode_key(model)));
            }
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::sync::Arc;

    fn endpoint(kind: Kind, key: Option<&str>) -> Endpoint {
        Endpoint {
            root: "http://gpu-box:8000".into(),
            kind,
            name: "gpu-box".into(),
            key: key.map(String::from),
        }
    }

    #[test]
    fn urls_give_the_server_root_host_and_port() {
        let url = parse_url("http://gpu-box:11434/").unwrap();
        assert_eq!(
            url,
            Url {
                root: "http://gpu-box:11434".into(),
                host: "gpu-box".into(),
                port: 11434
            }
        );
        // /v1 is Raido's to add.
        assert_eq!(
            parse_url("http://gpu-box:8000/v1").unwrap().root,
            "http://gpu-box:8000"
        );
        assert_eq!(
            parse_url("https://llm.example.com/vllm/v1").unwrap().root,
            "https://llm.example.com/vllm"
        );
        assert_eq!(parse_url("https://llm.example.com").unwrap().port, 443);
        assert_eq!(parse_url("http://box").unwrap().port, 80);
        let v6 = parse_url("http://[::1]:8000").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 8000));
        assert!(parse_url("gpu-box:11434").is_err(), "needs a scheme");
        assert!(parse_url("ftp://gpu-box").is_err());
        assert!(parse_url("http://gpu-box:port").is_err());
    }

    #[test]
    fn config_toml_names_the_server() {
        let config: crate::config::Config = toml::from_str(
            "[model_server]\nurl = \"http://gpu-box:8000\"\nkind = \"vllm\"\ntunnel = \"gpu-box\"\nkey_env = \"VLLM_API_KEY\"\n",
        )
        .unwrap();
        let server = config.model_server.unwrap();
        assert_eq!(server.kind, Kind::Vllm);
        assert_eq!(
            (server.tunnel.as_str(), server.key_env.as_str()),
            ("gpu-box", "VLLM_API_KEY")
        );
        let parse = |text: &str| {
            toml::from_str::<crate::config::Config>(text)
                .unwrap()
                .model_server
                .unwrap()
        };
        assert_eq!(parse("[model_server]\nurl = \"http://x\"").kind, Kind::Auto);
        assert_eq!(
            parse("[model_server]\nurl = \"http://x\"\nkind = \"openai\"").kind,
            Kind::Vllm
        );
        assert!(toml::from_str::<crate::config::Config>("")
            .unwrap()
            .model_server
            .is_none());
    }

    #[test]
    fn the_name_is_the_tunnels_host_or_the_urls() {
        let mut settings = Settings {
            url: "http://localhost:11434".into(),
            ..Default::default()
        };
        assert_eq!(display_name(&settings), "localhost");
        settings.tunnel = "me@gpu-box".into();
        assert_eq!(display_name(&settings), "gpu-box");
    }

    #[test]
    fn models_and_answers_are_read_from_the_openai_api() {
        // vLLM's model card says its context length; Ollama's doesn't.
        let models = parse_models(
            r#"{"object":"list","data":[{"id":"Qwen/Qwen3-Coder-30B","owned_by":"vllm","max_model_len":32768},{"id":"qwen3:8b","owned_by":"library"}]}"#,
        )
        .unwrap();
        assert_eq!(
            models,
            [
                Model {
                    id: "Qwen/Qwen3-Coder-30B".into(),
                    context: Some(32768)
                },
                Model {
                    id: "qwen3:8b".into(),
                    context: None
                }
            ]
        );
        let (text, usage) = parse_chat(
            r#"{"choices":[{"message":{"role":"assistant","content":"<think>hmm</think>\n\nFILE a.rs: Adds x."}}],"usage":{"prompt_tokens":812,"completion_tokens":40}}"#,
        )
        .unwrap();
        assert_eq!(text, "FILE a.rs: Adds x.");
        assert_eq!((usage.input, usage.output, usage.cost), (812, 40, None));
        assert!(parse_chat(r#"{"error":"nope"}"#).is_err());
    }

    #[test]
    fn errors_say_what_the_server_said() {
        let reply = Reply {
            status: 401,
            body: r#"{"error":"Unauthorized"}"#.into(),
        };
        let error = reply
            .ok(&endpoint(Kind::Vllm, None))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("gpu-box answered 401: Unauthorized"),
            "{error}"
        );
        assert!(error.contains("key_env"), "{error}");
        let reply = Reply {
            status: 404,
            body: r#"{"error":{"message":"model \"x\" not found"}}"#.into(),
        };
        let error = reply
            .ok(&endpoint(Kind::Ollama, None))
            .unwrap_err()
            .to_string();
        assert!(error.ends_with("404: model \"x\" not found"), "{error}");
    }

    #[test]
    fn opencode_2_uses_its_ollama_provider_at_the_server() {
        let ollama = endpoint(Kind::Ollama, None);
        let config = opencode_config(&ollama, Some("qwen3:8b"), &[], true);
        assert_eq!(
            config,
            serde_json::json!({
                "providers": { "ollama": { "settings": { "baseURL": "http://gpu-box:8000/v1" } } },
                "model": "ollama/qwen3:8b"
            })
        );
        // No model chosen: OpenCode's own choice, among the server's models.
        assert!(opencode_config(&ollama, None, &[], true)
            .get("model")
            .is_none());
    }

    #[test]
    fn opencode_2_lists_a_vllm_servers_models_with_tools_on() {
        let models = [Model {
            id: "Qwen/Qwen3-Coder-30B".into(),
            context: Some(32768),
        }];
        let (args, env) = opencode(
            &endpoint(Kind::Vllm, Some("s3cret")),
            Some("Qwen/Qwen3-Coder-30B"),
            &models,
            true,
        );
        assert_eq!(args, ["--standalone"]);
        assert_eq!(env[0].0, "OPENCODE_CONFIG_CONTENT");
        let config: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(config["model"], "server/Qwen-Qwen3-Coder-30B");
        let server = &config["providers"]["server"];
        assert_eq!(server["name"], "gpu-box (vLLM)");
        assert_eq!(
            server["package"],
            "@opencode/ai/providers/openai-compatible"
        );
        assert_eq!(server["settings"]["baseURL"], "http://gpu-box:8000/v1");
        assert_eq!(server["settings"]["apiKey"], "s3cret");
        let model = &server["models"]["Qwen-Qwen3-Coder-30B"];
        assert_eq!(model["modelID"], "Qwen/Qwen3-Coder-30B");
        assert_eq!(model["capabilities"]["tools"], true);
        assert_eq!(model["limit"]["context"], 32768);
        assert_eq!(model["limit"]["output"], 8192);
    }

    #[test]
    fn vision_models_are_told_apart_by_name() {
        for id in [
            "Qwen/Qwen2.5-VL-7B-Instruct",
            "qwen2.5vl:7b",
            "qwen3-vl:8b",
            "OpenGVLab/InternVL3-8B",
            "deepseek-ai/deepseek-vl2-small",
            "moonshotai/Kimi-VL-A3B-Instruct",
            "llava:13b",
            "llama3.2-vision:11b",
            "meta-llama/Llama-4-Scout-17B-16E-Instruct",
            "gemma3:4b",
            "google/gemma-3-27b-it",
            "mistralai/Pixtral-12B-2409",
            "mistral-small3.1:24b",
            "minicpm-v:8b",
            "moondream",
            "zai-org/GLM-4.5V",
        ] {
            assert!(sees_images(id), "{id}");
        }
        for id in [
            "Qwen/Qwen3-Coder-30B",
            "qwen3:8b",
            "gpt-oss:20b",
            "gemma3:1b",
            "google/gemma-3-1b-it",
            "gemma3:270m",
            "gemma3n:e4b",
            "deepseek-r1:14b",
            "meta-llama/Llama-3.1-8B-Instruct",
            "kimi-k2.6",
        ] {
            assert!(!sees_images(id), "{id}");
        }
    }

    #[test]
    fn opencode_is_told_which_models_take_images() {
        let models = [
            Model {
                id: "Qwen/Qwen2.5-VL-7B-Instruct".into(),
                context: None,
            },
            Model {
                id: "Qwen/Qwen3-Coder-30B".into(),
                context: None,
            },
        ];
        // OpenCode 2 on vLLM: image input for the vision model only.
        let (_, env) = opencode(&endpoint(Kind::Vllm, None), None, &models, true);
        let config: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        let listed = &config["providers"]["server"]["models"];
        assert_eq!(
            listed["Qwen-Qwen2.5-VL-7B-Instruct"]["capabilities"]["input"],
            serde_json::json!(["text", "image"])
        );
        assert_eq!(
            listed["Qwen-Qwen3-Coder-30B"]["capabilities"]["input"],
            serde_json::json!(["text"])
        );
        // OpenCode 1: attachments and image input, the same way.
        let (_, env) = opencode(&endpoint(Kind::Vllm, None), None, &models, false);
        let config: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        let listed = &config["provider"]["server"]["models"];
        let vision = &listed["Qwen/Qwen2.5-VL-7B-Instruct"];
        assert_eq!(vision["attachment"], true);
        assert_eq!(
            vision["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );
        assert!(listed["Qwen/Qwen3-Coder-30B"].get("attachment").is_none());
    }

    #[test]
    fn opencode_1_gets_its_older_config() {
        let models = [Model {
            id: "qwen3:8b".into(),
            context: None,
        }];
        let (args, env) = opencode(
            &endpoint(Kind::Ollama, None),
            Some("qwen3:8b"),
            &models,
            false,
        );
        assert!(args.is_empty());
        let config: serde_json::Value = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(config["model"], "server/qwen3:8b");
        assert_eq!(
            config["provider"]["server"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            config["provider"]["server"]["options"]["baseURL"],
            "http://gpu-box:8000/v1"
        );
        assert_eq!(
            config["provider"]["server"]["models"]["qwen3:8b"]["name"],
            "qwen3:8b"
        );
    }

    #[test]
    fn the_tunnel_forwards_a_local_port_without_prompting() {
        assert_eq!(
            tunnel_args("me@gpu-box", 43123, "localhost", 11434),
            [
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                "-L",
                "127.0.0.1:43123:localhost:11434",
                "me@gpu-box"
            ]
        );
        assert_eq!(
            tunnel_args("box", 1, "::1", 8000)[8],
            "127.0.0.1:1:[::1]:8000"
        );
    }

    /// A request the fake server got: method and path, its Authorization header,
    /// and its body.
    type Seen = Arc<Mutex<Vec<(String, Option<String>, String)>>>;

    /// A fake model server answering `routes` (path → (status, body)) on a free port.
    fn fake_server(routes: Vec<(&'static str, u16, String)>) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let root = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let (mut length, mut auth) = (0, None);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    let (name, value) = line.split_once(": ").unwrap_or((line, ""));
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.parse().unwrap(),
                        "authorization" => auth = Some(value.to_string()),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = first
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                let path = request.split(' ').nth(1).unwrap_or_default().to_string();
                log.lock()
                    .unwrap()
                    .push((request, auth, String::from_utf8_lossy(&body).into()));
                let (status, reply) = routes.iter().find(|(p, ..)| *p == path).map_or(
                    (404, "{\"error\":\"no route\"}".to_string()),
                    |(_, s, b)| (*s, b.clone()),
                );
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (root, seen)
    }

    #[test]
    fn ollama_says_which_models_read_images() {
        if crate::platform::which("curl").is_none() {
            return;
        }
        // As Ollama answers /api/show (abridged).
        let show = |capabilities: &str| {
            format!(
                r#"{{"modelfile":"FROM x","details":{{"family":"gemma3"}},"model_info":{{}},"capabilities":{capabilities},"modified_at":"2026-11-01T10:00:00Z"}}"#
            )
        };
        let (root, seen) =
            fake_server(vec![("/api/show", 200, show(r#"["completion","vision"]"#))]);
        assert_eq!(ollama_sees_images(&root, None, "gemma3"), Some(true));
        let (method, _, body) = seen.lock().unwrap()[0].clone();
        assert_eq!(method, "POST /api/show");
        assert_eq!(body, r#"{"model":"gemma3"}"#);
        let (root, _) = fake_server(vec![("/api/show", 200, show(r#"["completion","tools"]"#))]);
        assert_eq!(ollama_sees_images(&root, None, "qwen3-coder"), Some(false));
        // An unknown model, or a server that isn't Ollama: can't say.
        let (root, _) = fake_server(Vec::new());
        assert_eq!(ollama_sees_images(&root, None, "nope"), None);
    }

    #[test]
    fn a_vllm_server_is_found_listed_and_asked_with_its_key() {
        if crate::platform::which("curl").is_none() {
            return;
        }
        let (root, seen) = fake_server(vec![
            ("/v1/models", 200, r#"{"data":[{"id":"Qwen/Qwen3-Coder-30B","max_model_len":32768}]}"#.into()),
            (
                "/v1/chat/completions",
                200,
                r#"{"choices":[{"message":{"content":"OVERALL: Fine."}}],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#.into(),
            ),
        ]);
        // vLLM doesn't answer /api/version: not Ollama.
        assert_eq!(probe(&root, None).unwrap(), Kind::Vllm);
        let server = Endpoint {
            root,
            kind: Kind::Vllm,
            name: "gpu-box".into(),
            key: Some("s3cret".into()),
        };
        let models = list(&server).unwrap();
        assert_eq!(models[0].context, Some(32768));
        let reply = http(
            &format!("{}/chat/completions", server.api()),
            Some(r#"{"model":"Qwen/Qwen3-Coder-30B","messages":[]}"#),
            server.key.as_deref(),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(parse_chat(&reply.body).unwrap().0, "OVERALL: Fine.");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].0, "GET /api/version");
        assert_eq!(seen[1].0, "GET /v1/models");
        assert_eq!(seen[1].1.as_deref(), Some("Bearer s3cret"));
        assert_eq!(seen[2].0, "POST /v1/chat/completions");
        assert!(seen[2].2.contains("Qwen/Qwen3-Coder-30B"));
        // The key's header file is gone again.
        let prefix = header_prefix(std::process::id());
        let left = std::fs::read_dir(std::env::temp_dir()).map_or(0, |d| {
            d.flatten()
                .filter(|f| f.file_name().to_string_lossy().starts_with(&prefix))
                .count()
        });
        assert_eq!(left, 0);
    }

    fn fresh() -> Live {
        Live {
            tunnel: None,
            kind: None,
            models: Vec::new(),
        }
    }

    #[test]
    fn a_server_is_asked_its_kind_once_and_answers_the_review() {
        if crate::platform::which("curl").is_none() {
            return;
        }
        let (root, seen) = fake_server(vec![
            ("/api/version", 200, r#"{"version":"0.12.3"}"#.into()),
            ("/v1/models", 200, r#"{"data":[{"id":"qwen3:8b"},{"id":"gemma3"}]}"#.into()),
            (
                "/v1/chat/completions",
                200,
                r#"{"choices":[{"message":{"content":"FILE a.rs: Adds x."}}],"usage":{"prompt_tokens":120,"completion_tokens":8}}"#.into(),
            ),
        ]);
        let settings = Settings {
            url: format!("{root}/v1/"),
            key_env: "RAIDO_NO_SUCH_VARIABLE".into(),
            ..Default::default()
        };
        let mut live = fresh();
        let server = resolve(&settings, &mut live).unwrap();
        assert_eq!(server.kind, Kind::Ollama, "it answered /api/version");
        assert_eq!(server.root, root, "/v1 is Raido's to add");
        assert_eq!(server.name, "127.0.0.1");
        assert_eq!(server.key, None, "no such variable, no key");
        // Known from now on: not asked again.
        resolve(&settings, &mut live).unwrap();
        // The review's question goes to its first model when none is named.
        let (text, usage, by) = chat_on(&server, None, "Explain.", Duration::from_secs(5)).unwrap();
        assert_eq!(text, "FILE a.rs: Adds x.");
        assert_eq!((usage.input, usage.output), (120, 8));
        assert_eq!(by, "127.0.0.1 (qwen3:8b)");
        let seen = seen.lock().unwrap();
        let asked: Vec<&str> = seen.iter().map(|s| s.0.as_str()).collect();
        assert_eq!(
            asked,
            [
                "GET /api/version",
                "GET /v1/models",
                "POST /v1/chat/completions"
            ]
        );
        let body: serde_json::Value = serde_json::from_str(&seen[2].2).unwrap();
        assert_eq!(body["model"], "qwen3:8b");
        assert_eq!(body["messages"][0]["content"], "Explain.");
        assert_eq!(body["stream"], false);
        // Said to be vLLM: not asked.
        let vllm = Settings {
            kind: Kind::Vllm,
            ..settings
        };
        assert_eq!(resolve(&vllm, &mut fresh()).unwrap().kind, Kind::Vllm);
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn https_through_a_tunnel_is_refused_with_the_reason() {
        let settings = Settings {
            url: "https://gpu-box".into(),
            tunnel: "gpu-box".into(),
            ..Default::default()
        };
        let error = resolve(&settings, &mut fresh()).unwrap_err().to_string();
        assert!(error.starts_with("tunnel carries plain http"), "{error}");
    }

    #[test]
    fn a_tunnel_that_cant_open_says_why() {
        if crate::platform::which("ssh").is_none() {
            return;
        }
        let settings = Settings {
            url: "http://localhost:11434".into(),
            tunnel: "raido-no-such-host.invalid".into(),
            kind: Kind::Ollama,
            ..Default::default()
        };
        let error = format!("{:#}", resolve(&settings, &mut fresh()).unwrap_err());
        assert!(
            error.contains("ssh raido-no-such-host.invalid ended"),
            "{error}"
        );
        assert!(error.contains("without a password prompt"), "{error}");
    }

    /// A real server: `RAIDO_MODEL_SERVER=http://gpu-box:11434 cargo test
    /// real_model_server -- --ignored` (it runs the server's first model).
    #[test]
    #[ignore = "asks a real model server: RAIDO_MODEL_SERVER=<url> cargo test real_model_server -- --ignored"]
    fn real_model_server_lists_and_answers() {
        let url = std::env::var("RAIDO_MODEL_SERVER").expect("RAIDO_MODEL_SERVER=<url>");
        let settings = Settings {
            url,
            ..Default::default()
        };
        let server = resolve(&settings, &mut fresh()).unwrap();
        let models = list(&server).unwrap();
        println!("{:?} with {models:?}", server.kind);
        assert!(!models.is_empty());
        let (text, usage, by) = chat_on(
            &server,
            None,
            "Answer with one word: hello.",
            Duration::from_secs(120),
        )
        .unwrap();
        println!("{by}: {text:?} ({usage:?})");
        assert!(!text.is_empty());
        assert!(usage.input > 0);
    }

    #[test]
    fn an_ollama_server_says_so() {
        if crate::platform::which("curl").is_none() {
            return;
        }
        let (root, _) = fake_server(vec![(
            "/api/version",
            200,
            r#"{"version":"0.12.3"}"#.into(),
        )]);
        assert_eq!(probe(&root, None).unwrap(), Kind::Ollama);
    }

    #[test]
    fn an_unreachable_server_is_an_error_not_a_hang() {
        if crate::platform::which("curl").is_none() {
            return;
        }
        // A port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let start = Instant::now();
        assert!(probe(&format!("http://127.0.0.1:{port}"), None).is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

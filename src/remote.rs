//! `raido FILE` typed in one of Raido's own terminal panes opens FILE in that Raido
//! instead of starting another one inside it. Raido listens and tells its terminals
//! where (`RAIDO_SOCKET`); the inner `raido` sends the path there and exits.
//!
//! On Unix that's a socket in a directory only you can enter. Windows has no Unix
//! sockets in Rust's standard library, so there Raido listens on a local TCP port
//! (127.0.0.1 only) and requires a random secret that only its own panes are
//! given: `RAIDO_SOCKET=tcp:127.0.0.1:PORT:SECRET`.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        OnceLock,
    },
    time::Duration,
};

pub const VAR: &str = "RAIDO_SOCKET";

/// Where this Raido listens, handed to every terminal pane it starts.
static ADDRESS: OnceLock<PathBuf> = OnceLock::new();

pub fn address() -> Option<&'static Path> {
    ADDRESS.get().map(PathBuf::as_path)
}

#[derive(Serialize, Deserialize)]
struct Ask {
    open: PathBuf,
    /// The secret from `RAIDO_SOCKET` (TCP only).
    #[serde(default)]
    secret: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// Done; the text says what happened.
    Opened(String),
    /// Not for this Raido (another project): start a separate one.
    Elsewhere,
    Failed(String),
}

/// One `raido PATH` from a terminal pane, answered from the main loop.
pub struct Request {
    pub path: PathBuf,
    reply: Sender<Reply>,
}

impl Request {
    pub fn answer(self, reply: Reply) {
        let _ = self.reply.send(reply);
    }
}

pub struct Server {
    pub requests: Receiver<Request>,
    /// The socket's private directory (Unix), removed when Raido exits.
    dir: Option<PathBuf>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Listen for `raido FILE` from this Raido's terminal panes.
pub fn listen() -> Result<Server> {
    #[cfg(unix)]
    return listen_unix();
    #[cfg(not(unix))]
    return listen_tcp();
}

/// Hand PATH (absolute) to the Raido at `address`. An error means no Raido answered,
/// so the caller should start normally.
pub fn send(address: &Path, path: &Path) -> Result<Reply> {
    if let Some(tcp) = address.to_str().and_then(|a| a.strip_prefix("tcp:")) {
        return send_tcp(tcp, path);
    }
    #[cfg(unix)]
    return send_unix(address, path);
    #[cfg(not(unix))]
    anyhow::bail!("Not a Raido address: {}", address.display())
}

/// Read one request from a connection, pass it to the main loop, write the answer.
fn serve<S: Read + Write>(mut stream: S, sender: &Sender<Request>, secret: Option<&str>) {
    let mut line = String::new();
    let mut reader = std::io::BufReader::new(Read::take(&mut stream, 64 * 1024));
    if reader.read_line(&mut line).is_err() {
        return;
    }
    drop(reader);
    let reply = match serde_json::from_str::<Ask>(&line) {
        Ok(ask) if secret.is_some_and(|s| s != ask.secret) => {
            Reply::Failed("Not one of this Raido's terminals".into())
        }
        Ok(ask) => {
            let (reply, answer) = mpsc::channel();
            let request = Request {
                path: ask.open,
                reply,
            };
            if sender.send(request).is_err() {
                return;
            }
            answer
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| Reply::Failed("Raido is busy; try again".into()))
        }
        Err(_) => Reply::Failed("Unreadable request".into()),
    };
    if let Ok(json) = serde_json::to_string(&reply) {
        let _ = writeln!(stream, "{json}");
    }
}

/// Send one request on a connection and read the answer.
fn ask<S: Read + Write>(mut stream: S, path: &Path, secret: &str) -> Result<Reply> {
    let json = serde_json::to_string(&Ask {
        open: path.to_path_buf(),
        secret: secret.to_string(),
    })?;
    writeln!(stream, "{json}")?;
    let mut line = String::new();
    std::io::BufReader::new(&mut stream).read_line(&mut line)?;
    if line.is_empty() {
        anyhow::bail!("Raido closed the connection");
    }
    Ok(serde_json::from_str(&line)?)
}

#[cfg(unix)]
fn listen_unix() -> Result<Server> {
    use std::{os::unix::fs::DirBuilderExt, os::unix::net::UnixListener};
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("raido-{}-{stamp:x}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("Create {}", dir.display()))?;
    let socket = dir.join("socket");
    let listener = match UnixListener::bind(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(error).context("Listen for `raido FILE` from terminal panes");
        }
    };
    let (sender, requests) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let sender = sender.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                serve(stream, &sender, None);
            });
        }
    });
    let _ = ADDRESS.set(socket);
    Ok(Server {
        requests,
        dir: Some(dir),
    })
}

#[cfg(unix)]
fn send_unix(socket: &Path, path: &Path) -> Result<Reply> {
    let stream = std::os::unix::net::UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    ask(stream, path, "")
}

/// A secret no other program can guess: hashed from fresh random keys, the time
/// and the process.
pub(crate) fn secret() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for round in 0..2u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(round);
        hasher.write_u32(std::process::id());
        hasher.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

/// Windows: a local TCP port and a secret (see the module notes).
#[cfg_attr(unix, allow(dead_code))]
fn listen_tcp() -> Result<Server> {
    let (server, address) = tcp_server()?;
    let _ = ADDRESS.set(address);
    Ok(server)
}

/// A TCP listener on 127.0.0.1 and its address with a fresh secret.
fn tcp_server() -> Result<(Server, PathBuf)> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("Listen for `raido FILE` from terminal panes")?;
    let port = listener.local_addr()?.port();
    let secret = secret();
    let (sender, requests) = mpsc::channel();
    let expected = secret.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let (sender, expected) = (sender.clone(), expected.clone());
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                serve(stream, &sender, Some(&expected));
            });
        }
    });
    let server = Server {
        requests,
        dir: None,
    };
    Ok((
        server,
        PathBuf::from(format!("tcp:127.0.0.1:{port}:{secret}")),
    ))
}

/// `127.0.0.1:PORT:SECRET`, from `RAIDO_SOCKET` without its `tcp:`.
fn send_tcp(address: &str, path: &Path) -> Result<Reply> {
    let (host, secret) = address
        .rsplit_once(':')
        .context("RAIDO_SOCKET is not host:port:secret")?;
    let target = host.parse().context("RAIDO_SOCKET has no usable address")?;
    let stream = std::net::TcpStream::connect_timeout(&target, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    ask(stream, path, secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(server: &Server, address: PathBuf) {
        let client = std::thread::spawn(move || send(&address, Path::new("/project/notes.txt")));
        let request = server
            .requests
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(request.path, Path::new("/project/notes.txt"));
        request.answer(Reply::Opened("Opened notes.txt".into()));
        assert_eq!(
            client.join().unwrap().unwrap(),
            Reply::Opened("Opened notes.txt".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_terminal_hands_its_path_to_the_running_raido() {
        let server = listen().unwrap();
        let socket = address().unwrap().to_path_buf();
        let dir = socket.parent().unwrap().to_path_buf();
        round_trip(&server, socket.clone());
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "only you can reach the socket");
        }
        drop(server);
        assert!(!dir.exists(), "the socket goes away with Raido");
        assert!(
            send(&socket, Path::new("/x")).is_err(),
            "then raido starts normally"
        );
    }

    /// What Windows uses, tested everywhere.
    #[test]
    fn over_tcp_only_a_pane_with_the_secret_is_heard() {
        let (server, address) = tcp_server().unwrap();
        round_trip(&server, address.clone());
        let text = address.to_string_lossy().into_owned();
        let (host, secret) = text.rsplit_once(':').unwrap();
        assert_eq!(secret.len(), 32);
        let reply = send(Path::new(&format!("{host}:guess")), Path::new("/x")).unwrap();
        assert!(matches!(reply, Reply::Failed(_)), "{reply:?}");
        assert!(server.requests.try_recv().is_err(), "nothing reached Raido");
        // Another server, another secret.
        let (_, other) = tcp_server().unwrap();
        assert_ne!(other.to_string_lossy().rsplit_once(':').unwrap().1, secret);
    }
}

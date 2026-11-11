//! Nmap scan reports, rendered for reading. Nmap's normal (`-oN`, `.nmap`), grepable
//! (`-oG`, `.gnmap`) and XML (`-oX`) outputs are read into one model and shown in the
//! preview pane the way Markdown is: a summary of the hosts, then each host as a
//! heading (`[` / `]` jump between them) with its ports in columns colored by state
//! and script output nested under its port. Noise folds to one line: scripts that
//! failed to run, and the fingerprints Nmap asks you to submit.
use crate::{
    config::{color, Theme},
    editor::Buffer,
    markdown::{pad, spans_width, truncate, wrap, Document, Palette},
    ui::legible,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Reports get more room than prose: version strings and script output are wide.
pub const MAX_WIDTH: usize = 140;

/// Whether `buffer` holds Nmap output: a `.nmap` / `.gnmap` file, or a file that
/// starts the way Nmap's reports do (`-oN scan.txt`, `-oX scan.xml`).
pub fn is_report(buffer: &Buffer) -> bool {
    let named = buffer
        .path
        .as_deref()
        .and_then(Path::extension)
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("nmap") || e.eq_ignore_ascii_case("gnmap"));
    let text = &buffer.text;
    named || starts_report(&text.slice(..text.len_chars().min(4096)).to_string())
}

fn starts_report(head: &str) -> bool {
    let first = head.lines().next().unwrap_or_default();
    (first.starts_with("# Nmap ") && first.contains(" scan initiated "))
        || (head.trim_start().starts_with("<?xml")
            && (head.contains("<!DOCTYPE nmaprun>") || head.contains("<nmaprun ")))
}

#[derive(Default)]
struct Scan {
    /// Source line of the header that starts the scan.
    line: usize,
    version: String,
    started: String,
    command: String,
    /// Nmap's closing summary; missing while a scan runs, or when it was stopped.
    done: Option<String>,
    hosts: Vec<Host>,
    /// Pre-scan and post-scan script results.
    before: Vec<Script>,
    after: Vec<Script>,
    notes: Vec<String>,
}

impl Scan {
    fn is_empty(&self) -> bool {
        self.version.is_empty()
            && self.done.is_none()
            && self.hosts.is_empty()
            && self.before.is_empty()
            && self.after.is_empty()
            && self.notes.is_empty()
    }
}

#[derive(Default)]
struct Host {
    line: usize,
    address: String,
    name: String,
    up: bool,
    /// Latency, and the reason with `--reason`.
    status: String,
    ports: Vec<Port>,
    /// Ports left out of the table ("998 closed tcp ports (reset)").
    hidden: Vec<String>,
    /// `Key: value` lines: MAC address, OS details, service info…
    details: Vec<(String, String)>,
    scripts: Vec<Script>,
    sections: Vec<Section>,
    notes: Vec<String>,
}

#[derive(Default)]
struct Port {
    line: usize,
    /// "22/tcp"; a bare number in a protocol scan (`-sO`).
    port: String,
    state: String,
    service: String,
    reason: String,
    version: String,
    scripts: Vec<Script>,
}

impl Port {
    fn open(&self) -> bool {
        self.state == "open"
    }
}

#[derive(Default)]
struct Script {
    id: String,
    /// Output on the script's own line, after its name.
    head: String,
    lines: Vec<String>,
}

impl Script {
    fn failed(&self) -> bool {
        self.lines.is_empty() && self.head.starts_with("ERROR: Script execution failed")
    }
}

/// A block shown under its title: traceroute hops, or (folded) a fingerprint.
struct Section {
    title: String,
    lines: Vec<String>,
    folded: bool,
}

/// Every scan in `text`; a file can hold several (`--append-output`, or `cat`).
fn parse(text: &str) -> Vec<Scan> {
    let mut scans = if text.trim_start().starts_with('<') {
        parse_xml(text)
    } else if text
        .lines()
        .any(|l| l.starts_with("Host: ") && l.contains('\t'))
    {
        parse_grepable(text)
    } else {
        parse_normal(text)
    };
    scans.retain(|s| !s.is_empty());
    scans
}

/// Nmap's `#` lines: the header that starts a scan, and the closing summary.
fn comment(scans: &mut Vec<Scan>, line: &str, n: usize) {
    if let Some(rest) = line.strip_prefix("# Nmap done at ") {
        let summary = rest.split_once(" -- ").map_or(rest, |(_, s)| s);
        if let Some(scan) = scans.last_mut() {
            scan.done = Some(summary.to_string());
        }
    } else if let Some((version, rest)) = line
        .strip_prefix("# Nmap ")
        .and_then(|r| r.split_once(" scan initiated "))
    {
        if !scans.last().is_some_and(Scan::is_empty) {
            scans.push(Scan::default());
        }
        let (started, command) = rest.split_once(" as: ").unwrap_or((rest, ""));
        *scans.last_mut().expect("just ensured") = Scan {
            line: n,
            version: version.into(),
            started: started.into(),
            command: short_command(command),
            ..Scan::default()
        };
    }
}

/// The command as typed: Nmap records the program's full path when run by one.
fn short_command(command: &str) -> String {
    let end = command
        .find(".exe ")
        .map(|i| i + 4)
        .or_else(|| command.find(' '))
        .unwrap_or(command.len());
    let (program, args) = command.split_at(end);
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    format!("{name}{args}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Port,
    Host,
    Before,
    After,
}

/// Where script output goes: the last port, its host, or the scan.
fn script_list(scan: &mut Scan, into: Owner) -> &mut Vec<Script> {
    match into {
        Owner::Before => &mut scan.before,
        Owner::After => &mut scan.after,
        Owner::Port | Owner::Host => match scan.hosts.last_mut() {
            None => &mut scan.before,
            Some(host) => match host.ports.last_mut() {
                Some(port) if into == Owner::Port => &mut port.scripts,
                _ => &mut host.scripts,
            },
        },
    }
}

/// Lines Nmap ends a report with, outside any host.
const FOOTERS: &[&str] = &[
    "Service detection performed",
    "OS detection performed",
    "OS and Service detection performed",
    "Read data files from",
];

/// Nmap's normal output (`-oN`), the text it prints on screen.
fn parse_normal(text: &str) -> Vec<Scan> {
    let mut scans = vec![Scan::default()];
    // Port table columns: (title, first character), from the table's header row.
    let mut columns: Vec<(String, usize)> = Vec::new();
    let mut into = Owner::Host;
    let mut script_open = false;
    let mut section: Option<Section> = None;
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        let scan = scans.last_mut().expect("at least one scan");
        // Traceroute runs to a blank line; a fingerprint as far as its lines go.
        if let Some(s) = &mut section {
            let more = if s.folded {
                is_fingerprint(line)
            } else {
                !line.is_empty()
                    && !line.starts_with("Nmap scan report for ")
                    && !line.starts_with('#')
            };
            if more {
                s.lines.push(line.to_string());
                continue;
            }
            let s = section.take().expect("checked");
            match scan.hosts.last_mut() {
                Some(host) => host.sections.push(s),
                None => scan.notes.extend([s.title].into_iter().chain(s.lines)),
            }
        }
        if line.is_empty() {
            script_open = false;
            continue;
        }
        if line.starts_with("# ") {
            comment(&mut scans, line, n);
            columns.clear();
            continue;
        }
        if let Some(target) = line.strip_prefix("Nmap scan report for ") {
            let mut host = Host {
                line: n,
                up: true,
                ..Host::default()
            };
            let mut target = target;
            // With -v, down hosts get a line too: "10.0.0.3 [host down, received …]".
            if let Some((t, down)) = target.split_once(" [host down") {
                host.up = false;
                host.status = down
                    .trim_end_matches(']')
                    .trim_start_matches(',')
                    .trim()
                    .to_string();
                target = t;
            }
            (host.name, host.address) =
                match target.strip_suffix(')').and_then(|t| t.split_once(" (")) {
                    Some((name, address)) => (name.into(), address.into()),
                    None => (String::new(), target.into()),
                };
            scan.hosts.push(host);
            columns.clear();
            into = Owner::Host;
            script_open = false;
            continue;
        }
        // Script output: "| id: first line", "|   more", and "|_" on the last line.
        if let Some(body) = line.strip_prefix('|') {
            let last = body.starts_with('_');
            let body = body.strip_prefix(['_', ' ']).unwrap_or(body);
            let list = script_list(scan, into);
            match list.last_mut() {
                Some(script) if script_open => script.lines.push(body.to_string()),
                _ => {
                    let (id, head) = body.split_once(':').unwrap_or((body, ""));
                    let id = id.trim();
                    match list.last_mut() {
                        // A line without a name continues the script before it.
                        Some(script) if id.is_empty() || id.contains(' ') => {
                            script.lines.push(body.to_string())
                        }
                        _ => list.push(Script {
                            id: id.into(),
                            head: head.trim().into(),
                            lines: Vec::new(),
                        }),
                    }
                }
            }
            script_open = !last;
            continue;
        }
        script_open = false;
        if (line.starts_with("PORT ") || line.starts_with("PROTOCOL ")) && line.contains(" STATE") {
            columns = header_columns(line);
            into = Owner::Port;
            continue;
        }
        // Before port rows: "1 service unrecognized …" starts with a digit too.
        if line == "TCP/IP fingerprint:"
            || line.contains(" unrecognized despite returning data")
            || is_fingerprint(line)
        {
            let title = match line.split_once(". ") {
                Some((first, _)) if !line.starts_with("TCP/IP") => first,
                _ => "TCP/IP fingerprint",
            };
            let lines = if is_fingerprint(line) {
                vec![line.into()]
            } else {
                Vec::new()
            };
            section = Some(Section {
                title: title.into(),
                lines,
                folded: true,
            });
            continue;
        }
        if !columns.is_empty() && line.starts_with(|c: char| c.is_ascii_digit()) {
            if let Some(host) = scan.hosts.last_mut() {
                host.ports.push(port_row(line, &columns, n));
            }
            into = Owner::Port;
            continue;
        }
        columns.clear();
        match line {
            "Pre-scan script results:" => into = Owner::Before,
            "Post-scan script results:" => into = Owner::After,
            "Host script results:" => into = Owner::Host,
            _ => {}
        }
        if line.ends_with("script results:") {
            continue;
        }
        if FOOTERS.iter().any(|f| line.starts_with(f)) {
            scan.notes.push(line.into());
            continue;
        }
        if line.starts_with("TRACEROUTE") {
            section = Some(Section {
                title: line.into(),
                lines: Vec::new(),
                folded: false,
            });
            continue;
        }
        let Some(host) = scan.hosts.last_mut() else {
            scan.notes.push(line.into());
            continue;
        };
        if let Some(rest) = line.strip_prefix("Host is up") {
            host.up = true;
            host.status = status_detail(rest);
        } else if line.starts_with("Host is down") || line.starts_with("Host seems down") {
            host.up = false;
        } else if let Some(rest) = line.strip_prefix("Not shown: ") {
            host.hidden.push(rest.into());
        } else if let Some((key, value)) = line.split_once(": ").filter(|(k, _)| is_key(k)) {
            host.details.push((key.into(), value.trim().into()));
        } else {
            host.notes.push(line.into());
        }
    }
    if let Some(s) = section {
        if let Some(host) = scans.last_mut().and_then(|s| s.hosts.last_mut()) {
            host.sections.push(s);
        }
    }
    // "All 1000 scanned ports … are in ignored states" says no more than "Not shown".
    for host in scans.iter_mut().flat_map(|s| &mut s.hosts) {
        if !host.hidden.is_empty() {
            host.notes
                .retain(|n| !(n.starts_with("All ") && n.contains(" scanned ports on ")));
        }
    }
    scans
}

fn is_fingerprint(line: &str) -> bool {
    line.starts_with("SF-Port")
        || line.starts_with("SF:")
        || line.starts_with("OS:")
        || (line.starts_with("====") && line.contains("FINGERPRINT"))
}

/// "MAC Address", "OS details", "Service Info": a label, not a sentence.
fn is_key(key: &str) -> bool {
    key.len() <= 60
        && key.starts_with(|c: char| c.is_ascii_alphabetic())
        && !key.contains(". ")
        && !key.contains('|')
}

/// " (0.045s latency)." → "0.045s latency"; with `--reason`,
/// ", received syn-ack ttl 63 (0.045s latency)." → "received syn-ack ttl 63 · 0.045s latency".
fn status_detail(rest: &str) -> String {
    let rest = rest.trim_start_matches(',').trim().trim_end_matches('.');
    rest.replace(" (", " · ").replace(['(', ')'], "")
}

/// Column titles and where each starts: rows line up under their header.
fn header_columns(line: &str) -> Vec<(String, usize)> {
    let mut columns = Vec::new();
    let mut start = None;
    for (i, c) in line.chars().chain([' ']).enumerate() {
        match (c == ' ', start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                columns.push((line.chars().skip(s).take(i - s).collect(), s));
                start = None;
            }
            _ => {}
        }
    }
    columns
}

fn port_row(line: &str, columns: &[(String, usize)], n: usize) -> Port {
    let chars: Vec<char> = line.chars().collect();
    let mut port = Port {
        line: n,
        ..Port::default()
    };
    for (i, (title, start)) in columns.iter().enumerate() {
        let end = columns
            .get(i + 1)
            .map_or(chars.len(), |c| c.1)
            .min(chars.len());
        let cell: String = chars.get(*start..end).unwrap_or_default().iter().collect();
        let cell = cell.trim().to_string();
        match title.as_str() {
            "PORT" | "PROTOCOL" => port.port = cell,
            "STATE" => port.state = cell,
            "SERVICE" => port.service = cell,
            "VERSION" => port.version = cell,
            _ if cell.is_empty() => {}
            _ if port.reason.is_empty() => port.reason = cell,
            _ => port.reason = format!("{} {cell}", port.reason),
        }
    }
    // A row that doesn't line up with its header: read it by words instead.
    if port.port.is_empty() || port.port.contains(' ') || port.state.contains(' ') {
        let mut words = line.split_whitespace();
        port.port = words.next().unwrap_or_default().into();
        port.state = words.next().unwrap_or_default().into();
        port.service = words.next().unwrap_or_default().into();
        port.reason.clear();
        port.version = words.collect::<Vec<_>>().join(" ");
    }
    port
}

/// Grepable output (`-oG`): a `Host:` line per host with tab-separated fields.
fn parse_grepable(text: &str) -> Vec<Scan> {
    let mut scans = vec![Scan::default()];
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("# ") {
            comment(&mut scans, line, n);
            continue;
        }
        let scan = scans.last_mut().expect("at least one scan");
        let Some(rest) = line.strip_prefix("Host: ") else {
            if !line.trim().is_empty() {
                scan.notes.push(line.into());
            }
            continue;
        };
        let mut fields = rest.split('\t');
        let target = fields.next().unwrap_or_default();
        let (address, name) = match target.strip_suffix(')').and_then(|t| t.split_once(" (")) {
            Some((address, name)) => (address, name),
            None => (target, ""),
        };
        // A host's Status and Ports lines come one after the other.
        if scan.hosts.last().is_none_or(|h| h.address != address) {
            scan.hosts.push(Host {
                line: n,
                address: address.into(),
                name: name.into(),
                up: true,
                ..Host::default()
            });
        }
        let host = scan.hosts.last_mut().expect("just pushed");
        for field in fields {
            let Some((key, value)) = field
                .split_once(": ")
                .or_else(|| field.trim_end().strip_suffix(':').map(|k| (k, "")))
            else {
                continue;
            };
            match key {
                "Status" => host.up = value.trim() == "Up",
                "Ports" => host.ports.extend(grepable_ports(value, n)),
                // "closed (998)" → "998 closed ports"
                "Ignored State" => host.hidden.push(
                    match value.strip_suffix(')').and_then(|v| v.split_once(" (")) {
                        Some((state, count)) => format!("{count} {state} ports"),
                        None => value.into(),
                    },
                ),
                _ => host.details.push((key.into(), value.trim().into())),
            }
        }
    }
    scans
}

/// "22/open/tcp//ssh//OpenSSH 8.2p1/, 80/…": seven `/`-ended fields per port.
/// Nmap writes a `/` inside a field as `|`.
fn grepable_ports(value: &str, line: usize) -> Vec<Port> {
    let mut ports = Vec::new();
    let mut rest = value.trim();
    while !rest.is_empty() {
        let mut fields = Vec::with_capacity(7);
        for _ in 0..7 {
            let (field, tail) = rest.split_once('/').unwrap_or((rest, ""));
            fields.push(field.replace('|', "/"));
            rest = tail;
        }
        rest = rest.trim_start_matches(',').trim_start();
        let [number, state, protocol, owner, service, rpc, version] =
            <[String; 7]>::try_from(fields).expect("seven fields");
        ports.push(Port {
            line,
            port: format!("{number}/{protocol}"),
            state,
            service,
            reason: owner,
            version: [rpc, version]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            scripts: Vec::new(),
        });
    }
    ports
}

/// One XML tag: `<name a="1">`, `</name>`, or `<name a="1"/>`.
struct Tag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, String)>,
    end: bool,
    line: usize,
}

impl Tag<'_> {
    fn get(&self, key: &str) -> &str {
        self.attrs
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, v)| v.as_str())
    }
}

/// The tags of an XML document, in order; text between them is skipped (Nmap puts
/// everything worth showing in attributes).
fn tags(xml: &str) -> Vec<Tag<'_>> {
    let mut tags = Vec::new();
    let (mut at, mut line, mut counted) = (0, 0, 0);
    while let Some(start) = xml[at..].find('<').map(|p| p + at) {
        line += xml[counted..start].matches('\n').count();
        counted = start;
        let rest = &xml[start..];
        let skip = [("<!--", "-->"), ("<?", "?>"), ("<!", ">")]
            .iter()
            .find(|(open, _)| rest.starts_with(open))
            .map(|(_, close)| rest.find(close).map(|e| e + close.len()));
        if let Some(skip) = skip {
            match skip {
                Some(len) => at = start + len,
                None => break,
            }
            continue;
        }
        // The tag ends at the first `>` outside quotes.
        let mut quote = None;
        let Some(len) = rest.bytes().enumerate().skip(1).find_map(|(i, b)| {
            match (quote, b) {
                (Some(q), _) if b == q => quote = None,
                (None, b'"' | b'\'') => quote = Some(b),
                (None, b'>') => return Some(i),
                _ => {}
            }
            None
        }) else {
            break;
        };
        at = start + len + 1;
        let inner = &rest[1..len];
        let (end, inner) = match inner.strip_prefix('/') {
            Some(name) => (true, name),
            None => (false, inner.trim_end_matches('/')),
        };
        let name_end = inner.find(char::is_whitespace).unwrap_or(inner.len());
        tags.push(Tag {
            name: &inner[..name_end],
            attrs: attributes(&inner[name_end..]),
            end,
            line,
        });
    }
    tags
}

fn attributes(text: &str) -> Vec<(&str, String)> {
    let mut attrs = Vec::new();
    let mut rest = text;
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim();
        let value = rest[eq + 1..].trim_start();
        let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) else {
            break;
        };
        let Some(close) = value[1..].find(quote) else {
            break;
        };
        attrs.push((name, unescape(&value[1..close + 1])));
        rest = &value[close + 2..];
    }
    attrs
}

/// XML entities: the five named ones and numeric ones (`&#xa;` is a newline).
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| entity(&rest[1..end]).map(|c| (c, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => name.strip_prefix('#')?.parse().ok(),
            };
            code.and_then(char::from_u32)
        }
    }
}

/// Nmap's XML output (`-oX`).
fn parse_xml(xml: &str) -> Vec<Scan> {
    let mut scan = Scan::default();
    let mut host: Option<Host> = None;
    let mut port: Option<Port> = None;
    let mut into = Owner::Host;
    // Ignored ports: state, count, and the reasons Nmap gives for them.
    let mut extra: Option<(String, String, Vec<String>)> = None;
    // Service info gathered from the ports: host names, OS, device types.
    let mut info: [Vec<String>; 3] = Default::default();
    let mut guesses: Vec<(String, String)> = Vec::new();
    let mut unrecognized: Vec<String> = Vec::new();
    let mut trace: Option<Section> = None;
    for tag in tags(xml) {
        if tag.end {
            match tag.name {
                "port" => {
                    if let (Some(h), Some(p)) = (&mut host, port.take()) {
                        h.ports.push(p);
                    }
                }
                "extraports" => {
                    if let (Some(h), Some((state, count, reasons))) = (&mut host, extra.take()) {
                        h.hidden.push(if reasons.is_empty() {
                            format!("{count} {state} ports")
                        } else {
                            format!("{count} {state} ports ({})", reasons.join(", "))
                        });
                    }
                }
                "trace" => {
                    if let (Some(h), Some(t)) = (&mut host, trace.take()) {
                        h.sections.push(t);
                    }
                }
                "host" => {
                    if let Some(mut h) = host.take() {
                        finish_host(&mut h, &mut info, &mut guesses, &mut unrecognized);
                        scan.hosts.push(h);
                    }
                }
                "hostscript" | "prescript" | "postscript" => into = Owner::Host,
                _ => {}
            }
            continue;
        }
        match tag.name {
            "nmaprun" => {
                scan.line = tag.line;
                scan.command = short_command(tag.get("args"));
                scan.started = tag.get("startstr").into();
                scan.version = tag.get("version").into();
            }
            "host" => {
                host = Some(Host {
                    line: tag.line,
                    up: true,
                    ..Host::default()
                })
            }
            "prescript" => into = Owner::Before,
            "postscript" => into = Owner::After,
            "finished" => {
                let summary = tag.get("summary");
                scan.done = Some(summary.split_once("; ").map_or(summary, |(_, s)| s).into());
            }
            _ => {}
        }
        if tag.name == "script" {
            let output = tag.get("output").replace("\r\n", "\n");
            let mut lines = output.split('\n').map(|l| l.trim_end().to_string());
            let script = Script {
                id: tag.get("id").into(),
                head: lines.next().unwrap_or_default(),
                lines: lines.collect(),
            };
            match (&mut port, &mut host) {
                (Some(p), _) => p.scripts.push(script),
                (None, Some(h)) => h.scripts.push(script),
                (None, None) if into == Owner::After => scan.after.push(script),
                (None, None) => scan.before.push(script),
            }
            continue;
        }
        // Everything else describes the host being read (not a <hosthint>).
        let Some(h) = &mut host else {
            continue;
        };
        match tag.name {
            "status" => h.up = tag.get("state") == "up",
            "address" if tag.get("addrtype") == "mac" => {
                let vendor = tag.get("vendor");
                let mac = if vendor.is_empty() {
                    tag.get("addr").to_string()
                } else {
                    format!("{} ({vendor})", tag.get("addr"))
                };
                h.details.push(("MAC Address".into(), mac));
            }
            "address" if h.address.is_empty() => h.address = tag.get("addr").into(),
            "hostname" if h.name.is_empty() => h.name = tag.get("name").into(),
            "times" => {
                if let Ok(micros) = tag.get("srtt").parse::<f64>() {
                    h.status = format!("{}s latency", significant(micros / 1e6));
                }
            }
            "extraports" => {
                extra = Some((tag.get("state").into(), tag.get("count").into(), Vec::new()))
            }
            "extrareasons" => {
                if let Some((_, _, reasons)) = &mut extra {
                    reasons.push(tag.get("reason").into());
                }
            }
            "port" => {
                port = Some(Port {
                    line: tag.line,
                    port: format!("{}/{}", tag.get("portid"), tag.get("protocol")),
                    ..Port::default()
                })
            }
            "state" => {
                if let Some(p) = &mut port {
                    p.state = tag.get("state").into();
                    // REASON is a column only when the scan asked for it, as on screen.
                    if scan.command.contains("--reason") {
                        p.reason = match tag.get("reason_ttl") {
                            "" | "0" => tag.get("reason").into(),
                            ttl => format!("{} ttl {ttl}", tag.get("reason")),
                        };
                    }
                }
            }
            "service" => {
                if let Some(p) = &mut port {
                    p.service = match tag.get("tunnel") {
                        "" => tag.get("name").into(),
                        tunnel => format!("{tunnel}/{}", tag.get("name")),
                    };
                    let product = [tag.get("product"), tag.get("version")]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ");
                    p.version = match tag.get("extrainfo") {
                        "" => product,
                        extra => format!("{product} ({extra})").trim().into(),
                    };
                    for (list, key) in info.iter_mut().zip(["hostname", "ostype", "devicetype"]) {
                        let value = tag.get(key);
                        if !value.is_empty() && !list.iter().any(|v| v == value) {
                            list.push(value.into());
                        }
                    }
                    if !tag.get("servicefp").is_empty() {
                        unrecognized.push(tag.get("servicefp").into());
                    }
                }
            }
            "osmatch" => guesses.push((tag.get("name").into(), tag.get("accuracy").into())),
            "osfingerprint" => h.sections.push(Section {
                title: "TCP/IP fingerprint".into(),
                lines: tag.get("fingerprint").lines().map(String::from).collect(),
                folded: true,
            }),
            "uptime" => {
                if let Ok(seconds) = tag.get("seconds").parse::<f64>() {
                    let days = format!("{:.3} days", seconds / 86_400.0);
                    let since = tag.get("lastboot");
                    h.details.push((
                        "Uptime guess".into(),
                        if since.is_empty() {
                            days
                        } else {
                            format!("{days} (since {since})")
                        },
                    ));
                }
            }
            "distance" => {
                let hops = tag.get("value");
                let unit = if hops == "1" { "hop" } else { "hops" };
                h.details
                    .push(("Network Distance".into(), format!("{hops} {unit}")));
            }
            "trace" => {
                let using = match (tag.get("port"), tag.get("proto")) {
                    ("", _) => String::new(),
                    (port, proto) => format!(" (using port {port}/{proto})"),
                };
                trace = Some(Section {
                    title: format!("TRACEROUTE{using}"),
                    lines: vec![format!("{:<4}{:<11}ADDRESS", "HOP", "RTT")],
                    folded: false,
                });
            }
            "hop" => {
                if let Some(t) = &mut trace {
                    let address = match tag.get("host") {
                        "" => tag.get("ipaddr").to_string(),
                        name => format!("{name} ({})", tag.get("ipaddr")),
                    };
                    let rtt = format!("{} ms", tag.get("rtt"));
                    t.lines
                        .push(format!("{:<4}{rtt:<11}{address}", tag.get("ttl")));
                }
            }
            _ => {}
        }
    }
    // A scan still running stops mid-host; show what it has so far.
    if let Some(mut h) = host {
        if let Some(p) = port {
            h.ports.push(p);
        }
        finish_host(&mut h, &mut info, &mut guesses, &mut unrecognized);
        scan.hosts.push(h);
    }
    vec![scan]
}

/// The details XML spreads over the host: OS guesses, service info, fingerprints.
fn finish_host(
    host: &mut Host,
    info: &mut [Vec<String>; 3],
    guesses: &mut Vec<(String, String)>,
    unrecognized: &mut Vec<String>,
) {
    match guesses.as_slice() {
        [] => {}
        [(name, accuracy), ..] if accuracy == "100" => {
            host.details.push(("OS details".into(), name.clone()))
        }
        _ => {
            let list: Vec<String> = guesses
                .iter()
                .take(5)
                .map(|(name, accuracy)| format!("{name} ({accuracy}%)"))
                .collect();
            host.details.push(("OS guesses".into(), list.join(", ")));
        }
    }
    let parts: Vec<String> = info
        .iter()
        .zip(["Host", "OS", "Device"])
        .filter(|(values, _)| !values.is_empty())
        .map(|(values, key)| format!("{key}: {}", values.join(", ")))
        .collect();
    if !parts.is_empty() {
        host.details.push(("Service Info".into(), parts.join("; ")));
    }
    if !unrecognized.is_empty() {
        let n = unrecognized.len();
        host.sections.push(Section {
            title: format!(
                "{n} {} unrecognized despite returning data",
                if n == 1 { "service" } else { "services" }
            ),
            lines: std::mem::take(unrecognized),
            folded: true,
        });
    }
    *info = Default::default();
    guesses.clear();
}

/// `x` to two significant digits, the way Nmap prints latency ("0.00014").
fn significant(x: f64) -> String {
    if x <= 0.0 || !x.is_finite() {
        return "0".into();
    }
    let decimals = (1 - x.log10().floor() as i32).max(0) as usize;
    format!("{x:.decimals$}")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "22/tcp" → "22": the overview lists TCP ports by number, others in full.
fn short_port(port: &str) -> &str {
    port.strip_suffix("/tcp").unwrap_or(port)
}

/// Render Nmap output as a report `width` columns wide.
pub fn render(source: &str, width: usize, theme: &Theme) -> Document {
    let scans = parse(source);
    let mut out = Out::new(theme, width.max(24));
    for scan in &scans {
        out.scan(scan);
    }
    if scans.is_empty() {
        out.push(vec![Span::styled(
            "No Nmap results in this file yet · e edits it",
            Style::default().fg(out.p.muted),
        )]);
    }
    while out.doc.lines.last().is_some_and(|l| l.spans.is_empty()) {
        out.doc.lines.pop();
    }
    let hosts = scans.iter().flat_map(|s| &s.hosts);
    let up = hosts.clone().filter(|h| h.up).count();
    let open = hosts.flat_map(|h| &h.ports).filter(|p| p.open()).count();
    out.doc.title = format!(
        "Nmap report  ·  {}  ·  {}",
        plural(up, "host up", "hosts up"),
        plural(open, "open port", "open ports")
    );
    out.doc.marks = "hosts";
    out.doc
}

struct Out {
    p: Palette,
    open: Color,
    closed: Color,
    width: usize,
    doc: Document,
}

impl Out {
    fn new(theme: &Theme, width: usize) -> Self {
        let p = Palette::new(theme);
        let green = theme.ansi.as_ref().map_or(p.string, |a| color(&a[10]));
        Self {
            open: legible(green, theme),
            closed: legible(p.danger, theme),
            p,
            width,
            doc: Document::default(),
        }
    }
    fn style(&self, fg: Color) -> Style {
        Style::default().fg(fg)
    }
    /// Add a line, cut to the width: nothing spills past the pane.
    fn push(&mut self, spans: Vec<Span<'static>>) {
        self.doc.lines.push(Line::from(truncate(spans, self.width)));
    }
    /// One blank line between blocks, never two, never at the top.
    fn gap(&mut self) {
        if self.doc.lines.last().is_some_and(|l| !l.spans.is_empty()) {
            self.push(Vec::new());
        }
    }
    fn label(&self, text: &str) -> Span<'static> {
        Span::styled(
            text.to_string(),
            self.style(self.p.muted).add_modifier(Modifier::BOLD),
        )
    }
    /// `text` after a lead, continued under it: the first line starts with `first`,
    /// the others with `rest` (an indent, or a gutter that runs on).
    fn hang(
        &mut self,
        first: Vec<Span<'static>>,
        rest: Vec<Span<'static>>,
        text: &str,
        style: Style,
    ) {
        let lead = spans_width(&first).max(spans_width(&rest));
        let room = self.width.saturating_sub(lead).max(8);
        for (i, line) in fit(text, style, room).into_iter().enumerate() {
            let mut spans = if i == 0 { first.clone() } else { rest.clone() };
            spans.extend(line);
            self.push(spans);
        }
    }

    fn scan(&mut self, scan: &Scan) {
        self.gap();
        self.doc.anchor(scan.line);
        let pill = Style::default()
            .fg(self.p.background)
            .bg(self.p.accent)
            .add_modifier(Modifier::BOLD);
        let name = match scan.version.as_str() {
            "" => " NMAP SCAN ".to_string(),
            version => format!(" NMAP {version} "),
        };
        let first = vec![Span::styled(name, pill), Span::raw("  ")];
        self.hang(first, Vec::new(), &scan.started, self.style(self.p.muted));
        if !scan.command.is_empty() {
            self.gap();
            let band = Style::default().bg(self.p.code_bg);
            let lines = fit(
                &scan.command,
                band.fg(self.p.fg),
                self.width.saturating_sub(4).max(8),
            );
            for (i, line) in lines.into_iter().enumerate() {
                let lead = if i == 0 { " $ " } else { "   " };
                let mut spans = vec![Span::styled(lead, band.fg(self.p.muted))];
                spans.extend(line);
                self.push(pad(spans, self.width, band));
            }
        }
        let open = scan
            .hosts
            .iter()
            .flat_map(|h| &h.ports)
            .filter(|p| p.open())
            .count();
        // A ping scan (-sn) checks no ports, so it has no port count to give.
        let scanned = scan
            .hosts
            .iter()
            .any(|h| !h.ports.is_empty() || !h.hidden.is_empty());
        let ports = match open {
            _ if !scanned => None,
            0 => Some("no open ports".to_string()),
            n => Some(plural(n, "open port", "open ports")),
        };
        let (summary, tint) = match &scan.done {
            Some(done) => (done.clone(), self.p.muted),
            None => (
                "no end line yet: the scan is still running, or was stopped".to_string(),
                self.p.warning,
            ),
        };
        let summary = match ports {
            Some(ports) if scan.done.is_some() => format!("{ports} · {summary}"),
            Some(ports) => format!("{ports} so far · {summary}"),
            None => summary,
        };
        self.hang(Vec::new(), Vec::new(), &summary, self.style(tint));
        if !scan.before.is_empty() {
            self.gap();
            let label = self.label("PRE-SCAN SCRIPTS");
            self.push(vec![label]);
            self.scripts(&scan.before, 0);
        }
        if scan.hosts.len() > 1 {
            self.overview(&scan.hosts);
        }
        for host in &scan.hosts {
            if host.up || scan.hosts.len() == 1 {
                self.host(host);
            }
        }
        if !scan.after.is_empty() {
            self.gap();
            let label = self.label("POST-SCAN SCRIPTS");
            self.push(vec![label]);
            self.scripts(&scan.after, 0);
        }
        if !scan.notes.is_empty() {
            self.gap();
            for note in &scan.notes {
                let warn = note.to_ascii_lowercase().starts_with("warning");
                let style = self.style(if warn { self.p.warning } else { self.p.muted });
                self.hang(Vec::new(), Vec::new(), note, style);
            }
        }
    }

    /// Every host on a line with its open ports; hosts that are down, together.
    fn overview(&mut self, hosts: &[Host]) {
        self.gap();
        let up: Vec<&Host> = hosts.iter().filter(|h| h.up).collect();
        let down: Vec<&str> = hosts
            .iter()
            .filter(|h| !h.up)
            .map(|h| h.address.as_str())
            .collect();
        let label = self.label("HOSTS");
        // Down hosts are listed only with -v; without it, say nothing about them.
        let count = match down.len() {
            0 => format!("  {} up", up.len()),
            n => format!("  {} up · {n} down", up.len()),
        };
        self.push(vec![label, Span::styled(count, self.style(self.p.muted))]);
        // Names and addresses in columns of their own, so the addresses line up.
        let widest = |cell: fn(&Host) -> &str| {
            up.iter()
                .map(|h| cell(h).width())
                .max()
                .unwrap_or(0)
                .min(self.width / 3)
        };
        let names = widest(|h| &h.name);
        let addresses = widest(|h| &h.address);
        let gap = |width: usize| if width > 0 { width + 2 } else { 0 };
        let column = gap(names) + addresses + 3;
        for host in up {
            let open: Vec<String> = host
                .ports
                .iter()
                .filter(|p| p.open())
                .map(|p| {
                    format!("{} {}", short_port(&p.port), p.service)
                        .trim()
                        .to_string()
                })
                .collect();
            let (text, tint) = if open.is_empty() {
                let mut text = "no open ports".to_string();
                for state in ["open|filtered", "filtered", "closed"] {
                    let n = host.ports.iter().filter(|p| p.state == state).count();
                    if n > 0 {
                        text.push_str(&format!(" · {n} {state}"));
                    }
                }
                (text, self.p.muted)
            } else {
                (open.join(" · "), self.p.fg)
            };
            let dot = if tint == self.p.fg {
                self.open
            } else {
                self.p.muted
            };
            let cell = |text: &str, width: usize, tint: Color| {
                let text = vec![Span::styled(text.to_string(), self.style(tint))];
                pad(truncate(text, width), width, Style::default())
            };
            let mut first = vec![Span::raw("  "), Span::styled("● ", self.style(dot))];
            first.extend(cell(&host.name, gap(names), self.p.accent));
            first.extend(cell(&host.address, addresses + 3, self.p.fg));
            let rest = vec![Span::raw(" ".repeat(4 + column))];
            self.hang(first, rest, &text, self.style(tint));
        }
        if !down.is_empty() {
            let first = vec![Span::raw("  "), Span::styled("○ ", self.style(self.closed))];
            // A verbose sweep reports every address it tried: name the first few.
            const NAMED: usize = 24;
            let mut text = format!(
                "{} down: {}",
                down.len(),
                down[..down.len().min(NAMED)].join(", ")
            );
            if down.len() > NAMED {
                text.push_str(&format!(" and {} more", down.len() - NAMED));
            }
            self.hang(
                first,
                vec![Span::raw("    ")],
                &text,
                self.style(self.p.muted),
            );
        }
    }

    fn host(&mut self, host: &Host) {
        self.gap();
        self.doc.anchor(host.line);
        self.doc.headings.push(self.doc.lines.len());
        let strong = self.style(self.p.accent).add_modifier(Modifier::BOLD);
        let dot = if host.up {
            Span::styled("● ", self.style(self.open))
        } else {
            Span::styled("○ ", self.style(self.closed))
        };
        let mut head = vec![dot];
        if host.name.is_empty() {
            head.push(Span::styled(host.address.clone(), strong));
        } else {
            head.push(Span::styled(host.name.clone(), strong));
            head.push(Span::styled(
                format!("  {}", host.address),
                self.style(self.p.fg),
            ));
        }
        self.push(truncate(head, self.width));
        let mut status = vec![if host.up { "up" } else { "down" }.to_string()];
        if !host.status.is_empty() {
            status.push(host.status.clone());
        }
        if !host.ports.is_empty() || !host.hidden.is_empty() {
            status.push(match host.ports.iter().filter(|p| p.open()).count() {
                0 => "no open ports".into(),
                n => plural(n, "open port", "open ports"),
            });
        }
        let lead = vec![Span::raw("  ")];
        self.hang(
            lead.clone(),
            lead.clone(),
            &status.join(" · "),
            self.style(self.p.muted),
        );
        self.push(vec![Span::styled(
            "─".repeat(self.width),
            self.style(self.p.border),
        )]);
        if !host.ports.is_empty() {
            self.ports(&host.ports);
        }
        for hidden in &host.hidden {
            self.hang(
                lead.clone(),
                lead.clone(),
                &format!("{hidden} not shown"),
                self.style(self.p.muted),
            );
        }
        if !host.details.is_empty() {
            self.gap();
            self.details(&host.details);
        }
        if !host.scripts.is_empty() {
            self.gap();
            let label = self.label("HOST SCRIPTS");
            self.push(vec![Span::raw("  "), label]);
            self.scripts(&host.scripts, 2);
        }
        for section in &host.sections {
            self.gap();
            self.section(section);
        }
        if !host.notes.is_empty() {
            self.gap();
            for note in &host.notes {
                self.hang(lead.clone(), lead.clone(), note, self.style(self.p.muted));
            }
        }
    }

    /// The port table: columns as wide as their longest entry, the version wrapping
    /// in the room left, and each port's script output underneath it.
    fn ports(&mut self, ports: &[Port]) {
        self.gap();
        let column = |title: &str, cap: usize, cell: fn(&Port) -> &str| {
            ports
                .iter()
                .map(|p| cell(p).width())
                .chain([title.len()])
                .max()
                .unwrap_or(0)
                .min(cap)
        };
        let protocols = ports.iter().all(|p| !p.port.contains('/'));
        let first = if protocols { "PROTOCOL" } else { "PORT" };
        let mut widths = vec![
            (first, column(first, 16, |p| &p.port)),
            ("STATE", column("STATE", 16, |p| &p.state)),
            ("SERVICE", column("SERVICE", 20, |p| &p.service)),
        ];
        if ports.iter().any(|p| !p.reason.is_empty()) {
            widths.push(("REASON", column("REASON", 24, |p| &p.reason)));
        }
        let lead_width = 2 + widths.iter().map(|(_, w)| w + 2).sum::<usize>();
        // Versions wrap in a column of their own, or go under their port when narrow.
        let beside = self.width.saturating_sub(lead_width) >= 16;
        let versions = beside && ports.iter().any(|p| !p.version.is_empty());
        let cell = |text: &str, width: usize, style: Style| {
            let mut spans = pad(
                truncate(vec![Span::styled(text.to_string(), style)], width),
                width,
                Style::default(),
            );
            spans.push(Span::raw("  "));
            spans
        };
        let header = self.style(self.p.muted).add_modifier(Modifier::BOLD);
        let mut spans = vec![Span::raw("  ")];
        for (title, width) in &widths {
            spans.extend(cell(title, *width, header));
        }
        if versions {
            spans.push(Span::styled("VERSION", header));
        }
        self.push(spans);
        for port in ports {
            self.doc.anchor(port.line);
            let (text, service) = if port.open() {
                (self.p.fg, self.p.function)
            } else {
                (self.p.muted, self.p.muted)
            };
            let state = match port.state.as_str() {
                "open" => self.open,
                "closed" => self.closed,
                _ => self.p.warning,
            };
            let number = if port.open() {
                self.style(text).add_modifier(Modifier::BOLD)
            } else {
                self.style(text)
            };
            let mut lead = vec![Span::raw("  ")];
            lead.extend(cell(&port.port, widths[0].1, number));
            lead.extend(cell(&port.state, widths[1].1, self.style(state)));
            lead.extend(cell(&port.service, widths[2].1, self.style(service)));
            if let Some(&(_, width)) = widths.get(3) {
                lead.extend(cell(&port.reason, width, self.style(self.p.muted)));
            }
            if beside {
                let rest = vec![Span::raw(" ".repeat(lead_width))];
                self.hang(lead, rest, &port.version, self.style(text));
            } else {
                self.push(lead);
                if !port.version.is_empty() {
                    let indent = vec![Span::raw(" ".repeat(4 + widths[0].1))];
                    self.hang(indent.clone(), indent, &port.version, self.style(text));
                }
            }
            self.scripts(&port.scripts, 2 + widths[0].1 + 2);
        }
    }

    /// Script output behind a gutter, each script's name in color; scripts that
    /// failed to run are listed together on one line.
    fn scripts(&mut self, scripts: &[Script], indent: usize) {
        let gutter = vec![
            Span::raw(" ".repeat(indent)),
            Span::styled("│ ", self.style(self.p.border)),
        ];
        let name = self.style(self.p.function);
        let body = self.style(self.p.fg);
        for script in scripts.iter().filter(|s| !s.failed()) {
            let mut first = gutter.clone();
            first.push(Span::styled(script.id.clone(), name));
            let beside = indent + 2 + script.id.width() + 2 + 20 <= self.width;
            if script.head.is_empty() {
                self.push(first);
            } else if !beside {
                // No room beside a long name: the output goes on the lines below.
                self.push(first);
                let mut lead = gutter.clone();
                lead.push(Span::raw("  "));
                self.hang(lead.clone(), lead, &script.head, body);
            } else {
                first.push(Span::raw("  "));
                let mut rest = gutter.clone();
                rest.push(Span::raw(" ".repeat(script.id.width() + 2)));
                self.hang(first, rest, &script.head, body);
            }
            // Script output is indented by Nmap and by the script; keep the script's.
            let common = script
                .lines
                .iter()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.len() - l.trim_start().len())
                .min()
                .unwrap_or(0);
            for line in &script.lines {
                let own = line.len() - line.trim_start().len();
                let mut lead = gutter.clone();
                lead.push(Span::raw(" ".repeat(2 + own.saturating_sub(common))));
                self.hang(lead.clone(), lead, line.trim(), body);
            }
        }
        let failed: Vec<&str> = scripts
            .iter()
            .filter(|s| s.failed())
            .map(|s| s.id.as_str())
            .collect();
        if !failed.is_empty() {
            let text = format!(
                "{} failed to run (nmap -d shows why): {}",
                plural(failed.len(), "script", "scripts"),
                failed.join(", ")
            );
            self.hang(gutter.clone(), gutter, &text, self.style(self.p.muted));
        }
    }

    /// `Key: value` lines with the values lined up.
    fn details(&mut self, details: &[(String, String)]) {
        let column = details
            .iter()
            .map(|(k, _)| k.width())
            .max()
            .unwrap_or(0)
            .min(24);
        for (key, value) in details {
            let tint = if key.to_ascii_lowercase().starts_with("warning") {
                self.p.warning
            } else {
                self.p.muted
            };
            let mut first = vec![Span::raw("  ")];
            first.extend(pad(
                truncate(vec![Span::styled(key.clone(), self.style(tint))], column),
                column + 2,
                Style::default(),
            ));
            let rest = vec![Span::raw(" ".repeat(column + 4))];
            self.hang(first, rest, value, self.style(self.p.fg));
        }
    }

    fn section(&mut self, section: &Section) {
        let lead = vec![Span::raw("  ")];
        if section.folded {
            let text = format!(
                "▸ {} · {} folded · e shows the source",
                section.title,
                plural(section.lines.len(), "line", "lines")
            );
            let style = self.style(self.p.muted).add_modifier(Modifier::ITALIC);
            self.hang(lead, vec![Span::raw("    ")], &text, style);
            return;
        }
        let label = self.label(&section.title);
        self.push(vec![Span::raw("  "), label]);
        for line in &section.lines {
            self.hang(lead.clone(), lead.clone(), line, self.style(self.p.fg));
        }
    }
}

/// `text` broken into lines `width` columns wide: at spaces for prose, anywhere
/// for text set in columns (runs of spaces), so its alignment survives.
fn fit(text: &str, style: Style, width: usize) -> Vec<Vec<Span<'static>>> {
    let text = text.replace('\t', "    ");
    let text = text.trim_end();
    if !text.trim_start().contains("  ") {
        return wrap(
            vec![Span::styled(text.trim_start().to_string(), style)],
            width,
        );
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            lines.push(vec![Span::styled(std::mem::take(&mut line), style)]);
            used = 0;
        }
        line.push(c);
        used += w;
    }
    lines.push(vec![Span::styled(line, style)]);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nmap 7.992's `-sV -sC` report on this machine's loopback, trimmed.
    const NORMAL: &str = r#"# Nmap 7.992 scan initiated Wed Nov 11 04:09:04 2026 as: /usr/bin/nmap -sV -sC -p 22,631,18080,34125 -oA local 127.0.0.1
Nmap scan report for localhost (127.0.0.1)
Host is up (0.000044s latency).

PORT      STATE  SERVICE VERSION
22/tcp    closed ssh
631/tcp   open   ipp     CUPS 2.4
|_hnap-info: ERROR: Script execution failed (use -d to debug)
|_http-title: Home - CUPS 2.4.20
| http-robots.txt: 1 disallowed entry
|_/
|_rpcinfo: ERROR: Script execution failed (use -d to debug)
18080/tcp open   http    SimpleHTTPServer 0.6 (Python 3.14.8)
|_http-title: Raido test page
34125/tcp open   unknown
| fingerprint-strings:
|   FourOhFourRequest, GetRequest:
|     HTTP/1.1 401 Unauthorized
|     Content-Length: 0
|_    Connection: close
1 service unrecognized despite returning data. If you know the service/version, please submit the following fingerprint at https://nmap.org/cgi-bin/submit.cgi?new-service :
SF-Port34125-TCP:V=7.992%I=7%D=11/11%Time=6AF3C036%P=x86_64-pc-linux-gnu%r
SF:(GetRequest,43,"HTTP/1\.1\x20401\x20Unauthorized\r\nContent-Length:\x20
SF:0\r\nConnection:\x20close\r\n\r\n");

Service detection performed. Please report any incorrect results at https://nmap.org/submit/ .
# Nmap done at Wed Nov 11 04:11:26 2026 -- 1 IP address (1 host up) scanned in 141.18 seconds
"#;

    /// A verbose `-A` scan of a small network: down hosts, OS detection, traceroute.
    const NETWORK: &str = "# Nmap 7.94 scan initiated Mon Nov 10 12:00:00 2025 as: nmap -v -A -oN lab.nmap 10.0.0.0/30
Nmap scan report for 10.0.0.0 [host down]
Nmap scan report for gw.lab (10.0.0.1)
Host is up (0.0012s latency).
Not shown: 998 filtered tcp ports (no-response)
PORT   STATE SERVICE VERSION
22/tcp open  ssh     OpenSSH 9.6 (protocol 2.0)
| ssh-hostkey:
|   256 aa:bb:cc:dd (ECDSA)
|_  256 ee:ff:00:11 (ED25519)
80/tcp open  http    nginx 1.24.0
|_http-title: Router
MAC Address: 00:11:22:33:44:55 (Example Vendor)
Device type: general purpose
Running: Linux 5.X
OS details: Linux 5.0 - 5.14
Network Distance: 1 hop
Service Info: OS: Linux; CPE: cpe:/o:linux:linux_kernel

Host script results:
|_clock-skew: -1s

TRACEROUTE
HOP RTT     ADDRESS
1   1.20 ms gw.lab (10.0.0.1)

Nmap scan report for 10.0.0.2
Host is up (0.0020s latency).
All 1000 scanned ports on 10.0.0.2 are in ignored states.
Not shown: 1000 closed tcp ports (reset)
MAC Address: 66:77:88:99:AA:BB (Other Vendor)
Too many fingerprints match this host to give specific OS details
Network Distance: 1 hop

Nmap scan report for 10.0.0.3 [host down, received no-response]
Read data files from: /usr/bin/../share/nmap
OS and Service detection performed. Please report any incorrect results at https://nmap.org/submit/ .
# Nmap done at Mon Nov 10 12:01:00 2025 -- 4 IP addresses (2 hosts up) scanned in 60.00 seconds
";

    /// Nmap 7.992's `--reason -sV -oG` of three loopback addresses.
    const GREPABLE: &str = "# Nmap 7.992 scan initiated Wed Nov 11 04:11:26 2026 as: nmap --reason -sV -p 631,18080,1-100 -oA reason 127.0.0.1-3
Host: 127.0.0.1 (localhost)\tStatus: Up
Host: 127.0.0.1 (localhost)\tPorts: 631/open/tcp//ipp//CUPS 2.4/, 443/open/tcp//ssl|http//nginx 1.24 (Ubuntu)/, 18080/open/tcp//http//SimpleHTTPServer 0.6 (Python 3.14.8)/\tIgnored State: closed (100)
Host: 127.0.0.2 (localhost)\tStatus: Up
Host: 127.0.0.2 (localhost)\tPorts: \tIgnored State: closed (102)
Host: 127.0.0.3 ()\tStatus: Down
# Nmap done at Wed Nov 11 04:11:32 2026 -- 3 IP addresses (2 hosts up) scanned in 6.31 seconds
";

    /// Nmap 7.992's `-oX` of the scan in [`NORMAL`], trimmed.
    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nmaprun>
<?xml-stylesheet href="file:///usr/share/nmap/nmap.xsl" type="text/xsl"?>
<!-- Nmap 7.992 scan initiated Wed Nov 11 04:09:04 2026 as: /usr/bin/nmap -sV -sC -p 22,631,34125 -oA local 127.0.0.1 -->
<nmaprun scanner="nmap" args="/usr/bin/nmap -sV -sC -p 22,631,34125 -oA local 127.0.0.1" start="1794359344" startstr="Wed Nov 11 04:09:04 2026" version="7.992" xmloutputversion="1.05">
<scaninfo type="connect" protocol="tcp" numservices="3" services="22,631,34125"/>
<hosthint><status state="up" reason="unknown-response" reason_ttl="0"/>
<address addr="127.0.0.9" addrtype="ipv4"/>
<hostnames>
</hostnames>
</hosthint>
<host starttime="1794359345" endtime="1794359486"><status state="up" reason="conn-refused" reason_ttl="0"/>
<address addr="127.0.0.1" addrtype="ipv4"/>
<hostnames>
<hostname name="localhost" type="PTR"/>
</hostnames>
<ports><extraports state="closed" count="997">
<extrareasons reason="conn-refused" count="997" proto="tcp" ports="1-21"/>
</extraports>
<port protocol="tcp" portid="22"><state state="closed" reason="conn-refused" reason_ttl="0"/><service name="ssh" method="table" conf="3"/></port>
<port protocol="tcp" portid="631"><state state="open" reason="syn-ack" reason_ttl="0"/><service name="ipp" product="CUPS" version="2.4" method="probed" conf="10"><cpe>cpe:/a:apple:cups:2.4</cpe></service><script id="hnap-info" output="ERROR: Script execution failed (use -d to debug)"/><script id="http-title" output="Home - CUPS 2.4.20"><elem key="title">Home - CUPS 2.4.20</elem>
</script><script id="http-robots.txt" output="1 disallowed entry &#xa;/"/></port>
<port protocol="tcp" portid="34125"><state state="open" reason="syn-ack" reason_ttl="0"/><service name="unknown" servicefp="SF-Port34125-TCP:V=7.992%r(GetRequest,43,&quot;HTTP/1\.1\x20401&quot;);" method="table" conf="3"/><script id="fingerprint-strings" output="&#xa;  FourOhFourRequest, GetRequest: &#xa;    HTTP/1.1 401 Unauthorized&#xa;    Connection: close"/></port>
</ports>
<times srtt="44" rttvar="698" to="100000"/>
</host>
<runstats><finished time="1794359486" timestr="Wed Nov 11 04:11:26 2026" summary="Nmap done at Wed Nov 11 04:11:26 2026; 1 IP address (1 host up) scanned in 141.18 seconds" elapsed="141.18" exit="success"/><hosts up="1" down="0" total="1"/>
</runstats>
</nmaprun>
"#;

    fn plain(doc: &Document) -> Vec<String> {
        doc.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn theme() -> Theme {
        crate::config::load_theme_from("visual-studio-dark", None).expect("bundled theme")
    }

    #[test]
    fn reports_are_recognized_by_name_or_by_how_they_start() {
        assert!(starts_report(NORMAL));
        assert!(starts_report(GREPABLE));
        assert!(starts_report(XML));
        assert!(!starts_report("# Nmap notes\nscan initiated by hand"));
        assert!(!starts_report("<?xml version=\"1.0\"?>\n<project/>"));
        let mut buffer = Buffer::empty();
        buffer.path = Some("scan.gnmap".into());
        assert!(is_report(&buffer), "by extension, even while empty");
        buffer.path = Some("scan.txt".into());
        assert!(!is_report(&buffer));
        buffer.insert(NORMAL);
        assert!(is_report(&buffer), "by content");
    }

    #[test]
    fn normal_output_reads_ports_scripts_and_fingerprints() {
        let scans = parse(NORMAL);
        assert_eq!(scans.len(), 1);
        let scan = &scans[0];
        assert_eq!(scan.version, "7.992");
        assert_eq!(
            scan.command,
            "nmap -sV -sC -p 22,631,18080,34125 -oA local 127.0.0.1"
        );
        assert_eq!(
            scan.done.as_deref(),
            Some("1 IP address (1 host up) scanned in 141.18 seconds")
        );
        let host = &scan.hosts[0];
        assert_eq!(
            (host.name.as_str(), host.address.as_str()),
            ("localhost", "127.0.0.1")
        );
        assert_eq!(host.status, "0.000044s latency");
        let ports: Vec<(&str, &str, &str)> = host
            .ports
            .iter()
            .map(|p| (p.port.as_str(), p.state.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(
            ports,
            [
                ("22/tcp", "closed", ""),
                ("631/tcp", "open", "CUPS 2.4"),
                ("18080/tcp", "open", "SimpleHTTPServer 0.6 (Python 3.14.8)"),
                ("34125/tcp", "open", ""),
            ]
        );
        let cups = &host.ports[1].scripts;
        assert_eq!(cups.len(), 4);
        assert_eq!(cups.iter().filter(|s| s.failed()).count(), 2);
        assert_eq!(
            (cups[2].id.as_str(), cups[2].head.as_str()),
            ("http-robots.txt", "1 disallowed entry")
        );
        assert_eq!(cups[2].lines, ["/"]);
        let strings = &host.ports[3].scripts[0];
        assert_eq!(strings.id, "fingerprint-strings");
        assert_eq!(strings.lines.len(), 4);
        assert_eq!(strings.lines[3], "    Connection: close");
        let [fingerprint] = host.sections.as_slice() else {
            panic!("one section");
        };
        assert!(fingerprint.folded);
        assert_eq!(
            fingerprint.title,
            "1 service unrecognized despite returning data"
        );
        assert_eq!(fingerprint.lines.len(), 3);
        assert_eq!(
            scan.notes.len(),
            1,
            "the closing remark is the scan's, not the host's"
        );
        assert!(host.notes.is_empty());
    }

    #[test]
    fn verbose_output_reads_down_hosts_os_details_and_traceroute() {
        let scan = &parse(NETWORK)[0];
        let up: Vec<&str> = scan
            .hosts
            .iter()
            .filter(|h| h.up)
            .map(|h| h.address.as_str())
            .collect();
        assert_eq!(up, ["10.0.0.1", "10.0.0.2"]);
        assert_eq!(scan.hosts.len(), 4);
        assert_eq!(scan.hosts[3].status, "received no-response");
        let gw = &scan.hosts[1];
        assert_eq!(gw.hidden, ["998 filtered tcp ports (no-response)"]);
        let keys: Vec<&str> = gw.details.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "MAC Address",
                "Device type",
                "Running",
                "OS details",
                "Network Distance",
                "Service Info"
            ]
        );
        assert_eq!(gw.details[5].1, "OS: Linux; CPE: cpe:/o:linux:linux_kernel");
        assert_eq!(gw.ports[0].scripts[0].lines.len(), 2);
        assert_eq!(
            gw.scripts[0].id, "clock-skew",
            "host scripts stay off the last port"
        );
        assert_eq!(gw.ports[1].scripts.len(), 1);
        assert_eq!(gw.sections[0].title, "TRACEROUTE");
        assert_eq!(gw.sections[0].lines.len(), 2);
        let quiet = &scan.hosts[2];
        assert_eq!(
            quiet.notes,
            ["Too many fingerprints match this host to give specific OS details"],
            "\"All 1000 scanned ports\" repeats \"Not shown\""
        );
        assert_eq!(scan.notes.len(), 2);
    }

    #[test]
    fn grepable_output_merges_each_hosts_lines() {
        let scan = &parse(GREPABLE)[0];
        assert_eq!(scan.hosts.len(), 3);
        let first = &scan.hosts[0];
        assert_eq!(first.name, "localhost");
        assert_eq!(first.hidden, ["100 closed ports"]);
        assert_eq!(first.ports.len(), 3);
        assert_eq!(first.ports[1].service, "ssl/http", "| stands for /");
        assert_eq!(
            first.ports[2].version,
            "SimpleHTTPServer 0.6 (Python 3.14.8)"
        );
        assert!(scan.hosts[1].ports.is_empty());
        assert!(!scan.hosts[2].up);
        assert_eq!(scan.hosts[2].name, "");
    }

    #[test]
    fn xml_output_reads_hosts_but_not_host_hints() {
        let scan = &parse(XML)[0];
        assert_eq!(scan.version, "7.992");
        assert_eq!(scan.started, "Wed Nov 11 04:09:04 2026");
        assert_eq!(
            scan.command,
            "nmap -sV -sC -p 22,631,34125 -oA local 127.0.0.1"
        );
        assert_eq!(
            scan.done.as_deref(),
            Some("1 IP address (1 host up) scanned in 141.18 seconds")
        );
        let [host] = scan.hosts.as_slice() else {
            panic!("one host, the hint skipped");
        };
        assert_eq!(
            (host.name.as_str(), host.address.as_str()),
            ("localhost", "127.0.0.1")
        );
        assert_eq!(host.status, "0.000044s latency");
        assert_eq!(host.hidden, ["997 closed ports (conn-refused)"]);
        assert_eq!(host.ports.len(), 3);
        assert_eq!(host.ports[1].version, "CUPS 2.4");
        let scripts = &host.ports[1].scripts;
        assert!(scripts[0].failed());
        assert_eq!(scripts[1].head, "Home - CUPS 2.4.20");
        assert_eq!(
            (scripts[2].head.as_str(), scripts[2].lines.as_slice()),
            ("1 disallowed entry", &["/".to_string()][..])
        );
        let strings = &host.ports[2].scripts[0];
        assert_eq!(strings.head, "");
        assert_eq!(strings.lines[1], "    HTTP/1.1 401 Unauthorized");
        assert!(host.sections[0].folded);
        assert!(
            host.sections[0].lines[0].contains("\"HTTP/1\\.1\\x20401\""),
            "entities decoded"
        );
        assert_eq!(
            unescape("a &lt;b&gt; &amp;c &#65;&#x42; &bogus; &"),
            "a <b> &c AB &bogus; &"
        );
    }

    #[test]
    fn the_report_lines_ports_up_and_folds_the_noise() {
        let doc = render(NORMAL, 100, &theme());
        let text = plain(&doc);
        assert!(
            text[0].starts_with(" NMAP 7.992   Wed Nov 11 04:09:04 2026"),
            "{:?}",
            text[0]
        );
        assert!(text
            .iter()
            .any(|l| l == " $ nmap -sV -sC -p 22,631,18080,34125 -oA local 127.0.0.1"));
        assert!(text
            .iter()
            .any(|l| l.starts_with("3 open ports · 1 IP address (1 host up)")));
        assert!(
            text.iter()
                .any(|l| l == "  PORT       STATE   SERVICE  VERSION"),
            "{text:#?}"
        );
        assert!(text
            .iter()
            .any(|l| l == "  631/tcp    open    ipp      CUPS 2.4"));
        assert!(text
            .iter()
            .any(|l| l == "             │ http-title  Home - CUPS 2.4.20"));
        assert!(text.iter().any(|l| l
            == "             │ 2 scripts failed to run (nmap -d shows why): hnap-info, rpcinfo"));
        assert!(text
            .iter()
            .any(|l| l == "             │   FourOhFourRequest, GetRequest:"));
        assert!(text
            .iter()
            .any(|l| l == "             │     HTTP/1.1 401 Unauthorized"));
        assert!(text.iter().any(
            |l| l.starts_with("  ▸ 1 service unrecognized despite returning data · 3 lines folded")
        ));
        assert!(
            !text.iter().any(|l| l.contains("SF:")),
            "fingerprints stay folded"
        );
        assert_eq!(doc.headings.len(), 1);
        assert_eq!(doc.title, "Nmap report  ·  1 host up  ·  3 open ports");
        // Source and rendered view line up both ways, for `e` and the live preview.
        let cups = text
            .iter()
            .position(|l| l.starts_with("  631/tcp"))
            .unwrap();
        assert_eq!(doc.line_for_source(6), cups);
        assert_eq!(doc.source_for_line(cups + 1), 6);
    }

    #[test]
    fn several_hosts_get_an_overview_and_down_hosts_one_line() {
        let doc = render(NETWORK, 90, &theme());
        let text = plain(&doc);
        assert!(
            text.iter().any(|l| l == "HOSTS  2 up · 2 down"),
            "{text:#?}"
        );
        assert!(text
            .iter()
            .any(|l| l == "  ● gw.lab  10.0.0.1   22 ssh · 80 http"));
        assert!(text
            .iter()
            .any(|l| l == "  ●         10.0.0.2   no open ports"));
        assert!(text.iter().any(|l| l == "  ○ 2 down: 10.0.0.0, 10.0.0.3"));
        assert_eq!(doc.headings.len(), 2, "down hosts get no section");
        assert!(text
            .iter()
            .any(|l| l == "  OS details        Linux 5.0 - 5.14"));
        assert!(text
            .iter()
            .any(|l| l == "  1000 closed tcp ports (reset) not shown"));
        assert!(
            text.iter().any(|l| l == "  HOP RTT     ADDRESS"),
            "columns keep their spacing"
        );
    }

    #[test]
    fn a_ping_scan_gives_no_port_count() {
        // Nmap 7.992's `-v -sn` of an empty bridge network.
        let ping =
            "# Nmap 7.992 scan initiated Wed Nov 11 04:27:47 2026 as: nmap -v -sn 172.17.0.1-2
Nmap scan report for 172.17.0.1 [host down]
Nmap scan report for 172.17.0.2 [host down]
# Nmap done at Wed Nov 11 04:27:50 2026 -- 2 IP addresses (0 hosts up) scanned in 3.00 seconds
";
        let text = plain(&render(ping, 80, &theme()));
        assert!(
            text.iter()
                .any(|l| l == "2 IP addresses (0 hosts up) scanned in 3.00 seconds"),
            "{text:#?}"
        );
        assert!(text.iter().any(|l| l == "HOSTS  0 up · 2 down"));
        assert!(text
            .iter()
            .any(|l| l == "  ○ 2 down: 172.17.0.1, 172.17.0.2"));
    }
    #[test]
    fn narrow_panes_wrap_instead_of_overflowing() {
        for source in [NORMAL, NETWORK, GREPABLE, XML] {
            for width in [24, 40, 60] {
                let doc = render(source, width, &theme());
                for line in plain(&doc) {
                    assert!(line.width() <= width, "{width}: {line:?}");
                }
            }
        }
        assert_eq!(
            plain(&render("", 80, &theme()))[0],
            "No Nmap results in this file yet · e edits it"
        );
    }

    #[test]
    fn a_scan_still_running_says_so() {
        let running = NORMAL.lines().take(8).collect::<Vec<_>>().join("\n");
        let text = plain(&render(&running, 100, &theme()));
        assert!(text.iter().any(|l| l.contains("no end line yet")));
        let xml = XML.split("</host>").next().unwrap();
        let scan = &parse(xml)[0];
        assert_eq!(scan.hosts[0].ports.len(), 3, "the host read so far");
        assert!(scan.done.is_none());
    }

    #[test]
    fn appended_scans_each_get_their_own_report() {
        let both = format!("{NORMAL}{NORMAL}");
        assert_eq!(parse(&both).len(), 2);
        assert_eq!(render(&both, 100, &theme()).headings.len(), 2);
    }

    #[test]
    fn latency_keeps_two_significant_digits() {
        assert_eq!(significant(0.000144), "0.00014");
        assert_eq!(significant(0.045), "0.045");
        assert_eq!(significant(1.5), "1.5");
        assert_eq!(significant(12.3), "12");
        assert_eq!(
            short_command(r"C:\Program Files (x86)\Nmap\nmap.exe -sV host"),
            "nmap.exe -sV host"
        );
    }
}

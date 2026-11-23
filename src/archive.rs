//! Archives, read-only: zip and the formats built on it (.jar, .whl, .apk, .vsix,
//! .docx, .epub…), tar plain or compressed (gzip, bzip2, xz, zstd), a single
//! compressed file (.gz, .bz2, .xz, .zst) and 7z. Raido reads them itself, in
//! Rust, so no unzip, tar or 7z is needed on any system. The files show as a tree;
//! one you open is copied out, read-only, to a temporary folder and opens as any
//! file does: text, a spreadsheet, hex, or another archive.
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    hash::{Hash, Hasher},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A file larger than this isn't copied out to open.
const MAX_OPEN: u64 = 512 << 20;
/// Copies older than this are cleared away.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

/// How the archive's bytes are compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Tar(Packing),
    /// One compressed file (`.gz`, `.bz2`, `.xz`, `.zst`).
    Single(Packing),
    SevenZ,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Zip => "Zip archive",
            Format::SevenZ => "7-Zip archive",
            Format::Tar(Packing::None) => "tar archive",
            Format::Tar(Packing::Gzip) => "tar archive, gzip",
            Format::Tar(Packing::Bzip2) => "tar archive, bzip2",
            Format::Tar(Packing::Xz) => "tar archive, xz",
            Format::Tar(Packing::Zstd) => "tar archive, Zstandard",
            Format::Single(Packing::Gzip) => "gzip file",
            Format::Single(Packing::Bzip2) => "bzip2 file",
            Format::Single(Packing::Xz) => "xz file",
            Format::Single(_) => "Zstandard file",
        }
    }
}

/// The archive its first bytes say `path` is, if Raido reads that kind.
pub fn format(path: &Path) -> Option<Format> {
    let mut head = Vec::new();
    File::open(path)
        .ok()?
        .take(1024)
        .read_to_end(&mut head)
        .ok()?;
    let starts = |magic: &[u8]| head.starts_with(magic);
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") || starts(b"PK\x07\x08") {
        return Some(Format::Zip);
    }
    if starts(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
        return Some(Format::SevenZ);
    }
    let packing = if starts(&[0x1f, 0x8b]) {
        Packing::Gzip
    } else if starts(b"BZh") {
        Packing::Bzip2
    } else if starts(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Packing::Xz
    } else if starts(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Packing::Zstd
    } else {
        return tar_header(&head).then_some(Format::Tar(Packing::None));
    };
    // Compressed: a tar inside, or one file.
    let mut start = Vec::new();
    let reader = unpacked(packing, File::open(path).ok()?).ok()?;
    let _ = reader.take(1024).read_to_end(&mut start);
    Some(if tar_header(&start) {
        Format::Tar(packing)
    } else {
        Format::Single(packing)
    })
}

/// Whether bytes start with a tar header (POSIX or GNU).
fn tar_header(head: &[u8]) -> bool {
    head.get(257..262) == Some(b"ustar")
}

/// The archive's bytes, decompressed as they're read.
fn unpacked(packing: Packing, file: File) -> io::Result<Box<dyn Read>> {
    let reader = BufReader::new(file);
    Ok(match packing {
        Packing::None => Box::new(reader),
        Packing::Gzip => Box::new(flate2::read::MultiGzDecoder::new(reader)),
        Packing::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
        Packing::Xz => Box::new(lzma_rust2::XzReader::new(reader, true)),
        Packing::Zstd => Box::new(
            ruzstd::decoding::StreamingDecoder::new(reader)
                .map_err(|e| io::Error::other(e.to_string()))?,
        ),
    })
}

/// A file or folder in the archive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    /// Its path in the archive, `/` between folders, none at the end.
    pub path: String,
    pub dir: bool,
    /// Its size (a folder's: all its files').
    pub size: u64,
    /// Its compressed size, where the archive keeps one.
    pub packed: Option<u64>,
    /// When it was last changed: 2024-03-15 14:30.
    pub modified: Option<String>,
    /// Where a symbolic link points.
    pub link: Option<String>,
    /// How many files a folder holds, all the way down.
    pub files: usize,
    pub depth: usize,
    /// Its name as the archive has it, and its place there, to read it again.
    raw: String,
    index: usize,
    /// Added for a folder the archive only implies (`a/b.txt` without `a/`).
    implied: bool,
}

impl Entry {
    /// The last part of its path.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn parent(&self) -> Option<&str> {
        self.path.rsplit_once('/').map(|(parent, _)| parent)
    }
}

/// An archive in a pane.
pub struct ArchiveView {
    pub path: PathBuf,
    pub format: Option<Format>,
    /// Opened by browsing the explorer: the next file browsed takes its place.
    pub peeked: bool,
    /// Every file and folder, folders first then files, as a tree in order.
    pub entries: Vec<Entry>,
    pub expanded: HashSet<String>,
    /// What's listed (folders closed hide what's in them): indexes into `entries`.
    pub rows: Vec<usize>,
    pub selected: usize,
    pub top: usize,
    pub page: usize,
    pub error: Option<String>,
    /// `/` and what's typed so far, and the last search.
    pub typing: Option<String>,
    pub query: String,
    /// Where the list was drawn: its first row's y, for clicks.
    pub drawn_y: u16,
    /// The file being copied out to open.
    pub opening: Option<String>,
    loading: Option<Receiver<Result<Vec<Entry>, String>>>,
    copying: Option<Receiver<Result<PathBuf, String>>>,
    stamp: Option<(SystemTime, u64)>,
    /// The entry to select once the list is read (a session's, or the one before
    /// reading it again).
    wanted: Option<String>,
}

impl ArchiveView {
    pub fn open(path: PathBuf, peeked: bool, wanted: Option<String>) -> Self {
        let mut view = Self {
            format: format(&path),
            path,
            peeked,
            entries: Vec::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            top: 0,
            page: 20,
            error: None,
            typing: None,
            query: String::new(),
            drawn_y: 0,
            opening: None,
            loading: None,
            copying: None,
            stamp: None,
            wanted,
        };
        view.load();
        view
    }

    fn load(&mut self) {
        self.error = None;
        self.stamp = stamp(&self.path);
        let Some(format) = self.format else {
            self.error = Some("Raido can't read this kind of archive".into());
            return;
        };
        let (send, receive) = mpsc::channel();
        let path = self.path.clone();
        thread::spawn(move || {
            let listed = list(&path, format).map(arrange);
            let _ = send.send(listed.map_err(|e| describe(&e.to_string())));
        });
        self.loading = Some(receive);
    }

    /// Read the archive again (it changed on disk), keeping your place.
    pub fn reload(&mut self) {
        self.format = format(&self.path);
        if let Some(entry) = self.entry() {
            self.wanted = Some(entry.path.clone());
        }
        self.load();
    }

    pub fn changed_on_disk(&self) -> bool {
        let now = stamp(&self.path);
        now.is_some() && now != self.stamp && self.loading.is_none()
    }

    pub fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Take the list once it's read; true when there's something new to show.
    pub fn poll(&mut self) -> bool {
        let Some(loading) = &self.loading else {
            return false;
        };
        let listed = match loading.try_recv() {
            Ok(listed) => listed,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => Err("The archive couldn't be read".into()),
        };
        self.loading = None;
        match listed {
            Ok(entries) => {
                let first = self.entries.is_empty() && self.expanded.is_empty();
                self.entries = entries;
                if first {
                    // A small archive opens whole; a large one shows its top
                    // level, and inside its only folder when it has just one.
                    if self.entries.len() <= 300 {
                        self.expand_all(true);
                    } else {
                        let tops: Vec<&Entry> =
                            self.entries.iter().filter(|e| e.depth == 0).collect();
                        if let [only] = tops.as_slice() {
                            if only.dir {
                                self.expanded.insert(only.path.clone());
                            }
                        }
                    }
                }
                self.refresh();
                match self.wanted.take() {
                    Some(path) => self.select_path(&path),
                    // The one file of a .gz is ready to open.
                    None => {
                        let file = self.rows.iter().position(|&i| !self.entries[i].dir);
                        if self.entries.len() == 1 {
                            self.selected = file.unwrap_or(0);
                        }
                    }
                }
            }
            Err(error) => self.error = Some(error),
        }
        true
    }

    /// The copy being made to open, once it's ready: its path, or why it failed.
    pub fn poll_copy(&mut self) -> Option<Result<PathBuf, String>> {
        let copying = self.copying.as_ref()?;
        let done = match copying.try_recv() {
            Ok(done) => done,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err("The copy stopped".into()),
        };
        self.copying = None;
        self.opening = None;
        Some(done)
    }

    /// List the rows again from the folders open.
    pub fn refresh(&mut self) {
        let chosen = self.entry().map(|e| e.path.clone());
        self.rows.clear();
        let mut hidden_below: Option<usize> = None;
        for (i, entry) in self.entries.iter().enumerate() {
            if let Some(depth) = hidden_below {
                if entry.depth > depth {
                    continue;
                }
                hidden_below = None;
            }
            self.rows.push(i);
            if entry.dir && !self.expanded.contains(&entry.path) {
                hidden_below = Some(entry.depth);
            }
        }
        if let Some(path) = chosen {
            self.select_path(&path);
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    /// The selected file or folder.
    pub fn entry(&self) -> Option<&Entry> {
        self.entries.get(*self.rows.get(self.selected)?)
    }

    /// Select an entry by its path, opening the folders above it.
    fn select_path(&mut self, path: &str) {
        let Some(at) = self.entries.iter().position(|e| e.path == path) else {
            return;
        };
        let mut parent = self.entries[at].parent().map(str::to_string);
        let mut opened = false;
        while let Some(folder) = parent {
            opened |= self.expanded.insert(folder.clone());
            parent = folder.rsplit_once('/').map(|(p, _)| p.to_string());
        }
        if opened {
            self.refresh();
        }
        if let Some(row) = self.rows.iter().position(|&i| i == at) {
            self.selected = row;
        }
    }

    pub fn move_by(&mut self, by: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(by).min(last);
    }

    /// The wheel: the list moves and the selection stays on screen.
    pub fn scroll(&mut self, by: isize) {
        let last = self.rows.len().saturating_sub(1);
        self.top = self.top.saturating_add_signed(by).min(last);
        let page = self.page.max(1);
        self.selected = self
            .selected
            .clamp(self.top, (self.top + page - 1).min(last));
    }

    /// Open or close the selected folder (`open`: `Some` to say which).
    pub fn fold(&mut self, open: Option<bool>) -> bool {
        let Some(entry) = self.entry().filter(|e| e.dir) else {
            return false;
        };
        let path = entry.path.clone();
        let open = open.unwrap_or(!self.expanded.contains(&path));
        if open {
            self.expanded.insert(path);
        } else {
            self.expanded.remove(&path);
        }
        self.refresh();
        true
    }

    /// ←: close the selected folder, else go to the folder it's in.
    pub fn back(&mut self) {
        if self
            .entry()
            .is_some_and(|e| e.dir && self.expanded.contains(&e.path))
        {
            self.fold(Some(false));
        } else if let Some(parent) = self.entry().and_then(Entry::parent).map(str::to_string) {
            self.select_path(&parent);
        }
    }

    /// Every folder open (or closed).
    pub fn expand_all(&mut self, open: bool) {
        self.expanded = if open {
            self.entries
                .iter()
                .filter(|e| e.dir)
                .map(|e| e.path.clone())
                .collect()
        } else {
            HashSet::new()
        };
        self.refresh();
    }

    /// Select the next entry (or, backwards, the one before) whose path has the
    /// query in it, round the end, opening the folders above it. Case counts
    /// only when the query has a capital.
    pub fn find(&mut self, forwards: bool) -> bool {
        let query = self.query.trim();
        if query.is_empty() || self.entries.is_empty() {
            return false;
        }
        let exact = query.chars().any(char::is_uppercase);
        let query = if exact {
            query.to_string()
        } else {
            query.to_lowercase()
        };
        let here = self.rows.get(self.selected).copied().unwrap_or(0);
        let count = self.entries.len();
        let found = (1..=count)
            .map(|step| {
                if forwards {
                    (here + step) % count
                } else {
                    (here + count * 2 - step) % count
                }
            })
            .find(|&i| {
                let path = &self.entries[i].path;
                if exact {
                    path.contains(&query)
                } else {
                    path.to_lowercase().contains(&query)
                }
            });
        match found {
            Some(i) => {
                let path = self.entries[i].path.clone();
                self.select_path(&path);
                true
            }
            None => false,
        }
    }

    /// A click on a row selects it; on the one selected, opens or folds it. True
    /// when the selected file should open.
    pub fn click(&mut self, y: u16) -> bool {
        if y < self.drawn_y {
            return false;
        }
        let row = self.top + usize::from(y - self.drawn_y);
        if row >= self.rows.len() {
            return false;
        }
        if row != self.selected {
            self.selected = row;
            return false;
        }
        !self.fold(None)
    }

    /// Copy the selected file out, read-only, in the background, to open it.
    pub fn copy_out(&mut self) -> Result<(), String> {
        let format = self.format.ok_or("Raido can't read this kind of archive")?;
        let entry = self.entry().ok_or("Nothing is selected")?.clone();
        if entry.dir {
            return Err("A folder: Enter opens it in the list".into());
        }
        if let Some(target) = &entry.link {
            return Err(format!("{} is a link to {target}", entry.name()));
        }
        if entry.size > MAX_OPEN {
            return Err(format!(
                "{} is {}: Raido opens files up to {} from archives",
                entry.name(),
                crate::hex::size(entry.size),
                crate::hex::size(MAX_OPEN)
            ));
        }
        let (send, receive) = mpsc::channel();
        let archive = self.path.clone();
        self.opening = Some(entry.path.clone());
        thread::spawn(move || {
            let copied = copy_out(&archive, format, &entry).map_err(|e| describe(&e.to_string()));
            let _ = send.send(copied);
        });
        self.copying = Some(receive);
        Ok(())
    }

    /// Files, folders and sizes in all: 124 files, 18 folders · 3.4 MB (1.1 MB packed).
    pub fn summary(&self) -> String {
        let files = self.entries.iter().filter(|e| !e.dir).count();
        let folders = self.entries.len() - files;
        let size: u64 = self.entries.iter().filter(|e| !e.dir).map(|e| e.size).sum();
        let packed: Option<u64> = self
            .entries
            .iter()
            .filter(|e| !e.dir)
            .map(|e| e.packed)
            .sum();
        let plural = |n: usize, one: &str, many: &str| {
            format!(
                "{} {}",
                crate::sheet::thousands(&n.to_string()),
                if n == 1 { one } else { many }
            )
        };
        let mut out = plural(files, "file", "files");
        if folders > 0 {
            out.push_str(&format!(", {}", plural(folders, "folder", "folders")));
        }
        out.push_str(&format!(" · {}", short_size(size)));
        if let Some(packed) = packed.filter(|&p| p > 0 && files > 0) {
            out.push_str(&format!(" ({} packed)", short_size(packed)));
        }
        out
    }
}

/// 512 B, 4.2 KB, 3.4 MB.
pub fn short_size(len: u64) -> String {
    if len < 1024 {
        return format!("{len} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut value = len as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// An error in a few plain words.
fn describe(error: &str) -> String {
    let lower = error.to_lowercase();
    if lower.contains("password") || lower.contains("encrypt") {
        "it's password-protected".into()
    } else {
        error.to_string()
    }
}

/// A date and time in seconds since 1970 (UTC) as 2024-03-15 14:30.
fn date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        time / 3600,
        time / 60 % 60
    )
}

fn since_epoch(time: SystemTime) -> Option<i64> {
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64)
}

/// A path in an archive as `/`-separated parts, `./` and leading `/` left out.
fn tidy(name: &str) -> String {
    name.replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

/// The archive's entries as it lists them.
fn list(path: &Path, format: Format) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    match format {
        Format::Zip => {
            let mut zip = zip::ZipArchive::new(BufReader::new(File::open(path)?))
                .map_err(io::Error::other)?;
            for index in 0..zip.len() {
                let file = zip.by_index_raw(index).map_err(io::Error::other)?;
                let modified = file.last_modified().map(|t| {
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}",
                        t.year(),
                        t.month(),
                        t.day(),
                        t.hour(),
                        t.minute()
                    )
                });
                let name = match file.name() {
                    Ok(name) => name.into_owned(),
                    Err(_) => String::from_utf8_lossy(file.name_raw()).into_owned(),
                };
                entries.push(Entry {
                    path: tidy(&name),
                    dir: file.is_dir(),
                    size: file.size(),
                    packed: Some(file.compressed_size()),
                    modified,
                    link: file.is_symlink().then(String::new),
                    raw: name,
                    index,
                    ..Entry::default()
                });
            }
        }
        Format::Tar(packing) => {
            let mut archive = tar::Archive::new(unpacked(packing, File::open(path)?)?);
            for (index, item) in archive.entries()?.enumerate() {
                let item = item?;
                let header = item.header();
                let kind = header.entry_type();
                let raw = item.path()?.to_string_lossy().into_owned();
                let link = (kind.is_symlink() || kind.is_hard_link())
                    .then(|| item.link_name().ok().flatten())
                    .flatten()
                    .map(|l| l.to_string_lossy().into_owned());
                if !(kind.is_file() || kind.is_dir() || link.is_some() || kind.is_gnu_sparse()) {
                    continue;
                }
                entries.push(Entry {
                    path: tidy(&raw),
                    dir: kind.is_dir(),
                    size: header.size().unwrap_or(0),
                    packed: None,
                    modified: header.mtime().ok().map(|t| date(t as i64)),
                    link,
                    raw,
                    index,
                    ..Entry::default()
                });
            }
        }
        Format::Single(packing) => {
            // Its name: as gzip kept it, else the archive's without .gz.
            let file = File::open(path)?;
            let mut kept = None;
            let mut reader: Box<dyn Read> = if packing == Packing::Gzip {
                let decoder = flate2::read::MultiGzDecoder::new(BufReader::new(file));
                kept = decoder
                    .header()
                    .and_then(|h| h.filename())
                    .map(|n| String::from_utf8_lossy(n).into_owned());
                Box::new(decoder)
            } else {
                unpacked(packing, file)?
            };
            let size = io::copy(&mut reader, &mut io::sink())?;
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into());
            let name = kept
                .map(|n| tidy(&n))
                .filter(|n| !n.is_empty() && !n.contains('/'))
                .unwrap_or(stem);
            let modified = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(since_epoch)
                .map(date);
            entries.push(Entry {
                path: name.clone(),
                size,
                packed: std::fs::metadata(path).ok().map(|m| m.len()),
                modified,
                raw: name,
                ..Entry::default()
            });
        }
        Format::SevenZ => {
            let reader = sevenz_rust2::ArchiveReader::new(
                File::open(path)?,
                sevenz_rust2::Password::empty(),
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
            for (index, file) in reader.archive().files.iter().enumerate() {
                if file.is_anti_item {
                    continue;
                }
                let modified = file
                    .has_last_modified_date
                    .then(|| SystemTime::from(file.last_modified_date))
                    .and_then(since_epoch)
                    .map(date);
                entries.push(Entry {
                    path: tidy(&file.name),
                    dir: file.is_directory,
                    size: file.size,
                    packed: (file.compressed_size > 0).then_some(file.compressed_size),
                    modified,
                    raw: file.name.clone(),
                    index,
                    ..Entry::default()
                });
            }
        }
    }
    Ok(entries)
}

/// Entries as a tree in order: the folders an archive only implies added, folders
/// before files at each level (by name, case aside), each folder's files counted
/// and sized.
fn arrange(listed: Vec<Entry>) -> Vec<Entry> {
    let mut by_path: BTreeMap<String, Entry> = BTreeMap::new();
    for entry in listed {
        if entry.path.is_empty() {
            continue;
        }
        // Folders above it.
        let mut parent = entry.path.rsplit_once('/').map(|(p, _)| p.to_string());
        while let Some(folder) = parent {
            parent = folder.rsplit_once('/').map(|(p, _)| p.to_string());
            by_path.entry(folder.clone()).or_insert(Entry {
                path: folder,
                dir: true,
                implied: true,
                ..Entry::default()
            });
        }
        // An entry listed twice: the later one counts, as when it's unpacked.
        by_path.insert(entry.path.clone(), entry);
    }
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in by_path.keys() {
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        children
            .entry(parent.to_string())
            .or_default()
            .push(path.clone());
    }
    let mut ordered = Vec::with_capacity(by_path.len());
    fn walk(
        parent: &str,
        depth: usize,
        by_path: &mut BTreeMap<String, Entry>,
        children: &BTreeMap<String, Vec<String>>,
        ordered: &mut Vec<Entry>,
    ) -> (usize, u64) {
        let Some(kids) = children.get(parent) else {
            return (0, 0);
        };
        let mut kids: Vec<&String> = kids.iter().collect();
        kids.sort_by_key(|path| {
            let dir = by_path.get(*path).is_some_and(|e| e.dir);
            (!dir, path.rsplit('/').next().unwrap_or(path).to_lowercase())
        });
        let (mut files, mut size) = (0, 0);
        for path in kids {
            let Some(mut entry) = by_path.remove(path) else {
                continue;
            };
            entry.depth = depth;
            if entry.dir {
                let at = ordered.len();
                ordered.push(entry);
                let (inside, bytes) = walk(path, depth + 1, by_path, children, ordered);
                ordered[at].files = inside;
                ordered[at].size = bytes;
                files += inside;
                size += bytes;
            } else {
                files += 1;
                size += entry.size;
                ordered.push(entry);
            }
        }
        (files, size)
    }
    walk("", 0, &mut by_path, &children, &mut ordered);
    ordered
}

/// Where an archive's files are copied out to open: a folder per archive (and
/// version of it) in the temporary folder.
fn copies_for(archive: &Path) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    archive.hash(&mut hasher);
    stamp(archive).hash(&mut hasher);
    std::env::temp_dir()
        .join("raido-archives")
        .join(format!("{:016x}", hasher.finish()))
}

/// An entry's path made safe to write under a folder: never `..`, never absolute,
/// and (on Windows) no characters its file names can't have.
fn safe_path(path: &str) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.split('/') {
        if part == ".." {
            return Err(io::Error::other(format!(
                "{path} points outside the archive"
            )));
        }
        let part: String = if cfg!(windows) {
            part.chars()
                .map(|c| {
                    if "<>:\"|?*".contains(c) || c.is_control() {
                        '_'
                    } else {
                        c
                    }
                })
                .collect()
        } else {
            part.to_string()
        };
        out.push(part);
    }
    Ok(out)
}

/// Copy an entry out of the archive, read-only, and say where; a copy made
/// already is used again.
fn copy_out(archive: &Path, format: Format, entry: &Entry) -> io::Result<PathBuf> {
    let folder = copies_for(archive);
    clear_old(folder.parent().unwrap_or(&folder));
    let target = folder.join(safe_path(&entry.path)?);
    if std::fs::metadata(&target).is_ok_and(|m| m.len() == entry.size) {
        return Ok(target);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = target.with_extension(format!("raido-{}.part", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut out = File::create(&temporary)?;
        // Never more than it said it holds (or the limit): a "zip bomb" stops.
        let limit = entry.size.min(MAX_OPEN) + 1;
        let written = match format {
            Format::Zip => {
                let mut zip = zip::ZipArchive::new(BufReader::new(File::open(archive)?))
                    .map_err(io::Error::other)?;
                let mut file = zip
                    .by_index(entry.index)
                    .map_err(io::Error::other)?
                    .take(limit);
                io::copy(&mut file, &mut out)?
            }
            Format::Tar(packing) => {
                let mut tar = tar::Archive::new(unpacked(packing, File::open(archive)?)?);
                let mut written = None;
                for (index, item) in tar.entries()?.enumerate() {
                    let item = item?;
                    if index == entry.index {
                        written = Some(io::copy(&mut item.take(limit), &mut out)?);
                        break;
                    }
                }
                written.ok_or_else(|| io::Error::other("It's no longer in the archive"))?
            }
            Format::Single(packing) => io::copy(
                &mut unpacked(packing, File::open(archive)?)?.take(limit),
                &mut out,
            )?,
            Format::SevenZ => {
                let mut reader = sevenz_rust2::ArchiveReader::new(
                    File::open(archive)?,
                    sevenz_rust2::Password::empty(),
                )
                .map_err(|e| io::Error::other(e.to_string()))?;
                let data = reader
                    .read_file(&entry.raw)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let data = &data[..data.len().min(limit as usize)];
                io::Write::write_all(&mut out, data)?;
                data.len() as u64
            }
        };
        if written > entry.size.min(MAX_OPEN) {
            return Err(io::Error::other(
                "It holds more than the archive says: not opened",
            ));
        }
        out.sync_all()?;
        let mut permissions = out.metadata()?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&temporary, permissions)?;
        std::fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(target)
}

/// Clear away copies from archives opened over a week ago.
fn clear_old(all: &Path) {
    let Ok(folders) = std::fs::read_dir(all) else {
        return;
    };
    for folder in folders.flatten() {
        let old = folder
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().unwrap_or_default() > KEEP);
        if old {
            writable(&folder.path());
            let _ = std::fs::remove_dir_all(folder.path());
        }
    }
}

/// Make the copies under a folder writable again, so Windows lets them go.
fn writable(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                writable(&path);
            } else if let Ok(meta) = entry.metadata() {
                let mut permissions = meta.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                let _ = std::fs::set_permissions(&path, permissions);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A zip of `files` (path, text); a path ending in `/` is a folder.
    pub(crate) fn zip_of(path: &Path, files: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, text) in files {
            if name.ends_with('/') {
                zip.add_directory(*name, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(text.as_bytes()).unwrap();
            }
        }
        zip.finish().unwrap();
    }

    fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, text) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(text.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(1_710_513_000);
            header.set_cksum();
            builder
                .append_data(&mut header, name, text.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn read(view: &mut ArchiveView) {
        for _ in 0..500 {
            if view.poll() || !view.loading() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the archive was never read");
    }

    fn copied(view: &mut ArchiveView) -> Result<PathBuf, String> {
        for _ in 0..500 {
            if let Some(done) = view.poll_copy() {
                return done;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the copy never came");
    }

    #[test]
    fn archives_are_known_by_their_first_bytes_and_listed_as_a_tree() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("release.bin");
        zip_of(
            &zip,
            &[
                ("app-1.0/README.md", "# App\n"),
                ("app-1.0/src/main.rs", "fn main() {}\n"),
                ("app-1.0/src/lib/", ""),
                ("app-1.0/Cargo.toml", "[package]\n"),
                ("./app-1.0/assets/logo.svg", "<svg/>"),
            ],
        );
        assert_eq!(format(&zip), Some(Format::Zip));
        let mut view = ArchiveView::open(zip, false, None);
        read(&mut view);
        assert_eq!(view.error, None);
        let tree: Vec<(usize, &str, bool)> = view
            .entries
            .iter()
            .map(|e| (e.depth, e.name(), e.dir))
            .collect();
        assert_eq!(
            tree,
            [
                (0, "app-1.0", true),
                (1, "assets", true),
                (2, "logo.svg", false),
                (1, "src", true),
                (2, "lib", true),
                (2, "main.rs", false),
                (1, "Cargo.toml", false),
                (1, "README.md", false),
            ]
        );
        assert_eq!(view.entries[0].files, 4);
        assert_eq!(view.entries[0].size, 6 + 13 + 10 + 6);
        assert!(view.summary().starts_with("4 files, 4 folders · 35 B"));
        // A small archive opens whole; folders close and open.
        assert_eq!(view.rows.len(), 8);
        view.move_by(3);
        assert_eq!(view.entry().unwrap().name(), "src");
        view.fold(Some(false));
        assert_eq!(view.rows.len(), 6);
        view.back();
        assert_eq!(view.entry().unwrap().name(), "app-1.0");
        view.expand_all(false);
        assert_eq!(view.rows.len(), 1);
        // Finding opens the folders above what it finds.
        view.query = "main".into();
        assert!(view.find(true));
        assert_eq!(view.entry().unwrap().path, "app-1.0/src/main.rs");
        // Opening it copies it out, read-only.
        view.copy_out().unwrap();
        let copy = copied(&mut view).unwrap();
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "fn main() {}\n");
        assert!(std::fs::metadata(&copy).unwrap().permissions().readonly());
        assert!(copy.ends_with("app-1.0/src/main.rs"));
    }

    #[test]
    fn compressed_tars_and_single_files_open_with_no_tools_installed() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("notes/todo.txt", "buy milk\n"),
            ("notes/done.txt", "nothing\n"),
        ];
        let tar = tar_of(&files);
        let packings: [(&str, Packing, Vec<u8>); 4] = [
            ("a.tar.gz", Packing::Gzip, {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), Default::default());
                e.write_all(&tar).unwrap();
                e.finish().unwrap()
            }),
            ("a.tar.bz2", Packing::Bzip2, {
                let mut e = bzip2::write::BzEncoder::new(Vec::new(), Default::default());
                e.write_all(&tar).unwrap();
                e.finish().unwrap()
            }),
            ("a.tar", Packing::None, tar.clone()),
            ("a.tgz", Packing::Gzip, {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), Default::default());
                e.write_all(&tar).unwrap();
                e.finish().unwrap()
            }),
        ];
        for (name, packing, bytes) in packings {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(format(&path), Some(Format::Tar(packing)), "{name}");
            let mut view = ArchiveView::open(path, false, None);
            read(&mut view);
            let names: Vec<&str> = view.entries.iter().map(|e| e.path.as_str()).collect();
            assert_eq!(
                names,
                ["notes", "notes/done.txt", "notes/todo.txt"],
                "{name}"
            );
            assert_eq!(
                view.entries[2].modified.as_deref(),
                Some("2024-03-15 14:30")
            );
            view.query = "todo".into();
            assert!(view.find(true));
            view.copy_out().unwrap();
            let copy = copied(&mut view).unwrap();
            assert_eq!(
                std::fs::read_to_string(copy).unwrap(),
                "buy milk\n",
                "{name}"
            );
        }
        // One gzipped file: its name as gzip kept it, ready to open.
        let log = dir.path().join("server.log.gz");
        let mut e = flate2::GzBuilder::new()
            .filename("server.log")
            .write(Vec::new(), Default::default());
        e.write_all(b"GET / 200\n").unwrap();
        std::fs::write(&log, e.finish().unwrap()).unwrap();
        assert_eq!(format(&log), Some(Format::Single(Packing::Gzip)));
        let mut view = ArchiveView::open(log, false, None);
        read(&mut view);
        assert_eq!(view.entry().unwrap().name(), "server.log");
        assert_eq!(view.entry().unwrap().size, 10);
        view.copy_out().unwrap();
        assert_eq!(
            std::fs::read_to_string(copied(&mut view).unwrap()).unwrap(),
            "GET / 200\n"
        );
        // Not an archive.
        std::fs::write(dir.path().join("plain.txt"), "hello").unwrap();
        assert_eq!(format(&dir.path().join("plain.txt")), None);
    }

    #[test]
    fn xz_zstandard_and_7z_open_too() {
        let dir = tempfile::tempdir().unwrap();
        let tar = tar_of(&[("docs/guide.md", "# Guide\n")]);
        let mut xz =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        xz.write_all(&tar).unwrap();
        let zstd = ruzstd::encoding::compress_to_vec(
            tar.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        for (name, packing, bytes) in [
            ("a.tar.xz", Packing::Xz, xz.finish().unwrap()),
            ("a.tar.zst", Packing::Zstd, zstd),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(format(&path), Some(Format::Tar(packing)), "{name}");
            let mut view = ArchiveView::open(path, false, None);
            read(&mut view);
            assert_eq!(view.error, None, "{name}");
            view.move_by(1);
            assert_eq!(view.entry().unwrap().path, "docs/guide.md");
            view.copy_out().unwrap();
            assert_eq!(
                std::fs::read_to_string(copied(&mut view).unwrap()).unwrap(),
                "# Guide\n"
            );
        }
        // 7z, made from a folder.
        let source = dir.path().join("site");
        std::fs::create_dir_all(source.join("css")).unwrap();
        std::fs::write(source.join("index.html"), "<h1>hi</h1>").unwrap();
        std::fs::write(source.join("css/main.css"), "h1 {}").unwrap();
        let seven = dir.path().join("site.7z");
        sevenz_rust2::compress_to_path(&source, &seven).unwrap();
        assert_eq!(format(&seven), Some(Format::SevenZ));
        let mut view = ArchiveView::open(seven, false, None);
        read(&mut view);
        assert_eq!(view.error, None);
        view.query = "main.css".into();
        assert!(view.find(true));
        view.copy_out().unwrap();
        assert_eq!(
            std::fs::read_to_string(copied(&mut view).unwrap()).unwrap(),
            "h1 {}"
        );
    }

    #[test]
    fn nothing_is_written_outside_the_folder_for_copies() {
        assert!(safe_path("../../etc/passwd").is_err());
        assert!(safe_path("a/../../b").is_err());
        assert_eq!(
            safe_path("a/b.txt").unwrap(),
            PathBuf::from("a").join("b.txt")
        );
        assert_eq!(tidy("/abs//./x\\y.txt"), "abs/x/y.txt");
        assert_eq!(date(1_710_513_000), "2024-03-15 14:30");
        assert_eq!(date(0), "1970-01-01 00:00");
        assert_eq!(short_size(512), "512 B");
        assert_eq!(short_size(3 * 1024 * 1024 + 400 * 1024), "3.4 MB");
    }
}

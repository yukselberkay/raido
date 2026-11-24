//! Files that aren't text, read-only, as a hex dump: each row's offset, its bytes
//! in hex and as characters, and above them what kind of file it is ("PNG image,
//! 1200 × 800", "ELF 64-bit executable, x86-64"). Only the part on screen is
//! read, so a file of any size opens at once.
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

/// Bytes read at a time around what's on screen.
const WINDOW: usize = 64 * 1024;
/// Bytes looked at to tell text (in any encoding) from binary: all of most files.
const SNIFF: usize = 1 << 20;

/// Whether `path` is a file the editor can't take as text in any encoding: zero
/// bytes (outside UTF-16), many control characters.
pub fn is_binary(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = Vec::new();
    if file
        .by_ref()
        .take(SNIFF as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return false;
    }
    !crate::encoding::is_text(&head)
}

/// A file in a pane, as hex.
pub struct HexView {
    pub path: PathBuf,
    /// Opened by browsing the explorer: the next file browsed takes its place.
    pub peeked: bool,
    pub len: u64,
    /// The selected byte.
    pub cursor: u64,
    /// The first row on screen.
    pub top: u64,
    /// Bytes per row and rows on screen, set as it's drawn.
    pub per_row: u64,
    pub page: u64,
    /// What kind of file it is.
    pub what: String,
    pub error: Option<String>,
    /// `/` and what's typed so far.
    pub typing: Option<String>,
    /// The last search, for n and N.
    pub query: String,
    /// Where the bytes were drawn, for clicks: the hex and character columns'
    /// x, the first row's y, and the rows.
    pub drawn: (u16, u16, u16, u16),
    window: (u64, Vec<u8>),
    stamp: Option<(SystemTime, u64)>,
}

impl HexView {
    pub fn open(path: PathBuf, peeked: bool, cursor: u64) -> Self {
        let mut view = Self {
            path,
            peeked,
            len: 0,
            cursor,
            top: 0,
            per_row: 16,
            page: 16,
            what: String::new(),
            error: None,
            typing: None,
            query: String::new(),
            drawn: (0, 0, 0, 0),
            window: (0, Vec::new()),
            stamp: None,
        };
        view.reload();
        view
    }

    /// Read the file again: its size, its kind, the bytes on screen.
    pub fn reload(&mut self) {
        self.window = (0, Vec::new());
        self.error = None;
        match std::fs::metadata(&self.path) {
            Ok(meta) => {
                self.len = meta.len();
                self.stamp = meta.modified().ok().map(|t| (t, meta.len()));
            }
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        }
        let head = self.bytes(0, 8000);
        self.what = describe(&head, self.len);
        self.cursor = self.cursor.min(self.len.saturating_sub(1));
    }

    /// Whether the file changed since it was read.
    pub fn changed_on_disk(&self) -> bool {
        let now = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        now.is_some() && now != self.stamp
    }

    /// Up to `len` bytes from `from`, read from disk only when they aren't the
    /// ones read last.
    pub fn bytes(&mut self, from: u64, len: usize) -> Vec<u8> {
        let (start, data) = &self.window;
        let end = from + len as u64;
        let held = from >= *start && end <= *start + data.len() as u64;
        if !held || data.is_empty() {
            let start = from.saturating_sub(WINDOW as u64 / 4) & !0xfff;
            let mut data = Vec::new();
            if let Ok(mut file) = File::open(&self.path) {
                if file.seek(SeekFrom::Start(start)).is_ok() {
                    let _ = file.take((WINDOW + len) as u64).read_to_end(&mut data);
                }
            }
            self.window = (start, data);
        }
        let (start, data) = &self.window;
        let from = (from.saturating_sub(*start) as usize).min(data.len());
        let to = (from + len).min(data.len());
        data[from..to].to_vec()
    }

    /// Move the selected byte, staying in the file.
    pub fn move_by(&mut self, by: i64) {
        let last = self.len.saturating_sub(1) as i64;
        self.cursor = (self.cursor as i64 + by).clamp(0, last.max(0)) as u64;
    }

    pub fn go(&mut self, offset: u64) {
        self.cursor = offset.min(self.len.saturating_sub(1));
    }

    /// The wheel: the view moves and the selection stays on screen.
    pub fn scroll(&mut self, rows: i64) {
        let per_row = self.per_row.max(1);
        let last_row = self.len.saturating_sub(1) / per_row;
        self.top = (self.top as i64 + rows).clamp(0, last_row as i64) as u64;
        let row = self.cursor / per_row;
        let row = row.clamp(self.top, self.top + self.page.max(1) - 1);
        self.cursor = (row * per_row + self.cursor % per_row).min(self.len.saturating_sub(1));
    }

    /// Select the next place (or, backwards, the one before) the query is in,
    /// round the end of the file: text (case ignored unless the query has a
    /// capital), or bytes written in hex ("ff d8 ff", "0xcafe").
    pub fn find(&mut self, forwards: bool) -> bool {
        let Some((needle, fold)) = needle(&self.query) else {
            return false;
        };
        let Ok(mut file) = File::open(&self.path) else {
            return false;
        };
        let same = |a: &[u8]| {
            if fold {
                a.eq_ignore_ascii_case(&needle)
            } else {
                a == needle.as_slice()
            }
        };
        // Through the file a megabyte at a time, the pieces overlapping so a match
        // across two is found.
        const CHUNK: u64 = 1 << 20;
        let overlap = needle.len() as u64 - 1;
        let mut matches_in = |start: u64| -> Vec<u64> {
            let mut data = Vec::new();
            if file.seek(SeekFrom::Start(start)).is_err() {
                return Vec::new();
            }
            let _ = file.by_ref().take(CHUNK + overlap).read_to_end(&mut data);
            data.windows(needle.len())
                .enumerate()
                .filter(|(_, w)| same(w))
                .map(|(i, _)| start + i as u64)
                .collect()
        };
        let chunks = self.len.div_ceil(CHUNK).max(1);
        let here = (self.cursor / CHUNK).min(chunks - 1);
        let at = self.cursor;
        // From the cursor's piece on (or back) round the file, ending where it
        // started: the matches before the cursor come last.
        for step in 0..=chunks {
            let index = if forwards {
                (here + step) % chunks
            } else {
                (here + chunks - step % chunks) % chunks
            };
            let start = index * CHUNK;
            let hits: Vec<u64> = matches_in(start)
                .into_iter()
                .filter(|&h| h < start + CHUNK)
                .collect();
            let pick = match (forwards, step) {
                (true, 0) => hits.into_iter().find(|&h| h > at),
                (true, s) if s == chunks => hits.into_iter().find(|&h| h <= at),
                (true, _) => hits.into_iter().next(),
                (false, 0) => hits.into_iter().rev().find(|&h| h < at),
                (false, s) if s == chunks => hits.into_iter().rev().find(|&h| h >= at),
                (false, _) => hits.into_iter().next_back(),
            };
            if let Some(offset) = pick {
                self.cursor = offset;
                return true;
            }
        }
        false
    }

    /// A click on a byte, in hex or as a character, selects it.
    pub fn click(&mut self, x: u16, y: u16) {
        let (hex_x, chars_x, top_y, rows) = self.drawn;
        if y < top_y || y >= top_y + rows {
            return;
        }
        let row = self.top + u64::from(y - top_y);
        let column = if x >= chars_x {
            u64::from(x - chars_x)
        } else if x >= hex_x {
            // Each byte takes three columns, with one more between the halves.
            let at = u64::from(x - hex_x);
            let half = self.per_row / 2 * 3 + 1;
            if self.per_row >= 16 && at >= half {
                self.per_row / 2 + (at - half) / 3
            } else {
                at / 3
            }
        } else {
            return;
        };
        if column < self.per_row {
            self.go(row * self.per_row + column);
        }
    }

    /// The selected row's bytes in hex, for the clipboard.
    pub fn row_hex(&mut self) -> String {
        let per_row = self.per_row.max(1);
        let start = self.cursor / per_row * per_row;
        self.bytes(start, per_row as usize)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What to look for: bytes written in hex ("ff d8 ff", "0xffd8"), else the text,
/// and whether case doesn't matter.
pub fn needle(query: &str) -> Option<(Vec<u8>, bool)> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let hex = query.strip_prefix("0x").map(str::to_string).or_else(|| {
        let pairs: Vec<&str> = query.split_whitespace().collect();
        (pairs.len() > 1 && pairs.iter().all(|p| p.len() == 2)).then(|| pairs.concat())
    });
    if let Some(hex) = hex.filter(|h| h.len() % 2 == 0 && !h.is_empty()) {
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
            .collect();
        if let Some(bytes) = bytes {
            return Some((bytes, false));
        }
    }
    let fold = !query.chars().any(char::is_uppercase);
    Some((query.as_bytes().to_vec(), fold))
}

/// What kind of file the first bytes say it is, and its size.
pub fn describe(head: &[u8], len: u64) -> String {
    let kind = kind(head);
    format!("{kind} · {}", size(len))
}

/// 1,234 bytes; 1.5 MB.
pub fn size(len: u64) -> String {
    let bytes = format!(
        "{} {}",
        crate::sheet::thousands(&len.to_string()),
        if len == 1 { "byte" } else { "bytes" }
    );
    match len {
        0..1024 => bytes,
        _ => {
            let units = ["KB", "MB", "GB", "TB"];
            let mut value = len as f64 / 1024.0;
            let mut unit = 0;
            while value >= 1024.0 && unit < units.len() - 1 {
                value /= 1024.0;
                unit += 1;
            }
            format!("{value:.1} {} ({bytes})", units[unit])
        }
    }
}

fn u16_be(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u16_le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32_be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u32_le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The kind of file its first bytes (magic numbers) say it is.
pub(crate) fn kind(b: &[u8]) -> String {
    let starts = |magic: &[u8]| b.starts_with(magic);
    let at = |offset: usize, magic: &[u8]| b.get(offset..offset + magic.len()) == Some(magic);
    if b.is_empty() {
        return "Empty file".into();
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return match (u32_be(b, 16), u32_be(b, 20)) {
            (Some(w), Some(h)) => format!("PNG image, {w} × {h}"),
            _ => "PNG image".into(),
        };
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return match (u16_le(b, 6), u16_le(b, 8)) {
            (Some(w), Some(h)) => format!("GIF image, {w} × {h}"),
            _ => "GIF image".into(),
        };
    }
    if starts(&[0xff, 0xd8, 0xff]) {
        return match jpeg_size(b) {
            Some((w, h)) => format!("JPEG image, {w} × {h}"),
            None => "JPEG image".into(),
        };
    }
    if starts(b"RIFF") {
        return match b.get(8..12) {
            Some(b"WEBP") => "WebP image".into(),
            Some(b"WAVE") => "WAV audio".into(),
            Some(b"AVI ") => "AVI video".into(),
            _ => "RIFF data".into(),
        };
    }
    if starts(b"BM") && b.len() > 26 {
        if let (Some(w), Some(h)) = (u32_le(b, 18), u32_le(b, 22)) {
            return format!("BMP image, {w} × {}", (h as i32).unsigned_abs());
        }
    }
    if starts(&[0, 0, 1, 0]) {
        return "Windows icon".into();
    }
    if starts(b"%PDF-") {
        let version: String = b[5..]
            .iter()
            .take_while(|c| c.is_ascii_digit() || **c == b'.')
            .map(|&c| c as char)
            .collect();
        return format!("PDF document, version {version}");
    }
    if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        return "Zip archive".into();
    }
    if starts(&[0x1f, 0x8b]) {
        return "gzip compressed data".into();
    }
    if starts(b"BZh") {
        return "bzip2 compressed data".into();
    }
    if starts(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        return "xz compressed data".into();
    }
    if starts(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return "Zstandard compressed data".into();
    }
    if starts(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
        return "7-Zip archive".into();
    }
    if starts(b"Rar!\x1a\x07") {
        return "RAR archive".into();
    }
    if at(257, b"ustar") {
        return "tar archive".into();
    }
    if starts(b"SQLite format 3\0") {
        return "SQLite database".into();
    }
    if starts(b"\0asm") {
        return "WebAssembly module".into();
    }
    if starts(b"\x7fELF") {
        return elf(b);
    }
    if let Some(macho) = mach_o(b) {
        return macho;
    }
    if starts(b"MZ") {
        return pe(b);
    }
    if starts(b"ID3") || starts(&[0xff, 0xfb]) || starts(&[0xff, 0xf3]) {
        return "MP3 audio".into();
    }
    if starts(b"fLaC") {
        return "FLAC audio".into();
    }
    if starts(b"OggS") {
        return "Ogg audio or video".into();
    }
    if at(4, b"ftyp") {
        return match b.get(8..12) {
            Some(b"qt  ") => "QuickTime video".into(),
            Some(b"heic" | b"heix" | b"mif1") => "HEIC image".into(),
            Some(b"avif") => "AVIF image".into(),
            Some(b"M4A ") => "M4A audio".into(),
            _ => "MP4 video".into(),
        };
    }
    if starts(&[0, 1, 0, 0]) || starts(b"true") {
        return "TrueType font".into();
    }
    if starts(b"OTTO") {
        return "OpenType font".into();
    }
    if starts(b"wOFF") || starts(b"wOF2") {
        return "Web font (WOFF)".into();
    }
    if starts(&[0xff, 0xfe]) {
        return "UTF-16 text, little-endian".into();
    }
    if starts(&[0xfe, 0xff]) {
        return "UTF-16 text, big-endian".into();
    }
    let printable = b
        .iter()
        .filter(|&&c| c == b'\n' || c == b'\r' || c == b'\t' || c >= 0x20)
        .count();
    if !b.contains(&0) && printable * 10 >= b.len() * 9 {
        "Text, not UTF-8 (Latin-1 or another encoding)".into()
    } else {
        "Binary data".into()
    }
}

/// A JPEG's width and height, from its first frame header.
fn jpeg_size(b: &[u8]) -> Option<(u16, u16)> {
    let mut i = 2;
    while i + 9 < b.len() {
        if b[i] != 0xff {
            return None;
        }
        let marker = b[i + 1];
        let length = usize::from(u16_be(b, i + 2)?);
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return Some((u16_be(b, i + 7)?, u16_be(b, i + 5)?));
        }
        i += 2 + length;
    }
    None
}

fn elf(b: &[u8]) -> String {
    let bits = match b.get(4) {
        Some(1) => "32-bit",
        Some(2) => "64-bit",
        _ => "",
    };
    let little = b.get(5) == Some(&1);
    let half = |at| if little { u16_le(b, at) } else { u16_be(b, at) };
    let what = match half(16) {
        Some(1) => "object file",
        Some(2) => "executable",
        Some(3) => "shared library or executable",
        Some(4) => "core dump",
        _ => "file",
    };
    let machine = match half(18) {
        Some(0x3e) => ", x86-64",
        Some(0xb7) => ", ARM64",
        Some(0x03) => ", x86",
        Some(0x28) => ", ARM",
        Some(0xf3) => ", RISC-V",
        _ => "",
    };
    format!("ELF {bits} {what}{machine}")
}

fn mach_o(b: &[u8]) -> Option<String> {
    let magic = u32_be(b, 0)?;
    // CAFEBABE is also Java's: a universal binary lists only a few architectures.
    if magic == 0xcafebabe {
        return Some(match u32_be(b, 4) {
            Some(n) if n < 40 => format!("Mach-O universal binary, {n} architectures"),
            _ => "Java class file".into(),
        });
    }
    let little = match magic {
        0xcffaedfe | 0xcefaedfe => true,
        0xfeedfacf | 0xfeedface => false,
        _ => return None,
    };
    let word = |at| if little { u32_le(b, at) } else { u32_be(b, at) };
    let machine = match word(4) {
        Some(0x0100_0007) => ", x86-64",
        Some(0x0100_000c) => ", ARM64",
        Some(7) => ", x86",
        _ => "",
    };
    let what = match word(12) {
        Some(1) => "object file",
        Some(2) => "executable",
        Some(6) => "dynamic library",
        Some(8) => "bundle",
        _ => "file",
    };
    Some(format!("Mach-O {what}{machine}"))
}

fn pe(b: &[u8]) -> String {
    let Some(header) = u32_le(b, 0x3c).map(|h| h as usize) else {
        return "DOS executable".into();
    };
    if b.get(header..header + 4) != Some(b"PE\0\0") {
        return "DOS or Windows executable".into();
    }
    let machine = match u16_le(b, header + 4) {
        Some(0x8664) => ", x86-64",
        Some(0xaa64) => ", ARM64",
        Some(0x14c) => ", x86",
        _ => "",
    };
    let dll = u16_le(b, header + 22).is_some_and(|c| c & 0x2000 != 0);
    format!(
        "Windows {}{machine}",
        if dll { "DLL" } else { "executable" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_files_are_told_from_text_and_named() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(1200u32.to_be_bytes());
        png.extend(800u32.to_be_bytes());
        assert_eq!(kind(&png), "PNG image, 1200 × 800");
        assert_eq!(kind(b"%PDF-1.7\n"), "PDF document, version 1.7");
        assert_eq!(kind(b"GIF89a\x10\x00\x20\x00"), "GIF image, 16 × 32");
        let mut elf_head = b"\x7fELF\x02\x01\x01".to_vec();
        elf_head.resize(16, 0);
        elf_head.extend(2u16.to_le_bytes());
        elf_head.extend(0x3eu16.to_le_bytes());
        assert_eq!(kind(&elf_head), "ELF 64-bit executable, x86-64");
        let mut macho = 0xfeedfacfu32.to_le_bytes().to_vec();
        macho.extend(0x0100_000cu32.to_le_bytes());
        macho.extend(0u32.to_le_bytes());
        macho.extend(2u32.to_le_bytes());
        assert_eq!(kind(&macho), "Mach-O executable, ARM64");
        assert_eq!(kind(&[0xff, 0xfe, b'h', 0]), "UTF-16 text, little-endian");
        assert_eq!(kind(&[1, 2, 3, 0]), "Binary data");
        assert_eq!(size(512), "512 bytes");
        assert_eq!(size(1536), "1.5 KB (1,536 bytes)");
    }

    #[test]
    fn hex_and_text_are_found_round_the_end_of_the_file() {
        assert_eq!(needle("ff d8 ff"), Some((vec![0xff, 0xd8, 0xff], false)));
        assert_eq!(needle("0xcafe"), Some((vec![0xca, 0xfe], false)));
        assert_eq!(needle("Hello"), Some((b"Hello".to_vec(), false)));
        assert_eq!(needle("hello"), Some((b"hello".to_vec(), true)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let mut data = vec![0u8; 100];
        data[10..15].copy_from_slice(b"HELLO");
        data[70..73].copy_from_slice(&[0xca, 0xfe, 0x01]);
        std::fs::write(&path, &data).unwrap();
        let mut view = HexView::open(path, false, 0);
        assert_eq!(view.len, 100);
        assert_eq!(view.what, "Binary data · 100 bytes");
        view.query = "hello".into();
        assert!(view.find(true));
        assert_eq!(view.cursor, 10);
        view.query = "ca fe".into();
        assert!(view.find(true));
        assert_eq!(view.cursor, 70);
        // Round the end, and back.
        view.query = "hello".into();
        assert!(view.find(true));
        assert_eq!(view.cursor, 10);
        view.query = "ca fe".into();
        assert!(view.find(false));
        assert_eq!(view.cursor, 70);
        view.query = "nothing".into();
        assert!(!view.find(true));
        view.move_by(1000);
        assert_eq!(view.cursor, 99);
        view.per_row = 16;
        view.go(16);
        assert_eq!(view.row_hex().len(), 16 * 3 - 1);
    }
}

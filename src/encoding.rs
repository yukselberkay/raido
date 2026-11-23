//! Text in encodings other than UTF-8, edited as any other text and saved back as
//! it was: UTF-16 (with its byte order mark, or without one, told by where its
//! zero bytes fall), and the older one-byte and Asian encodings (Windows-1252 and
//! Latin-1, Windows-1254 for Turkish, Windows-1251 for Cyrillic, Shift_JIS, GBK
//! and others), guessed from the text as browsers guess them. A UTF-8 byte order
//! mark is kept too. What isn't text in any of these (zero bytes, many control
//! characters) isn't taken as text.
use encoding_rs::Encoding as Legacy;

/// How a file's text is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    /// One of the encodings before Unicode (Windows-1252, Shift_JIS…).
    Legacy(&'static Legacy),
}

/// A file's text, and how it was written.
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
    /// It started with a byte order mark (kept when it's saved).
    pub bom: bool,
}

impl Encoding {
    /// Its name, and the script or region a legacy one is for: "UTF-16 LE",
    /// "windows-1254 (Turkish)".
    pub fn name(self, bom: bool) -> String {
        match self {
            Encoding::Utf8 if bom => "UTF-8 with BOM".into(),
            Encoding::Utf8 => "UTF-8".into(),
            Encoding::Utf16Le => "UTF-16 LE".into(),
            Encoding::Utf16Be => "UTF-16 BE".into(),
            Encoding::Legacy(encoding) => {
                let name = encoding.name();
                match region(name) {
                    Some(region) => format!("{name} ({region})"),
                    None => name.to_string(),
                }
            }
        }
    }
}

/// What a legacy encoding is used for.
fn region(name: &str) -> Option<&'static str> {
    Some(match name {
        "windows-1252" => "Western",
        "windows-1250" | "ISO-8859-2" => "Central European",
        "windows-1251" | "KOI8-R" | "KOI8-U" | "IBM866" | "ISO-8859-5" => "Cyrillic",
        "windows-1253" | "ISO-8859-7" => "Greek",
        "windows-1254" => "Turkish",
        "windows-1255" | "ISO-8859-8" | "ISO-8859-8-I" => "Hebrew",
        "windows-1256" | "ISO-8859-6" => "Arabic",
        "windows-1257" | "ISO-8859-4" | "ISO-8859-13" => "Baltic",
        "windows-1258" => "Vietnamese",
        "windows-874" => "Thai",
        "Shift_JIS" | "EUC-JP" | "ISO-2022-JP" => "Japanese",
        "GBK" | "gb18030" => "Simplified Chinese",
        "Big5" => "Traditional Chinese",
        "EUC-KR" => "Korean",
        _ => return None,
    })
}

/// An encoding by a name you'd type: utf-8, utf-8-bom, utf-16le, utf-16be, or any
/// label browsers know (latin1, windows-1254, cp1251, shift_jis, gbk…). The flag
/// says whether it's written with a byte order mark.
pub fn by_name(name: &str) -> Option<(Encoding, bool)> {
    let label = name.trim().to_ascii_lowercase().replace('_', "-");
    Some(match label.as_str() {
        "utf-8" | "utf8" => (Encoding::Utf8, false),
        "utf-8-bom" | "utf8-bom" | "utf-8bom" => (Encoding::Utf8, true),
        "utf-16" | "utf-16le" | "utf16" | "utf16le" | "ucs-2" => (Encoding::Utf16Le, true),
        "utf-16be" | "utf16be" => (Encoding::Utf16Be, true),
        _ => {
            let encoding = Legacy::for_label_no_replacement(label.as_bytes())?;
            match encoding.name() {
                "UTF-8" => (Encoding::Utf8, false),
                "UTF-16LE" => (Encoding::Utf16Le, true),
                "UTF-16BE" => (Encoding::Utf16Be, true),
                _ => (Encoding::Legacy(encoding), false),
            }
        }
    })
}

/// A file's bytes as text, and how they were written; `None` when they aren't
/// text. A legacy encoding is guessed with the help of your locale's country.
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    decode_for(bytes, locale_country().as_deref())
}

/// [`decode`], guessing for the country `tld` (as in `.tr`, `.jp`).
pub fn decode_for(bytes: &[u8], tld: Option<&str>) -> Option<Decoded> {
    if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        if let Ok(text) = std::str::from_utf8(rest) {
            return Some(Decoded {
                text: text.to_string(),
                encoding: Encoding::Utf8,
                bom: true,
            });
        }
    }
    let utf16 = if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        Some((rest, Encoding::Utf16Le, true))
    } else if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        Some((rest, Encoding::Utf16Be, true))
    } else {
        utf16_without_bom(bytes).map(|encoding| (bytes, encoding, false))
    };
    if let Some((rest, encoding, bom)) = utf16 {
        let text = utf16_text(rest, encoding)?;
        return text_like(&text).then_some(Decoded {
            text,
            encoding,
            bom,
        });
    }
    if bytes.contains(&0) {
        return None;
    }
    // UTF-8 is text, whatever control characters it has (as it always was here).
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(Decoded {
            text: text.to_string(),
            encoding: Encoding::Utf8,
            bom: false,
        });
    }
    // An older encoding: the one the text reads most like, else Windows-1252,
    // which takes any bytes.
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let tld = tld.filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_lowercase()));
    let guess = detector.guess(tld.map(str::as_bytes), chardetng::Utf8Detection::Deny);
    let text = guess
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(|text| (text.into_owned(), guess))
        .or_else(|| {
            let western = encoding_rs::WINDOWS_1252;
            western
                .decode_without_bom_handling_and_without_replacement(bytes)
                .map(|text| (text.into_owned(), western))
        });
    let (text, encoding) = text?;
    text_like(&text).then_some(Decoded {
        text,
        encoding: Encoding::Legacy(encoding),
        bom: false,
    })
}

/// Bytes read again in the encoding you name (when the guess was wrong); `None`
/// when they aren't text in it.
pub fn decode_as(bytes: &[u8], encoding: Encoding) -> Option<Decoded> {
    let (rest, bom) = match encoding {
        Encoding::Utf8 => match bytes.strip_prefix(b"\xef\xbb\xbf") {
            Some(rest) => (rest, true),
            None => (bytes, false),
        },
        Encoding::Utf16Le => match bytes.strip_prefix(b"\xff\xfe") {
            Some(rest) => (rest, true),
            None => (bytes, false),
        },
        Encoding::Utf16Be => match bytes.strip_prefix(b"\xfe\xff") {
            Some(rest) => (rest, true),
            None => (bytes, false),
        },
        Encoding::Legacy(_) => (bytes, false),
    };
    let text = match encoding {
        Encoding::Utf8 => std::str::from_utf8(rest).ok()?.to_string(),
        Encoding::Utf16Le | Encoding::Utf16Be => utf16_text(rest, encoding)?,
        Encoding::Legacy(legacy) => legacy
            .decode_without_bom_handling_and_without_replacement(rest)?
            .into_owned(),
    };
    Some(Decoded {
        text,
        encoding,
        bom,
    })
}

/// Text written as `encoding` (with a byte order mark when `bom`), or the first
/// character it can't write.
pub fn encode(text: &str, encoding: Encoding, bom: bool) -> Result<Vec<u8>, char> {
    let mut out = Vec::with_capacity(text.len() + 3);
    match encoding {
        Encoding::Utf8 => {
            if bom {
                out.extend_from_slice(b"\xef\xbb\xbf");
            }
            out.extend_from_slice(text.as_bytes());
        }
        Encoding::Utf16Le => {
            if bom {
                out.extend_from_slice(b"\xff\xfe");
            }
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
        Encoding::Utf16Be => {
            if bom {
                out.extend_from_slice(b"\xfe\xff");
            }
            for unit in text.encode_utf16() {
                out.extend_from_slice(&unit.to_be_bytes());
            }
        }
        Encoding::Legacy(legacy) => {
            let (bytes, _, unmappable) = legacy.encode(text);
            if unmappable {
                // The first character it has no bytes for.
                let mut one = [0u8; 4];
                let missing = text
                    .chars()
                    .find(|c| legacy.encode(c.encode_utf8(&mut one)).2)
                    .unwrap_or('?');
                return Err(missing);
            }
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

/// Whether the start of a file is text in some encoding: what Raido can edit. A
/// character cut off at the end of what was read doesn't count.
pub fn is_text(head: &[u8]) -> bool {
    if !head.contains(&0) {
        match std::str::from_utf8(head) {
            Ok(_) => return true,
            Err(error) if error.error_len().is_none() => return true,
            Err(_) => {}
        }
    }
    decode(head).is_some() || (head.len() > 1 && decode(&head[..head.len() - 1]).is_some())
}

/// UTF-16 without a byte order mark: in text that's mostly ASCII, every other byte
/// is zero, the second of each pair in little-endian, the first in big-endian.
fn utf16_without_bom(bytes: &[u8]) -> Option<Encoding> {
    let pairs = bytes.len().min(8000) / 2;
    if pairs < 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let (mut first, mut second) = (0, 0);
    for pair in bytes[..pairs * 2].as_chunks::<2>().0 {
        first += usize::from(pair[0] == 0);
        second += usize::from(pair[1] == 0);
    }
    if second * 10 >= pairs * 4 && first * 50 <= pairs {
        Some(Encoding::Utf16Le)
    } else if first * 10 >= pairs * 4 && second * 50 <= pairs {
        Some(Encoding::Utf16Be)
    } else {
        None
    }
}

/// UTF-16 bytes as text; `None` for an odd length or a broken surrogate pair.
fn utf16_text(bytes: &[u8], encoding: Encoding) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units = bytes.as_chunks::<2>().0.iter().map(|&pair| {
        if encoding == Encoding::Utf16Be {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

/// Whether a control character other than tab, line breaks, form feed and escape
/// is rare enough for this to be text.
fn text_like(text: &str) -> bool {
    let odd = text
        .chars()
        .filter(|&c| {
            c.is_control()
                && !matches!(c, '\t' | '\n' | '\r' | '\x0c' | '\x1b')
                && !('\u{80}'..='\u{9f}').contains(&c)
        })
        .count();
    odd == 0 || odd * 200 < text.chars().count()
}

/// Your locale's country, lowercase (tr in tr_TR.UTF-8): a hint for guessing an
/// older encoding.
fn locale_country() -> Option<String> {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))?;
    let country = locale.split(['.', '@']).next()?.split('_').nth(1)?;
    (country.len() == 2 && country.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| country.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(name: &str) -> Encoding {
        by_name(name).unwrap().0
    }

    #[test]
    fn text_in_any_encoding_is_read_and_written_back_byte_for_byte() {
        // UTF-16 with a byte order mark, as Windows PowerShell writes it.
        let mut utf16 = b"\xff\xfe".to_vec();
        for unit in "Grüße\r\nçok".encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        let decoded = decode_for(&utf16, None).unwrap();
        assert_eq!(decoded.text, "Grüße\r\nçok");
        assert_eq!((decoded.encoding, decoded.bom), (Encoding::Utf16Le, true));
        assert_eq!(
            encode(&decoded.text, decoded.encoding, true).unwrap(),
            utf16
        );
        // Without one, told by its zero bytes; big-endian too.
        let bare: Vec<u8> = "plain text"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        let decoded = decode_for(&bare, None).unwrap();
        assert_eq!(
            (decoded.text.as_str(), decoded.encoding),
            ("plain text", Encoding::Utf16Be)
        );
        // Latin-1 / Windows-1252.
        let western = b"caf\xe9 cr\xe8me br\xfbl\xe9e, na\xefve fa\xe7ade";
        let decoded = decode_for(western, None).unwrap();
        assert_eq!(decoded.text, "café crème brûlée, naïve façade");
        assert_eq!(decoded.encoding, legacy("windows-1252"));
        assert_eq!(
            encode(&decoded.text, decoded.encoding, false).unwrap(),
            western
        );
        // Turkish (Windows-1254), with its own letters.
        let turkish = encode(
            "Türkçe metin: ağaç, şişe, ılık, İstanbul, ğüşıöç ĞÜŞİÖÇ",
            legacy("windows-1254"),
            false,
        )
        .unwrap();
        let decoded = decode_for(&turkish, Some("tr")).unwrap();
        assert_eq!(decoded.encoding, legacy("windows-1254"));
        assert!(decoded.text.contains("İstanbul"));
        // UTF-8 keeps its byte order mark.
        let decoded = decode_for(b"\xef\xbb\xbfid,name\n", None).unwrap();
        assert_eq!((decoded.text.as_str(), decoded.bom), ("id,name\n", true));
        assert_eq!(encode("x", Encoding::Utf8, true).unwrap(), b"\xef\xbb\xbfx");
        // A character the encoding has no bytes for is named.
        assert_eq!(encode("price: 5€ ✓", legacy("iso-8859-2"), false), Err('€'));
    }

    #[test]
    fn binary_data_isnt_text_in_any_encoding() {
        assert!(decode_for(b"abc\0def", None).is_none());
        assert!(decode_for(
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0],
            None
        )
        .is_none());
        assert!(decode_for(&[1, 2, 3, 4, 5, 6, 7, 8, 0x81, 0x92], None).is_none());
        assert!(is_text(b"plain"));
        assert!(is_text(b"caf\xe9"));
        assert!(is_text("é".as_bytes().split_at(1).0));
        assert!(!is_text(b"\x7fELF\x02\x01\x01\0\0\0"));
        assert_eq!(by_name("latin1"), Some((legacy("windows-1252"), false)));
        assert_eq!(by_name("UTF-16"), Some((Encoding::Utf16Le, true)));
        assert_eq!(
            by_name("cp1254").map(|e| e.0.name(false)),
            Some("windows-1254 (Turkish)".into())
        );
        assert_eq!(by_name("nonsense"), None);
        assert_eq!(
            decode_as(b"\xfe\xfd", legacy("windows-1254")).unwrap().text,
            "şı"
        );
    }
}

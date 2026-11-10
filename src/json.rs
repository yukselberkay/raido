//! Pretty-printing JSON for `:format`: the same document laid out one value per
//! line and indented, with nothing else changed. Keys keep their order, and strings
//! and numbers their exact text (`1.10` stays `1.10`, big integers keep every digit),
//! which parsing into values and printing them back would not promise.

/// Nesting deeper than this is refused rather than risking the stack.
const MAX_DEPTH: usize = 512;

/// Where `text` stops being JSON: 1-based line and column, and why.
#[derive(Debug, PartialEq, Eq)]
pub struct Invalid {
    pub line: usize,
    pub column: usize,
    pub reason: &'static str,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "not valid JSON at line {}, column {}: {}",
            self.line, self.column, self.reason
        )
    }
}

/// `text` laid out with `indent` per level and `eol` between lines. A final line
/// ending stays if `text` had one; an empty `{}` or `[]` stays on one line.
pub fn pretty(text: &str, indent: &str, eol: &str) -> Result<String, Invalid> {
    let bom = text.starts_with('\u{feff}');
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut writer = Writer {
        text: body,
        at: 0,
        out: String::with_capacity(body.len() + body.len() / 4),
        indent,
        eol,
    };
    if bom {
        writer.out.push('\u{feff}');
    }
    writer.space();
    if writer.at == body.len() {
        return Err(writer.invalid("the file is empty"));
    }
    writer.value(0)?;
    writer.space();
    if writer.at < body.len() {
        return Err(writer.invalid("more text after the JSON value"));
    }
    if body.ends_with('\n') {
        writer.out.push_str(eol);
    }
    Ok(writer.out)
}

/// Where the char at `index` of `before` is in `after`, which holds the same text
/// with only whitespace outside strings changed: the cursor stays on its character.
pub fn same_place(before: &str, after: &str, index: usize) -> usize {
    let wanted = before
        .chars()
        .take(index)
        .filter(|c| !c.is_whitespace())
        .count();
    let mut seen = 0;
    for (at, c) in after.chars().enumerate() {
        if !c.is_whitespace() {
            if seen == wanted {
                return at;
            }
            seen += 1;
        }
    }
    after.chars().count()
}

struct Writer<'a> {
    text: &'a str,
    /// Byte position in `text`.
    at: usize,
    out: String,
    indent: &'a str,
    eol: &'a str,
}

impl Writer<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn invalid(&self, reason: &'static str) -> Invalid {
        let before = &self.text[..self.at.min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        Invalid {
            line,
            column,
            reason,
        }
    }

    fn line(&mut self, depth: usize) {
        self.out.push_str(self.eol);
        for _ in 0..depth {
            self.out.push_str(self.indent);
        }
    }

    fn value(&mut self, depth: usize) -> Result<(), Invalid> {
        if depth > MAX_DEPTH {
            return Err(self.invalid("nested too deeply"));
        }
        match self.peek() {
            Some(b'{') => self.container(depth, b'}', true),
            Some(b'[') => self.container(depth, b']', false),
            Some(b'"') => self.string(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(b't') => self.word("true"),
            Some(b'f') => self.word("false"),
            Some(b'n') => self.word("null"),
            Some(b'/') => Err(self.invalid("comments aren't allowed in JSON")),
            Some(b'\'') => Err(self.invalid("strings take double quotes in JSON")),
            Some(_) => Err(self.invalid("expected a value")),
            None => Err(self.invalid("the text ends where a value should be")),
        }
    }

    /// An object (`keyed`) or an array, one member per line.
    fn container(&mut self, depth: usize, close: u8, keyed: bool) -> Result<(), Invalid> {
        self.out.push(self.peek().unwrap_or_default() as char);
        self.at += 1;
        self.space();
        if self.peek() == Some(close) {
            self.out.push(close as char);
            self.at += 1;
            return Ok(());
        }
        loop {
            self.line(depth + 1);
            if keyed {
                match self.peek() {
                    Some(b'"') => self.string()?,
                    Some(b'/') => return Err(self.invalid("comments aren't allowed in JSON")),
                    Some(b'}') => {
                        return Err(self.invalid("a comma before } isn't allowed in JSON"))
                    }
                    _ => return Err(self.invalid("expected a key in double quotes")),
                }
                self.space();
                if self.peek() != Some(b':') {
                    return Err(self.invalid("expected : after the key"));
                }
                self.at += 1;
                self.out.push_str(": ");
                self.space();
            } else if self.peek() == Some(b']') {
                return Err(self.invalid("a comma before ] isn't allowed in JSON"));
            }
            self.value(depth + 1)?;
            self.space();
            match self.peek() {
                Some(b',') => {
                    self.at += 1;
                    self.out.push(',');
                    self.space();
                }
                Some(found) if found == close => {
                    self.at += 1;
                    self.line(depth);
                    self.out.push(close as char);
                    return Ok(());
                }
                Some(b'/') => return Err(self.invalid("comments aren't allowed in JSON")),
                _ if keyed => return Err(self.invalid("expected , or } after the value")),
                _ => return Err(self.invalid("expected , or ] after the value")),
            }
        }
    }

    /// A string, copied as written once its escapes are checked.
    fn string(&mut self) -> Result<(), Invalid> {
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek() {
                None => return Err(self.invalid("the string never ends")),
                Some(b'"') => break,
                Some(b'\\') => {
                    // A bad escape is reported where its backslash is.
                    let slash = self.at;
                    self.at += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {}
                        Some(b'u') => {
                            let hex = self.text.as_bytes().get(self.at + 1..self.at + 5);
                            if !hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
                                self.at = slash;
                                return Err(self.invalid("\\u needs four hex digits"));
                            }
                            self.at += 4;
                        }
                        _ => {
                            self.at = slash;
                            return Err(self.invalid("unknown escape in a string"));
                        }
                    }
                }
                Some(byte) if byte < 0x20 => {
                    return Err(self.invalid("a line break or control character in a string"))
                }
                Some(_) => {}
            }
            self.at += 1;
        }
        self.at += 1;
        self.out.push_str(&self.text[start..self.at]);
        Ok(())
    }

    /// A number, copied as written once it's checked: `-`, digits without a leading
    /// zero, an optional fraction and exponent.
    fn number(&mut self) -> Result<(), Invalid> {
        let start = self.at;
        let digits = |w: &mut Self| {
            let from = w.at;
            while matches!(w.peek(), Some(b'0'..=b'9')) {
                w.at += 1;
            }
            w.at > from
        };
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.invalid("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !digits(self) {
                return Err(self.invalid("expected a digit after the decimal point"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return Err(self.invalid("expected a digit in the exponent"));
            }
        }
        if matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.')
        ) {
            return Err(self.invalid("not a JSON number"));
        }
        self.out.push_str(&self.text[start..self.at]);
        Ok(())
    }

    fn word(&mut self, word: &'static str) -> Result<(), Invalid> {
        let end = self.at + word.len();
        let rest = self.text.as_bytes().get(end).copied();
        if self.text.get(self.at..end) != Some(word)
            || rest.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(self.invalid("expected a value"));
        }
        self.out.push_str(word);
        self.at = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_laid_out_with_its_order_and_exact_values_kept() {
        let text = r#"{"zeta":1.10,"alpha":[1,2,{"x":null}],"big":12345678901234567890123,
  "s":"a \"q\" é ☕ \\ /","empty":{},"none":[],"t":true,"e":-1.5E+10}
"#;
        let want = r#"{
  "zeta": 1.10,
  "alpha": [
    1,
    2,
    {
      "x": null
    }
  ],
  "big": 12345678901234567890123,
  "s": "a \"q\" é ☕ \\ /",
  "empty": {},
  "none": [],
  "t": true,
  "e": -1.5E+10
}
"#;
        assert_eq!(pretty(text, "  ", "\n").unwrap(), want);
        // Formatting again changes nothing; tabs and CRLF are kept as asked.
        assert_eq!(pretty(want, "  ", "\n").unwrap(), want);
        assert_eq!(pretty("[1]", "\t", "\r\n").unwrap(), "[\r\n\t1\r\n]");
        assert_eq!(pretty(" 42 ", "  ", "\n").unwrap(), "42");
        assert_eq!(pretty("\u{feff}{}\n", "  ", "\n").unwrap(), "\u{feff}{}\n");
    }

    #[test]
    fn invalid_json_says_where_and_why() {
        let at = |text: &str| {
            let e = pretty(text, "  ", "\n").unwrap_err();
            (e.line, e.column, e.reason)
        };
        assert_eq!(
            at("{\n  \"a\": 1\n  \"b\": 2\n}"),
            (3, 3, "expected , or } after the value")
        );
        assert_eq!(
            at("{\"a\": 1,}"),
            (1, 9, "a comma before } isn't allowed in JSON")
        );
        assert_eq!(
            at("[1, 2,]"),
            (1, 7, "a comma before ] isn't allowed in JSON")
        );
        assert_eq!(
            at("{\n  // note\n}"),
            (2, 3, "comments aren't allowed in JSON")
        );
        assert_eq!(at("{'a': 1}"), (1, 2, "expected a key in double quotes"));
        assert_eq!(at("[01]"), (1, 3, "not a JSON number"));
        assert_eq!(at("[\"a\\x\"]"), (1, 4, "unknown escape in a string"));
        assert_eq!(at("[\"open"), (1, 7, "the string never ends"));
        assert_eq!(at("{} {}"), (1, 4, "more text after the JSON value"));
        assert_eq!(at("  \n"), (2, 1, "the file is empty"));
        assert_eq!(at("[truth]"), (1, 2, "expected a value"));
        assert_eq!(at(&"[".repeat(600)), (1, 514, "nested too deeply"));
        assert_eq!(
            pretty("{,}", "  ", "\n").unwrap_err().to_string(),
            "not valid JSON at line 1, column 2: expected a key in double quotes"
        );
    }

    #[test]
    fn the_cursor_stays_on_its_character() {
        let before = "{\"a\":[1,22]}";
        let after = pretty(before, "  ", "\n").unwrap();
        let on = |index: usize| after.chars().nth(same_place(before, &after, index));
        // On the second 2 of 22, on `]`, and at the very end.
        assert_eq!(on(9), Some('2'));
        assert_eq!(same_place(before, &after, 9), after.find("22").unwrap() + 1);
        assert_eq!(on(10), Some(']'));
        assert_eq!(same_place(before, &after, 12), after.chars().count());
    }
}

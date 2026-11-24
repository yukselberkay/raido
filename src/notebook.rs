//! Jupyter notebooks (`.ipynb`) open rendered, the way Jupyter and GitHub show
//! them: Markdown cells as text, code cells highlighted in the kernel's language
//! with their execution count, and under each what it printed, its result, its
//! error with the traceback, an image as what it is (`PNG image, 640 × 480`).
//! The notebook stays its JSON: `e` edits it, and `Ctrl-b M` shows it beside the
//! rendered cells, which follow the cursor from cell to cell.
use crate::{
    config::Theme,
    highlight::Syntax,
    markdown::{self, Document},
};
use serde_json::Value;
use std::{fmt::Write as _, path::Path};

/// Outputs (data frames) get more room than prose.
pub const MAX_WIDTH: usize = 120;
/// How much of an image is decoded to tell its size.
const IMAGE_HEAD: usize = 64 * 1024;

pub fn is_notebook(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ipynb"))
}

/// The notebook's cells rendered, each tied to where it starts in the JSON.
pub fn render(source: &str, width: usize, theme: &Theme, syntax: &Syntax) -> Document {
    let notebook: Value = match serde_json::from_str(source) {
        Ok(notebook) => notebook,
        Err(error) => {
            let mut doc = markdown::render(
                &format!(
                    "> [!WARNING]\n> This notebook's JSON doesn't read: {error}. Press **e** to fix it."
                ),
                width,
                theme,
                syntax,
            );
            doc.title = "Jupyter notebook".into();
            return doc;
        }
    };
    let empty = Vec::new();
    // nbformat 4 keeps the cells at the top; 3 in its one worksheet.
    let cells = notebook["cells"]
        .as_array()
        .or_else(|| notebook["worksheets"][0]["cells"].as_array())
        .unwrap_or(&empty);
    let language = language(&notebook);
    let mut text = String::new();
    let mut starts = Vec::new();
    for cell in cells {
        starts.push(text.bytes().filter(|b| *b == b'\n').count());
        let source = joined(cell.get("source").or(cell.get("input")));
        match cell["cell_type"].as_str() {
            Some("markdown") | Some("heading") => {
                let source = source.trim_end();
                text.push_str(source);
                // A fence left open would take the cells after it.
                let fences = source
                    .lines()
                    .filter(|l| l.trim_start().starts_with("```"))
                    .count();
                if fences % 2 == 1 {
                    text.push_str("\n```");
                }
                text.push_str("\n\n");
            }
            Some("code") => {
                let count = cell["execution_count"]
                    .as_u64()
                    .or(cell["prompt_number"].as_u64());
                let label = count.map_or_else(|| " ".to_string(), |c| c.to_string());
                let _ = writeln!(text, "*In \\[{label}\\]:*\n");
                fenced(&mut text, &language, &source);
                for output in cell["outputs"].as_array().unwrap_or(&empty) {
                    self::output(&mut text, output);
                }
            }
            _ => fenced(&mut text, "raw", &source),
        }
    }
    if cells.is_empty() {
        text.push_str("*This notebook has no cells yet · e edits it*\n");
    }
    let mut doc = markdown::render(&text, width, theme, syntax);
    let rendered: Vec<usize> = starts
        .iter()
        .map(|&line| doc.line_for_source(line))
        .collect();
    // The cells' JSON lines stand for them, so the editor and this view follow
    // each other; when they can't be told apart (all on one line), nothing does.
    let lines = cell_lines(source);
    let anchors = match lines.len() == rendered.len() && lines.windows(2).all(|w| w[0] < w[1]) {
        true => lines.into_iter().zip(rendered.iter().copied()).collect(),
        false => Vec::new(),
    };
    doc.set_anchors(anchors);
    doc.headings = rendered;
    doc.marks = "cells";
    doc.title = format!(
        "Jupyter notebook  ·  {}  ·  {} cell{}",
        kernel(&notebook),
        cells.len(),
        if cells.len() == 1 { "" } else { "s" }
    );
    doc
}

/// The language the code cells are in, as a grammar's name: `python`, `r`.
fn language(notebook: &Value) -> String {
    let metadata = &notebook["metadata"];
    metadata["language_info"]["name"]
        .as_str()
        .or(metadata["kernelspec"]["language"].as_str())
        .or(metadata["language"].as_str())
        .unwrap_or("python")
        .to_lowercase()
}

/// The kernel it was written for: "Python 3 (ipykernel)", else its language.
fn kernel(notebook: &Value) -> String {
    let metadata = &notebook["metadata"];
    if let Some(name) = metadata["kernelspec"]["display_name"].as_str() {
        return name.to_string();
    }
    let info = &metadata["language_info"];
    match (info["name"].as_str(), info["version"].as_str()) {
        (Some(name), Some(version)) => format!("{} {version}", capitalized(name)),
        (Some(name), None) => capitalized(name),
        _ => "Python".into(),
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A notebook's multi-line string: one string, or a list of lines.
fn joined(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// `text` in a fenced block labeled `label`, its fence longer than any run of
/// backticks in it.
fn fenced(out: &mut String, label: &str, text: &str) {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let _ = writeln!(out, "{fence}{label}");
    let text = text.trim_end_matches('\n');
    if !text.is_empty() {
        out.push_str(text);
        out.push('\n');
    }
    let _ = writeln!(out, "{fence}\n");
}

/// One output of a code cell.
fn output(out: &mut String, output: &Value) {
    match output["output_type"].as_str().unwrap_or_default() {
        "stream" => {
            let label = match output["name"].as_str() {
                Some("stderr") => "stderr",
                _ => "output",
            };
            let text = crate::summary::plain(&joined(output.get("text")));
            fenced(out, label, &text);
        }
        "execute_result" | "pyout" => {
            let label = output["execution_count"]
                .as_u64()
                .or(output["prompt_number"].as_u64())
                .map_or_else(|| "output".to_string(), |c| format!("Out[{c}]:"));
            data(out, output, &label);
        }
        "display_data" => data(out, output, "output"),
        "error" | "pyerr" => {
            let name = output["ename"].as_str().unwrap_or("Error");
            let value = output["evalue"].as_str().unwrap_or_default();
            let said = format!("{name}: {value}");
            let _ = writeln!(
                out,
                "> [!CAUTION]\n> {}\n",
                escape(&said.split_whitespace().collect::<Vec<_>>().join(" "))
            );
            let traceback: Vec<String> = output["traceback"]
                .as_array()
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(Value::as_str)
                        .map(crate::summary::plain)
                        .collect()
                })
                .unwrap_or_default();
            let mut traceback = traceback.join("\n");
            // Its last line is the error said above.
            if traceback.trim_end().ends_with(said.trim_end()) {
                let keep = traceback.trim_end().len() - said.trim_end().len();
                traceback.truncate(keep);
            }
            if !traceback.trim().is_empty() {
                fenced(out, "traceback", traceback.trim_end());
            }
        }
        _ => {}
    }
}

/// A result or a display: the richest form a terminal shows. An image is said
/// for what it is: a plot's text form is only `<Figure size 640x480>`.
fn data(out: &mut String, output: &Value, label: &str) {
    // nbformat 4 keeps the forms in `data`; 3 next to `output_type`.
    let data = match output.get("data") {
        Some(data) => data,
        None => output,
    };
    let form = |mime: &str| data.get(mime).map(|v| joined(Some(v)));
    let images: Vec<String> = [
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "png",
        "jpeg",
    ]
    .iter()
    .filter_map(|mime| Some(image(&form(mime)?)))
    .chain(
        form("image/svg+xml")
            .or(form("svg"))
            .map(|_| "SVG image".into()),
    )
    .collect();
    if let Some(image) = images.first() {
        let _ = writeln!(out, "*▣ {} · not shown in the terminal*\n", escape(image));
        return;
    }
    if let Some(text) = form("text/markdown") {
        let _ = writeln!(out, "{}\n", text.trim_end());
        return;
    }
    let text = form("text/plain")
        .or_else(|| form("text"))
        .or_else(|| {
            form("text/html")
                .or(form("html"))
                .map(|html| markdown::strip_html(&html))
        })
        .or_else(|| form("text/latex").or(form("latex")))
        .or_else(|| {
            data.get("application/json")
                .and_then(|v| serde_json::to_string_pretty(v).ok())
        });
    match text {
        Some(text) => fenced(out, label, &crate::summary::plain(&text)),
        None => {
            let kinds: Vec<&str> = data
                .as_object()
                .map(|forms| forms.keys().map(String::as_str).collect())
                .unwrap_or_default();
            if !kinds.is_empty() {
                let _ = writeln!(
                    out,
                    "*{} · not shown in the terminal*\n",
                    escape(&kinds.join(", "))
                );
            }
        }
    }
}

/// What an image is, from its base64: "PNG image, 640 × 480".
fn image(base64: &str) -> String {
    crate::hex::kind(&decode(base64, IMAGE_HEAD))
}

/// Up to `max` bytes of `text`'s base64, skipping line breaks; it stops at what
/// isn't base64.
fn decode(text: &str, max: usize) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut bytes = Vec::new();
    let (mut bits, mut held) = (0u32, 0u32);
    for c in text.bytes() {
        if bytes.len() >= max || c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        let Some(v) = value(c) else { break };
        bits = (bits << 6) | u32::from(v);
        held += 6;
        if held >= 8 {
            held -= 8;
            bytes.push((bits >> held) as u8);
            bits &= (1 << held) - 1;
        }
    }
    bytes
}

/// Markdown's own characters as plain text.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '#' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The JSON line each cell starts on: its `{`, or the line of its `"cell_type"`
/// (a key only cells have, and which a string can't fake: its quotes would be
/// escaped).
fn cell_lines(json: &str) -> Vec<usize> {
    let lines: Vec<&str> = json.lines().collect();
    let mut starts = Vec::new();
    for (number, line) in lines.iter().enumerate() {
        let Some(at) = line.find("\"cell_type\"") else {
            continue;
        };
        if line[..at].ends_with('\\') {
            continue;
        }
        let opened = number > 0 && lines[number - 1].trim() == "{";
        starts.push(if opened { number - 1 } else { number });
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A notebook as Jupyter writes it: a Markdown cell, a cell that printed and
    /// returned, one that drew a plot, one that failed, and one never run.
    const NOTEBOOK: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "a",
   "metadata": {},
   "source": [
    "# Word counts\n",
    "\n",
    "How often each word appears."
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 3,
   "id": "b",
   "metadata": {},
   "outputs": [
    {
     "name": "stdout",
     "output_type": "stream",
     "text": [
      "counting\n"
     ]
    },
    {
     "data": {
      "text/plain": [
       "Counter({'apple': 3, 'banana': 2})"
      ]
     },
     "execution_count": 3,
     "metadata": {},
     "output_type": "execute_result"
    }
   ],
   "source": [
    "from collections import Counter\n",
    "print(\"counting\")\n",
    "Counter(words)"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 4,
   "id": "c",
   "metadata": {},
   "outputs": [
    {
     "data": {
      "image/png": "iVBORw0KGgoAAAANSUhEUgAAAoAAAAHgCAYAAAA10dzk",
      "text/plain": [
       "<Figure size 640x480 with 1 Axes>"
      ]
     },
     "metadata": {},
     "output_type": "display_data"
    }
   ],
   "source": "plt.plot(xs)"
  },
  {
   "cell_type": "code",
   "execution_count": 5,
   "id": "d",
   "metadata": {},
   "outputs": [
    {
     "ename": "ZeroDivisionError",
     "evalue": "division by zero",
     "output_type": "error",
     "traceback": [
      "\u001b[0;31m---------------------------------------------------------------------------\u001b[0m",
      "\u001b[0;31mZeroDivisionError\u001b[0m                         Traceback (most recent call last)",
      "Cell \u001b[0;32mIn[5], line 1\u001b[0m\n\u001b[0;32m----> 1\u001b[0m \u001b[38;5;241;43m1\u001b[39;49m\u001b[38;5;241;43m/\u001b[39;49m\u001b[38;5;241;43m0\u001b[39;49m\n",
      "\u001b[0;31mZeroDivisionError\u001b[0m: division by zero"
     ]
    }
   ],
   "source": [
    "1/0"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "e",
   "metadata": {},
   "outputs": [],
   "source": [
    "words.sort()"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3 (ipykernel)",
   "language": "python",
   "name": "python3"
  },
  "language_info": {
   "name": "python",
   "version": "3.11.4"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

    fn rendered(source: &str) -> (Document, Vec<String>) {
        let theme = Theme::default();
        let syntax = Syntax::new(&theme);
        let doc = render(source, 80, &theme, &syntax);
        let lines = doc
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        (doc, lines)
    }

    #[test]
    fn cells_show_with_their_outputs_as_jupyter_shows_them() {
        let (doc, lines) = rendered(NOTEBOOK);
        let text = lines.join("\n");
        assert_eq!(
            doc.title,
            "Jupyter notebook  ·  Python 3 (ipykernel)  ·  5 cells"
        );
        assert_eq!(doc.marks, "cells");
        assert!(text.contains("Word counts"), "{text}");
        assert!(text.contains("How often each word appears."));
        // Code with its count, then what it printed and what it returned.
        let order = [
            "In [3]:",
            "from collections import Counter",
            "counting",
            "Out[3]:",
            "Counter({'apple': 3, 'banana': 2})",
        ];
        let mut from = 0;
        for part in order {
            let at = text[from..]
                .find(part)
                .unwrap_or_else(|| panic!("{part} after {from}:\n{text}"));
            from += at + part.len();
        }
        // A plot is said for what it is, not its <Figure …> text.
        assert!(
            text.contains("PNG image, 640 × 480 · not shown in the terminal"),
            "{text}"
        );
        assert!(!text.contains("<Figure"));
        // An error: what it was, and the traceback without its colors.
        assert!(text.contains("ZeroDivisionError: division by zero"));
        assert!(text.contains("----> 1 1/0"), "{text}");
        assert!(!text.contains('\u{1b}'));
        // A cell never run has no count.
        assert!(text.contains("In [ ]:"));
        assert!(text.contains("words.sort()"));
        // [ ] jump between the cells, in order.
        assert_eq!(doc.headings.len(), 5);
        assert!(doc.headings.windows(2).all(|w| w[0] < w[1]));
        assert!(lines[doc.headings[3]..].join("\n").starts_with("In [5]:"));
    }

    #[test]
    fn the_json_and_the_cells_follow_each_other() {
        let (doc, _) = rendered(NOTEBOOK);
        let json: Vec<&str> = NOTEBOOK.lines().collect();
        let starts = cell_lines(NOTEBOOK);
        assert_eq!(starts.len(), 5);
        assert!(starts.iter().all(|&l| json[l].trim() == "{"));
        // The cursor in a cell's JSON shows that cell; a cell shown puts the
        // cursor on its JSON.
        for (cell, &line) in starts.iter().enumerate() {
            assert_eq!(doc.line_for_source(line + 3), doc.headings[cell]);
            assert_eq!(doc.source_for_line(doc.headings[cell]), line);
        }
        // All on one line: nothing to follow, nothing wrong.
        let minified: Value = serde_json::from_str(NOTEBOOK).unwrap();
        let (doc, lines) = rendered(&minified.to_string());
        assert_eq!(doc.headings.len(), 5);
        assert_eq!(doc.line_for_source(0), 0);
        assert!(lines.join("\n").contains("division by zero"));
    }

    #[test]
    fn broken_or_empty_notebooks_say_so() {
        let (_, lines) = rendered("{ \"cells\": [ ");
        // (Its apostrophe comes out curly.)
        assert!(lines.join("\n").contains("JSON doesn’t read"));
        let (doc, lines) =
            rendered(r#"{"cells": [], "metadata": {}, "nbformat": 4, "nbformat_minor": 5}"#);
        assert!(lines.join("\n").contains("no cells yet"));
        assert_eq!(doc.title, "Jupyter notebook  ·  Python  ·  0 cells");
        // A Markdown cell with a fence left open doesn't take the cells after it.
        let open = r#"{"cells": [
 {"cell_type": "markdown", "metadata": {}, "source": "```python\nx = 1"},
 {"cell_type": "code", "execution_count": 1, "metadata": {}, "outputs": [], "source": "y = 2"}
], "metadata": {"kernelspec": {"display_name": "Julia 1.10", "language": "julia"}}}"#;
        let (doc, lines) = rendered(open);
        assert_eq!(doc.title, "Jupyter notebook  ·  Julia 1.10  ·  2 cells");
        assert!(lines[doc.headings[1]..].join("\n").starts_with("In [1]:"));
    }

    #[test]
    fn rich_outputs_show_their_best_text_form() {
        let mut text = String::new();
        let html = serde_json::json!({"output_type": "display_data", "data": {"text/html": "<b>bold</b> table"}});
        output(&mut text, &html);
        assert!(
            text.contains("bold table") && !text.contains("<b>"),
            "{text}"
        );
        let mut text = String::new();
        let markdown = serde_json::json!({"output_type": "display_data", "data": {"text/markdown": "**done**", "text/plain": "<IPython.Markdown>"}});
        output(&mut text, &markdown);
        assert_eq!(text, "**done**\n\n");
        let mut text = String::new();
        let svg =
            serde_json::json!({"output_type": "display_data", "data": {"image/svg+xml": "<svg/>"}});
        output(&mut text, &svg);
        assert!(text.contains("SVG image"));
        let mut text = String::new();
        let widget = serde_json::json!({"output_type": "display_data", "data": {"application/vnd.jupyter.widget-view+json": {}}});
        output(&mut text, &widget);
        assert!(
            text.contains("application/vnd.jupyter.widget-view+json · not shown"),
            "{text}"
        );
        // Backticks in the output don't end its block.
        let mut text = String::new();
        fenced(&mut text, "output", "a ``` b");
        assert!(text.starts_with("````output\n"), "{text}");
    }

    #[test]
    fn base64_decodes_enough_to_tell_an_image() {
        assert_eq!(decode("aGVs\nbG8=", 100), b"hello");
        assert_eq!(decode("aGVsbG8=", 2), b"he");
        assert_eq!(
            image("iVBORw0KGgoAAAANSUhEUgAAAoAAAAHgCAYAAAA10dzk"),
            "PNG image, 640 × 480"
        );
    }
}

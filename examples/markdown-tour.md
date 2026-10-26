---
title: Markdown tour
purpose: Exercise every construct Raido's preview renders
---

# Markdown tour

This file exercises **every construct** Raido's Markdown preview renders. Open it
with `raido examples/markdown-tour.md`, scroll with `j`/`k`, jump between headings
with `]` and `[`, and press `e` to see the source of whatever you are reading.

## Inline formatting

Plain text, **bold**, *italic*, ***bold italic***, ~~strikethrough~~, and
`inline code with spaces`. Formatting can **nest *inside* other `formatting`**.
Smart punctuation turns "straight quotes" into curly ones -- and dashes too...

A [link with a target](https://github.com/rust-lang/rust), a bare autolink
<https://example.com>, an [in-page anchor](#tables) (no target shown), and an
email autolink <hello@example.com>.

A line ending with two spaces  
becomes a hard break, and so does a backslash\
at the end of a line.

Unicode should line up: café, naïve, Ελληνικά, 日本語のテキスト, and emoji-free
box drawing ╭─╮. A very long word that must be split by character:
Pneumonoultramicroscopicsilicovolcanoconiosis_supercalifragilisticexpialidocious_and_then_some_more_text_to_force_a_hard_split.

## Headings

### Third level

#### Fourth level

##### Fifth level

###### Sixth level

## Lists

- First bullet
- Second bullet with a long sentence that keeps going so the wrapped continuation
  lines stay indented under the text instead of under the bullet marker.
  - Nested bullet
    - Third level bullet
      - Fourth level wraps back to the first symbol
  - Back to the second level
- Third bullet

1. Ordered item one
2. Ordered item two
   1. Nested ordered item
   2. Another nested item
3. Ordered item three

A paragraph ends that list, so the next one keeps its own numbering:

5. This list starts at five
6. And continues at six

### Task list

- [x] Visual Studio 2022 Dark theme
- [x] VS Code Dark Modern theme
- [x] Markdown preview
- [ ] Language server support
  - [ ] Diagnostics
  - [ ] Go to definition

### Loose list

- A loose list item has blank lines between items.

- Each item becomes its own paragraph.

  A second paragraph inside the same item stays indented.

- Last loose item.

## Block quotes

> A plain quote is dimmed and marked with a bar on the left. It wraps across
> lines without losing the bar.
>
> A second paragraph in the same quote.
>
> > A nested quote gets a second bar.
>
> Quotes can hold lists:
>
> - one
> - two

## GitHub alerts

> [!NOTE]
> Useful information that users should know, even when skimming.

> [!TIP]
> Press `Ctrl-b M` for a live preview beside the source.

> [!IMPORTANT]
> Key information users need to know to achieve their goal.

> [!WARNING]
> Urgent info that needs immediate user attention to avoid problems.

> [!CAUTION]
> Advises about risks or negative outcomes of certain actions.

## Code blocks

Rust:

```rust
use std::collections::HashMap;

/// Count words, case-insensitively.
fn count(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word.to_lowercase()).or_insert(0) += 1;
    }
    counts
}
```

Python:

```python
def fib(n: int) -> list[int]:
    """Return the first n Fibonacci numbers."""
    a, b, out = 0, 1, []
    while len(out) < n:
        out.append(a)
        a, b = b, a + b
    return out
```

C:

```c
#include <stdio.h>

int main(void) {
    for (int i = 0; i < 3; i++) {
        printf("hello %d\n", i);
    }
    return 0;
}
```

Shell, then a fence with no language:

```sh
cargo install --path . --locked --force
raido examples/markdown-tour.md
```

```
No language: plain text, no highlighting.
	A tab-indented line expands to spaces.
```

An indented code block:

    indented code block
    with two lines

A line far longer than the pane gets truncated with an ellipsis instead of wrapping:

```text
0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789
```

## Tables

| Theme | Background | Accent | Notes |
|:------|:----------:|-------:|-------|
| visual-studio-dark | `#1e1e1e` | `#007acc` | Visual Studio 2022, the IDE |
| vscode-dark-2026 | `#121314` | `#3994bc` | VS Code's newest dark theme |
| dracula | `#282a36` | `#bd93f9` | **bold** and *italic* work in cells |

Left, center and right alignment:

| Left | Center | Right |
|:-----|:------:|------:|
| a | b | c |
| longer left | middle | 1,234.56 |

A table too wide for narrow panes shrinks its widest columns and truncates cells:

| Column one | Column two with a much longer header | Column three | Column four is also fairly long |
|---|---|---|---|
| short | This cell contains a long sentence that will not fit in a narrow pane | x | Another long cell to squeeze |

## Images and HTML

![Raido running with Rust source, an AI launcher, shell, and Git tree](../docs/raido.png)

<p align="center">
  <img src="../docs/raido.png" alt="Centered logo from an HTML block">
</p>

<!-- This HTML comment should not appear in the preview. -->

Inline HTML tags such as <kbd>Ctrl</kbd>+<kbd>S</kbd> keep their text, and a<br>line break tag breaks the line.

## Footnotes

Raido renders footnote references[^speed] and their definitions[^long].

[^speed]: Rendering is cached and redone only when the text or pane width changes.
[^long]: A longer footnote that wraps onto a second line to check that its
    continuation stays indented under the label.

---

## Empty and edge cases

- 
- The item above is empty but still shows its bullet.

That's the whole tour. Press `e` to edit, change a word, and toggle back with
`Ctrl-b m` to watch it re-render.

# Changelog

All notable changes to Raido are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Raido uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): while it is 0.x, a minor
release (0.2 → 0.3) may change keys, settings or behavior, and a patch release
(0.2.0 → 0.2.1) only fixes things.

## [Unreleased]

### Added

- **The Ocean Drive theme** (`:theme ocean-drive`), a Raido original after the
  neon-sunset look of GTA VI's trailers and key art: a deep indigo night, hot neon
  pink, sunset orange and golden-hour yellow, ocean aqua and dusky violet, across
  the UI, syntax colors and the terminal's 16 colors.

- **A `/` menu in notes**, as in Notion: in a Markdown file, `/` at the start of a
  line or after a space opens a menu of blocks (Text, Heading 1–3, bulleted,
  numbered and to-do lists, Quote, the five callouts, Code block, Table, Divider,
  Toggle, Math block, Link, Image, Date, Table of contents), found by name or
  another word (`/todo`, `/checkbox`, `/warn`). An empty line becomes the block;
  after text, the block starts the line below. Esc keeps what was typed, and a
  `/` in a path, inline code or a code block is just a `/`.

- **Markdown notes are drawn rendered while you write them**, like Obsidian's Live
  Preview: headings in their colors without their `#`s, bold, italic, struck and
  `inline code` without their marks, links as underlined text, bullets and to-dos
  (`☐`, `☑`, done ones struck out), quotes and callouts with their bar and title,
  code blocks shaded and colored in their language, tables lined up in a grid, and
  rules. The line the cursor is on shows its Markdown, to edit as text, and so do a
  selection's lines. Long lines wrap at words, a list item's under its text. A click
  lands on the character drawn there, and a click on a `☐` ticks it. `.md` files
  now open in the editor this way instead of the read-only preview, which
  `Ctrl-b m` still opens; `Ctrl-b R` (`:render`) shows a file as written, and
  `markdown_live = false` brings back the old way.

- **The docs take a question of several lines**, so you can paste a code snippet and
  ask how it works. A paste used to become one line, its line breaks turned into
  spaces and its tabs dropped; now it goes in as it was, and the agent gets the
  question exactly as typed. `Alt-Enter` and `Ctrl-j` start a new line (`Shift-Enter`
  too, in terminals that tell it from Enter), `Tab` types a tab, and `Ctrl-v` pastes
  what you copied in Raido. The question grows to half the pane, ↑↓ and Home End
  move in it, and above the answer the code shows as a block in your file's
  language, indented as you pasted it; `Ctrl-y` still copies the answer's example.

- **The docs ask which AI answers**, the first time you ask while `docs_ai` is
  still `"auto"` and more than one AI could: two of the AI CLIs, or Ollama or your
  model server with its models. They took the first installed without a word, so
  Claude Code answered even with Codex beside it, and a local model was whichever
  Ollama had pulled last. The pane lists Claude Code on Sonnet, Opus and Haiku,
  Codex, Gemini CLI, Antigravity and OpenCode on their own models, and each local
  model, with a note on each; the pick is saved in `config.toml` as
  `docs_ai = ["<pick>", "auto"]`, so it's asked once and the others still stand in
  for it. `:ask-with` asks again, and `:ask-with codex` names one.

- **The sidebar hides by itself in a small window**, so the code isn't squeezed
  beside it, and comes back when the window grows: the explorer and the Git section
  when the panes would have under 70 columns beside them (under 99 at the default
  width), the Git section alone under 24 rows, the whole sidebar under 16. The status
  line says why the first time. `Ctrl-e`, `:files` and `Ctrl-b g` still show them
  while you use them, `Ctrl-b s` keeps the sidebar on screen, and
  `auto_hide_sidebar = false` turns it off. It used to hide only under 70 columns.

- **Assembly for x86, ARM and RISC-V is highlighted.** `.s`, `.S`, `.asm`, `.nasm`
  files opened as plain text: syntect has no assembly grammar. Raido's own grammars
  now color them for x86 and x86-64 (GNU as in AT&T or Intel syntax, NASM, YASM,
  MASM), 32-bit ARM and Thumb (GNU as, armasm), AArch64 (Apple's assembler too) and
  RISC-V, and Go's assembler on all of them: labels, instructions, branches and calls
  in the control-flow color, registers (`%rax`, `x0`, `v0.4s`, `a0`, CSRs),
  directives, immediates, relocations (`@PLT`, `:lo12:`, `msg@PAGEOFF`, `%hi(x)`),
  sizes, strings, comments and `.S` files' `#include` lines. The extensions don't
  say which CPU, so Raido reads it from the project's `.asm-lsp.toml`, the file's
  name (`memmove_arm64.s`), the registers and instructions in it, its folders
  (`arch/riscv/`), and last the computer's own CPU; an empty file switches grammar
  as soon as its code says another CPU. Enter after a label indents, a label typed
  on an indented line goes back to the labels' column, and `Ctrl-/` comments lines
  out with the mark the file's assembler reads (`;`, `@`, `//` or `#`). Markdown's
  `nasm`, `armasm`, `arm64` and `riscv` blocks are highlighted, and `asm` blocks by
  their code. **asm-lsp** adds hover docs for instructions and registers,
  completion, go to definition and the assembler's errors: Raido starts it for the
  files it reads right without settings (GNU as for this computer's CPU), and for
  the others `:lsp` says what to put in `.asm-lsp.toml`. The install scripts set it
  up (Homebrew, else `cargo install`).

- **Operators reach as far as relative numbers show**, as in Vim: `d3j` deletes the
  cursor's line and the 3 below it, `y3k` copies it and the 3 above, `gc3j`
  comments them. Typing the count with `:set rnu` on says what `j` and `k` will do.

- **Jupyter notebooks open as their cells**, as Jupyter and GitHub show them:
  Markdown cells rendered, code cells highlighted in the kernel's language with
  `In [3]:`, and under each what it printed, its result (`Out[3]:`), and its error
  with the traceback, its colors taken out. A plot is named for what it is
  (`PNG image, 640 × 480`). `]` and `[` go from cell to cell; `e` edits the
  notebook's JSON at that cell, and `Ctrl-b M` shows the JSON beside the cells,
  which follow the cursor.

- **Interactive documentation** (`Ctrl-b /`, `:ask`): ask how to do something in
  the language you write, what a function does, or how this project does a thing,
  and an AI agent answers in a pane beside the code like the official docs and
  Stack Overflow, briefly, then stops: the function and its import, a minimal
  example and what it prints. When unsure of the output, Claude Code, Codex and
  OpenCode run the example in a scratch folder and show what it really printed.
  Code wraps to the pane's width. Enter on an empty line explains the name under the
  cursor or the code you selected; follow-ups remember the questions before;
  `Ctrl-y` copies the example on screen. It works with every AI the launcher has
  (`docs_ai`, "auto" by default), read-only, and changes nothing in the project.
- The review's explanations (`review_ai`) can come from Antigravity (`"agy"`) and
  OpenCode (`"opencode"`) too.

- **The terminal tab shows the project's name**, the folder Raido works in, instead
  of `raido`, so a tab per project tells them apart. Its own title comes back when
  Raido quits: from the terminal's title stack where it keeps one, and in macOS
  Terminal, which keeps none, as its usual title.

- **Archives open as a tree of their files**, read by Raido itself, so no unzip,
  tar or 7z is needed on any system: zip and what's built on it (`.jar`, `.whl`,
  `.apk`, `.vsix`, `.docx`…), tar plain or compressed with gzip, bzip2, xz or
  Zstandard (`.tar.gz`, `.tgz`, `.tar.xz`, `.crate`…), one compressed file (`.gz`,
  `.bz2`, `.xz`, `.zst`) and 7z, each known by its first bytes. Enter on a file
  opens a read-only copy of it as any file opens: text with its highlighting and
  language server, a spreadsheet as a grid, an archive inside as another tree.
  `/` finds a file by name, `c` and `E` fold everything. RAR still opens as hex.

- **Text in other encodings opens as text.** UTF-16 files (Windows PowerShell
  writes them), Latin-1 and the other older encodings (Windows-1254 for Turkish,
  Windows-1251 for Cyrillic, Shift_JIS, GBK…), refused before, open in the editor
  and save back as they were written, byte for byte; the status bar names the
  encoding. A character the encoding can't hold stops the save instead of turning
  into something else. `:encoding utf-8` converts a file, and `:encoding! NAME`
  reads it again when the guess was wrong. Project search and the Git marks in the
  gutter read these files too.

- **Spreadsheets, read-only.** `.xlsx`, `.xlsm`, `.xlsb`, `.xls` and `.ods` files,
  refused as binary before, open as a grid laid out as Excel left them: each
  column's width, frozen columns and rows, merged cells, and the sheet and cell the
  workbook was saved on. Text Excel wraps wraps in its column and the row grows;
  other text runs on into empty cells as in Excel; `w` switches to one line per
  row. Each number shows in its cell's format (`15%`, `1,234.50`, `€ 9.99`, dates
  as `2024-03-15`), `####` when its column is too narrow. Below the grid is the
  selected cell in full with its formula and exact value, and Enter opens the whole
  cell to scroll and copy. The sheets are tabs (`]` / `[`), `/` finds text, `y`
  copies a cell and `Y` its row, and the grid reads the file again when it changes
  on disk. Formulas aren't recalculated: cells show what the file saved.

- **SQLite databases and CSV files in the same grid.** A SQLite database (known by
  its first bytes, whatever its name) opens read-only with a tab per table and
  view, the column names on top, `NULL` and blobs shown as such, and the first
  100,000 rows of each table. `.csv` and `.tsv` files open as a grid the way
  Markdown opens rendered and stay editable text: `e` edits the text at the row,
  `Ctrl-b m` switches back and forth, and `Ctrl-b M` puts the grid beside the text,
  following your edits.

- **Files that aren't text open as hex** instead of being refused: images, PDFs,
  executables, fonts. Above the bytes Raido says
  what the file is (`PNG image, 1200 × 800`, `ELF 64-bit executable, x86-64`,
  `Mach-O executable, ARM64`), and `/` finds text or bytes in hex (`ff d8 ff`).
  Only the part on screen is read, so any size opens at once.

- **`Ctrl-p` finds a file by a few of its letters**, as VS Code's quick open does:
  `mnrs` opens `src/main.rs`, the best matches first and the matched letters
  highlighted, the files you opened last first when nothing's typed, and `main.rs:42`
  to a line. It replaces the path prompt and its Tab completion.

- **Completions bring their imports.** Accepting `useState` in TypeScript adds
  `import { useState } from "react";`, and the Go, Rust, Python and C++ servers'
  suggestions do the same, as in VS Code; the status line says what was added.

- **Comment out lines** with `Ctrl-/` in any mode, `gcc` (`3gcc` for three lines) or
  `gc` on a selection, and back in with the same keys, as in VS Code: each
  language's own mark at the lines' shared indentation, blank lines left alone,
  `<!-- -->` and `/* */` where a language has no line comment.

- **TypeScript and JavaScript.** `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs` and `.cjs` files,
  plain text in Raido before, are highlighted by its own grammar, JSX included
  (`<T>` stays a generic in `.ts`). `typescript-language-server` brings type errors,
  completion, hover, go to definition and formatting, and with
  `format_on_save = ["typescript"]` organizes imports on save. It needs TypeScript
  6 beside it (`npm install -g typescript-language-server typescript@6`); the
  install scripts set that up. HTML, XML and JSX tags and their attributes take
  the theme's keyword and variable colors.

- **Highlighting for PowerShell, `.env`, INI, ignore files, SCSS, Terraform / HCL
  and Protocol Buffers**, by Raido's own grammars: `.ps1` scripts with their
  cmdlets, parameters and `$(…)` in strings; `.env` and `.env.local`; `.ini`,
  `.cfg`, `.editorconfig`, `.gitconfig` and systemd units; `.gitignore`,
  `.dockerignore` and the other ignore files; SCSS with its variables, mixins and
  nesting; `.tf`, `.tfvars` and `.hcl` with interpolation, directives and heredocs;
  and `.proto` files. Markdown code blocks too, by any of their names.

- **YAML and JSON checked against their schemas.** Red Hat's `yaml-language-server`
  and VS Code's JSON server underline what a file's published schema doesn't allow,
  and complete and explain its keys: GitHub Actions workflows, `package.json`,
  `tsconfig.json` and the other files SchemaStore's catalog names, and Kubernetes
  manifests, known by their `apiVersion:` and `kind:`. `tsconfig.json`, `.vscode`
  settings and the like may hold comments, as in VS Code. `:format` lays out YAML
  (`format_on_save = ["yaml"]`). `lsp_schemas = false` keeps them offline. The
  install scripts set up both servers.

- **Shell script support.** Scripts are known by extension, by name (`.bashrc`,
  `.zshrc`, `.envrc`, `PKGBUILD`) and, without an extension, by their `#!` line, for
  highlighting, typing and the status line (`Shell (bash)`). `fi`, `done`, `esac`,
  `else` and `elif` line up with the block they close, case patterns indent and `;;`
  goes back, and commands that go on over lines indent them. With
  `bash-language-server`: ShellCheck's warnings, completion, hover, go to
  definition, and `:format` with shfmt in the file's own indentation (also on save
  with `format_on_save = ["shell"]`). The install scripts set up all three.
- `:format` asks every server for the file's own indentation, tabs included.
- Scripts without an extension are highlighted by their `#!` line
  (`#!/usr/bin/env python3` too).

- **Docker support.** Dockerfiles and Containerfiles (`Dockerfile.dev`,
  `api.dockerfile` too) are highlighted by Raido's own grammar: instructions, images
  and build stages, flags, variables, strings, comments, and `RUN` / `CMD` read as
  shell, heredocs included. Enter indents the lines an instruction goes on over.
  Docker Compose files (`compose.yaml`, `docker-compose.yml` and their overrides)
  show as *Docker Compose* and complete Compose's keys. With Docker's
  `docker-language-server` installed, Compose files get completion, errors, hover,
  go to definition and formatting, and Dockerfiles BuildKit's warnings; Raido turns
  the server's usage reports to Docker off. The install scripts build it with
  `go install` where Docker (or Podman) and Go are installed.
- Files without an extension are highlighted by their name (`Makefile`, `.bashrc`).

- **`Ctrl-a` selects the whole file** in an editor, in any mode: then Backspace
  deletes it, `Ctrl-c` copies it, `Ctrl-x` cuts it and `Ctrl-v` pastes over it
  (typing replaces it too, when you were typing). Terminal and AI panes keep `Ctrl-a`
  for their program.

- **`Ctrl-b n` starts a new conversation in the AI pane** (or `:ai new`): its CLI
  starts again in place, on the same model and command, so it works for every agent
  and local model (Claude Code, Codex, Gemini CLI, Antigravity, OpenCode, Ollama, LM
  Studio, your `[ai.<name>]`). While the agent is answering a prompt it asks first;
  `Ctrl-b n` again stops it.

- **⌘V pastes screenshots into AI panes on macOS** (`paste_screenshots`): a
  terminal's ⌘V pastes only text, so while an AI pane is open Raido saves a
  screenshot put on the clipboard and adds its path beside the image. ⌘V then pastes
  the path and Claude Code, Codex and OpenCode attach the image. The path is there
  only while Raido is in front: other apps (a browser pasting into GitLab, Slack)
  get the screenshot exactly as copied. Text you copy is never touched.

- **Screenshots for AI panes:** `Ctrl-b I` (`:image`) attaches the image on the
  clipboard, else the screenshot you took in the last 30 minutes, to the AI pane's
  prompt, in any terminal (also Terminal.app, where ⌘V can't paste an image).
  Claude Code and Codex attach it as an image. Ctrl-V in an AI pane keeps working as
  outside Raido, and ⌘V's empty paste, in terminals that send one for an image, now
  reaches Codex as Ctrl-V too. `raido --keys` shows pastes and focus changes, so you
  can see what your terminal sends for ⌘V.

- **Local vision models take screenshots:** OpenCode on your model server is told
  which models read images (Qwen2.5-VL, InternVL, Pixtral, Gemma 3, llava… by name
  on vLLM), so it sends them; the Local LLM (`ollama run`) gets the path to ask
  about. `Ctrl-b I` says when the pane's Ollama model has no `vision` capability.

- **AI panes show their model and effort** at the bottom right of the pane, such as
  `Opus 5.5 · xhigh` for Claude Code or `gpt-5.6-sol · medium` for Codex, read from
  the conversation the agent writes. Switching in the pane (`/model`, `/effort`)
  updates it within a moment, without waiting for the next answer.

- **Gitignored notes are reviewed** (`review_ignored`): when an agent edits, adds or
  deletes a gitignored `.md` or `.txt` file (a `TODO.txt`, a private `plans/`
  folder), the review shows it and can keep or undo it like any other file. Your
  repository still ignores them; only Raido's own snapshot takes them in. Other
  patterns can be named (`"*.org"`, `"plans/*.md"`), `[]` turns it off. Files over
  2 MB and dependency, build and cache folders stay out.

- **JSON formatting:** `:format` lays out a `.json` file one value per line,
  indented as the file is (else `tab_width`), with no language server. Key order and
  the exact text of strings and numbers are kept; invalid JSON is left alone and
  the status line says where and why. `format_on_save = ["go", "json"]` formats JSON
  on every save.

- **TOML is highlighted:** `pyproject.toml`, `Cargo.toml`, Raido's own `config.toml`
  and ```` ```toml ```` blocks in Markdown color their tables, keys, strings, numbers,
  dates and comments in every theme. The grammar is Raido's own and loads apart from
  the bundled ones, so starting stays as quick as before.

- **Drag a pane border to resize**, as in tmux: the panes on both sides of the line
  follow the mouse, every one along it, so a pane the whole height on the right
  resizes with all the panes stacked beside it. Focus stays where it was.

- **Your own model server** (`[model_server]`): Ollama or vLLM on another machine
  while the agents and your code stay here. OpenCode lists and runs on its models,
  the Local LLM chats with an Ollama server, and `review_ai = "local"` explanations
  come from it. `tunnel` reaches it through SSH so it needn't be on the network, and
  `key_env` names the variable with its API key.

### Fixed

- **An AI agent in a Raido pane answers questions about Raido from its skill.**
  Agents found the skill but weren't told they run inside Raido, so a question
  like "how do I split this pane?" didn't make them load it unless you said "in
  Raido". Claude Code is now told in its system prompt, and Codex in its developer
  instructions (after any of yours); the skill's description tells Gemini CLI,
  Antigravity and OpenCode that `RAIDO=1` marks a Raido pane.
- **Backspace and Ctrl-C work in terminals that send them differently.** A
  terminal that sends ^H for Backspace (xterm as it comes, some others too) had
  Backspace do nothing in Raido: Ctrl-H now deletes as Backspace, as in Vim,
  except in a terminal pane (the program there gets it as typed) and the find bar
  (where it opens replace). And a terminal left by an earlier program in xterm's
  modifyOtherKeys or kitty's keyboard mode sends Ctrl-C in a form Raido couldn't
  read: Raido turns those modes off when it starts.
- **Keys typed while an AI answers in the background stay Raido's.** The docs and
  the review's explanations ran their AI CLI attached to Raido's terminal, and
  Antigravity opens it (`/dev/tty`) to read keys and set its own modes, so text
  typed in the docs pane while it worked, Backspace among it, could go to it
  instead. They now run in a session of their own (`setsid` on Linux, the same
  through perl on macOS, a hidden console on Windows), with no terminal to open.
- **A red dot no longer turns up on a line with nothing wrong.** Diagnostics were
  placed by line number in the text as it was when they arrived, so adding or
  removing lines left a dot on whatever line took that number until the language
  server checked again (half a second or more with pyright), and a check of a text
  you had typed past since landed on the wrong lines. Now the marks move with
  their lines as you type, one on a removed line goes, and what a server says
  about an older version lands where those lines are now.
- **A dot says why it's there.** An error's or warning's message shows, faint,
  after the line it marks (`inline_diagnostics = false` hides it); the cursor's
  line has it at the bottom of the pane, as before.
- **Closing a file without saving lets go of your edits.** `:q!` closed the pane
  but kept your unsaved edits in memory, so opening the file again showed them
  instead of what an AI had written to it since. Now the file opens as it is on
  disk (`u` brings your edits back while the file hasn't changed again); a file
  still shown in another pane keeps them there. `:qa!` also discards for real: the
  next session used to bring the unsaved edits back.
- **A file keeps following an AI after a clash with your unsaved edits.** Once
  an AI changed the same lines as an unsaved edit of yours, even a stray keystroke,
  the pane kept showing only your text, and every later change it made to that
  file was left out too, wherever it was. Now only the lines where the two meet
  keep yours (Raido names them once); the rest of that change comes in, and so does
  everything the AI writes after it. `:w` still won't write over the change it
  left out: `:e!` loads the file (`u` brings yours back), `:w!` saves yours.
- **Browsing the explorer no longer opens a pane per file.** A file open already
  (one whose pane you closed, one a definition took you to, one a restored session
  loaded) showed in the preview pane, after which every file browsed opened in a
  new pane of its own, until there was no room left. Browsing past such files now
  keeps to the one preview pane; Enter or an edit keeps a file there, as before.
- A long completion list (TypeScript's globals) no longer leaves out what you're
  typing: the suggestions for the word come first before the list is cut at 300.

- **Copy and paste carry `\n` line breaks only.** A terminal sends a paste's line
  breaks as `\r`, and the editor kept them, so pasted lines ended with CR (and
  Windows text with CRLF). Pastes (⌘V, Ctrl-v, `p`) now take the file's own line
  breaks (`\n`, or `\r\n` in a file kept with CRLF), copies leave Raido with `\n`,
  and a file whose lines a paste ended with CR opens with `\n` under
  `line_endings = "lf"`.

- **A click no longer copies over your clipboard:** a click whose pointer slipped
  a cell selected and copied a character or two, replacing a screenshot you were
  about to paste into an AI pane. Selecting now takes two cells of movement, or one
  held a moment.

- **Lines end with `\n`** (`line_endings = "lf"`, the default): a file with
  Windows `\r\n` endings opens with `\n` and is saved that way (the status bar
  shows `CRLF→LF` until it is), so lines you type never end in `\r\n`. Before, one
  `\r\n` anywhere in a file made every new line `\r\n`. `"auto"` keeps each file's
  own (what most of its lines use), `"crlf"` writes `\r\n`; `.bat` and `.cmd` files
  keep theirs.

- **Programs in panes hear when they gain and lose the focus** (focus reports, as
  in tmux with `focus-events on`): switching panes or windows tells them. Claude
  Code then shows its `Image in clipboard · ctrl+v to paste` hint inside Raido.

- **Security:** opening a project whose `.raido/session.json` came with it (a
  repository you cloned) could start a program or script it named, as if it were an
  AI pane, and hand a CLI flags through a saved conversation id or model. A session
  now only starts the AI CLIs Raido knows and your `[ai.<name>]` entries, with ids
  that are UUIDs and models that aren't flags.
- Pasted text can no longer end a bracketed paste early and run what follows.
- With many panes docked, the top bar could draw past the screen's edge and crash.
- OpenCode starts, without your model server, when the server can't be reached;
  a restored OpenCode pane no longer comes back as a plain shell then.
- Text sizes with decimals (`font_size = 12.5`, `:font size 12.7`) work in macOS
  Terminal on Macs whose region writes decimals with a comma (Turkish, German,
  French…). There, such a size kept Raido's font from applying at all. `:font size`
  also takes `12,5`.
- Python imports resolve from the project's virtual environment (uv's `.venv`,
  Poetry's, `venv`) without activating it first: the server runs on it, as VS Code
  does once you pick its interpreter. Packages added later (`uv add`) and modules an
  AI or a terminal creates resolve within about two seconds instead of staying
  "could not be resolved" until `:lsp restart`, and a venv made or rebuilt while
  Raido runs restarts the server on it. `:lsp` names the environment, and a
  `pyright-langserver` or `basedpyright-langserver` in the venv is used first.
- A file open in a pane shows what an AI or a terminal writes to it within a quarter
  of a second (it took up to two), keeping your cursor.
- **A file with unsaved edits follows the disk too.** It used to keep showing only
  your edits while an AI changed the file; now changes on other lines are merged in
  (one undo step), as `git merge` would. Where both changed the same lines, yours
  stay and Raido says so once: `:e!` (or `:reload!`) loads the file with `u` to get
  yours back, and `:w!` saves yours over it.
- The explorer shows files created, deleted or renamed outside it (in a terminal,
  by an AI pane, by another program) within half a second, with their Git marks,
  instead of waiting for `r` or a fold. It looks only at the folders on show.

### Changed

- **When an AI can't answer, the next one does.** The docs (`docs_ai`) and the
  review's explanations (`review_ai`) asked only the first AI installed of their
  list, so a Claude Code that was signed out or out of quota gave an error while
  Codex sat installed beside it. Now each installed AI on the list is asked in turn
  until one answers, and the answer says who couldn't and why. `"auto"` in a list
  adds only the AIs the list doesn't name, so `["claude:opus", "auto"]` doesn't ask
  Claude Code twice. Esc in the docs stops the question without asking the next.

- **The theme picker lists light and dark themes apart**, as VS Code does: the
  light themes under their heading first, then the dark ones, each by name. Your
  own and Omarchy themes join the group their background puts them in, and
  filtering keeps the heading of each group that still has a match.
- **AI agents start in auto mode**, without asking before each command or file
  edit: Claude Code with `--permission-mode auto`, Codex in its yolo mode
  (`--dangerously-bypass-approvals-and-sandbox`), Gemini CLI with `--yolo` and
  Antigravity with `--dangerously-skip-permissions`, new or reopened. OpenCode asks
  for nothing but `.env` files anyway. `ai_auto_mode = false` starts them in their
  own default modes, as before.
- The review explains new agent changes by itself once the agents stop, so the
  explanation is waiting when you open it; `s` still asks at once. It asks once per
  round of agent changes, sends only files with new changes, and costs nothing while
  you keep or undo. `review_ai_auto = false` goes back to asking only on `s`.
- The top bar no longer shows Raido's version; `raido --version` prints it.
- **The text size sticks** in macOS Terminal: the next Raido starts at the size you
  zoomed to with `Ctrl-b +` / `-`, or set with `:font size 12.5` (which now works
  there). Raido saves it as `font_size` itself, as the theme picker saves the theme;
  `Ctrl-b 0` returns to the default and forgets it.

## [0.2.0] - 2026-11-06

### Changed

- **Renamed from Odin to Raido.** The command is `raido`; settings move from
  `~/.config/odin` to `~/.config/raido` and project sessions from `.odin/` to
  `.raido/` on first start; the `ODIN*` variables in terminal panes are now `RAIDO*`.
- The default theme is Visual Studio Dark, and the default font JetBrains Mono Nerd
  Font at 14 pt.
- Terminal and AI panes draw output and your typing as soon as they arrive, batched
  into whole frames, so typing and scrolling feel smoother on every OS.
- The project's name stands out in the top bar, bold on a band of its own.
- Pane titles number the open panes 1 to N (docked ones included) instead of
  counting every pane ever opened.
- PgUp / PgDn scroll terminal and AI panes by half a page.
- The explorer always shows dotfiles.
- Your own `[ai.<name>]` launchers take keys 7–9, after the built-in ones.

### Added

- **Windows support**, with a one-step installer; an install script for macOS and
  Linux. Both put `raido` on `PATH`.
- **Sessions:** each project reopens as you left it: layout, open files with unsaved
  edits, terminal output, and each AI pane in its own conversation.
- **Git pane** (`Ctrl-b G`): branches, a colored commit graph and commit details;
  Enter shows a commit's diff in its language's colors; `e` or `:explain <commit>`
  has the AI pane's agent explain and review a commit.
- **Review of AI changes** (`Ctrl-b v`): every change AI agents made, to keep or undo
  piece by piece, in syntax colors, with a summary of what was added, changed and
  removed. `s` asks an AI to explain it, sending only what's new, saying what it
  didn't read and what it cost; `review_ai` picks the AI and model.
- **AI panes:** a chime when a prompt finishes (Windows' own sound on Windows);
  Claude Code connects to Raido as it does to VS Code; `Ctrl-b i` hands the agent
  your file and selection; closing an AI pane and opening its CLI again reopens the
  conversation.
- **Editor:** find and replace (`Ctrl-f` / `Ctrl-h`); completions while typing;
  per-language indenting and bracket closing; Git change marks in the gutter; a
  minimap; Option / Cmd arrows move by word and line like a Mac; counts before `dd`
  and `yy`; Go files get imports organized and formatted on save.
- **Explorer:** Git status on files and folders; collapse or expand all; search file
  names and text across the project (`Ctrl-f`); paging with PgUp / PgDn.
- **Panes:** dock a pane to hide it while it keeps running; move panes and flip
  splits; open a shell beside the active pane (`Ctrl-b C`); a maximized pane says how
  many others are hidden; the pane taking your keys is marked with Raido's dot.
- **Mouse:** scroll with the wheel and focus with a click; drag to select and copy
  within one pane; open links with a modifier-click or double-click; `Ctrl-b y`
  hands the mouse to the terminal.
- Copy from terminal and AI panes without the rest of the screen (`Ctrl-b Z`).
- Text size with `Ctrl-b +` / `-` / `0`; `:font` saves the terminal's font size and
  family.
- `raido FILE` in a terminal pane opens the file in the running Raido, Windows
  included.
- An Agent Skill so AI agents can answer how-to questions about Raido.
- Seven light themes.
- **OpenCode** in the AI launcher (`6`, `o` or `:ai opencode`). With Ollama
  installed it first lists your Ollama models to start OpenCode on, or leaves the
  model to OpenCode; `:ai opencode <model>` skips the list.

### Fixed

- The top bar showed `v0.1.0` whatever the version; it now shows the real one.
- Tab-indented code (Go) in the review showed replacement marks instead of spaces.
- Terminal panes reflow their text on resize, use the theme's default colors, and
  tiny panes no longer crash Raido.
- Focus moves only to panes on the arrow's side; panes resize with `Ctrl-b r` where
  Ctrl+arrows are taken.
- Editor: syntax colors stay while typing in long files; docstrings and block
  comments close as they open; suggestions stay inside the pane and out of strings,
  comments and finished words; pasting replaces the selection; problem underlines no
  longer dim code in Terminal.app.
- Keys are named the way each OS's keyboard names them.
- The explorer falls back to plain icons without a Nerd Font, and its hints fit the
  sidebar.
- Raido says how to install rust-analyzer when rustup lacks it.
- Windows: panes and language servers start in the project instead of
  `C:\Windows`, and CLIs installed with npm run through their `.cmd` shim.
- Windows: the review reads each file's earlier version correctly in checkouts
  with CRLF line endings, and the repository checks text out with LF everywhere, so
  the built-in README, skill and example config are the same on every platform.

## [0.1.0] - 2026-10-19

First version, as Odin: a terminal workspace with a modal editor (syntax colors,
language servers, Markdown preview), a file explorer, a Git sidebar, terminal panes
and a launcher for AI CLIs and local models.

[Unreleased]: https://github.com/yukselberkay/raido/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/yukselberkay/raido/releases/tag/v0.2.0
[0.1.0]: https://github.com/yukselberkay/raido/commit/e91e6f5

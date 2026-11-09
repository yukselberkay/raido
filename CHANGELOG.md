# Changelog

All notable changes to Raido are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Raido uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html): while it is 0.x, a minor
release (0.2 → 0.3) may change keys, settings or behavior, and a patch release
(0.2.0 → 0.2.1) only fixes things.

## [Unreleased]

### Added

- **Your own model server** (`[model_server]`): Ollama or vLLM on another machine
  while the agents and your code stay here. OpenCode lists and runs on its models,
  the Local LLM chats with an Ollama server, and `review_ai = "local"` explanations
  come from it. `tunnel` reaches it through SSH so it needn't be on the network, and
  `key_env` names the variable with its API key.

### Fixed

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
- The explorer shows files created, deleted or renamed outside it (in a terminal,
  by an AI pane, by another program) within half a second, with their Git marks,
  instead of waiting for `r` or a fold. It looks only at the folders on show.

### Changed

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

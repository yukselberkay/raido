# Verification record

Date: 2026-10-18. Host: Linux x86_64. Rust 1.99.1.

## Completed locally

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: passed.
- `cargo test --locked --all-targets`: 17 passed.
- `cargo build --release --locked --examples --bin raido`: built native optimized
  editor and runnable Rust example.
- `python3 scripts/pty_smoke.py target/release/raido`: passed against the real
  application and an OS pseudo-terminal. Exercises startup/rendering, UTF-8
  bracketed paste, atomic save, search, numeric jump, splitting, nested shell
  output, AI launcher, resizing, normal exit, alternate-screen and raw-mode cleanup.
- TUI tests render layouts at 160×45, 80×24, 45×12, and 20×8.
- Editor tests cover Unicode, undo/redo, CRLF, permissions, destination protection,
  externally modified files, binary rejection and wrapped search.
- PTY tests cover actual shell I/O, resize, control/application key encoding,
  bounded terminal-query replies, and rejection of host clipboard forwarding.
- File-tree test covers lazy expansion, sorting and symlink-loop avoidance.
- Grammar inventory test confirms Rust, C, C++, Python and Go are bundled.

## Screenshot provenance

The README screenshots (`raido.png`, `raido-c-tiles.png`, `raido-markdown.png`,
`raido-lsp.png`, `raido-theme-picker.png`, `raido-field-guide.png`,
`raido-dark-themes.png`) are renders of Raido's real UI in the Visual Studio Dark theme
(the sheet shows every bundled theme): the app's own draw code fills a ratatui test
buffer, which is converted to HTML in JetBrainsMono Nerd Font and captured with
headless Chromium at 2×. Shell panes are real PTYs (`bash --norc` with an `raido $`
prompt) running `cargo run --example beacon` and the `forge.c` build; the LSP image
is a live clangd session. No editor contents, terminal output or UI labels are
fabricated for the images.

The image shows the AI **launcher**, not an authenticated model response.

## Documentation and C tiling follow-up

- Expanded `README.md` with Linux/macOS prerequisites, build/install/update/
  uninstall instructions, a beginner walkthrough, detailed shortcut tables,
  configuration, troubleshooting, and steps to reproduce the C layout.
- Verified `cargo build --release --locked` and an isolated installation with
  `cargo install --path . --locked --root target/install-check`.
- The installed `target/install-check/bin/raido --version` reports `raido 0.1.0`.
- Compiled `examples/forge.c` with `cc -std=c11 -Wall -Wextra -Werror` and ran it
  inside Raido's terminal pane. The displayed five FNV-1a fingerprints are actual
  program output; the sample explicitly identifies FNV-1a as non-cryptographic.
- `raido-c-tiles.png` captures the installed editor in a 168×48 PTY. Actual keyboard
  input opens the C file, makes a side-by-side split with Ctrl-b %, opens the
  README on the right, and creates the lower-right shell with Ctrl-b c. The C file
  stays on the left, demonstrating vertical and horizontal dividing borders together.
- The screenshot is rendered from the captured ANSI stream using the same
  xterm.js/Playwright method above. Screen reconstruction checks the pane titles,
  documentation heading, Git section, and the program's completion text before
  capture. Raw ANSI substring matching is not used because screen-diff escape
  sequences can split visible text.
- Verified README local links and `git diff --check`. No editor implementation
  changes were needed for this documentation/capture follow-up.

## Ten-theme follow-up

- Bundled Visual Studio Dark+–inspired, Dracula, Nord, One Dark, Gruvbox Dark,
  Tokyo Night, Catppuccin Mocha, Solarized Dark, Monokai, and GitHub Dark palettes.
  Default Phosphor and legacy external themes are unchanged.
- `cargo test --locked --all-targets`: **23 passed**. New tests validate all ten
  palettes, reject malformed syntax/ANSI colors, check comment/string/number
  coloring across Rust/C/C++/Python/Go, render every theme, preserve dirty buffers
  and reset syntax caches on switching, map ANSI colors without overriding
  explicit RGB/256-color output, and answer terminal color queries by theme.
- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`,
  `cargo build --release --locked`, and the real-PTY smoke script all passed.
- `raido-dark-themes.png` is a comparison sheet of ten real terminal captures.
  One release process ran in a 144×36 PTY with C and Rust editor panes and an
  interactive shell running the Forge sample plus ANSI-color output. Actual
  keyboard commands selected each theme without reopening panes or restarting
  the shell. Captured ANSI streams were reconstructed in xterm.js and checked
  for both file titles, the program's completion message, and the correct active
  theme before Playwright captured the sheet. Only the external catalogue labels
  and framing were added; the editor screens were not fabricated.
- No additional Cargo dependencies, network theme fetching, or AI requests were
  introduced. These palettes are adaptations, not pixel-identical upstream ports.

## Not claimed as verified

- macOS runtime behavior: Linux/macOS CI is configured, but no macOS host was
  available for local validation and the workflow has not been published/run.
- Authenticated Codex or Claude model calls: no credentials were requested,
  installed, displayed, or charged. The adapter starts the installed CLI in a
  real PTY; users authenticate through their own CLI.
- Full terminal protocol compatibility, full Vim compatibility, large-file
  benchmarks and formal security audit are outside this initial release.

The repository is initialized locally. No remote repository is configured or
published.

## Visual Studio themes and Markdown preview follow-up

- `visual-studio-dark` now follows **Visual Studio 2022 Dark** (the IDE) rather
  than VS Code Dark+: `#dcdcdc` text, `#007acc` accent, `#2b91af` line numbers,
  `#57a64a` comments, `#d69d85` strings, `#d8a0df` control keywords, Campbell ANSI.
- `vscode-dark-modern` was later replaced by `vscode-dark-2026` (see below).
- Theme schema gained optional `line_number` and `syntax_colors.control`; existing
  themes load unchanged.
- New `src/markdown.rs` renders Markdown (pulldown-cmark 0.13) into themed,
  width-wrapped lines; `.md` files open rendered, with in-place and live split
  previews that keep source position.
- `cargo test --locked --all-targets`: **28 passed** (5 new Markdown tests: block
  rendering, wrapping/indent, source↔render anchors, HTML reduction, detection).
- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`
  and `cargo build --release --locked` passed. Rendering was inspected at 110–170
  columns against this README and a sample covering every construct.

## Dark 2026 and theme plugins follow-up

- Replaced `vscode-dark-modern` with `vscode-dark-2026`, taken from Microsoft's
  `extensions/theme-defaults/themes/2026-dark.json` (editor `#121314`, accent
  `#3994bc`). Token colors follow VS Code's resolution, including the more specific
  Dark+ rules the 2026 theme inherits.
- Themes are runtime plugins: user `.toml` or VS Code `.json` files in the themes
  directory override built-ins, `:themes` lists them, Tab completes `:theme`, and the
  active file reloads live. New `src/vscode.rs` (adds `serde_json`).
- Verified by importing Microsoft's real `2026-dark.json` (with its include chain)
  from a temporary config directory: UI colors equal the built-in TOML and the
  rendered editor is pixel-identical. Live reload and broken-file recovery checked.
- `cargo test --locked --all-targets`: **32 passed**; fmt, clippy `-D warnings`,
  release build and the PTY smoke test passed.

## Copy/paste and language servers follow-up

- **Copy and paste:** `v`/`V` and Shift+arrow selections; `y`/`d`/`p`/`P` and
  Ctrl-c/Ctrl-x/Ctrl-v share the system clipboard (`wl-copy`/`wl-paste`, `xclip`,
  `xsel`, `pbcopy`/`pbpaste`, or OSC 52). Without a selection, Ctrl-c/Ctrl-x act on the
  line. Terminal paste works in every mode. New `src/clipboard.rs`; tests never touch
  the real clipboard.
- **LSP:** new `src/lsp.rs` — diagnostics, hover (`K`), go-to-definition (`gd`,
  `Ctrl-o` back), completion (`Ctrl-Space`, auto after trigger characters), `:format`,
  `:lsp`. Full-document sync, UTF-32/UTF-16 position negotiation, percent-encoded URIs.
- Verified end to end against **clangd 23** on a C file in a directory with a space:
  the undeclared-identifier error, hover with docs, definition jump and return,
  struct-member completion after `p.`, and 10 formatting edits. A scripted Python
  server covers the protocol in `cargo test`. rust-analyzer, gopls and pyright were not
  installed on the test host; Raido reports them with install commands.
- `cargo test --locked --all-targets`: **42 passed**; fmt and clippy `-D warnings` clean.

## Persistent theme follow-up

- `:theme name` now saves the choice: it rewrites only the top-level `theme = ...`
  line of the user's `config.toml` (adding it, or creating the file, if absent),
  validates the result loads, and replaces the file atomically. Comments, other keys
  and `[tables]` are preserved. Tests never write the real config.

## Theme picker follow-up

- `:themes`, a bare `:theme`, or Ctrl-b t opens a picker listing built-in and user
  themes. Arrow keys preview each theme live, typing filters, Enter keeps and saves it,
  Esc restores the theme that was active. Documented at the top of the F1 guide.
- `cargo test --locked --all-targets`: **44 passed** (new picker test).

## Omarchy themes follow-up

- New `src/omarchy.rs`: Omarchy themes (`colors.toml`) from the user and system theme
  directories are listed as `omarchy-<name>`; `omarchy` follows the active desktop theme
  and live-reloads on `omarchy theme set` (the copied `current/theme/colors.toml` gets a
  fresh mtime on every switch).
- Verified on this machine: all 23 installed Omarchy themes plus `omarchy` (currently
  Hackerman) load and validate, including light themes; the picker previews them live.
- `cargo test --locked --all-targets`: **47 passed** (3 new Omarchy tests).

## Explorer live preview follow-up

- Up/Down (`j`/`k`) in the Files and Git sidebar sections show the selected file in the
  editor pane while the sidebar keeps focus. One preview buffer is reused; Enter or an
  edit keeps the file open. Browsing never creates panes or discards edits.
- `cargo test --locked --all-targets`: **48 passed** (new explorer preview test).

## Docked field guide follow-up

- F1 (or `:help`, Ctrl-b ?) toggles the field guide as a panel docked on the right; it
  no longer closes on the next key, so commands, editing and terminals keep working
  while it is open. Alt-↑/↓ and Alt-PgUp/PgDn scroll it; narrow panels wrap text.
- `cargo test --locked --all-targets`: **49 passed** (new test types a command and edits
  with the guide open).

## Small-screen guide, relative jumps, Gemini/Antigravity, AI placement

- Field guide: on screens under 140 columns the sidebar yields to the open guide; the
  guide takes at least half the width (all of it below 70 columns), keeps keys and
  wrapped descriptions side by side, and shows "▼ more · Alt-↑↓ · n%" when it scrolls.
  A test scrolls it to the end at 80×24, 60×20 and 120×30.
- Relative jumps: `15j`/`15k`, `15<Enter>`/`15+`/`15-`, `:+15`/`:-15`; typing a count with
  relative numbers on explains the options. `:15` and `15G` stay absolute.
- AI launcher: Gemini CLI (`gemini`, key 3) and Antigravity CLI (`agy`, key 4) join
  Codex and Claude; missing CLIs report their install command. `Ctrl-b A`, `:ai … below`
  and `ai_split = "below"` stack AI panes; the default stays side by side.
- `cargo test --locked --all-targets`: **52 passed**.

## Explorer icons and dotfiles

- New `src/icons.rs`: Nerd Font file-type icons with per-language colors (the glyphs
  Omarchy's tools use), keyed by file name, then extension; folders get open/closed and
  special-folder icons. `icons = false` restores the plain markers.
- Dotfiles show by default (`show_hidden = true`, `.` toggles); `.git`, `target` and
  `node_modules` stay hidden. Rendered with JetBrainsMono Nerd Font to confirm glyphs.
- `cargo test --locked --all-targets`: **53 passed**.

## Bring-your-own AI and local LLMs

- New `src/local_ai.rs`: `[ai.<name>]` config entries (program, args with `{model}`,
  model, env applied via `env(1)`) become AI launcher entries (keys 6–9) and
  `:ai <name> [model]`; they can override built-ins. Launcher key 5 / `:ai local [model]`
  runs `[ai.local]`, else detects Ollama (`ollama run`, first pulled model via a bounded
  `ollama list`) or LM Studio (`lms chat`), else explains how to configure one.
- Verified with launchers parsed from real TOML and run in PTY panes (env and model
  substitution). Ollama and LM Studio are not installed on the test host, so their
  detection is covered by unit tests rather than a live model.
- `cargo test --locked --all-targets`: **57 passed**.

## Windows support

- New `src/platform.rs` holds the OS differences: executable lookup with PATHEXT (so
  npm `.cmd` shims resolve), the default shell (PowerShell → Windows PowerShell →
  `cmd.exe`; `$SHELL -i` → `/bin/sh -i` on Unix) and the config base (`%APPDATA%` on
  Windows, `~/.config` elsewhere, `$XDG_CONFIG_HOME` everywhere).
- Terminals, LSP servers and AI CLIs launch resolved paths; launcher `env` is passed to
  the PTY directly (no `env(1)`). LSP URIs use `file:///C:/…` on Windows. The clipboard
  uses PowerShell `Set-Clipboard`/`Get-Clipboard` with UTF-8.
- Unix-only tests (permissions, symlinks, `/bin/sh`, Python fake servers) are gated.
- Verified here with `cargo clippy --target x86_64-pc-windows-gnu --all-targets
  -D warnings` (clean); no Windows machine or linker was available, so the new
  `windows-latest` CI job is the first real Windows build and test run.

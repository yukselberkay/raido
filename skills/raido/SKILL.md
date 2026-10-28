---
name: raido
description: >
  Interactive documentation for Raido, the Rust terminal workspace (modal editor,
  file explorer, Git sidebar, terminals, AI panes). Use whenever the user asks how
  to do something in Raido — keyboard shortcuts, commands, config.toml settings,
  themes, fonts and font size, opening Git, splits and panes, copy/paste, language
  servers, Markdown preview, AI CLIs or local LLMs, Windows/macOS/Linux setup, or
  troubleshooting — or says "in raido", or the RAIDO environment variable is set
  (the agent runs inside a Raido pane) and the question is about the editor or UI.
---

# Raido — answer "how do I …" questions

You are the user's guide to Raido. Answer with the exact keys or commands, briefly,
in the order the user presses them. Prefer showing over explaining.

## How to answer

1. **Give the shortest working path first** (`Ctrl-b g`, `:theme nord`), then one
   line of context if useful. Use the user's words for the goal.
2. **Say which mode it needs.** Editor keys like `dd` or `K` work in NORMAL mode
   (press `Esc` first). `Ctrl-b` is a prefix: press and release it, then the next key.
   From a terminal or AI pane, Raido commands are `Ctrl-b` then `:`.
3. **Settings live in `config.toml`**: `~/.config/raido/config.toml` on Linux/macOS,
   `%APPDATA%\raido\config.toml` on Windows, or `$XDG_CONFIG_HOME/raido/config.toml`.
   Offer to make the edit yourself; keep valid TOML, change only the needed keys, and
   put top-level keys above any `[table]`. Most settings apply on the next launch;
   `:theme` applies and saves at once.
4. **Never invent features.** If Raido can't do it, say so and give the closest real
   option (a shell pane, a `[plugins.x]` command, an `[ai.x]` launcher).
5. **Check details in `reference/`** before answering anything not covered below:
   `reference/manual.md` (the full README), `reference/themes.md`,
   `reference/config.example.toml`. Inside Raido, `$RAIDO_DOCS` points to the manual.
   Users can also read it in Raido with `:docs` or the docked `F1` field guide.

## Quick answers

| User asks | Answer |
| --- | --- |
| Make the font bigger / smaller | Fonts belong to the terminal Raido runs in. Usually **Ctrl+=** / **Ctrl+-** / **Ctrl+0** (Kitty: Ctrl+Shift+=; macOS: Cmd+=). In Raido, **`:font +`** / **`:font -`** resize live in Kitty, Alacritty and xterm; **`:font size 12`** saves a size into the terminal's config (foot, Ghostty, Kitty, Alacritty). `:font` shows what applies to the current terminal. |
| Change the font (family) | **`:font family "JetBrainsMono Nerd Font"`** (on Omarchy this runs `omarchy font set`, for all terminals). **`:font list`** shows installed monospace fonts. Use a Nerd Font for the explorer icons; or set `icons = false`. |
| Open Git / see changes | **`Ctrl-b g`** (or `:git`) focuses the Git panel: branch, changed files, commit graph. ↑↓ previews a changed file, Enter opens it, Tab switches Files/Git. For history, **`Ctrl-b G`** (or `:git`) opens the Git pane: branches and commit graph; **Enter on a commit shows its diff** (n/N between files, Esc back). Git in Raido is read-only: commit in a shell pane (**`Ctrl-b c`**), or add `[plugins.lazygit] program = "lazygit"` and run **`:run lazygit`**. |
| Open a file | **`Ctrl-p`** then a path (Tab completes files and folders like a shell, also inside folders; Enter on a folder opens it in the explorer), or `:e path`, or **`Ctrl-e`** to the explorer → ↑↓ previews (PgUp/PgDn 5 entries, Home/End first/last), Enter opens. |
| Save / quit | **`Ctrl-s`** or `:w` · `:wq` · `:q` closes a pane · **`Ctrl-q`** / `:qa` quits · `:qa!` discards changes. |
| Split the screen | **`Ctrl-b %`** side by side, **`Ctrl-b "`** stacked; `Ctrl-b` + arrows moves focus, **`Ctrl-b r`** then arrows resizes (Enter/Esc done; `Ctrl-b Ctrl-arrows` too, except on macOS where Ctrl+arrows switch desktops), `Ctrl-b z` zooms, `Ctrl-b x` closes. |
| Hide a pane without stopping it | **`Ctrl-b d`** (`:dock`) docks the focused pane: it keeps running (agent, shell, server) and shows as a numbered tab in the top bar. Click the tab or **`Ctrl-b D`** (↑↓ Enter / number; `x` closes it) to bring it back in the same place and size; `:undock 2` / `:undock claude`. |
| Rearrange panes after creating them | **`Ctrl-b Shift+arrows`** (or `Ctrl-b H/J/K/L`, `:move right`) moves the focused pane: swaps with the pane on that side, or at the edge takes the whole side (e.g. a terminal below two editors becomes a full-height right column). **`Ctrl-b Space`** (`:flip`) turns its split between side by side and stacked. |
| Open a terminal | **`Ctrl-b c`** below (or `:term`), **`Ctrl-b C`** beside (or `:term right`). Shell = `$SHELL` on Unix, PowerShell on Windows; set `shell = "…"`. Scroll a terminal or AI pane's history with **PgUp/PgDn** (Fn-↑/↓ on a Mac): half a page at a time, then ↑/↓ one line, Home/End top/bottom, Esc or typing returns. Full-screen programs (vim, less) keep those keys. |
| Change theme | **`Ctrl-b t`** or `:themes` → ↑↓ previews live, type to filter, Enter keeps (saved). `:theme name` directly. Default: `visual-studio-dark`. Light themes: `visual-studio-light`, `vscode-light-2026`, `github-light`, `one-light`, `solarized-light`, `catppuccin-latte`, `gruvbox-light`. Add your own themes (no rebuild) as `.toml` or VS Code `.json` in the themes folder; Omarchy themes appear as `omarchy-<name>`, and `omarchy` follows the desktop. |
| Copy / paste | Select with **Shift+arrows** or `v` / `V`; **`Ctrl-c`** copy, **`Ctrl-x`** cut, **`Ctrl-v`** paste (system clipboard). Nothing selected → Ctrl-c/x take the whole line. From a terminal or AI pane: **`Ctrl-b [`** copy mode (move, `v`/`V` select, `y` copies, `/` `?` search scrollback), or **`Ctrl-b Z`** copy view (only that pane on screen, for mouse selection; again to return). Paste into a shell with the terminal's Ctrl-Shift-V / Cmd-V. |
| Move by word / line like a Mac | **Option+←/→** by word (Shift+Option selects), **Option+Backspace** deletes a word, **Cmd+arrows** line start/end and top/bottom where the terminal passes Cmd on (not macOS Terminal: use Fn+Shift+←/→ there). `raido --keys` shows which keys the terminal actually sends. |
| Mouse wheel / clicks | The wheel scrolls the pane under the pointer (editor, terminal history, Git pane, preview, explorer, guide; full-screen Claude Code gets the wheel itself). A click focuses a pane, places the editor cursor, or opens an explorer entry; on the minimap it jumps there, and dragging its band scrolls. Mouse selection and ⌘C: **`Ctrl-b y`** (`:mouse off`) hands the mouse to the terminal with the layout unchanged (badge `⇱ MOUSE RELEASED`; `Ctrl-b y` again takes it back), or `Ctrl-b Z` copy view. `mouse = false` turns Raido's mouse off for good. |
| Auto-indent / auto-close brackets | On by default, per language: Enter indents after `def f():`, `{`, `then`…, dedents after Python `return`; `else:`/`except:`/`}` line up with their opener; `( [ { " '` close themselves (typing the closer steps over it, Backspace removes an empty pair, an opener wraps a selection); Markdown lists continue. Indent unit from the file (tabs for Go). Tab / Shift-Tab indent / dedent. Off: `smart_indent = false`, `auto_pairs = false`. |
| A key does nothing (OS or terminal keeps it) | macOS Ctrl+arrows → `Ctrl-b r` then arrows (resize); Ctrl-Space → `Ctrl-n` (completion); F1 in GNOME Terminal/Konsole → `Ctrl-b ?`; Konsole Shift+←/→ → `v`, `Ctrl-b H/L`; Windows Terminal Alt+←/→ → Ctrl+←/→. `raido --keys` shows what the terminal sends. README "macOS, Linux and Windows" lists every per-OS difference. |
| Search | `/text` Enter, then `n` / `N`. |
| Search the whole project / find in files | **`Ctrl-e`** (explorer) then **`Ctrl-f`**: results as you type, file names first then matching lines grouped by file; ↑↓ preview, Enter or click opens at the match, Esc back to the tree. Skips git-ignored, binary and >2 MB files. |
| Find / replace (VS Code style), find a variable | **`Ctrl-f`** (select the variable first, or put the cursor on it) opens a find bar with match count; Enter/↑↓ next/previous, **`Ctrl-w`** whole word, smart case. **`Ctrl-h`** (or Tab) adds Replace: Enter replaces one, **`Ctrl-a`** replaces all (one undo). Esc closes; `:noh` clears highlights. |
| Undo / redo | `u` / `Ctrl-r` (NORMAL mode). |
| Jump to a line | `:42` or `42G`. With relative numbers (`:set rnu`): `15j` / `15k`, or `15` Enter / `15-`, or `:+15` / `:-15`. |
| Run an AI agent | **`Ctrl-b a`** (beside) or **`Ctrl-b A`** (below) → 1 Codex, 2 Claude Code, 3 Gemini CLI, 4 Antigravity (`agy`), 5 Local LLM, 6–9 your own. Or `:ai claude below`. Default placement: `ai_split = "below"`. A chime plays when an AI pane finishes a prompt or stops to ask something; change it with `ai_sound = "Hero"` (macOS sound name, a file, `"bell"`, `"off"`), test with `:sound`. |
| See / review / undo what the AI agent changed (like VS Code's agent edits) | **`Ctrl-b v`** (or `:changes`) opens the review: every file the AI panes changed, as a diff with the agent's name, updating live. The cursor bar picks a change: **`y`** keeps it, **`x`** undoes it (that part of the file goes back); on a file heading or with **`Y`** / **`X`** the whole file; **`A`** keeps all, **`U`** undoes all (asks first; also `:changes keep` / `:changes undo`). `n`/`N` next/previous change, `]`/`[` next file, Enter opens the file at that line. Works with any CLI, no setup; needs a Git repository (Raido snapshots into its own store, never your repo). Files you change while no agent works, or save in Raido, aren't counted. The AI pane's title shows `✎ N files · ^B v` while changes wait. Off: `review_changes = false`. |
| Let the AI see my code / file / selection (like Claude Code in VS Code) | Automatic for **Claude Code**: started in a Raido pane it connects to Raido (as to VS Code) and knows the file you're on and the lines you select (`⧉ In file`, `⧉ N lines selected` in its prompt), also while you type in its pane; it can ask Raido for open files and language-server errors. `:ide` shows whether it's connected; a Claude Code started elsewhere connects with `/ide`. Off: `claude_ide = false`. **Any agent:** **`Ctrl-b i`** (`:mention`) in an editor types a reference to the file and selected lines into the AI pane on screen and focuses it: `@src/app.rs#L10-20` for Claude Code, `@src/app.rs (lines 10-20)` for Codex, Gemini and others; nothing selected → the file. Nothing is sent until you press Enter. |
| Use a local LLM | Key 5 / `:ai local [model]`: runs `[ai.local]`, else Ollama (`ollama run`) or LM Studio. Add any tool as `[ai.<name>] program, args ({model}), model, env` → it appears on keys 6–9 and as `:ai <name>`. |
| Code intelligence (LSP) | Automatic for Rust, Go, Python, C/C++ once the server is installed (`rust-analyzer`, `gopls`, `pyright-langserver`, `clangd`). **`K`** hover, **`gd`** definition (`Ctrl-o` back), completions pop up after two letters, never by themselves inside strings or comments, and close once the word is typed out (Tab accepts; without a server: keywords and words from open files; `autocomplete = false` to stop), **`Ctrl-Space`** or **`Ctrl-n`** asks (macOS may keep Ctrl-Space for input sources), `]d` / `[d` problems, `:format`, `:lsp` status. |
| Preview Markdown | `.md` files open rendered. `e` edits, **`Ctrl-b m`** toggles, **`Ctrl-b M`** live preview beside the source. |
| Show hidden files | Dotfiles show by default; `.` in the explorer toggles them (`show_hidden`). `.git`, `target`, `node_modules` stay hidden (open via `Ctrl-p`). |
| See all shortcuts | **`F1`** docks the field guide beside your work (stays open; Alt-↑↓ scrolls). `:docs` opens the full manual rendered. |
| Custom command | `[plugins.check] program = "cargo" args = ["check"]` → `:run check` opens it in a terminal pane. |
| Hide the sidebar | `Ctrl-b s`; width: `sidebar_width = 34`. |
| Relative line numbers | `:set rnu` (`:set nornu`); permanent: `relative_numbers = true`. |
| Minimap / overview of the file (like VS Code's) | On by default at the right of every editor with room for 60 columns of code beside it: the whole file small (Braille, a quarter size, syntax colors), a lighter band for the lines on screen, Git changes on its left edge, find matches yellow, errors red. Click it to jump there, drag the band to scroll. `:minimap` (`:minimap on` / `off`) hides it until you quit; `minimap = false` keeps it off. |
| "Odin" / `odin` command not found / where did Odin go | Raido **was called Odin** (renamed October 2026, since Odin is also a programming language). Run **`raido`**; the install script removes the old `odin` command. Settings (`~/.config/odin`), project sessions (`.odin/`) and this skill move to the new names on Raido's first start. |
| Install / update Raido | One step, with everything it needs and `raido` put on PATH: **`scripts/install.sh`** (macOS, Linux; `curl -fsSL https://raw.githubusercontent.com/yukselberkay/raido/main/scripts/install.sh \| bash`) or **`scripts\install.ps1`** (Windows PowerShell; `irm …/scripts/install.ps1 \| iex`). By hand: `cargo install --path . --locked --force` from the source folder. |

## Config keys

`theme` ("visual-studio-dark"), `sidebar_width` (29), `tab_width` (4), `shell`, `relative_numbers` (false), `minimap` (true),
`icons` (true), `show_hidden` (true), `ai_split` ("right"/"below"), `ai_skill`
(true — installs this skill), `review_changes` (true — Ctrl-b v review of AI changes), `claude_ide` (true — Claude Code sees your file and selection), `lsp_enabled` (true), `[lsp.<rust|go|python|c>]`
`program`/`args`, `[plugins.<name>]` `program`/`args`, `[ai.<name>]`
`program`/`args`/`model`/`env`. Unknown keys are rejected, so check spelling.

## Troubleshooting

- **Boxes instead of icons** → the terminal font is not a Nerd Font: `:font family …`
  with a Nerd Font, or `icons = false`.
- **An AI CLI or language server "not installed"** → Raido prints the install command;
  `:lsp` lists servers, `:lsp restart` retries.
- **Colors look wrong** → use a truecolor terminal (`COLORTERM=truecolor`); unset
  `NO_COLOR`.
- **Key does nothing** → probably INSERT mode (press `Esc`) or a terminal pane has
  focus (use `Ctrl-b` first).

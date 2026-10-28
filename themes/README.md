# Raido dark themes

Eleven coordinated, built-in dark palettes, plus any theme you drop into your
themes directory (Raido TOML or VS Code JSON — no rebuild), for editor panes, the sidebar, Git view,
AI launcher, syntax highlighting, and embedded terminals. Visual Studio Dark is
Raido's default; Raido's original Phosphor and Amber, and the external Ice example,
remain available.

[Compare the themes in the live UI preview](../docs/raido-dark-themes.png).
The preview shows actual Raido terminal output with C, Rust, and a running shell.

## Select a theme

Build/install the updated Raido from the repository root:

```sh
cargo install --path . --locked --force
raido .
```

Press **Esc** in an editor, type `:theme visual-studio-dark`, and press **Enter**.
In an embedded terminal or AI pane, use **Ctrl-b**, then **:** to enter Raido's
command prompt; type `theme visual-studio-dark` without a leading colon.
Switching themes does not restart shells, close panes, or modify buffers.

The choice persists: `:theme` saves `theme = "visual-studio-dark"` as the top-level
line of `~/.config/raido/config.toml` (or `$XDG_CONFIG_HOME/raido/config.toml`),
creating the file if needed and changing nothing else in it.

## Catalogue and inspiration

These are Raido-specific adaptations of familiar palette families, not official
ports or pixel-identical reproductions. Syntax uses TextMate scopes, not an LSP's
semantic tokens. Some muted/comment colors are lightened for terminal readability.
Names identify their inspiration; Raido is not affiliated with the original projects.

| Built-in name / editable source | Inspiration | Background | Accent |
| --- | --- | --- | --- |
| [visual-studio-dark](visual-studio-dark.toml) | [Visual Studio 2022](https://visualstudio.microsoft.com/) Dark (the IDE: C#/C++ editor colors, Campbell terminal) | `#1e1e1e` | `#007acc` |
| [vscode-dark-2026](vscode-dark-2026.toml) | [Visual Studio Code Dark 2026](https://github.com/microsoft/vscode/blob/main/extensions/theme-defaults/themes/2026-dark.json) | `#121314` | `#3994bc` |
| [dracula](dracula.toml) | [Dracula](https://draculatheme.com/contribute) | `#282a36` | `#bd93f9` |
| [nord](nord.toml) | [Nord](https://www.nordtheme.com/docs/colors-and-palettes) | `#2e3440` | `#88c0d0` |
| [one-dark](one-dark.toml) | [Atom One Dark](https://github.com/atom/one-dark-syntax) | `#282c34` | `#61afef` |
| [gruvbox-dark](gruvbox-dark.toml) | [Gruvbox](https://github.com/morhetz/gruvbox) | `#282828` | `#b8bb26` |
| [tokyo-night](tokyo-night.toml) | [Tokyo Night](https://github.com/folke/tokyonight.nvim) | `#1a1b26` | `#7aa2f7` |
| [catppuccin-mocha](catppuccin-mocha.toml) | [Catppuccin](https://github.com/catppuccin/catppuccin) | `#1e1e2e` | `#cba6f7` |
| [solarized-dark](solarized-dark.toml) | [Solarized](https://ethanschoonover.com/solarized/) | `#002b36` | `#2aa198` |
| [monokai](monokai.toml) | [Monokai](https://monokai.pro/) | `#272822` | `#a6e22e` |
| [github-dark](github-dark.toml) | [GitHub themes](https://github.com/primer/github-vscode-theme) | `#0d1117` | `#58a6ff` |

## Customize safely

Copy a palette under a **new** name, for example:

```sh
mkdir -p "$HOME/.config/raido/themes"
cp -i themes/dracula.toml "$HOME/.config/raido/themes/my-night.toml"
```

Edit that file, then run `:theme my-night`. Use your XDG directory instead if set.
While a theme file is active, saving it re-themes Raido within two seconds.
A user file with a built-in name (e.g. `dracula.toml`) **overrides** the bundled
theme. Files in the current project are never automatically loaded.

### Omarchy themes

On Omarchy, Raido reads each theme's `colors.toml` from `~/.config/omarchy/themes/`
(preferred) and the system install (`$OMARCHY_PATH/themes`, `/usr/share/omarchy/themes`).
They are listed as `omarchy-<name>`; `omarchy` tracks the active desktop theme
(`~/.local/state/omarchy/current/theme`) and reloads when it changes. Mapping:
`background`, `foreground`, `accent`, `selection` as named; `muted` → borders;
`dark_foreground` → comments, line numbers and dim UI text; `magenta` keywords,
`blue` functions, `cyan` types, `green` strings, `orange` numbers/constants; the
named and `bright_*` colors fill the terminal's 16 ANSI colors.

### VS Code themes

Any VS Code color theme works as a plugin: save its `.json` as
`~/.config/raido/themes/<name>.json` and run `:theme <name>`. Comments and trailing
commas are accepted. Raido reads `editor.background`/`foreground`, `focusBorder`
(accent), `textLink.foreground`, `descriptionForeground`, borders, the line
highlight, `editorLineNumber.foreground`, `terminal.ansi*` (VS Code defaults fill
gaps) and resolves `tokenColors` with TextMate specificity for comments, keywords,
control flow, functions, types, strings, numbers, constants, operators and variables.
Semantic-token colors and descendant selectors (`source.rust keyword`) are ignored.
If a theme `"include"`s a parent file, put the parent next to it.

### TOML schema

All colors are exactly `#RRGGBB`; malformed colors, unknown fields, incomplete
syntax palettes, and ANSI arrays other than 16 entries are rejected.
Theme names allow only ASCII letters, digits, hyphens, and underscores.

- Top-level `background`, `foreground`, `muted`, `accent`, `secondary`, `warning`,
  `border`, `selection`: UI colors. Omitted fields inherit Phosphor defaults.
- Top-level `syntax`: legacy Syntect theme name, default `base16-ocean.dark`.
  Used only when `syntax_colors` is absent. An unknown Syntect name falls back to
  `base16-ocean.dark`, preserving previous behavior.
- Optional top-level `ansi`: 16 color strings, normal black/red/green/yellow/blue/
  magenta/cyan/white (0–7), then the same bright colors (8–15). Put this **before**
  `[syntax_colors]`, since TOML keys after a table heading belong to that table.
- Optional top-level `line_number`: editor gutter color; defaults to `muted`.
- Optional `[syntax_colors]`: all nine fields are required when present:
  `comment`, `keyword`, `function`, `type`, `string`, `number`, `constant`,
  `operator`, `variable`. An optional tenth, `control`, colors control-flow
  keywords (`if`, `while`, `return`) and defaults to `keyword`. This overrides `syntax` and uses the UI foreground for
  unclassified text. Markdown headings/diffs and the rendered Markdown preview
  also receive coordinated colors.

No credentials, scripts, commands, or executable plugins belong in theme files.

### Terminal behavior

Default foreground/background and ANSI indices 0–15 use the active theme, even
in already-open panes. OSC 10/11 foreground/background queries report the current
theme. Explicit RGB output and indices 16–255 retain the application's chosen
colors. A CLI with its own hard-coded theme may therefore look different; select
that CLI's own matching theme if available. An application that caches color
queries may need a redraw or restart after a theme change.

Legacy themes without `ansi` retain the host terminal's indexed-color behavior.
Use a truecolor-capable terminal with `COLORTERM=truecolor` for the intended look.
If `NO_COLOR` is inherited, run `env -u NO_COLOR raido .` to opt into colors for
that launch. No extra runtime packages or model/API calls are needed.

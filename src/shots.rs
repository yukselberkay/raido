//! The README screenshots, rendered from Raido's real UI. Each scene drives the app
//! with keys in a fresh clone of this repository (real shells, real clangd), draws
//! it into a ratatui test buffer like any frame, and writes that frame as HTML in
//! JetBrainsMono Nerd Font. Nothing on screen is made up for the picture.
//!
//! `scripts/screenshots.sh` runs these (they're ignored in normal test runs) and
//! photographs the pages with headless Chrome at 2×.
use crate::{
    app::{App, Pane},
    config::{self, Theme},
    layout::{Axis, Tile},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, style::Color, style::Modifier};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

/// Cell size in CSS pixels: JetBrains Mono is 0.6 em wide.
const FONT: f32 = 16.0;

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/shots")
}

/// A scratch home for Raido's config, so scenes never read or write yours, and a
/// fresh clone of this repository named `raido`: clean, with its history.
fn setup() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
    std::env::set_var("PS1", "raido $ ");
    std::env::set_var("BASH_SILENCE_DEPRECATION_WARNING", "1");
    let root = dir.path().join("raido");
    let ok = Command::new("git")
        .args(["clone", "--quiet", env!("CARGO_MANIFEST_DIR")])
        .arg(&root)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git clone");
    let root = root.canonicalize().unwrap();
    (dir, root)
}

fn app(root: &Path, theme: &str) -> App {
    let mut app = App::new(root.to_path_buf(), None, false).unwrap();
    app.command(&format!("theme {theme}")).unwrap();
    app
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.event(Event::Key(KeyEvent::new(code, modifiers)));
}

fn keys(app: &mut App, text: &str) {
    for c in text.chars() {
        let code = if c == '\n' {
            KeyCode::Enter
        } else {
            KeyCode::Char(c)
        };
        key(app, code, KeyModifiers::NONE);
    }
}

/// Ctrl-b, then `c`.
fn prefix(app: &mut App, c: char) {
    key(app, KeyCode::Char('b'), KeyModifiers::CONTROL);
    key(app, KeyCode::Char(c), KeyModifiers::NONE);
}

fn bash(app: &mut App, axis: Axis) {
    app.open_terminal(
        Some(("/bin/bash", vec!["--norc".into(), "--noprofile".into()])),
        Some(axis),
    )
    .unwrap();
}

/// Type a command line into the focused terminal.
fn run(app: &mut App, line: &str) {
    if let Some(Pane::Terminal(t)) = app.panes.get_mut(&app.focused) {
        t.paste(line).unwrap();
        t.send(b"\r").unwrap();
    }
}

fn frame(app: &mut App, width: u16, height: u16) -> Buffer {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn text(buffer: &Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Keep Raido running (and drawing at this size) until `wanted` is on screen.
fn until(app: &mut App, width: u16, height: u16, wanted: &[&str]) {
    let start = Instant::now();
    loop {
        app.tick();
        let screen = text(&frame(app, width, height));
        if wanted.iter().all(|w| screen.contains(w)) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "never showed {wanted:?}:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for what loads in the background: Git's status and history, and the
/// language server (`server`), which says "starting…" until it's ready.
fn ready(app: &mut App, width: u16, height: u16, server: &str) {
    let start = Instant::now();
    loop {
        app.tick();
        let screen = text(&frame(app, width, height));
        // Git has answered: the top bar names the branch, or (narrow, no room for
        // it there) the sidebar lists the commits.
        let git = !screen.contains("No commits yet")
            && (screen.contains(" main  ·") || screen.contains("HEAD"));
        let lsp = server.is_empty() || (screen.contains(server) && !screen.contains("starting"));
        if git && lsp {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "never ready:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Keep Raido running for a moment (servers answering, terminals settling).
fn settle(app: &mut App, width: u16, height: u16, time: Duration) {
    let start = Instant::now();
    while start.elapsed() < time {
        app.tick();
        frame(app, width, height);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn css(color: Color, fallback: &str, theme: &Theme) -> String {
    let index = match color {
        Color::Reset => return fallback.to_string(),
        Color::Rgb(r, g, b) => return format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => i,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
    };
    if let (Some(ansi), true) = (&theme.ansi, index < 16) {
        return css(config::color(&ansi[index as usize]), fallback, theme);
    }
    let rgb = match index {
        0..=15 => [
            0x000000, 0xcd0000, 0x00cd00, 0xcdcd00, 0x0000ee, 0xcd00cd, 0x00cdcd, 0xe5e5e5,
            0x7f7f7f, 0xff0000, 0x00ff00, 0xffff00, 0x5c5cff, 0xff00ff, 0x00ffff, 0xffffff,
        ][index as usize],
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v as u32 };
            (level(i / 36) << 16) | (level(i / 6 % 6) << 8) | level(i % 6)
        }
        _ => {
            let v = 8 + 10 * (index as u32 - 232);
            (v << 16) | (v << 8) | v
        }
    };
    format!("#{rgb:06x}")
}

/// One frame as absolutely placed cells, `font` pixels high.
fn grid(buffer: &Buffer, theme: &Theme, font: f32) -> String {
    let (cw, ch) = (font * 0.6, font * 1.25);
    let fg = css(config::color(&theme.foreground), "#ffffff", theme);
    let bg = css(config::color(&theme.background), "#000000", theme);
    let area = buffer.area;
    let mut html = format!(
        "<div class=g style=\"width:{}px;height:{}px;background:{bg};font-size:{font}px;line-height:{ch}px\">",
        area.width as f32 * cw,
        area.height as f32 * ch
    );
    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            let wide = unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
            let (mut fore, mut back) = (css(cell.fg, &fg, theme), css(cell.bg, &bg, theme));
            let modifier = cell.modifier;
            if modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fore, &mut back);
            }
            let mut style = format!(
                "left:{}px;top:{}px;width:{}px;color:{fore};background:{back}",
                x as f32 * cw,
                y as f32 * ch,
                wide as f32 * cw
            );
            if modifier.contains(Modifier::BOLD) {
                style.push_str(";font-weight:700");
            }
            if modifier.contains(Modifier::ITALIC) {
                style.push_str(";font-style:italic");
            }
            if modifier.contains(Modifier::UNDERLINED) {
                style.push_str(";text-decoration:underline");
            }
            if modifier.contains(Modifier::CROSSED_OUT) {
                style.push_str(";text-decoration:line-through");
            }
            if modifier.contains(Modifier::DIM) {
                style.push_str(";opacity:.6");
            }
            let symbol = symbol
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let _ = write!(html, "<span style=\"{style}\">{symbol}</span>");
            x += wide;
        }
    }
    html.push_str("</div>");
    html
}

fn font_faces() -> String {
    let fonts = directories::BaseDirs::new()
        .unwrap()
        .home_dir()
        .join("Library/Fonts");
    [
        ("Regular", "normal", "400"),
        ("Bold", "normal", "700"),
        ("Italic", "italic", "400"),
        ("BoldItalic", "italic", "700"),
    ]
    .iter()
    .map(|(file, style, weight)| {
        format!(
            "@font-face{{font-family:R;src:url('file://{}/JetBrainsMonoNerdFontMono-{file}.ttf');font-style:{style};font-weight:{weight}}}",
            fonts.display()
        )
    })
    .collect()
}

/// Write `body` as a page of `width` × `height` CSS pixels, for the screenshot script.
fn page(name: &str, body: &str, width: f32, height: f32, background: &str) {
    let html = format!(
        "<!doctype html><meta charset=utf-8><style>{}*{{margin:0;padding:0}}body{{background:{background};width:{width}px;height:{height}px;overflow:hidden;font-family:R,monospace;-webkit-font-smoothing:antialiased}}.g{{position:relative;overflow:hidden}}.g span{{position:absolute;white-space:pre;text-align:left;overflow:visible}}.label{{font:700 18px/26px R;padding:12px 14px 8px}}.sheet{{display:grid;grid-template-columns:repeat(3,auto);gap:0}}.cell{{padding:0 0 12px}}</style><body>{body}</body>",
        font_faces()
    );
    fs::create_dir_all(out_dir()).unwrap();
    fs::write(out_dir().join(format!("{name}.html")), html).unwrap();
    fs::write(
        out_dir().join(format!("{name}.size")),
        format!("{} {}\n", width.ceil(), height.ceil()),
    )
    .unwrap();
}

fn shot(name: &str, app: &mut App, width: u16, height: u16) {
    let buffer = frame(app, width, height);
    let body = grid(&buffer, &app.theme, FONT);
    let background = css(config::color(&app.theme.background), "#000", &app.theme);
    page(
        name,
        &body,
        width as f32 * FONT * 0.6,
        height as f32 * FONT * 1.25,
        &background,
    );
}

/// Give the first split this share of its room.
fn share(app: &mut App, percent: u16) {
    if let Tile::Split { percent: p, .. } = &mut app.tiles {
        *p = percent;
    }
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_workspace() {
    let (_dir, root) = setup();
    let (w, h) = (160, 46);
    let beacon = root.join("target/beacon");
    fs::create_dir_all(root.join("target")).unwrap();
    let built = Command::new("rustc")
        .args(["-O", "--edition", "2021", "examples/beacon.rs", "-o"])
        .arg(&beacon)
        .current_dir(&root)
        .status()
        .unwrap()
        .success();
    assert!(built, "rustc beacon");
    let mut app = app(&root, "visual-studio-dark");
    app.command("e examples/beacon.rs").unwrap();
    let editor = app.focused;
    prefix(&mut app, 'a');
    bash(&mut app, Axis::Vertical);
    share(&mut app, 58);
    settle(&mut app, w, h, Duration::from_millis(800));
    run(&mut app, &format!("clear; {}", beacon.display()));
    until(&mut app, w, h, &["All systems green", "raido $"]);
    app.focused = editor;
    ready(&mut app, w, h, "rust-analyzer");
    app.message = "All systems in view.  Code · shell · AI · Git".into();
    shot("raido", &mut app, w, h);
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_c_tiles() {
    let (_dir, root) = setup();
    let (w, h) = (168, 46);
    let mut app = app(&root, "visual-studio-dark");
    app.command("e examples/forge.c").unwrap();
    let editor = app.focused;
    prefix(&mut app, '%');
    app.command("e README.md").unwrap();
    bash(&mut app, Axis::Vertical);
    settle(&mut app, w, h, Duration::from_millis(800));
    run(
        &mut app,
        "mkdir -p target && cc -std=c11 -Wall -Wextra -Werror examples/forge.c -o target/forge && clear && ./target/forge",
    );
    until(&mut app, w, h, &["fingerprints computed", "raido $"]);
    app.tree.reveal(&root.join("README.md"));
    app.focused = editor;
    ready(&mut app, w, h, "clangd");
    app.message = "Splits: C source · rendered README · live build".into();
    shot("raido-c-tiles", &mut app, w, h);
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_lsp() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
    let root = dir.path().join("raido-demo");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("main.c"),
        r#"#include <stdio.h>

/// Adds two numbers together.
static int add(int a, int b) { return a + b; }

/// A point on the plane.
struct point {
    int x;
    int y;
};

static void print_point(const struct point *p) {
    printf("(%d, %d)\n", p->x, p->y);
}

/// Taxicab distance between two points.
static int manhattan(struct point a, struct point b) {
    int dx = a.x > b.x ? a.x - b.x : b.x - a.x;
    int dy = a.y > b.y ? a.y - b.y : b.y - a.y;
    return add(dx, dy);
}

int main(void) {
    struct point p = {1, 2};
    struct point q = {4, 6};
    print_point(&p);
    printf("%d\n", manhattan(p, q));
    printf("%d\n", add(p.x, q.y));
    printf("%d\n", totl);
    return 0;
}
"#,
    )
    .unwrap();
    let root = root.canonicalize().unwrap();
    let (w, h) = (120, 34);
    let mut app = app(&root, "visual-studio-dark");
    app.sidebar_visible = false;
    app.command("e main.c").unwrap();
    until(&mut app, w, h, &["1E"]);
    app.command("28").unwrap();
    keys(&mut app, "020l");
    keys(&mut app, "K");
    until(&mut app, w, h, &["Adds two numbers together", "Parameters"]);
    app.message = "clangd · K hover · gd definition · ]d next problem · :format".into();
    shot("raido-lsp", &mut app, w, h);
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_markdown() {
    let (_dir, root) = setup();
    let (w, h) = (150, 42);
    let mut app = app(&root, "visual-studio-dark");
    app.sidebar_visible = false;
    app.command("e examples/markdown-tour.md").unwrap();
    keys(&mut app, "e");
    prefix(&mut app, 'M');
    let source = app
        .panes
        .iter()
        .find(|(_, p)| matches!(p, Pane::Editor(_)))
        .map(|(id, _)| *id)
        .unwrap();
    app.focused = source;
    app.command("95").unwrap();
    ready(&mut app, w, h, "");
    app.message = "Live preview: it follows your cursor as you edit".into();
    shot("raido-markdown", &mut app, w, h);
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_theme_picker() {
    let (_dir, root) = setup();
    let (w, h) = (130, 36);
    let mut app = app(&root, "visual-studio-dark");
    app.command("e examples/beacon.rs").unwrap();
    ready(&mut app, w, h, "rust-analyzer");
    prefix(&mut app, 't');
    keys(&mut app, "dark");
    for _ in 0..12 {
        let selected = app.picker.as_ref().and_then(|p| {
            let index = *p.shown.get(p.selected)?;
            Some(p.themes[index].0.clone())
        });
        if selected.as_deref() == Some("visual-studio-dark") {
            break;
        }
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
    }
    app.message = "Themes: ↑↓ preview live · type to filter · Enter keeps · Esc cancels".into();
    shot("raido-theme-picker", &mut app, w, h);
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_field_guide() {
    let (_dir, root) = setup();
    let (w, h) = (150, 40);
    let mut app = app(&root, "visual-studio-dark");
    app.command("e examples/beacon.rs").unwrap();
    ready(&mut app, w, h, "rust-analyzer");
    // F1 opens the guide focused; Esc goes back to work and leaves it open.
    key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
    key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
    app.message = "Field guide open beside your work · F1 to scroll it · F1 F1 closes".into();
    shot("raido-field-guide", &mut app, w, h);
}

#[test]
#[ignore = "renders the README screenshots; run scripts/screenshots.sh"]
fn shots_dark_themes() {
    let (_dir, root) = setup();
    let (w, h, font) = (104, 30, 9.6);
    let themes = [
        "visual-studio-dark",
        "vscode-dark-2026",
        "phosphor",
        "dracula",
        "nord",
        "one-dark",
        "gruvbox-dark",
        "tokyo-night",
        "catppuccin-mocha",
        "solarized-dark",
        "monokai",
        "github-dark",
    ];
    let mut app = app(&root, themes[0]);
    app.command("e examples/forge.c").unwrap();
    ready(&mut app, w, h, "clangd");
    let mut cells = String::new();
    for theme in themes {
        app.command(&format!("theme {theme}")).unwrap();
        app.message.clear();
        let buffer = frame(&mut app, w, h);
        let accent = css(config::color(&app.theme.accent), "#fff", &app.theme);
        let background = css(config::color(&app.theme.background), "#000", &app.theme);
        let _ = write!(
            cells,
            "<div class=cell style=\"background:{background}\"><div class=label style=\"color:{accent}\">{theme}</div>{}</div>",
            grid(&buffer, &app.theme, font)
        );
    }
    let (cw, ch) = (w as f32 * font * 0.6, h as f32 * font * 1.25);
    page(
        "raido-dark-themes",
        &format!("<div class=sheet>{cells}</div>"),
        3.0 * cw,
        4.0 * (ch + 12.0 + 46.0),
        "#111",
    );
}

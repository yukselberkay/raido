use crate::{
    app::{App, InputKind, Mode, Pane},
    config::{color, Theme},
    editor::Buffer,
    gitgraph::{self, Focus as GitFocus, GitView, RefKind},
    lsp::Diagnostic,
    markdown,
    quickdiff::Change,
    review::{self, ChangesView},
    workspace::{printable, Mark},
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthChar;

fn panel<'a>(title: impl Into<Line<'a>>, theme: &Theme, active: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(if active {
            &theme.accent
        } else {
            &theme.border
        })))
        .title(title)
        .title_style(Style::default().fg(color(if active { &theme.accent } else { &theme.muted })))
}
/// A pane's (or the explorer's, the Git section's) frame. The one taking your keys
/// carries Raido's ◉ in the accent color before its title.
fn pane_panel<'a>(title: &str, theme: &Theme, active: bool) -> Block<'a> {
    let title = if active {
        Line::from(vec![
            Span::styled(
                " ◉",
                Style::default()
                    .fg(color(&theme.accent))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(title.to_string()),
        ])
    } else {
        Line::from(title.to_string())
    };
    panel(title, theme, active)
}
fn text(frame: &mut Frame, area: Rect, value: impl Into<String>, fg: Color) {
    if area.height > 0 && area.width > 0 {
        frame.render_widget(
            Paragraph::new(value.into()).style(Style::default().fg(fg)),
            area,
        );
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let theme = app.theme.clone();
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(color(&theme.background))
                .fg(color(&theme.foreground)),
        ),
        area,
    );
    if area.width < 45 || area.height < 12 {
        frame.render_widget(
            Paragraph::new(
                "RAIDO\nTerminal too small.\nResize to at least 45 × 12.\nCtrl-q to quit.",
            )
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    // Copy view: only the focused pane's text, so a mouse selection takes nothing else.
    let bare = app.copy_view.is_some();
    let root = app.root.file_name().unwrap_or_default().to_string_lossy();
    let title = Line::from(vec![
        Span::styled(
            " ◉ RAIDO ",
            Style::default()
                .fg(color(&theme.background))
                .bg(color(&theme.accent))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", printable(&root)),
            Style::default().fg(color(&theme.muted)),
        ),
    ]);
    let title_width = title.width() as u16;
    if !bare {
        frame.render_widget(
            Paragraph::new(title),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    app.dock_tabs.clear();
    let end = if area.width > 110 {
        area.right() - 45
    } else {
        area.right()
    };
    let mut x = area.x + title_width + 2;
    // Ctrl-b y: a reminder that the mouse is the terminal's until it's taken back.
    if !bare && app.mouse_released {
        let badges = [
            format!(
                " ⇱ MOUSE RELEASED · select, {} · Ctrl-b y takes it back ",
                crate::platform::COPY_KEY
            ),
            " ⇱ MOUSE RELEASED · Ctrl-b y ".into(),
            " ⇱ MOUSE ".into(),
        ];
        if let Some(badge) = badges.iter().find(|b| x + b.chars().count() as u16 <= end) {
            let width = badge.chars().count() as u16;
            frame.render_widget(
                Paragraph::new(badge.as_str()).style(
                    Style::default()
                        .fg(color(&theme.background))
                        .bg(color(&theme.warning))
                        .add_modifier(Modifier::BOLD),
                ),
                Rect::new(x, area.y, width, 1),
            );
            x += width + 1;
        }
    }
    if !bare && !app.docked.is_empty() {
        dock_tabs(frame, app, x, end, area.y);
    }
    if area.width > 110 && !bare {
        let right = format!(
            "{}  ·  {} panes  ·  v0.1.0 ",
            if app.git.branch.is_empty() {
                "local"
            } else {
                &app.git.branch
            },
            app.visible().len()
        );
        frame.render_widget(
            Paragraph::new(right)
                .style(Style::default().fg(color(&theme.muted)))
                .alignment(Alignment::Right),
            Rect::new(area.x + area.width - 45, area.y, 45, 1),
        );
    }
    let body = if bare { area } else { layout[1] };
    app.explorer_rows = Rect::default();
    app.git_area = Rect::default();
    app.help_area = Rect::default();
    // On narrower screens the open guide takes the sidebar's room.
    let sidebar_shown =
        !bare && app.sidebar_visible && area.width >= 70 && !(app.help && area.width < 140);
    let content = if sidebar_shown {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(app.config.sidebar_width),
                Constraint::Min(25),
            ])
            .split(body);
        sidebar(frame, app, columns[0]);
        columns[1]
    } else {
        body
    };
    // The guide docks on the right so the panes and command line stay usable.
    let content = if app.help && !bare {
        let width = if content.width >= 110 {
            (content.width * 45 / 100).clamp(56, 86)
        } else if content.width >= 70 {
            // Keep ~30 columns for your work and give the guide the rest, up to 56.
            (content.width / 2).max(content.width.saturating_sub(30).min(56))
        } else {
            // Too small to share: the guide covers the panes; F1 gives them back.
            content.width
        };
        let guide = Rect::new(content.right() - width, content.y, width, content.height);
        help(frame, app, guide);
        app.help_area = guide;
        Rect::new(content.x, content.y, content.width - width, content.height)
    } else {
        content
    };
    app.rectangles.clear();
    if app.zoom || bare {
        app.rectangles.push((app.focused, content));
    } else {
        app.tiles.layout(content, &app.docked, &mut app.rectangles);
    }
    let rectangles = app.rectangles.clone();
    let selection = app.selection();
    let customs = app.custom_agents();
    for (id, rect) in rectangles {
        let active = id == app.focused && !app.sidebar_focus;
        let title = match app.panes.get(&id) {
            Some(Pane::Editor(buffer)) => format!(
                " {} {}{} ",
                id + 1,
                printable(&app.buffers[*buffer].name()),
                if app.buffers[*buffer].dirty {
                    " ●"
                } else {
                    ""
                }
            ),
            Some(Pane::Preview(p)) => format!(
                " {} {} · preview{} ",
                id + 1,
                printable(&app.buffers[p.buffer].name()),
                if app.buffers[p.buffer].dirty {
                    " ●"
                } else {
                    ""
                }
            ),
            Some(Pane::Terminal(t)) => format!(
                " {} {}{}{} ",
                id + 1,
                t.title,
                if app.copy.as_ref().is_some_and(|c| c.pane == id) {
                    " · COPY"
                } else if t.exited.is_some() {
                    " · exited"
                } else {
                    " · live"
                },
                // An AI pane's changes waiting for review.
                match app.changes.by(&t.title) {
                    0 => String::new(),
                    n => format!(" · ✎ {} · ^B v", review::files(n)),
                }
            ),
            Some(Pane::Git(view)) => format!(
                " {} GIT · {} ",
                id + 1,
                match &view.only {
                    Some(name) => format!("{} only · a shows all", printable(name)),
                    None => "all branches".into(),
                }
            ),
            Some(Pane::Changes(_)) => {
                let (added, removed) = app.changes.totals();
                match app.changes.files.len() {
                    0 => format!(" {} CHANGES · nothing to review ", id + 1),
                    n => format!(
                        " {} CHANGES · {} +{added} −{removed} ",
                        id + 1,
                        review::files(n)
                    ),
                }
            }
            _ => format!(" {} AI / copilot ", id + 1),
        };
        let inner = if bare {
            rect
        } else {
            let block = pane_panel(&title, &theme, active);
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            // Maximized (Ctrl-b z): say so, and that other panes are still running.
            let hidden = app.visible().len().saturating_sub(1);
            if app.zoom && hidden > 0 {
                let badge = [
                    format!(
                        " ⛶ MAXIMIZED · {hidden} more {} · Ctrl-b z restores ",
                        if hidden == 1 { "pane" } else { "panes" }
                    ),
                    format!(
                        " ⛶ +{hidden} {} · Ctrl-b z ",
                        if hidden == 1 { "pane" } else { "panes" }
                    ),
                    format!(" ⛶ +{hidden} "),
                ];
                let room = rect
                    .width
                    .saturating_sub(title.chars().count() as u16 + 4 + if active { 2 } else { 0 });
                if let Some(badge) = badge.iter().find(|b| b.chars().count() as u16 <= room) {
                    let width = badge.chars().count() as u16;
                    frame.render_widget(
                        Paragraph::new(badge.clone()).style(
                            Style::default()
                                .fg(color(&theme.background))
                                .bg(color(&theme.accent))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Rect::new(rect.right() - 2 - width, rect.y, width, 1),
                    );
                }
            }
            inner
        };
        if inner.width < 2 || inner.height < 1 {
            continue;
        }
        match app.panes.get_mut(&id) {
            Some(Pane::Editor(buffer_id)) => {
                let buffer_id = *buffer_id;
                let buffer = &mut app.buffers[buffer_id];
                let footer = !bare && inner.height > 3;
                let height = inner.height - u16::from(footer);
                if buffer.row < buffer.scroll {
                    buffer.scroll = buffer.row;
                }
                if buffer.row >= buffer.scroll + height as usize {
                    buffer.scroll = buffer.row.saturating_sub(height.saturating_sub(1) as usize);
                }
                app.redraw |=
                    app.syntax
                        .prepare(buffer_id, buffer, buffer.scroll + height as usize);
                let lines = app.syntax.cache.get(&buffer_id).map(|c| &c.lines);
                let selection = if active { selection } else { None };
                let diagnostics = app
                    .lsp
                    .diagnostics
                    .get(&buffer_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let changes = app
                    .diffs
                    .get(&buffer_id)
                    .map(|d| d.marks.as_slice())
                    .unwrap_or_default();
                let cursor = editor(
                    frame,
                    buffer,
                    lines,
                    selection,
                    diagnostics,
                    changes,
                    Rect::new(inner.x, inner.y, inner.width, height),
                    &theme,
                    active && app.input.is_none(),
                    app.config.relative_numbers,
                    app.config.tab_width,
                    bare,
                );
                if active {
                    app.cursor_at = cursor;
                }
                if footer {
                    let area = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
                    // The diagnostic under the cursor line replaces the status details.
                    if let Some(d) = diagnostics.iter().find(|d| d.row == buffer.row) {
                        text(
                            frame,
                            area,
                            format!(" ● {}", printable(&d.message)),
                            severity_color(&theme, d.severity),
                        );
                    } else {
                        let language = app.syntax.language(buffer);
                        let errors = diagnostics.iter().filter(|d| d.severity == 1).count();
                        let warnings = diagnostics.iter().filter(|d| d.severity == 2).count();
                        let mut server = app
                            .lsp
                            .status(buffer_id, buffer)
                            .map(|s| format!("  ·  {s}"))
                            .unwrap_or_default();
                        if errors + warnings > 0 {
                            server.push_str(&format!("  ·  {errors}E {warnings}W"));
                        }
                        let detail = format!(
                            " {language}{server}  ·  UTF-8 / {}     {}:{}  {}%",
                            if buffer.crlf { "CRLF" } else { "LF" },
                            buffer.row + 1,
                            buffer.col + 1,
                            (buffer.row + 1) * 100 / buffer.lines()
                        );
                        text(frame, area, detail, color(&theme.muted));
                    }
                }
            }
            Some(Pane::Terminal(_)) if app.copy.as_ref().is_some_and(|c| c.pane == id) => {
                if let Some(mode) = app.copy.as_mut() {
                    copy_mode(frame, mode, inner, &theme);
                }
            }
            Some(Pane::Terminal(terminal)) => {
                let _ = terminal.resize(inner.height, inner.width);
                if let Ok(parser) = terminal.parser.lock() {
                    let screen = parser.screen();
                    // A pane smaller than the terminal's minimum shows the rows
                    // around the cursor, where the prompt is.
                    let (height, _) = screen.size();
                    let (cursor_row, _) = screen.cursor_position();
                    let top = if height > inner.height {
                        cursor_row
                            .saturating_sub(inner.height - 1)
                            .min(height - inner.height)
                    } else {
                        0
                    };
                    for row in 0..inner.height {
                        for col in 0..inner.width {
                            if let Some(cell) = screen.cell(row + top, col) {
                                if cell.is_wide_continuation() {
                                    continue;
                                }
                                let style = cell_style(cell, &theme);
                                let contents = cell.contents();
                                let symbol = if contents.is_empty() { " " } else { &contents };
                                let x = inner.x + col;
                                let y = inner.y + row;
                                frame.buffer_mut().set_stringn(
                                    x,
                                    y,
                                    symbol,
                                    (inner.width - col) as usize,
                                    style,
                                );
                            }
                        }
                    }
                    if active
                        && app.input.is_none()
                        && !screen.hide_cursor()
                        && screen.scrollback() == 0
                        && terminal.exited.is_none()
                    {
                        let (row, col) = screen.cursor_position();
                        if row >= top && row - top < inner.height && col < inner.width {
                            frame.set_cursor_position((inner.x + col, inner.y + row - top));
                        }
                    }
                    let back = screen.scrollback();
                    if back > 0 && !bare {
                        let hint = format!(" ↑ {back} lines · ↑↓ PgUp PgDn scroll · End returns ");
                        let width = (hint.chars().count() as u16).min(inner.width);
                        text(
                            frame,
                            Rect::new(inner.right() - width, inner.y, width, 1),
                            hint,
                            color(&theme.accent),
                        );
                    }
                }
                if let Some(exit) = terminal.exited.as_ref().filter(|_| !bare) {
                    text(
                        frame,
                        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                        format!("{exit} · Ctrl-b x closes pane"),
                        color(&theme.warning),
                    );
                }
            }
            Some(Pane::Preview(preview)) => {
                let buffer = &app.buffers[preview.buffer];
                let footer = !bare && inner.height > 3;
                let height = (inner.height - u16::from(footer)) as usize;
                // One column for the scrollbar, at least two of margin per side.
                let room = (inner.width as usize).saturating_sub(5);
                let width = room.clamp(1, markdown::MAX_WIDTH);
                let left = inner.x + 2 + ((room - width) / 2) as u16;
                let doc = app
                    .previews
                    .document(preview.buffer, buffer, width, &theme, &app.syntax);
                // Follow the source cursor (live split preview, or returning from the editor).
                if preview.synced != Some(buffer.row) {
                    preview.synced = Some(buffer.row);
                    preview.scroll = doc.line_for_source(buffer.row).saturating_sub(1);
                }
                let max = doc.lines.len().saturating_sub(height);
                preview.scroll = preview.scroll.min(max);
                preview.height = height;
                for (offset, line) in doc
                    .lines
                    .iter()
                    .skip(preview.scroll)
                    .take(height)
                    .enumerate()
                {
                    frame.render_widget(
                        Paragraph::new(line.clone()),
                        Rect::new(left, inner.y + offset as u16, width as u16, 1),
                    );
                }
                if doc.lines.is_empty() {
                    text(
                        frame,
                        Rect::new(left, inner.y, width as u16, 1),
                        "Empty document — press e to write",
                        color(&theme.muted),
                    );
                }
                if max > 0 && height > 1 && !bare {
                    let x = inner.right() - 1;
                    let thumb = (height * height / doc.lines.len()).max(1);
                    let at = preview.scroll * (height - thumb) / max;
                    for row in 0..height {
                        let on = (at..at + thumb).contains(&row);
                        text(
                            frame,
                            Rect::new(x, inner.y + row as u16, 1, 1),
                            if on { "┃" } else { "│" },
                            color(if on { &theme.accent } else { &theme.border }),
                        );
                    }
                }
                if footer {
                    let percent = (preview.scroll * 100).checked_div(max).unwrap_or(100);
                    let detail = format!(
                        " Markdown preview  ·  {} lines  ·  e edit  ·  [ ] headings     {percent}%",
                        buffer.lines()
                    );
                    text(
                        frame,
                        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                        detail,
                        color(&theme.muted),
                    );
                }
            }
            Some(Pane::Ai) => ai(frame, inner, &theme, &customs),
            Some(Pane::Git(view)) => git_view(frame, view, inner, &theme),
            Some(Pane::Changes(view)) => changes_view(frame, &app.changes, view, inner, &theme),
            None => {}
        }
    }
    if let Some(find) = &app.find {
        let pane = app.rectangles.iter().find(|(id, _)| *id == app.focused);
        if let Some(&(_, rect)) = pane {
            find_bar(frame, find, &app.buffers[find.buffer], rect, &theme);
        }
    }
    if app.input.is_none() && app.find.is_none() {
        if let Some(at) = app.cursor_at.filter(|_| app.buffer_id().is_some()) {
            let pane = app
                .rectangles
                .iter()
                .find(|(id, _)| *id == app.focused)
                .map(|&(_, r)| {
                    Rect::new(
                        r.x + 1,
                        r.y + 1,
                        r.width.saturating_sub(2),
                        r.height.saturating_sub(2),
                    )
                });
            if let (Some(pane), true) = (pane, app.menu.is_some()) {
                completion_menu(frame, app, at, pane);
            } else if let Some(hover) = &app.hover {
                // Docs (K) may use the whole screen, short of the top and bottom bars.
                let screen = Rect::new(
                    area.x,
                    area.y + 1,
                    area.width,
                    area.height.saturating_sub(3),
                );
                hover_popup(frame, app, hover, at, screen);
            }
        }
    }
    match app.copy_view {
        None => footer(frame, app, layout[2]),
        // A reminder for the first moments only, so it can't end up in a copy.
        Some(since) if since.elapsed() < std::time::Duration::from_secs(2) => {
            let hint = " Copy view · select with the mouse · Ctrl-b Z returns ";
            let width = (hint.chars().count() as u16).min(area.width);
            frame.render_widget(
                Paragraph::new(hint).style(
                    Style::default()
                        .fg(color(&theme.background))
                        .bg(color(&theme.accent)),
                ),
                Rect::new(area.right() - width, area.bottom() - 1, width, 1),
            );
        }
        Some(_) => {}
    }
    if app.picker.is_some() {
        theme_picker(frame, app, area);
    }
    if app.dock_list.is_some() {
        dock_list(frame, app, area);
    }
}

/// The dock in the top bar: a tab per docked pane, numbered like the dock list; a
/// click brings it back. Tabs that don't fit become "+N".
fn dock_tabs(frame: &mut Frame, app: &mut App, start: u16, end: u16, y: u16) {
    let theme = &app.theme;
    let label = " dock ";
    let mut x = start;
    if x + label.len() as u16 + 8 > end {
        return;
    }
    text(
        frame,
        Rect::new(x, y, label.len() as u16, 1),
        label,
        color(&theme.muted),
    );
    x += label.len() as u16;
    let count = app.docked.len();
    let mut tabs = Vec::new();
    for (n, docked) in app.docked.iter().enumerate() {
        let mut name = app.pane_name(*docked);
        if name.chars().count() > 18 {
            name = name.chars().take(17).collect::<String>() + "…";
        }
        let exited = matches!(app.panes.get(docked), Some(Pane::Terminal(t)) if t.exited.is_some());
        let tab = format!(
            " {} {name}{} ",
            n + 1,
            if exited { " · exited" } else { "" }
        );
        let width = tab.chars().count() as u16;
        let more = format!(" +{} ", count - n);
        // Leave room for "+N" unless this is the last tab.
        let reserve = if n + 1 < count { more.len() as u16 } else { 0 };
        if x + width + reserve > end {
            text(
                frame,
                Rect::new(x, y, more.len() as u16, 1),
                more,
                color(&theme.accent),
            );
            break;
        }
        frame.render_widget(
            Paragraph::new(tab).style(
                Style::default()
                    .fg(color(&theme.foreground))
                    .bg(color(&theme.selection)),
            ),
            Rect::new(x, y, width, 1),
        );
        tabs.push((*docked, Rect::new(x, y, width, 1)));
        x += width + 1;
    }
    app.dock_tabs = tabs;
}

/// Ctrl-b D: the docked panes; Enter (or a digit) brings one back.
fn dock_list(frame: &mut Frame, app: &App, area: Rect) {
    let Some(selected) = app.dock_list else {
        return;
    };
    let theme = &app.theme;
    let width = area.width.saturating_sub(6).min(56);
    let height = (app.docked.len() as u16 + 3).min(area.height.saturating_sub(4));
    if width < 20 || height < 4 {
        return;
    }
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = panel(" DOCK · Enter brings back · x closes · Esc ", theme, true)
        .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    for (n, docked) in app
        .docked
        .iter()
        .enumerate()
        .take(inner.height as usize - 1)
    {
        let state = match app.panes.get(docked) {
            Some(Pane::Terminal(t)) if t.exited.is_some() => "exited",
            Some(Pane::Terminal(_)) => "running",
            Some(Pane::Editor(b)) if app.buffers[*b].dirty => "unsaved",
            _ => "",
        };
        let row = format!(" {}  {:<28} {state}", n + 1, app.pane_name(*docked));
        let style = if n == selected {
            Style::default()
                .fg(color(&theme.background))
                .bg(color(&theme.accent))
        } else {
            Style::default().fg(color(&theme.foreground))
        };
        frame.render_widget(
            Paragraph::new(row).style(style),
            Rect::new(inner.x, inner.y + n as u16, inner.width, 1),
        );
    }
}

fn theme_picker(frame: &mut Frame, app: &App, area: Rect) {
    let Some(picker) = &app.picker else {
        return;
    };
    let theme = &app.theme;
    let width = area.width.saturating_sub(6).min(52);
    let rows = picker.shown.len().clamp(1, 16);
    let height = (rows as u16 + 4).min(area.height.saturating_sub(4));
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = panel(
        format!(
            " THEMES · {} · Enter keep · Esc cancel ",
            picker.shown.len()
        ),
        theme,
        true,
    )
    .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.height < 2 {
        return;
    }
    let filter = if picker.filter.is_empty() {
        Span::styled(" type to filter…", Style::default().fg(color(&theme.muted)))
    } else {
        Span::styled(
            format!(" filter: {}", printable(&picker.filter)),
            Style::default().fg(color(&theme.accent)),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(filter)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let visible = (inner.height - 1) as usize;
    let top = picker.selected.saturating_sub(visible.saturating_sub(1));
    let chosen = crate::markdown::mix(color(&theme.selection), color(&theme.accent), 0.45);
    if picker.shown.is_empty() {
        text(
            frame,
            Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(1), 1),
            "No theme matches",
            color(&theme.muted),
        );
    }
    for (offset, &index) in picker.shown.iter().skip(top).take(visible).enumerate() {
        let (name, origin) = &picker.themes[index];
        let selected = top + offset == picker.selected;
        let mark = if *name == picker.original { "●" } else { " " };
        let base = if selected {
            Style::default().bg(chosen)
        } else {
            Style::default()
        };
        let name_width = (inner.width as usize).saturating_sub(15);
        let line = Line::from(vec![
            Span::styled(format!(" {mark} "), base.fg(color(&theme.accent))),
            Span::styled(
                format!("{:<name_width$}", printable(name)),
                base.fg(color(&theme.foreground)),
            ),
            Span::styled(
                format!("{origin:>9} "),
                base.fg(color(if selected {
                    &theme.foreground
                } else {
                    &theme.muted
                })),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, inner.y + 1 + offset as u16, inner.width, 1),
        );
    }
}

/// A terminal cell's colors and attributes in the theme.
fn cell_style(cell: &vt100::Cell, theme: &Theme) -> Style {
    let mut style = Style::default()
        .fg(term_color(cell.fgcolor(), color(&theme.foreground), theme))
        .bg(term_color(cell.bgcolor(), color(&theme.background), theme));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// A terminal pane in copy mode: the frozen history, the selection and the cursor.
fn copy_mode(frame: &mut Frame, mode: &mut crate::copymode::CopyMode, area: Rect, theme: &Theme) {
    mode.height = area.height as usize;
    mode.follow();
    let selected = color(&theme.selection);
    for offset in 0..area.height {
        let row = mode.top + offset as usize;
        let Some((cells, _)) = mode.rows.get(row) else {
            break;
        };
        for (col, cell) in cells.iter().enumerate().take(area.width as usize) {
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = cell_style(cell, theme);
            if mode.selected(row, col) {
                style = style.bg(selected).fg(color(&theme.foreground));
            }
            if row == mode.row && col == mode.col {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let contents = cell.contents();
            let symbol = if contents.is_empty() { " " } else { &contents };
            frame.buffer_mut().set_stringn(
                area.x + col as u16,
                area.y + offset,
                symbol,
                area.width as usize - col,
                style,
            );
        }
    }
}

/// Quick diff gutter colors, VS Code's: green added, blue modified, red removed.
pub(crate) fn change_color(change: Change, theme: &Theme) -> Color {
    let ansi = |index: usize| theme.ansi.as_ref().map(|a| color(&a[index]));
    match change {
        Change::Added => ansi(10).unwrap_or_else(|| color(&theme.accent)),
        Change::Modified => ansi(12).unwrap_or_else(|| color(&theme.secondary)),
        Change::Deleted | Change::DeletedAbove => ansi(9).unwrap_or(Color::LightRed),
    }
}

/// Explorer colors for Git marks, from the theme's terminal palette when it has one.
pub(crate) fn mark_color(mark: Mark, theme: &Theme) -> Color {
    let ansi = |index: usize| theme.ansi.as_ref().map(|a| color(&a[index]));
    match mark {
        Mark::Ignored => color(&theme.muted),
        Mark::Untracked | Mark::Added | Mark::Renamed => {
            ansi(10).unwrap_or_else(|| color(&theme.accent))
        }
        Mark::Modified => color(&theme.warning),
        Mark::Deleted | Mark::Conflict => ansi(9).unwrap_or(Color::LightRed),
    }
}

/// The explorer's key hint: the longest wording that fits, never cut off.
fn explorer_hint(width: usize) -> &'static str {
    [
        " ^F search  ←→ fold  c/E all  r refresh",
        " ^F search  ←→ fold  c/E all  r ↻",
        " ^F search  ←→ fold  c/E all",
        " ^F search ←→ fold c/E all",
        " ^F search  ←→ fold",
        " ^F search",
    ]
    .into_iter()
    .find(|hint| hint.chars().count() <= width)
    .unwrap_or("")
}

/// The sidebar's Git section: branch, changed files, graph.
fn git_section(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let block = pane_panel(" GIT / TREE ", theme, app.sidebar_focus && app.git_focus);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    if let Some(error) = &app.git.error {
        lines.push(Line::styled(
            format!(" {error}"),
            Style::default().fg(color(&theme.muted)),
        ));
    } else {
        lines.push(Line::styled(
            format!(" ⎇ {}", app.git.branch),
            Style::default()
                .fg(color(&theme.accent))
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            format!(
                " {} changed · {}",
                app.git.changes.len(),
                if app.git.changes.is_empty() {
                    "clean"
                } else {
                    "working tree"
                }
            ),
            Style::default().fg(color(&theme.muted)),
        ));
        let max_changes = if app.git.graph.is_empty() {
            inner.height.saturating_sub(3) as usize
        } else {
            (inner.height / 3).max(1) as usize
        };
        let start = if app.git_focus {
            app.git_selected
                .saturating_sub(max_changes.saturating_sub(1))
        } else {
            0
        };
        for (index, (code, path)) in app
            .git
            .changes
            .iter()
            .enumerate()
            .skip(start)
            .take(max_changes)
        {
            let mut style = Style::default().fg(color(&theme.warning));
            if app.sidebar_focus && app.git_focus && index == app.git_selected {
                style = style.bg(color(&theme.selection));
            }
            lines.push(Line::styled(format!(" {code} {}", printable(path)), style));
        }
        if !app.git.graph.is_empty() {
            lines.push(Line::from(""));
        }
        for graph in &app.git.graph {
            lines.push(Line::styled(
                format!(" {graph}"),
                Style::default().fg(color(&theme.secondary)),
            ));
        }
        if app.git.graph.is_empty() && app.git.changes.is_empty() {
            lines.push(Line::styled(
                " No commits yet",
                Style::default().fg(color(&theme.muted)),
            ));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Find in files (Ctrl-f in the explorer), in place of the file tree: the query,
/// a count, then the results grouped by file.
fn explorer_search(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let focused = app.sidebar_focus && !app.git_focus;
    let block = pane_panel(" SEARCH ", &theme, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(search) = app.explorer_search.as_mut() else {
        return;
    };
    if inner.height < 4 || inner.width < 8 {
        return;
    }
    let width = inner.width as usize;
    // The query, its end kept in view.
    let query: Vec<char> = search.query.chars().collect();
    let room = width.saturating_sub(4);
    let shown: String = query[query.len().saturating_sub(room)..].iter().collect();
    let shown = printable(&shown);
    text(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        format!(" ⌕ {shown}"),
        color(&theme.accent),
    );
    if focused {
        let x = 3 + display_width(shown.chars(), 4) as u16;
        frame.set_cursor_position((inner.x + x.min(inner.width - 1), inner.y));
    }
    let status = if search.query.is_empty() {
        "Type to search file names and text".to_string()
    } else if search.running && search.hits.is_empty() {
        "Searching…".to_string()
    } else if search.hits.is_empty() {
        "No results".to_string()
    } else {
        format!(
            "{}{} results in {} files{}",
            if search.truncated { "First " } else { "" },
            search.hits.len(),
            search.files(),
            if search.running {
                " · searching…"
            } else {
                ""
            }
        )
    };
    text(
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        format!(" {status}"),
        color(
            if search.hits.is_empty() && !search.query.is_empty() && !search.running {
                &theme.warning
            } else {
                &theme.muted
            },
        ),
    );
    // Rows: files whose name matches, then each file's matching lines under it.
    let mut rows: Vec<(Option<usize>, Line)> = Vec::new();
    let mut current: Option<&std::path::Path> = None;
    for (index, hit) in search.hits.iter().enumerate() {
        let name = hit.path.file_name().unwrap_or_default().to_string_lossy();
        let folder = hit
            .path
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let file_line = |marker: &str| {
            Line::from(vec![
                Span::styled(
                    format!(" {marker}{}", printable(&name)),
                    Style::default()
                        .fg(color(&theme.foreground))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", printable(&folder)),
                    Style::default().fg(color(&theme.muted)),
                ),
            ])
        };
        match hit.line {
            None => rows.push((Some(index), file_line("▸ "))),
            Some(line) => {
                if current != Some(hit.path.as_path()) {
                    rows.push((None, file_line("")));
                    current = Some(hit.path.as_path());
                }
                rows.push((
                    Some(index),
                    Line::from(vec![
                        Span::styled(
                            format!("  {:>4} ", line + 1),
                            Style::default().fg(color(&theme.muted)),
                        ),
                        Span::styled(
                            printable(&hit.text),
                            Style::default().fg(color(&theme.foreground)),
                        ),
                    ]),
                ));
            }
        }
    }
    let list = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(3),
    );
    let height = list.height as usize;
    let selected_row = rows
        .iter()
        .position(|(hit, _)| *hit == Some(search.selected))
        .unwrap_or(0);
    if selected_row < search.offset {
        search.offset = selected_row.saturating_sub(1);
    }
    if height > 0 && selected_row >= search.offset + height {
        search.offset = selected_row + 1 - height;
    }
    app.search_lines.clear();
    for (n, (hit, line)) in rows
        .into_iter()
        .skip(search.offset)
        .take(height)
        .enumerate()
    {
        let selected = hit == Some(search.selected);
        let style = if selected {
            Style::default().bg(color(&theme.selection))
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(line).style(style),
            Rect::new(list.x, list.y + n as u16, list.width, 1),
        );
        app.search_lines.push(hit);
    }
    app.explorer_rows = list;
    text(
        frame,
        Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        if width >= 34 {
            " ↑↓ preview  Enter open  Esc tree"
        } else {
            " ↑↓ Enter Esc"
        },
        color(&theme.muted),
    );
}

fn sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let git_height = (area.height / 3).clamp(8, 16).min(area.height / 2);
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(git_height)])
        .split(area);
    app.git_area = regions[1];
    if app.explorer_search.is_some() {
        explorer_search(frame, app, regions[0]);
        git_section(frame, app, regions[1]);
        return;
    }
    let theme = &app.theme;
    let block = pane_panel(" EXPLORER ", theme, app.sidebar_focus && !app.git_focus);
    let inner = block.inner(regions[0]);
    frame.render_widget(block, regions[0]);
    let name = app.root.file_name().unwrap_or_default().to_string_lossy();
    text(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        format!(" {} /", printable(&name)),
        color(&theme.accent),
    );
    let visible = inner.height.saturating_sub(3) as usize;
    app.explorer_rows = Rect::new(inner.x, inner.y + 1, inner.width, visible as u16);
    if app.tree.selected < app.tree.offset {
        app.tree.offset = app.tree.selected;
    }
    if visible > 0 && app.tree.selected >= app.tree.offset + visible {
        app.tree.offset = app.tree.selected + 1 - visible;
    }
    for (offset, (index, entry)) in app
        .tree
        .entries
        .iter()
        .enumerate()
        .skip(app.tree.offset)
        .take(visible)
        .enumerate()
    {
        let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let marker = if entry.directory {
            if app.tree.expanded.contains(&entry.path) {
                "▾"
            } else {
                "▸"
            }
        } else if entry.symlink {
            "↗"
        } else {
            "·"
        };
        let mark = entry.path.strip_prefix(&app.root).ok().and_then(|p| {
            let relative = p.to_string_lossy();
            app.git
                .mark(&relative.replace(std::path::MAIN_SEPARATOR, "/"))
        });
        let mut style = Style::default().fg(match mark {
            Some(mark) => mark_color(mark, theme),
            None if entry.directory => color(&theme.secondary),
            None => color(&theme.foreground),
        });
        if index == app.tree.selected {
            style = style.bg(color(&theme.selection));
            if app.sidebar_focus && !app.git_focus {
                style = style.add_modifier(Modifier::BOLD);
                if mark.is_none() {
                    style = style.fg(color(&theme.accent));
                }
            }
        }
        // VS Code style: a letter beside changed files, a dot beside folders
        // holding changes, ignored paths dimmed.
        let badge = match mark {
            Some(Mark::Ignored) | None => "",
            Some(_) if entry.directory => "●",
            Some(mark) => mark.letter(),
        };
        let indent = "  ".repeat(entry.depth);
        let line = if app.config.icons != crate::config::Icons::Off {
            let open = app.tree.expanded.contains(&entry.path);
            let icon = if entry.directory {
                crate::icons::folder(open, &name)
            } else {
                crate::icons::file(&entry.path)
            };
            let (glyph, tint) = if app.config.icons == crate::config::Icons::Basic {
                crate::icons::basic(icon, entry.directory, open)
            } else {
                icon
            };
            let folder_tint = entry.directory.then(|| color(&theme.secondary));
            let icon_style = match tint.or(folder_tint) {
                Some(tint) => style.fg(tint),
                None => style,
            };
            let arrow = if entry.directory {
                marker
            } else if entry.symlink {
                "↗"
            } else {
                " "
            };
            Line::from(vec![
                Span::styled(format!(" {indent}{arrow} "), style),
                Span::styled(format!("{glyph} "), icon_style),
                Span::styled(printable(&name), style),
            ])
        } else {
            Line::styled(format!(" {indent}{marker} {}", printable(&name)), style)
        };
        let row = Rect::new(inner.x, inner.y + 1 + offset as u16, inner.width, 1);
        let badge_width = if badge.is_empty() { 0 } else { 2 }.min(row.width);
        frame.render_widget(
            Paragraph::new(line).style(style),
            Rect::new(row.x, row.y, row.width - badge_width, 1),
        );
        if badge_width > 0 {
            frame.render_widget(
                Paragraph::new(format!("{badge} ")).style(style),
                Rect::new(row.right() - badge_width, row.y, badge_width, 1),
            );
        }
    }
    if inner.height > 3 {
        text(
            frame,
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            explorer_hint(inner.width as usize),
            color(&theme.muted),
        );
    }
    git_section(frame, app, regions[1]);
}

#[allow(clippy::too_many_arguments)]
fn editor(
    frame: &mut Frame,
    buffer: &mut Buffer,
    highlight: Option<&Vec<Vec<(syntect::highlighting::Style, String)>>>,
    selection: Option<(usize, usize)>,
    diagnostics: &[Diagnostic],
    changes: &[Option<Change>],
    area: Rect,
    theme: &Theme,
    active: bool,
    relative: bool,
    tab_width: usize,
    bare: bool,
) -> Option<(u16, u16)> {
    let digits = buffer.lines().to_string().len().max(3);
    // Copy view leaves out line numbers and marks: only the text is on screen.
    let gutter = if bare { 0 } else { (digits + 2) as u16 };
    if area.width <= gutter {
        return None;
    }
    let width = area.width - gutter;
    let current = buffer.line(buffer.row);
    let cursor_x = display_width(current.chars().take(buffer.col), tab_width);
    if cursor_x < buffer.hscroll {
        buffer.hscroll = cursor_x;
    }
    if cursor_x >= buffer.hscroll + width as usize {
        buffer.hscroll = cursor_x + 1 - width as usize;
    }
    for offset in 0..area.height {
        let row = buffer.scroll + offset as usize;
        let y = area.y + offset;
        if row >= buffer.lines() {
            if !bare {
                text(
                    frame,
                    Rect::new(area.x, y, gutter, 1),
                    "  ~",
                    color(&theme.border),
                );
            }
            continue;
        }
        let current_row = row == buffer.row;
        if current_row {
            frame.render_widget(
                Block::default().style(Style::default().bg(color(&theme.selection))),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        let number = if relative && !current_row {
            row.abs_diff(buffer.row)
        } else {
            row + 1
        };
        let marks: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.row == row).collect();
        if !bare {
            text(
                frame,
                Rect::new(area.x, y, gutter, 1),
                format!("{number:>digits$} {}", if current_row { "▎" } else { "│" }),
                color(if current_row {
                    &theme.accent
                } else {
                    theme.line_number.as_ref().unwrap_or(&theme.muted)
                }),
            );
            // Quick diff: the separator becomes a colored bar on lines that differ from
            // Git, and a red marker where lines were removed.
            if let Some(Some(change)) = changes.get(row) {
                let glyph = match change {
                    Change::Added | Change::Modified => "▌",
                    Change::Deleted => "▁",
                    Change::DeletedAbove => "▔",
                };
                text(
                    frame,
                    Rect::new(area.x + digits as u16 + 1, y, 1, 1),
                    glyph,
                    change_color(*change, theme),
                );
            }
            if let Some(worst) = marks.iter().map(|d| d.severity).min() {
                text(
                    frame,
                    Rect::new(area.x + digits as u16 + 1, y, 1, 1),
                    "●",
                    severity_color(theme, worst),
                );
            }
        }
        let raw = buffer.line(row);
        let regions = highlight
            .and_then(|lines| lines.get(row))
            .filter(|line| !line.is_empty());
        let mut spans = Vec::new();
        let mut column = 0;
        if let Some(regions) = regions {
            for (style, value) in regions {
                let fg = style.foreground;
                let mut rs = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::BOLD)
                {
                    rs = rs.add_modifier(Modifier::BOLD);
                }
                if style
                    .font_style
                    .contains(syntect::highlighting::FontStyle::ITALIC)
                {
                    rs = rs.add_modifier(Modifier::ITALIC);
                }
                let clipped = clip(
                    value,
                    &mut column,
                    buffer.hscroll,
                    width as usize,
                    tab_width,
                );
                if !clipped.is_empty() {
                    spans.push(Span::styled(clipped, rs));
                }
            }
        } else {
            // Not highlighted (yet): the theme's text color, not the terminal's.
            spans.push(Span::styled(
                clip(&raw, &mut column, buffer.hscroll, width as usize, tab_width),
                Style::default().fg(color(&theme.foreground)),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x + gutter, y, width, 1),
        );
        for d in &marks {
            let x0 = display_width(raw.chars().take(d.start), tab_width);
            let x1 = display_width(raw.chars().take(d.end), tab_width).max(x0 + 1);
            let style = Style::default()
                .add_modifier(Modifier::UNDERLINED)
                .underline_color(severity_color(theme, d.severity));
            for col in x0.max(buffer.hscroll)..x1.min(buffer.hscroll + width as usize) {
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut((area.x + gutter + (col - buffer.hscroll) as u16, y))
                {
                    cell.set_style(style);
                }
            }
        }
        if let Some((start, end)) = selection {
            let line_start = buffer.text.line_to_char(row);
            let line_end = line_start + raw.chars().count();
            if start <= line_end && end > line_start {
                let from = start.max(line_start) - line_start;
                let to = end.min(line_end) - line_start;
                let x0 = display_width(raw.chars().take(from), tab_width);
                // A selection running past the line end also covers the newline cell.
                let x1 =
                    display_width(raw.chars().take(to), tab_width) + usize::from(end > line_end);
                let bg = crate::markdown::mix(color(&theme.selection), color(&theme.accent), 0.45);
                for col in x0.max(buffer.hscroll)..x1.min(buffer.hscroll + width as usize) {
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut((area.x + gutter + (col - buffer.hscroll) as u16, y))
                    {
                        cell.set_bg(bg);
                    }
                }
            }
        }
        // Search marks are rendered after syntax, but never alter the source.
        // The match under the cursor stands out, as in VS Code's find.
        if !buffer.search.is_empty() {
            let found = crate::editor::find_all(
                &raw,
                &buffer.search,
                !buffer.search_case,
                buffer.search_word,
            );
            for (byte, end) in found {
                let x = display_width(raw[..byte].chars(), tab_width);
                let len = display_width(raw[byte..end].chars(), tab_width);
                let here = current_row && raw[..byte].chars().count() == buffer.col;
                let bg = if here { &theme.accent } else { &theme.warning };
                let from = x.max(buffer.hscroll);
                let to = (x + len).min(buffer.hscroll + width as usize);
                for col in from..to {
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut((area.x + gutter + (col - buffer.hscroll) as u16, y))
                    {
                        cell.set_bg(color(bg)).set_fg(color(&theme.background));
                    }
                }
            }
        }
    }
    if active && buffer.row >= buffer.scroll && buffer.row < buffer.scroll + area.height as usize {
        let at = (
            area.x + gutter + (cursor_x.saturating_sub(buffer.hscroll) as u16).min(width - 1),
            area.y + (buffer.row - buffer.scroll) as u16,
        );
        frame.set_cursor_position(at);
        return Some(at);
    }
    None
}

fn severity_color(theme: &Theme, severity: u8) -> Color {
    match severity {
        1 => theme
            .ansi
            .as_ref()
            .map(|a| color(&a[9]))
            .unwrap_or_else(|| color(&theme.warning)),
        2 => color(&theme.warning),
        3 => color(&theme.secondary),
        _ => color(&theme.muted),
    }
}

/// Rows free below and above the cursor row inside `bounds`.
fn popup_room(at: (u16, u16), bounds: Rect) -> (u16, u16) {
    let below = bounds.bottom().saturating_sub(at.1 + 1);
    let above = at.1.saturating_sub(bounds.y);
    (below, above)
}

/// Place a popup of `width`×`height` below the cursor, or above it when there is
/// more room there, never outside `bounds`.
fn popup_rect(at: (u16, u16), width: u16, height: u16, bounds: Rect) -> Rect {
    let width = width.min(bounds.width);
    let x = at.0.max(bounds.x).min(bounds.right().saturating_sub(width));
    let (below, above) = popup_room(at, bounds);
    if below >= height || below >= above {
        Rect::new(x, at.1 + 1, width, height.min(below))
    } else {
        let height = height.min(above);
        Rect::new(x, at.1 - height, width, height)
    }
}

/// The completion list, inside the pane being typed in: it never covers another
/// pane, and one that pops up by itself stays small.
fn completion_menu(frame: &mut Frame, app: &App, at: (u16, u16), pane: Rect) {
    let Some(menu) = &app.menu else {
        return;
    };
    let theme = &app.theme;
    let (below, above) = popup_room(at, pane);
    let most = if menu.auto { 6 } else { 10 };
    let rows = menu
        .shown
        .len()
        .min(most)
        .min(usize::from(below.max(above).saturating_sub(2)));
    if rows == 0 || pane.width < 12 {
        return;
    }
    let top = menu.selected.saturating_sub(rows - 1);
    let visible = &menu.shown[top..top + rows];
    let mut label_width = visible
        .iter()
        .map(|&i| menu.items[i].label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 40);
    let mut detail_width = visible
        .iter()
        .map(|&i| menu.items[i].detail.chars().count())
        .max()
        .unwrap_or(0)
        .min(36);
    // Narrow pane: the details give way first, then the labels.
    let room = usize::from(pane.width).saturating_sub(5);
    if label_width + detail_width > room {
        detail_width = room.saturating_sub(label_width);
        label_width = label_width.min(room);
    }
    let width = (label_width + detail_width + 5) as u16;
    // Align the list with the start of the word being completed.
    let b = &app.buffers[menu.buffer];
    let shift = b.col.saturating_sub(menu.start) as u16;
    let rect = popup_rect(
        (at.0.saturating_sub(shift + 1), at.1),
        width,
        rows as u16 + 2,
        pane,
    );
    frame.render_widget(Clear, rect);
    let block = panel(
        format!(" {}/{} ", menu.selected + 1, menu.shown.len()),
        theme,
        true,
    )
    .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let chosen = crate::markdown::mix(color(&theme.selection), color(&theme.accent), 0.45);
    for (offset, &index) in visible.iter().enumerate() {
        let item = &menu.items[index];
        let selected = top + offset == menu.selected;
        let mut label: String = item.label.chars().take(label_width).collect();
        label = format!(" {label:<label_width$} ");
        let detail: String = item.detail.chars().take(detail_width).collect();
        let base = if selected {
            Style::default().bg(chosen)
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::styled(label, base.fg(color(&theme.foreground))),
            Span::styled(
                format!("{detail:<detail_width$} "),
                base.fg(color(&theme.muted)),
            ),
        ]);
        if offset < inner.height as usize {
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, inner.y + offset as u16, inner.width, 1),
            );
        }
    }
}

/// The find / replace bar in the top-right corner of the editor, like VS Code's.
fn find_bar(
    frame: &mut Frame,
    find: &crate::app::Find,
    buffer: &Buffer,
    pane: Rect,
    theme: &Theme,
) {
    let width = 58.min(pane.width.saturating_sub(2));
    let height = if find.replacing { 4 } else { 3 };
    if width < 24 || pane.height < height + 2 {
        return;
    }
    let rect = Rect::new(pane.right() - 1 - width, pane.y + 1, width, height);
    frame.render_widget(Clear, rect);
    let title = if find.replacing {
        " Find & replace "
    } else {
        " Find "
    };
    let block = panel(title, theme, true).style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let (at, total) = buffer.match_position();
    let status = match (find.query.is_empty(), total) {
        (true, _) => String::new(),
        (false, 0) => "No results".to_string(),
        (false, n) if at == 0 => format!("? of {n}"),
        (false, n) => format!("{at} of {n}"),
    };
    let flags = format!(
        "{}{}",
        if buffer.search_case { " Aa" } else { "" },
        if find.whole_word { " ab|" } else { "" }
    );
    let right = format!("{status}{flags} ");
    let right_width = right.chars().count() as u16;
    let label = 9;
    let field_width = inner.width.saturating_sub(label + right_width + 1) as usize;
    let mut cursor = None;
    let rows = [
        ("Find", &find.query, !find.in_replace),
        ("Replace", &find.replace, find.in_replace),
    ];
    for (row, (name, value, typing)) in rows.iter().take(inner.height as usize).enumerate() {
        let y = inner.y + row as u16;
        text(
            frame,
            Rect::new(inner.x, y, label, 1),
            format!(" {name}"),
            color(if *typing { &theme.accent } else { &theme.muted }),
        );
        // The end of a long value stays in view, where you type.
        let chars: Vec<char> = value.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(field_width.saturating_sub(1))..]
            .iter()
            .collect();
        let shown = printable(&shown);
        let mut style = Style::default().fg(color(&theme.foreground));
        if row == 0 && find.fresh {
            style = style.bg(color(&theme.selection));
        }
        frame.render_widget(
            Paragraph::new(shown.clone()).style(style),
            Rect::new(inner.x + label, y, field_width as u16, 1),
        );
        if *typing {
            let x = display_width(shown.chars(), 4).min(field_width) as u16;
            cursor = Some((inner.x + label + x, y));
        }
    }
    text(
        frame,
        Rect::new(inner.right() - right_width, inner.y, right_width, 1),
        right,
        color(if total == 0 && !find.query.is_empty() {
            &theme.warning
        } else {
            &theme.muted
        }),
    );
    if let Some(at) = cursor {
        frame.set_cursor_position(at);
    }
}

fn hover_popup(frame: &mut Frame, app: &App, hover: &str, at: (u16, u16), screen: Rect) {
    let theme = &app.theme;
    let width = 78.min(screen.width.saturating_sub(4)) as usize;
    let doc = crate::markdown::render(hover, width.saturating_sub(4), theme, &app.syntax);
    let used = doc
        .lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(10)
        .max(16);
    let height = doc.lines.len().clamp(1, 18);
    let rect = popup_rect((at.0, at.1), (used + 4) as u16, height as u16 + 2, screen);
    frame.render_widget(Clear, rect);
    let more = doc.lines.len() + 2 > rect.height as usize;
    let block = panel(if more { " more… " } else { "" }, theme, true)
        .style(Style::default().bg(color(&theme.background)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    for (offset, line) in doc.lines.iter().take(inner.height as usize).enumerate() {
        frame.render_widget(
            Paragraph::new(line.clone()),
            Rect::new(
                inner.x + 1,
                inner.y + offset as u16,
                inner.width.saturating_sub(2),
                1,
            ),
        );
    }
}

fn display_width(chars: impl Iterator<Item = char>, tab: usize) -> usize {
    chars.fold(0, |column, c| {
        column
            + if c == '\t' {
                tab - column % tab
            } else {
                UnicodeWidthChar::width(c).unwrap_or(0)
            }
    })
}

fn clip(value: &str, column: &mut usize, start: usize, width: usize, tab: usize) -> String {
    let mut out = String::new();
    for c in value.chars() {
        if c == '\n' || c == '\r' {
            continue;
        }
        let size = if c == '\t' {
            tab - *column % tab
        } else {
            UnicodeWidthChar::width(c).unwrap_or(0)
        };
        let end = *column + size;
        if c == '\t' {
            let visible = end.min(start + width).saturating_sub((*column).max(start));
            out.push_str(&" ".repeat(visible));
        } else if *column >= start && end <= start + width && !c.is_control() {
            out.push(c);
        } else if end > start && *column < start + width {
            out.push_str(&" ".repeat(end.min(start + width).saturating_sub((*column).max(start))));
        }
        *column = end;
    }
    out
}

fn term_color(value: vt100::Color, fallback: Color, theme: &Theme) -> Color {
    match value {
        vt100::Color::Default => fallback,
        vt100::Color::Idx(n @ 0..=15) if theme.ansi.is_some() => {
            color(&theme.ansi.as_ref().expect("palette checked")[n as usize])
        }
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn ai(frame: &mut Frame, area: Rect, theme: &Theme, customs: &[String]) {
    let fg = Style::default().fg(color(&theme.foreground));
    let muted = Style::default().fg(color(&theme.muted));
    let accent = Style::default()
        .fg(color(&theme.accent))
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(""),
        Line::styled("  YOUR TOOLS. YOUR KEYS.", accent),
        Line::from(""),
        Line::styled("  [1]  OpenAI Codex", fg),
        Line::styled("  [2]  Claude Code", fg),
        Line::styled("  [3]  Gemini CLI", fg),
        Line::styled("  [4]  Antigravity CLI (agy)", fg),
        Line::styled("  [5]  Local LLM (yours, or Ollama/LM Studio)", fg),
    ];
    // Your own [ai.<name>] launchers take keys 6–9.
    for (i, name) in customs.iter().take(4).enumerate() {
        lines.push(Line::styled(format!("  [{}]  {name}", i + 6), fg));
    }
    lines.extend([
        Line::from(""),
        Line::styled("  Native CLI · real terminal", muted),
        Line::styled("  Same project. Live file refresh.", muted),
        Line::from(""),
        Line::styled("  Sign in using the CLI's own flow.", muted),
        Line::styled("  Raido never stores API keys.", muted),
        Line::from(""),
        Line::styled(
            "  Focus: Ctrl-b → / o",
            Style::default().fg(color(&theme.secondary)),
        ),
        Line::styled(
            "  Launch: press a number · add your own: [ai.name]",
            Style::default().fg(color(&theme.secondary)),
        ),
    ]);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

/// Lane colors for the commit graph: the theme's own accents, then its ANSI palette.
fn lane_colors(theme: &Theme) -> Vec<Color> {
    let mut colors = vec![
        color(&theme.accent),
        color(&theme.secondary),
        color(&theme.warning),
    ];
    match &theme.ansi {
        Some(ansi) => colors.extend([13, 14, 9, 10, 12, 11].map(|i| color(&ansi[i]))),
        None => colors.extend([
            Color::Rgb(0xc6, 0x78, 0xdd),
            Color::Rgb(0x56, 0xb6, 0xc2),
            Color::Rgb(0xe0, 0x6c, 0x75),
            Color::Rgb(0x98, 0xc3, 0x79),
        ]),
    }
    colors
}

/// Keep `selected` inside a `height`-row window that starts at `scroll`.
fn follow(scroll: &mut usize, selected: usize, height: usize) {
    if selected < *scroll {
        *scroll = selected;
    } else if height > 0 && selected >= *scroll + height {
        *scroll = selected + 1 - height;
    }
}

/// `ahead 2, behind 1` → `↑2 ↓1`.
fn track(track: &str) -> String {
    track
        .split(", ")
        .filter(|t| !t.is_empty())
        .map(|t| {
            if let Some(n) = t.strip_prefix("ahead ") {
                format!("↑{n}")
            } else if let Some(n) = t.strip_prefix("behind ") {
                format!("↓{n}")
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fit `spans` into `width` columns, then right-align `right` when it still fits.
fn fit_line<'a>(spans: Vec<Span<'a>>, right: Span<'a>, width: usize, fill: Style) -> Line<'a> {
    let mut out = Vec::new();
    let mut used = 0;
    let right_width = display_width(right.content.chars(), 4);
    for span in spans {
        let w = display_width(span.content.chars(), 4);
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let mut column = 0;
        let room = width.saturating_sub(used + 1);
        let cut = clip(&span.content, &mut column, 0, room, 4);
        used += display_width(cut.chars(), 4) + 1;
        out.push(Span::styled(format!("{cut}…"), span.style));
        break;
    }
    if used + right_width + 2 <= width {
        out.push(Span::styled(" ".repeat(width - used - right_width), fill));
        out.push(right);
    } else if used < width {
        out.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(out)
}

/// A commit's diff in the Git pane: message, file summary, then each file's hunks
/// with old and new line numbers, added lines green and removed lines red.
fn commit_diff(frame: &mut Frame, diff: &mut gitgraph::Diff, area: Rect, theme: &Theme) {
    use gitgraph::Line as Kind;
    if area.height < 2 {
        return;
    }
    let fg = color(&theme.foreground);
    let muted = color(&theme.muted);
    let background = color(&theme.background);
    let added = change_color(Change::Added, theme);
    let removed = change_color(Change::Deleted, theme);
    let short: String = diff.hash.chars().take(9).collect();
    let (file, files) = diff.position();
    let title = if diff.loading {
        format!("── {short} · loading… ")
    } else {
        let place = match file {
            0 => format!("{files} file{}", if files == 1 { "" } else { "s" }),
            n => format!("file {n}/{files}"),
        };
        format!(
            "── {short} · {place} · +{} −{} · n N files  j k scroll  h l sideways  Esc back ",
            diff.added, diff.removed
        )
    };
    text(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        format!("{title}{}", "─".repeat(area.width as usize)),
        color(&theme.border),
    );
    let rows = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    diff.height = rows.height as usize;
    diff.scroll_by(0);
    // Line numbers when there is room for them and the code.
    let gutter = if rows.width >= 60 { 10 } else { 0 };
    let tint = |c: Color| crate::markdown::mix(background, c, 0.14);
    for (offset, line) in diff
        .lines
        .iter()
        .skip(diff.scroll)
        .take(rows.height as usize)
        .enumerate()
    {
        let y = rows.y + offset as u16;
        let (style, row_bg) = match line.kind {
            Kind::Added => (Style::default().fg(added), Some(tint(added))),
            Kind::Removed => (Style::default().fg(removed), Some(tint(removed))),
            Kind::Context => (Style::default().fg(fg), None),
            Kind::Hunk => (Style::default().fg(color(&theme.secondary)), None),
            Kind::File => (
                Style::default()
                    .fg(color(&theme.accent))
                    .add_modifier(Modifier::BOLD),
                Some(color(&theme.selection)),
            ),
            Kind::Meta | Kind::Stat => (Style::default().fg(muted), None),
            Kind::Message => (Style::default().fg(fg), None),
        };
        if let Some(bg) = row_bg {
            frame.render_widget(
                Block::default().style(Style::default().bg(bg)),
                Rect::new(rows.x, y, rows.width, 1),
            );
        }
        let style = row_bg.map_or(style, |bg| style.bg(bg));
        let number = |n: Option<usize>| n.map_or(String::new(), |n| n.to_string());
        let mut spans = Vec::new();
        // The message and summary aren't code: no line-number column for them.
        if gutter > 0 && !matches!(line.kind, Kind::Message | Kind::Stat) {
            spans.push(Span::styled(
                format!("{:>4} {:>4} ", number(line.old), number(line.new)),
                Style::default().fg(muted).bg(row_bg.unwrap_or(background)),
            ));
        }
        let body: String = match line.kind {
            Kind::File => format!(" ▌ {}", line.text),
            Kind::Message => format!(" {}", line.text),
            _ => line.text.chars().skip(diff.hscroll).collect(),
        };
        if line.kind == Kind::Stat {
            // " path | 12 ++--": the bar in the change colors.
            if let Some((name, bar)) = body.split_once(" | ") {
                spans.push(Span::styled(format!("{name} | "), style));
                for ch in bar.chars() {
                    let c = match ch {
                        '+' => added,
                        '-' => removed,
                        _ => muted,
                    };
                    spans.push(Span::styled(ch.to_string(), Style::default().fg(c)));
                }
            } else {
                spans.push(Span::styled(body, style));
            }
        } else {
            spans.push(Span::styled(body, style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(rows.x, y, rows.width, 1),
        );
    }
}

/// The review of AI agents' changes: a heading with the totals and keys, then each
/// changed file's hunks with old and new line numbers. The cursor row (a bar at the
/// left edge) is what y keeps and x undoes: its hunk, or its file on a heading.
fn changes_view(
    frame: &mut Frame,
    changes: &review::Changes,
    view: &mut ChangesView,
    area: Rect,
    theme: &Theme,
) {
    use review::{Kind, Row, Status};
    if area.height < 2 {
        return;
    }
    view.sync(changes);
    let fg = color(&theme.foreground);
    let muted = color(&theme.muted);
    let accent = color(&theme.accent);
    let background = color(&theme.background);
    let selection = color(&theme.selection);
    let added = change_color(Change::Added, theme);
    let removed = change_color(Change::Deleted, theme);
    // The keys, as many as fit.
    let keys = [
        "── y keep  x undo  Y X whole file  A U all  n N next change  ⏎ open ",
        "── y keep  x undo  Y X file  A U all  n N next  ⏎ open ",
        "── y keep  x undo  Y X file  n N next ",
        "── y keep  x undo ",
    ];
    let (heading, tone) = match &view.confirm {
        Some((_, question)) => (format!("── {question} "), color(&theme.warning)),
        None if changes.files.is_empty() => ("── Changes ".to_string(), color(&theme.border)),
        None => (
            keys.iter()
                .find(|k| k.chars().count() + 2 <= area.width as usize)
                .unwrap_or(&keys[3])
                .to_string(),
            color(&theme.border),
        ),
    };
    text(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        format!("{heading}{}", "─".repeat(area.width as usize)),
        tone,
    );
    let rows = Rect::new(area.x, area.y + 1, area.width, area.height - 1);
    if changes.files.is_empty() {
        let mut lines: Vec<String> = if let Some(problem) = &changes.problem {
            vec![
                "Agent changes are tracked in Git projects.".into(),
                format!("This folder: {problem}. `git init` makes it one."),
            ]
        } else if changes.loading() {
            vec!["Looking for changes…".into()]
        } else {
            vec![
                "No changes from AI agents to review.".into(),
                String::new(),
                "While an AI pane works, the files it changes show up here:".into(),
                "keep each change (y) or undo it (x).".into(),
            ]
        };
        if let Some(error) = &changes.error {
            lines.push(String::new());
            lines.push(error.clone());
        }
        let top = rows.y + rows.height.saturating_sub(lines.len() as u16) / 3;
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(muted)),
            Rect::new(rows.x, top, rows.width, rows.bottom() - top),
        );
        return;
    }
    view.height = rows.height as usize;
    follow(&mut view.scroll, view.cursor, view.height);
    view.scroll = view.scroll.min(view.rows.len().saturating_sub(view.height));
    // Line numbers when there is room for them and the code.
    let gutter = rows.width >= 60;
    let tint = |c: Color| crate::markdown::mix(background, c, 0.14);
    let number = |n: Option<usize>| n.map_or(String::new(), |n| n.to_string());
    for (index, row) in view
        .rows
        .iter()
        .enumerate()
        .skip(view.scroll)
        .take(view.height)
    {
        let y = rows.y + (index - view.scroll) as u16;
        let current = index == view.cursor;
        let mut row_bg = match *row {
            Row::File(_) => Some(selection),
            Row::Line(f, h, l) => match changes.files[f].hunks()[h].lines[l].kind {
                Kind::Added => Some(tint(added)),
                Kind::Removed => Some(tint(removed)),
                Kind::Context => None,
            },
            _ => None,
        };
        if current {
            row_bg = Some(crate::markdown::mix(selection, accent, 0.22));
        }
        let fill = Style::default().bg(row_bg.unwrap_or(background));
        if let Some(bg) = row_bg {
            frame.render_widget(
                Block::default().style(Style::default().bg(bg)),
                Rect::new(rows.x, y, rows.width, 1),
            );
        }
        let mut spans = vec![Span::styled(
            if current { "▌" } else { " " },
            fill.fg(accent),
        )];
        let width = rows.width.saturating_sub(1) as usize;
        let line = match *row {
            Row::Gap => Line::from(spans),
            Row::File(f) => {
                let file = &changes.files[f];
                spans.push(Span::styled(
                    format!("{} ", printable(&file.name)),
                    fill.fg(accent).add_modifier(Modifier::BOLD),
                ));
                let status = match file.status {
                    Status::Added => "new file ",
                    Status::Deleted => "deleted ",
                    Status::Modified => "",
                };
                spans.push(Span::styled(status, fill.fg(muted)));
                let mut right = format!("+{} −{}", file.added, file.removed);
                if !file.by.is_empty() {
                    right.push_str(&format!(" · {}", file.by.join(", ")));
                }
                let right = Span::styled(format!("{right} "), fill.fg(muted));
                fit_line(spans, right, width + 1, fill)
            }
            Row::Hunk(f, h) => {
                let span = changes.files[f].hunks()[h].span;
                // Like Git: an empty range starts at the line before it.
                let range = |(start, end): (usize, usize)| {
                    let length = end - start;
                    format!("{},{length}", if length == 0 { start } else { start + 1 })
                };
                spans.push(Span::styled(
                    format!("@@ -{} +{} @@", range(span.old), range(span.new)),
                    fill.fg(color(&theme.secondary)),
                ));
                // The hunk the cursor is in says what y and x do to it.
                let inside = view.rows.get(view.cursor).and_then(|r| r.hunk()) == Some((f, h));
                let right = if inside {
                    Span::styled("y keep · x undo ", fill.fg(accent))
                } else {
                    Span::raw("")
                };
                fit_line(spans, right, width + 1, fill)
            }
            Row::Note(f) => {
                let file = &changes.files[f];
                let note = match (&file.body, file.status) {
                    (review::Body::Binary, _) => "binary or very large: keep or undo it whole",
                    (_, Status::Added) => "new empty file",
                    (_, Status::Deleted) => "empty file deleted",
                    _ => "only its line endings or mode changed",
                };
                spans.push(Span::styled(note, fill.fg(muted)));
                Line::from(spans)
            }
            Row::Line(f, h, l) => {
                let diff_line = &changes.files[f].hunks()[h].lines[l];
                let (sign, style) = match diff_line.kind {
                    Kind::Added => ("+", fill.fg(added)),
                    Kind::Removed => ("-", fill.fg(removed)),
                    Kind::Context => (" ", fill.fg(fg)),
                };
                if gutter {
                    spans.push(Span::styled(
                        format!("{:>4} {:>4} ", number(diff_line.old), number(diff_line.new)),
                        fill.fg(muted),
                    ));
                }
                spans.push(Span::styled(format!("{sign} "), style));
                let body: String = printable(&diff_line.text.replace('\t', "    "))
                    .chars()
                    .skip(view.hscroll)
                    .collect();
                spans.push(Span::styled(body, style));
                if diff_line.unterminated {
                    spans.push(Span::styled("  ⏎ no newline at end", fill.fg(muted)));
                }
                Line::from(spans)
            }
        };
        frame.render_widget(Paragraph::new(line), Rect::new(rows.x, y, rows.width, 1));
    }
}

fn git_view(frame: &mut Frame, view: &mut GitView, area: Rect, theme: &Theme) {
    let fg = color(&theme.foreground);
    let muted = color(&theme.muted);
    let accent = color(&theme.accent);
    let secondary = color(&theme.secondary);
    let warning = color(&theme.warning);
    let background = color(&theme.background);
    let selection = color(&theme.selection);
    if let Some(error) = &view.data.error {
        text(frame, area, format!(" {error}"), muted);
        return;
    }
    if view.data.commits.is_empty() && view.data.branches.is_empty() {
        text(frame, area, " Reading history…", muted);
        return;
    }
    let width = area.width as usize;

    // Header: where you are and how it compares with upstream.
    let mut header = vec![Span::styled(
        " ⎇ ",
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    )];
    match view.current_branch() {
        Some(branch) => {
            header.push(Span::styled(
                branch.name.clone(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ));
            if !branch.upstream.is_empty() {
                header.push(Span::styled(
                    format!(" → {}", branch.upstream),
                    Style::default().fg(secondary),
                ));
                let state = track(&branch.track);
                header.push(Span::styled(
                    format!(
                        "  {}",
                        if state.is_empty() {
                            "in sync".into()
                        } else {
                            state
                        }
                    ),
                    Style::default().fg(warning),
                ));
            }
        }
        None => header.push(Span::styled(
            format!(
                "detached at {}",
                &view.data.head[..view.data.head.len().min(8)]
            ),
            Style::default().fg(warning),
        )),
    }
    let locals = view.data.branches.iter().filter(|b| !b.remote).count();
    header.push(Span::styled(
        format!(
            "   {locals} local · {} remote branches · {} commits",
            view.data.branches.len() - locals,
            view.data.commits.len()
        ),
        Style::default().fg(muted),
    ));
    frame.render_widget(
        Paragraph::new(fit_line(header, Span::raw(""), width, Style::default())),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let body = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    if let Some(diff) = view.diff.as_mut() {
        commit_diff(frame, diff, body, theme);
        return;
    }
    let detail_height = if view.details && body.height >= 12 {
        body.height * 2 / 5
    } else {
        0
    };
    let lists = Rect::new(body.x, body.y, body.width, body.height - detail_height);
    let (branch_rect, commit_rect) = if body.width >= 100 {
        let w = 34.min(body.width / 3);
        (
            Rect::new(lists.x, lists.y, w, lists.height),
            Rect::new(lists.x + w + 1, lists.y, lists.width - w - 1, lists.height),
        )
    } else {
        let h = (view.data.branches.len() as u16 + 1)
            .min(lists.height / 3)
            .max(2.min(lists.height));
        (
            Rect::new(lists.x, lists.y, lists.width, h),
            Rect::new(lists.x, lists.y + h, lists.width, lists.height - h),
        )
    };

    // Branches: yours first, then local and remote by recency.
    if branch_rect.height > 0 {
        let focused = view.focus == GitFocus::Branches;
        text(
            frame,
            Rect::new(branch_rect.x, branch_rect.y, branch_rect.width, 1),
            if focused {
                " BRANCHES · Enter shows one"
            } else {
                " BRANCHES · Tab"
            },
            if focused { accent } else { muted },
        );
        let rows = branch_rect.height.saturating_sub(1) as usize;
        follow(&mut view.branch_scroll, view.branch, rows);
        for (offset, (index, branch)) in view
            .data
            .branches
            .iter()
            .enumerate()
            .skip(view.branch_scroll)
            .take(rows)
            .enumerate()
        {
            let selected = index == view.branch;
            let shown = view.only.as_deref() == Some(branch.name.as_str());
            let mut style = Style::default().fg(if branch.remote { secondary } else { fg });
            if branch.current {
                style = style.fg(accent).add_modifier(Modifier::BOLD);
            }
            let fill = if selected && focused {
                Style::default().bg(selection)
            } else {
                Style::default()
            };
            let marker = if branch.current {
                "● "
            } else if shown {
                "▸ "
            } else {
                "  "
            };
            let mut spans = vec![
                Span::styled(format!(" {marker}"), style.patch(fill)),
                Span::styled(branch.name.clone(), style.patch(fill)),
            ];
            let state = track(&branch.track);
            if !state.is_empty() {
                spans.push(Span::styled(
                    format!(" {state}"),
                    Style::default().fg(warning).patch(fill),
                ));
            }
            let right = Span::styled(
                format!("{} ", gitgraph::age(branch.time)),
                Style::default().fg(muted).patch(fill),
            );
            frame.render_widget(
                Paragraph::new(fit_line(spans, right, branch_rect.width as usize, fill)),
                Rect::new(
                    branch_rect.x,
                    branch_rect.y + 1 + offset as u16,
                    branch_rect.width,
                    1,
                ),
            );
        }
    }

    // Commit graph: one color per lane, refs as badges, author and age on the right.
    let rows = commit_rect.height as usize;
    view.height = rows;
    follow(&mut view.scroll, view.commit, rows);
    let lanes = lane_colors(theme);
    let graph_width = view
        .data
        .graph
        .iter()
        .skip(view.scroll)
        .take(rows)
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min((commit_rect.width as usize / 3).max(8));
    for (offset, (index, commit)) in view
        .data
        .commits
        .iter()
        .enumerate()
        .skip(view.scroll)
        .take(rows)
        .enumerate()
    {
        let selected = index == view.commit;
        let fill = if selected {
            Style::default().bg(selection)
        } else {
            Style::default()
        };
        let head = commit.hash == view.data.head;
        let mut spans = vec![Span::styled(" ", fill)];
        let cells = view.data.graph.get(index).map(Vec::as_slice).unwrap_or(&[]);
        for (column, (glyph, lane)) in cells.iter().enumerate().take(graph_width) {
            let glyph = if *glyph == '●' && head {
                '◉'
            } else {
                *glyph
            };
            let glyph = if column + 1 == graph_width && cells.len() > graph_width {
                '…'
            } else {
                glyph
            };
            let mut style = Style::default().fg(lanes[lane % lanes.len()]).patch(fill);
            if glyph == '◉' || glyph == '●' {
                style = style.add_modifier(Modifier::BOLD);
            }
            spans.push(Span::styled(glyph.to_string(), style));
        }
        spans.push(Span::styled(
            " ".repeat(graph_width.saturating_sub(cells.len()) + 1),
            fill,
        ));
        spans.push(Span::styled(
            format!("{} ", commit.short),
            Style::default().fg(muted).patch(fill),
        ));
        for r in &commit.refs {
            let (label, style) = match r.kind {
                RefKind::Head => (
                    format!(" ◉ {} ", r.name),
                    Style::default()
                        .fg(background)
                        .bg(accent)
                        .add_modifier(Modifier::BOLD),
                ),
                RefKind::Local => (
                    format!(" {} ", r.name),
                    Style::default()
                        .fg(background)
                        .bg(secondary)
                        .add_modifier(Modifier::BOLD),
                ),
                RefKind::Remote => (r.name.clone(), Style::default().fg(secondary).patch(fill)),
                RefKind::Tag => (
                    format!("◆ {}", r.name),
                    Style::default()
                        .fg(warning)
                        .add_modifier(Modifier::BOLD)
                        .patch(fill),
                ),
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::styled(" ", fill));
        }
        let mut subject = Style::default().fg(fg).patch(fill);
        if head {
            subject = subject.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(commit.subject.clone(), subject));
        let right = Span::styled(
            format!("{} · {} ", commit.author, gitgraph::age(commit.time)),
            Style::default().fg(muted).patch(fill),
        );
        frame.render_widget(
            Paragraph::new(fit_line(spans, right, commit_rect.width as usize, fill)),
            Rect::new(
                commit_rect.x,
                commit_rect.y + offset as u16,
                commit_rect.width,
                1,
            ),
        );
    }

    // Details of the selected commit: full message, author, date and changed files.
    if detail_height > 1 {
        let rect = Rect::new(
            body.x,
            body.bottom() - detail_height,
            body.width,
            detail_height,
        );
        let title = match view.selected() {
            Some(c) => format!("── {} {}", c.short, "─".repeat(width)),
            None => "─".repeat(width),
        };
        text(
            frame,
            Rect::new(rect.x, rect.y, rect.width, 1),
            title,
            color(&theme.border),
        );
        let lines: Vec<Line> = match (&view.detail, view.selected()) {
            (Some((hash, lines)), Some(c)) if *hash == c.hash => lines
                .iter()
                .map(|line| {
                    if let Some((who, when)) = line.split_once('\u{1f}') {
                        Line::from(vec![
                            Span::styled(format!(" {who}"), Style::default().fg(secondary)),
                            Span::styled(format!("  {when}"), Style::default().fg(muted)),
                        ])
                    } else if let Some((file, bar)) = line.split_once(" | ") {
                        let mut spans = vec![Span::styled(
                            format!(" {file} | "),
                            Style::default().fg(muted),
                        )];
                        for ch in bar.chars() {
                            let tint = match ch {
                                '+' => accent,
                                '-' => warning,
                                _ => muted,
                            };
                            spans.push(Span::styled(ch.to_string(), Style::default().fg(tint)));
                        }
                        Line::from(spans)
                    } else if line.contains(" changed") && line.starts_with(' ') {
                        Line::styled(format!(" {line}"), Style::default().fg(muted))
                    } else {
                        Line::styled(format!(" {line}"), Style::default().fg(fg))
                    }
                })
                .collect(),
            (_, Some(_)) => vec![Line::styled(" Loading…", Style::default().fg(muted))],
            _ => Vec::new(),
        };
        frame.render_widget(
            Paragraph::new(lines),
            Rect::new(rect.x, rect.y + 1, rect.width, rect.height - 1),
        );
    }
}

fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let mode = if app.prefix {
        "PREFIX"
    } else if app.copy.is_some() {
        "COPY"
    } else if app.find.is_some() {
        "FIND"
    } else if app.dock_list.is_some() {
        "DOCK"
    } else if app.resizing {
        "RESIZE"
    } else if app.help && app.help_focus {
        "GUIDE"
    } else if app.sidebar_focus {
        if app.git_focus {
            "GIT"
        } else if app.explorer_search.is_some() {
            "SEARCH"
        } else {
            "FILES"
        }
    } else {
        match app.panes.get(&app.focused) {
            Some(Pane::Terminal(_)) => "TERMINAL",
            Some(Pane::Preview(_)) => "PREVIEW",
            Some(Pane::Ai) => "AI",
            Some(Pane::Git(_)) => "GIT",
            Some(Pane::Changes(_)) => "REVIEW",
            _ => match app.mode {
                Mode::Insert => "INSERT",
                Mode::Visual => "VISUAL",
                Mode::VisualLine => "V-LINE",
                Mode::Normal => "NORMAL",
            },
        }
    };
    let mode_color = color(match app.mode {
        Mode::Insert => &theme.warning,
        Mode::Visual | Mode::VisualLine => &theme.secondary,
        Mode::Normal => &theme.accent,
    });
    let lines = Line::from(vec![
        Span::styled(
            format!(" {mode} "),
            Style::default()
                .fg(color(&theme.background))
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", printable(&app.message)),
            Style::default().fg(color(if app.error {
                &theme.warning
            } else {
                &theme.foreground
            })),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if let Some(input) = &app.input {
        let prefix = match input.kind {
            InputKind::Command => ":",
            InputKind::Search => "/",
            InputKind::Open => "open > ",
        };
        let text = format!("{prefix}{}", input.text);
        let cols = display_width(text.chars(), 4);
        let skip = cols.saturating_sub(area.width.saturating_sub(1) as usize);
        let mut column = 0;
        let value = clip(&text, &mut column, skip, area.width as usize, 4);
        frame.render_widget(
            Paragraph::new(value).style(Style::default().fg(color(&theme.accent))),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        frame.set_cursor_position((area.x + (cols - skip) as u16, area.y + 1));
    } else if let Some(query) = app.copy.as_ref().and_then(|c| c.typing.as_ref()) {
        let value = format!("/{}", printable(query));
        frame.render_widget(
            Paragraph::new(value.clone()).style(Style::default().fg(color(&theme.accent))),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        let x = (value.chars().count() as u16).min(area.width.saturating_sub(1));
        frame.set_cursor_position((area.x + x, area.y + 1));
    } else {
        let hints = if app.resizing {
            " ← → ↑ ↓ or h j k l resize the focused pane   Enter or Esc done"
        } else if app.dock_list.is_some() {
            " ↑ ↓ choose   Enter or 1-9 bring back   x close the pane   Esc"
        } else if let Some(find) = &app.find {
            if find.replacing {
                " Enter replace   ^A replace all   ↑ ↓ previous/next   Tab find/replace   ^W whole word   Esc close"
            } else {
                " Enter ↓ next   ↑ previous   Tab or ^H replace   ^W whole word   Esc close (n N keep going)"
            }
        } else if app.copy.is_some() {
            " hjkl/arrows move   w b words   0 $ line   g G top/end   PgUp PgDn   v V select   y copy   / ? search   n N next   q leave"
        } else if app.prefix {
            " % split  \" stack  c shell  a/A AI beside/below  m preview  v review AI changes  t themes  arrows focus  ⇧arrows move  space flip  d dock  D docked  y mouse  r resize  z zoom  Z copy view  [ copy mode  +/-/0 text size  x close  ? help"
        } else if app.sidebar_focus && !app.git_focus && app.explorer_search.is_some() {
            " type to search names and text   ↑ ↓ preview   PgUp PgDn page   Enter open at the line   ^U clear   Esc back to the tree"
        } else if app.sidebar_focus && !app.git_focus {
            " ^F search files   j k move   PgUp PgDn page   → expand/open   ← collapse   space fold   c collapse all   E expand all   Enter open   n new   Esc back"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Git(v)) if v.diff.is_some())
            && !app.sidebar_focus
        {
            " j k scroll   Space PgDn page   n N next/previous file   h l sideways   g G top/end   y copy hash   Esc graph   ^B G maximize"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Changes(v)) if v.confirm.is_some())
            && !app.sidebar_focus
        {
            " y undo it   any other key keeps it"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Changes(_)))
            && !app.sidebar_focus
        {
            " j k move   n N next change   ] [ file   y keep   x undo   Y X whole file   A keep all   U undo all   Enter open   q close"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Git(_))) && !app.sidebar_focus {
            " Tab branches/commits   j k move   Enter branch only / commit diff   d details   a all   y copy hash   r refresh   ^B G maximize"
        } else if matches!(app.panes.get(&app.focused), Some(Pane::Preview(_)))
            && !app.sidebar_focus
        {
            " j k scroll   space ^D ^U page   g G top/end   [ ] headings   e edit   ^B m source"
        } else {
            match app.mode {
                Mode::Visual | Mode::VisualLine => {
                    " move to extend   y/^C copy   d/^X cut   p/^V replace   c change   Esc cancel"
                }
                _ => " ^S save  ^F find  ^C ^X ^V copy/cut/paste  v select  ^E files  ^P open  ^B panes  F1 help  ^Q quit",
            }
        };
        text(
            frame,
            Rect::new(area.x, area.y + 1, area.width, 1),
            hints,
            color(&theme.muted),
        );
    }
}

/// Greedy word wrap for plain text.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().expect("one line");
        let used = line.chars().count();
        if used > 0 && used + 1 + word.chars().count() > width {
            lines.push(word.to_string());
        } else {
            if used > 0 {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

fn help(frame: &mut Frame, app: &mut App, rect: Rect) {
    let theme = &app.theme.clone();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Block::default().style(Style::default().bg(color(&theme.background))),
        rect,
    );
    let title = if rect.width >= 52 {
        if app.help_focus {
            " FIELD GUIDE · ↑↓ scroll · Esc back · F1 closes "
        } else {
            " FIELD GUIDE · F1 to scroll · F1 F1 closes "
        }
    } else {
        " GUIDE · F1 closes "
    };
    let block =
        panel(title, theme, app.help_focus).title_style(Style::default().fg(color(&theme.accent)));
    let inner = block.inner(rect);
    let rows = [
        ("THEMES", "Pick visually — no names to remember"),
        (
            ":themes  or  Ctrl-b t",
            "Open the theme list; ↑↓ previews each theme live",
        ),
        (
            "  type / Enter / Esc",
            "Filter by name / keep and save as default / restore",
        ),
        (":theme name", "Switch directly by name (Tab completes)"),
        (
            "omarchy / omarchy-<name>",
            "Follow the desktop's Omarchy theme / use one of them",
        ),
        ("WORKSPACE", "One prefix. Everything in view."),
        ("Ctrl-b % / \"", "Split side-by-side / top-and-bottom"),
        ("Ctrl-b arrows / h j k l", "Focus adjacent pane; o cycles"),
        (
            "Ctrl-b r, arrows",
            "Resize the pane (Enter done); Ctrl-b Ctrl-arrows too",
        ),
        (
            "Ctrl-b Shift-arrows / HJKL",
            "Move the pane: swap, or take that whole side",
        ),
        ("Ctrl-b Space", "Flip split: side by side ↔ stacked"),
        (
            "Ctrl-b d / D",
            "Dock (hide, keeps running) / docked list; click a tab",
        ),
        ("Ctrl-b c / C", "New shell below / beside; :term right"),
        (
            "Ctrl-b a / A",
            "AI launcher beside / below (ai_split sets default)",
        ),
        (
            ":sound",
            "Chime when an AI prompt finishes (ai_sound picks it)",
        ),
        ("Ctrl-b z / x / s", "Zoom / close pane / toggle sidebar"),
        (
            "Ctrl-b + / - / 0",
            "Text bigger / smaller / reset (repeat + or -); :zoom in/out",
        ),
        (
            ":font size 12 / family X",
            "Save font size / family in your terminal; :font list",
        ),
        (
            ":docs / ask your AI agent",
            "Full manual / agents know Raido via its skill (:skill)",
        ),
        (
            "Ctrl-b [  then v V y",
            "Copy mode in a terminal / AI pane: select, y copies",
        ),
        (
            "Ctrl-b Z",
            "Copy view: only this pane's text, for mouse selection",
        ),
        (
            "Ctrl-b y",
            if cfg!(target_os = "macos") {
                "Mouse to the terminal: select + ⌘C; again for the wheel"
            } else {
                "Mouse to the terminal: select + Ctrl-Shift-C; again for the wheel"
            },
        ),
        (
            "Mouse wheel / click",
            "Scroll what's under the pointer / focus, place cursor, open",
        ),
        ("Ctrl-b g", "Focus the Git sidebar"),
        (
            "PgUp / PgDn (Fn-↑/↓)",
            "Scroll a terminal or AI pane half a page; ↑↓ a line, End returns",
        ),
        (
            ":git  or  Ctrl-b G",
            "Git pane: branches, graph; Enter on a commit shows its diff",
        ),
        (
            ":changes  or  Ctrl-b v",
            "Review what AI agents changed: y keeps, x undoes a change",
        ),
        (
            "  Y X / A U / Enter",
            "Keep or undo the whole file / every file / open it there",
        ),
        (
            ":session [save|forget]",
            "Reopen this project as you left it (.raido/)",
        ),
        ("Ctrl-b b", "Send literal Ctrl-b to a terminal"),
        (
            "FILES + GIT",
            "Ctrl-e focuses Explorer; Tab switches Files / Git",
        ),
        (
            "↑ ↓ / → / ← / Enter",
            "Browse, previewing files / expand / collapse / open",
        ),
        (
            "PgUp PgDn / Home End",
            "A screenful up / down, or to the first / last entry",
        ),
        (
            "Space / c / E",
            "Fold folder / collapse all / expand all inside",
        ),
        ("r / n", "Refresh / new file"),
        ("Ctrl-p / Tab", "Open path / complete files and folders"),
        ("EDITOR", "Vim-inspired, not a full Vim implementation"),
        (
            "i a I A o O / Esc",
            "Insert, append, open line / normal mode",
        ),
        ("h j k l / w b / 0 $", "Move / words / line start-end"),
        ("gg / G / 42G / :42", "Top / bottom / jump to line 42"),
        (
            "15j 15k / 15⏎ 15- / :+15",
            "Relative jump: the numbers shown with :set rnu",
        ),
        (
            "x dd D C / yy p P",
            "Delete / change / copy line / paste below, above",
        ),
        (
            "v / V / Shift-arrows",
            "Select characters / lines; y copy, d cut, p replace",
        ),
        crate::platform::WORD_KEYS,
        (
            "Ctrl-c / Ctrl-x / Ctrl-v",
            "Copy / cut / paste (line if nothing selected)",
        ),
        (
            "LANGUAGE SERVERS",
            "Rust, Go, Python, C, C++ — :lsp shows status",
        ),
        (
            "K / gd / Ctrl-o",
            "Hover docs / go to definition / jump back",
        ),
        (
            "Ctrl-Space or ^N / ]d [d / :format",
            "Complete (pops up by itself; Tab takes it) / problems / format",
        ),
        (
            "]c / [c",
            "Next / previous change from Git (gutter: ▌ new/changed, ▁ removed)",
        ),
        ("u / Ctrl-r / Ctrl-s", "Undo / redo / atomic save"),
        ("/query / n N", "Find / next / previous; wraps around"),
        (
            "Enter / ( [ { \" '",
            "Indents per language / close themselves; Shift-Tab dedents",
        ),
        (
            "Explorer: Ctrl-f",
            "Search the project: file names and text in files",
        ),
        (
            "Ctrl-f / Ctrl-h",
            "Find bar / find & replace (^W whole word, ^A all)",
        ),
        ("Tab / Shift-Tab", "Cycle open buffers"),
        ("MARKDOWN", ".md files open rendered; edits re-render live"),
        (
            ":preview / Ctrl-b m",
            "Toggle rendered view / source, keeping place",
        ),
        (
            ":preview split / Ctrl-b M",
            "Live preview beside the source",
        ),
        (
            "j k / space ^D ^U / [ ]",
            "Scroll / page / previous-next heading",
        ),
        ("COMMANDS", "Ctrl-b : also works inside terminals"),
        (":w [path] / :wa / :wq", "Save / save all / save and close"),
        (":e path / :reload", "Open or create / reload a clean file"),
        (":q / :qa / :qa!", "Close / quit / discard and quit"),
        (
            ":ai codex|claude|gemini|agy",
            "Run an AI CLI here; add `below` or `right` to place it",
        ),
        (
            ":ai local [model]",
            "Local LLM: [ai.local] in config, else Ollama / LM Studio",
        ),
        (
            ":ai <name> [model]",
            "Any tool you add as [ai.<name>] (keys 6–9 in launcher)",
        ),
        (":run name", "Launch a configured command plugin"),
        (":set rnu / :set nornu", "Relative / absolute line numbers"),
    ];
    // Keys on the left, descriptions wrapped beside them; a key too long for its
    // column gets its own line.
    let width = inner.width as usize;
    let key_col = if width >= 72 {
        29
    } else {
        (width / 2).clamp(12, 24)
    };
    let key_style = Style::default().fg(color(&theme.accent));
    let text_style = Style::default().fg(color(&theme.foreground));
    let mut lines = Vec::new();
    for (key, description) in rows {
        let heading = key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == ' ' || c == '+');
        if heading {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(
                format!(" {key}"),
                Style::default()
                    .fg(color(&theme.secondary))
                    .add_modifier(Modifier::BOLD),
            )));
            for part in wrap_words(description, width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(
                    format!(" {part}"),
                    Style::default().fg(color(&theme.muted)),
                )));
            }
            continue;
        }
        let beside = width.saturating_sub(key_col + 2);
        if key.chars().count() < key_col && beside >= 14 {
            for (i, part) in wrap_words(description, beside).into_iter().enumerate() {
                let label = if i == 0 { key } else { "" };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {label:<key_col$}"), key_style),
                    Span::styled(part, text_style),
                ]));
            }
        } else {
            lines.push(Line::from(Span::styled(format!(" {key}"), key_style)));
            for part in wrap_words(description, width.saturating_sub(5)) {
                lines.push(Line::from(Span::styled(format!("    {part}"), text_style)));
            }
        }
    }
    let total = lines.len();
    let max = total.saturating_sub(inner.height as usize);
    app.help_scroll = app.help_scroll.min(max);
    // Say when there is more, and how to reach it.
    let block = if max > 0 {
        let arrows = match (app.help_scroll > 0, app.help_scroll < max) {
            (true, true) => "▲▼",
            (false, _) => "▼",
            (true, false) => "▲",
        };
        block.title_bottom(
            Line::styled(
                format!(
                    " {arrows} more · {} · {}% ",
                    if app.help_focus {
                        "↑↓ PgDn"
                    } else {
                        "F1 then ↑↓"
                    },
                    app.help_scroll * 100 / max
                ),
                Style::default().fg(color(&theme.accent)),
            )
            .right_aligned(),
        )
    } else {
        block
    };
    frame.render_widget(block, rect);
    let visible: Vec<Line> = lines.into_iter().skip(app.help_scroll).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

#[cfg(test)]
mod tests {
    #[test]
    fn explorer_hint_always_fits() {
        for width in 0..40 {
            assert!(super::explorer_hint(width).chars().count() <= width);
        }
        // The default sidebar (29 columns, 27 inside the border) names search, fold
        // and collapse / expand all; the status line has the rest.
        assert_eq!(super::explorer_hint(27), " ^F search ←→ fold c/E all");
    }

    use super::*;
    #[test]
    fn terminal_palette_maps_ansi_but_preserves_extended_and_truecolor() {
        let theme = crate::config::load_theme_from("visual-studio-dark", None).unwrap();
        let fallback = color(&theme.foreground);
        assert_eq!(
            term_color(vt100::Color::Default, fallback, &theme),
            fallback
        );
        for index in 0..16 {
            assert_eq!(
                term_color(vt100::Color::Idx(index), fallback, &theme),
                color(&theme.ansi.as_ref().unwrap()[index as usize])
            );
        }
        assert_eq!(
            term_color(vt100::Color::Idx(200), fallback, &theme),
            Color::Indexed(200)
        );
        assert_eq!(
            term_color(vt100::Color::Rgb(1, 2, 3), fallback, &theme),
            Color::Rgb(1, 2, 3)
        );
        assert_eq!(
            term_color(vt100::Color::Idx(1), fallback, &Theme::default()),
            Color::Indexed(1)
        );
    }
    #[test]
    fn clipping_handles_tabs_and_wide_chars_without_splitting_cells() {
        let mut column = 0;
        assert_eq!(clip("a\t界z", &mut column, 2, 4, 4), "  界");
        assert_eq!(display_width("a\t界z".chars(), 4), 7);
    }
}

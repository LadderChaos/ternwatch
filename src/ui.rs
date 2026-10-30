//! Rendering: header, the swarm graph, the inspector and message feed, the help popup.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::time::Instant;

use chrono::TimeZone;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{
    Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Focus, Mode, PACKET_MS, WAIT_MS, packet_style};
use crate::model::{Agent, Kind, Message, Node, PaneId};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PANEL_WIDTH: u16 = 46;
const ACTIVE_EDGE_MS: i64 = 90_000;
const RECENT_EDGE_MS: i64 = 15 * 60_000;
const RECENT_SUB_MS: i64 = 10 * 60_000;

type Segment = ((f64, f64), (f64, f64), Color);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Size {
    Card,
    Small,
    Chip,
}

impl Size {
    fn dims(self) -> (u16, u16) {
        match self {
            Size::Card => (28, 6),
            Size::Small => (22, 4),
            Size::Chip => (18, 1),
        }
    }
}

/// Colors that collapse to the terminal default under `NO_COLOR`.
#[derive(Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn c(self, color: Color) -> Color {
        if self.color { color } else { Color::Reset }
    }
    fn fg(self, color: Color) -> Style {
        Style::new().fg(self.c(color))
    }
    fn dim(self) -> Style {
        if self.color {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::DIM)
        }
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme { color: app.color };
    let area = frame.area();
    app.hit_nodes.clear();
    app.hit_feed.clear();
    app.hit_copy = None;
    app.hit_graph = Rect::default();
    app.hit_inspector = Rect::default();
    app.hit_feed_area = Rect::default();
    if area.width < 60 || area.height < 16 {
        let text = format!(
            "ternwatch needs at least 60×16 (now {}×{}). q quits.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), area);
        return;
    }
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let (graph, panel) = if body.width >= 112 {
        let [graph, panel] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(PANEL_WIDTH)]).areas(body);
        (graph, panel)
    } else {
        let [graph, panel] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length((body.height / 2).min(14)),
        ])
        .areas(body);
        (graph, panel)
    };
    draw_header(frame, header, app, theme);
    draw_graph(frame, graph, app, theme);
    draw_panel(frame, panel, app, theme);
    draw_footer(frame, footer, app, theme);
    if app.help {
        draw_help(frame, area, app, theme);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    let now = App::now_ms();
    let agents = app.visible_agents();
    let working = agents
        .iter()
        .filter(|p| {
            app.model
                .agents
                .get(p)
                .is_some_and(|a| a.present && a.pane.working)
        })
        .count();
    let mut spans = vec![
        Span::styled(
            " ternwatch ",
            Style::new()
                .fg(theme.c(Color::Black))
                .bg(theme.c(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    match &app.mode {
        Mode::Live => spans.push(Span::styled(
            " LIVE ",
            Style::new()
                .fg(theme.c(Color::Black))
                .bg(theme.c(Color::Green))
                .add_modifier(Modifier::BOLD),
        )),
        Mode::Replay(replay) => {
            let state = if replay.paused { " paused" } else { "" };
            let text = format!(
                " REPLAY {} ×{}{} ",
                clock(replay.cursor),
                speed(replay.speed),
                state
            );
            spans.push(Span::styled(
                text,
                Style::new()
                    .fg(theme.c(Color::Black))
                    .bg(theme.c(Color::Magenta))
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                "  recorded history, not live",
                theme.fg(Color::Magenta),
            ));
        }
    }
    let shown = app
        .model
        .messages
        .iter()
        .filter(|m| app.visible_message(m))
        .count();
    spans.push(Span::raw(format!(
        "  {} agents · {} working · {} subagents running · {} messages",
        agents.len(),
        working,
        app.model.running_subs(now),
        shown
    )));
    if !app.loaded {
        spans.push(Span::styled("  reading transcripts…", theme.dim()));
    }
    if let Some(error) = &app.error {
        spans.push(Span::styled(
            format!("  {error}"),
            theme.fg(Color::LightRed),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    let line = if let Some(text) = &app.compose {
        let name = app
            .selected
            .map(|p| app.model.name(&Node::Pane(p)))
            .unwrap_or_default();
        Line::from(vec![
            Span::styled(
                format!(" message {name} › "),
                theme.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.clone()),
            Span::styled("▏", theme.fg(Color::Cyan)),
            Span::styled("   Enter sends · Esc cancels", theme.dim()),
        ])
    } else if let Some((status, _)) = &app.status {
        Line::from(Span::styled(format!(" {status}"), theme.fg(Color::Yellow)))
    } else {
        let replay = matches!(app.mode, Mode::Replay(_));
        let mut wide = vec![
            "←→ select",
            "Tab feed",
            "⏎ jump/details",
            "m message",
            "r replay",
            "f filter",
            "a panes",
            "c copy id",
            "? help",
            "q quit",
        ];
        let mut short = vec![
            "←→ select",
            "Tab feed",
            "⏎ jump",
            "m send",
            "r replay",
            "c copy",
            "? help",
            "q quit",
        ];
        if replay {
            wide.splice(4..5, ["r live", "space pause", "+/- speed"]);
            short.splice(4..5, ["r live", "space pause"]);
        }
        let tiny = vec!["←→ select", "? help", "q quit"];
        // The widest set that fits, so `q quit` is never cut off.
        let hints = [wide, short]
            .into_iter()
            // One leading space, then the hints.
            .find(|hints| hints.join("  ").width() < area.width as usize)
            .unwrap_or(tiny);
        Line::from(Span::styled(format!(" {}", hints.join("  ")), theme.dim()))
    };
    frame.render_widget(Paragraph::new(line), area);
}

struct Placed {
    rect: Rect,
    center: (f64, f64),
}

fn draw_graph(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    let agents = app.visible_agents();
    app.hit_graph = area;
    if agents.is_empty() {
        let text = if app.loaded {
            "No omp agents in this Tern window yet.\nStart one, or press a to show every pane."
        } else {
            "Reading Tern panes…"
        };
        frame.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .style(theme.dim()),
            center_rect(area, 50, 2),
        );
        return;
    }
    // A selected agent that left the graph is no longer selected; none means every message.
    if app.selected.is_some_and(|s| !agents.contains(&s)) {
        app.select(None);
    }
    let (rects, size, ring) = place(area, agents.len());
    let now = app.view_time();
    let mut placed: HashMap<Node, Placed> = HashMap::new();
    for (pane, rect) in agents.iter().zip(&rects) {
        placed.insert(
            Node::Pane(*pane),
            Placed {
                rect: *rect,
                center: center_of(*rect),
            },
        );
    }
    // Subagent chips sit between a card and the ring's center, so only a ring has room for them.
    let chips = if !ring {
        Vec::new()
    } else {
        place_subs(app, area, &agents, &placed, size, now)
    };
    for (node, rect) in &chips {
        placed.insert(
            node.clone(),
            Placed {
                rect: *rect,
                center: center_of(*rect),
            },
        );
    }

    // Edges under everything: cross-pane traffic between cards, and each card's subagent stalks.
    // One-line chips sit in rows, where every edge would run through other chips' labels.
    let mut edges: HashMap<(PaneId, PaneId), (usize, i64, bool)> = HashMap::new();
    for message in app.model.messages.iter().filter(|m| {
        size != Size::Chip && m.kind == Kind::Pane && !m.failed && app.visible_message(m)
    }) {
        let (a, b) = (message.from.pane(), message.to.pane());
        if a == b || !placed.contains_key(&Node::Pane(a)) || !placed.contains_key(&Node::Pane(b)) {
            continue;
        }
        let entry = edges.entry((a.min(b), a.max(b))).or_insert((0, 0, false));
        entry.0 += 1;
        if message.time() >= entry.1 {
            entry.1 = message.time();
            entry.2 = message.reply_to.is_some();
        }
    }
    // Oldest first, so the newest edge is drawn on top; a HashMap's order changes every frame.
    let mut edges: Vec<_> = edges.into_iter().collect();
    edges.sort_by_key(|((a, b), (_, last, _))| (*last, *a, *b));
    let mut lines: Vec<Segment> = Vec::new();
    for ((a, b), (_, last, reply)) in &edges {
        let age = now - last;
        let color = if age < ACTIVE_EDGE_MS {
            if *reply { Color::Magenta } else { Color::Cyan }
        } else if age < RECENT_EDGE_MS {
            Color::Blue
        } else {
            Color::DarkGray
        };
        lines.push((
            placed[&Node::Pane(*a)].center,
            placed[&Node::Pane(*b)].center,
            theme.c(color),
        ));
    }
    for (node, rect) in &chips {
        let parent = &placed[&Node::Pane(node.pane())];
        let running = sub_of(app, node).is_some_and(|s| s.running(App::now_ms()));
        lines.push((
            parent.center,
            center_of(*rect),
            theme.c(if running {
                Color::Green
            } else {
                Color::DarkGray
            }),
        ));
    }
    let (ox, oy, height) = (area.x as f64, area.y as f64, area.height as f64);
    let canvas = Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, area.width as f64])
        .y_bounds([0.0, height])
        .paint(move |ctx| {
            for ((x1, y1), (x2, y2), color) in &lines {
                ctx.draw(&CanvasLine::new(
                    x1 - ox,
                    height - (y1 - oy),
                    x2 - ox,
                    height - (y2 - oy),
                    *color,
                ));
            }
        });
    frame.render_widget(canvas, area);

    let buf = frame.buffer_mut();
    let rects_only: Vec<Rect> = placed.values().map(|p| p.rect).collect();
    for ((a, b), (count, _, _)) in &edges {
        let (p, q) = (
            placed[&Node::Pane(*a)].center,
            placed[&Node::Pane(*b)].center,
        );
        let mid = Position::new(((p.0 + q.0) / 2.0) as u16, ((p.1 + q.1) / 2.0) as u16);
        if !rects_only.iter().any(|r| r.contains(mid)) {
            buf.set_string(mid.x, mid.y, format!(" {count} "), theme.dim());
        }
    }
    let spin = spinner(app.animate);
    for (pane, rect) in agents.iter().zip(&rects) {
        let Some(agent) = app.model.agents.get(pane) else {
            continue;
        };
        let flash = app.flashes.get(&Node::Pane(*pane)).map(|(_, color)| *color);
        draw_card(
            buf,
            *rect,
            agent,
            size,
            app.selected == Some(*pane),
            flash,
            spin,
            theme,
        );
        app.hit_nodes.push((*rect, *pane));
    }
    for (node, rect) in &chips {
        draw_chip(buf, *rect, app, node, spin, theme);
    }
    draw_traffic(buf, area, app, &placed, theme);
}

/// Cards on an ellipse, largest size that does not overlap; else small cards in a grid; else a
/// grid of one-line chips. The flag is true for a ring, which has room for subagent chips inside.
fn place(area: Rect, count: usize) -> (Vec<Rect>, Size, bool) {
    for size in [Size::Card, Size::Small] {
        let (w, h) = size.dims();
        if area.width < w + 2 || area.height < h + 2 {
            continue;
        }
        let (cx, cy) = (
            area.x as f64 + area.width as f64 / 2.0,
            area.y as f64 + area.height as f64 / 2.0,
        );
        let rx = (area.width as f64 - w as f64) / 2.0 - 1.0;
        let ry = (area.height as f64 - h as f64) / 2.0 - 0.5;
        let rects: Vec<Rect> = ring_points(cx, cy, rx, ry, count, w as f64 + 2.0, h as f64 + 1.0)
            .into_iter()
            .map(|(x, y)| {
                clamp_rect(
                    Rect::new(
                        (x - w as f64 / 2.0).round().max(0.0) as u16,
                        (y - h as f64 / 2.0).round().max(0.0) as u16,
                        w,
                        h,
                    ),
                    area,
                )
            })
            .collect();
        let overlaps = rects
            .iter()
            .enumerate()
            .any(|(i, a)| rects[i + 1..].iter().any(|b| a.intersects(grow(*b))));
        if !overlaps {
            return (rects, size, true);
        }
    }
    for size in [Size::Small, Size::Chip] {
        if let Some(rects) = grid(area, count, size) {
            return (rects, size, false);
        }
    }
    // More agents than chip rows: fill row by row from the top; the rest are clamped to the edge.
    let (w, h) = Size::Chip.dims();
    let columns = (area.width / (w + 2)).max(1);
    let rects = (0..count as u16)
        .map(|i| {
            clamp_rect(
                Rect::new(
                    area.x + 1 + (i % columns) * (w + 2),
                    area.y + 1 + (i / columns) * (h + 1),
                    w,
                    h,
                ),
                area,
            )
        })
        .collect();
    (rects, Size::Chip, false)
}

/// `count` cards of `size` in rows spread over the whole area, the last row centered; `None` when
/// the rows do not fit (two columns and one row of gap between cards at least).
fn grid(area: Rect, count: usize, size: Size) -> Option<Vec<Rect>> {
    let (w, h) = size.dims();
    let count = count as u16;
    let columns = ((area.width + 2) / (w + 2)).min(count).max(1);
    let rows = count.div_ceil(columns);
    if area.width < w || rows * (h + 1) > area.height + 1 {
        return None;
    }
    let (cell_w, cell_h) = (area.width / columns, area.height / rows);
    Some(
        (0..count)
            .map(|i| {
                let (column, row) = (i % columns, i / columns);
                let in_row = (count - row * columns).min(columns);
                let indent = (columns - in_row) * cell_w / 2;
                Rect::new(
                    area.x + indent + column * cell_w + (cell_w - w) / 2,
                    area.y + row * cell_h + (cell_h - h) / 2,
                    w,
                    h,
                )
            })
            .collect(),
    )
}

/// `count` points on the ellipse, starting at the top, evenly spaced in card units: one unit is a
/// card's width across and its height down. Equal angles would crowd cards at the top and bottom
/// of a wide ellipse and leave the sides empty.
fn ring_points(
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    count: usize,
    unit_x: f64,
    unit_y: f64,
) -> Vec<(f64, f64)> {
    if count == 1 {
        return vec![(cx, cy)];
    }
    const SAMPLES: usize = 720;
    let at = |k: usize| {
        let angle = -PI / 2.0 + 2.0 * PI * k as f64 / SAMPLES as f64;
        (cx + rx * angle.cos(), cy + ry * angle.sin())
    };
    let mut walked = vec![0.0; SAMPLES + 1];
    for k in 1..=SAMPLES {
        let ((x1, y1), (x2, y2)) = (at(k - 1), at(k));
        walked[k] =
            walked[k - 1] + (((x2 - x1) / unit_x).powi(2) + ((y2 - y1) / unit_y).powi(2)).sqrt();
    }
    let total = walked[SAMPLES];
    (0..count)
        .map(|i| {
            let target = total * i as f64 / count as f64;
            at(walked.partition_point(|d| *d < target).min(SAMPLES - 1))
        })
        .collect()
}

/// Subagent chips between each card and the ring center: running ones first, then recent ones.
fn place_subs(
    app: &App,
    area: Rect,
    agents: &[PaneId],
    placed: &HashMap<Node, Placed>,
    size: Size,
    now: i64,
) -> Vec<(Node, Rect)> {
    let limit = if size == Size::Card { 3 } else { 2 };
    let center = (
        area.x as f64 + area.width as f64 / 2.0,
        area.y as f64 + area.height as f64 / 2.0,
    );
    let (cw, ch) = size.dims();
    let mut chips = Vec::new();
    for pane in agents {
        let Some(agent) = app.model.agents.get(pane) else {
            continue;
        };
        let busy: Vec<&String> = app
            .packets
            .iter()
            .flat_map(|p| [&p.from, &p.to])
            .filter_map(|n| match n {
                Node::Sub(owner, name) if owner == pane => Some(name),
                _ => None,
            })
            .collect();
        let mut subs: Vec<(&String, bool, i64)> = agent
            .subs
            .iter()
            .filter(|(name, sub)| {
                sub.running(App::now_ms())
                    || now - sub.last_ms < RECENT_SUB_MS
                    || busy.contains(name)
            })
            .map(|(name, sub)| (name, sub.running(App::now_ms()), sub.last_ms))
            .collect();
        subs.sort_by_key(|s| Reverse((s.1, s.2)));
        if subs.is_empty() {
            continue;
        }
        let card = &placed[&Node::Pane(*pane)];
        let (mut dx, mut dy) = (center.0 - card.center.0, center.1 - card.center.1);
        let length = (dx * dx + dy * dy).sqrt();
        if length < 1.0 {
            (dx, dy) = (0.0, 1.0);
        } else {
            (dx, dy) = (dx / length, dy / length);
        }
        let extent = dx.abs() * cw as f64 / 2.0 + dy.abs() * ch as f64 / 2.0 * 2.0;
        let base = (
            card.center.0 + dx * (extent + 4.0),
            card.center.1 + dy * (extent / 2.0 + 2.0),
        );
        let shown = subs.len().min(limit);
        for (i, (name, _, _)) in subs.iter().take(shown).enumerate() {
            let label = name.rsplit('.').next().unwrap_or(name);
            let width = (label.width() as u16 + 3).min(18);
            let y = base.1 + i as f64 - (shown as f64 - 1.0) / 2.0;
            let rect = clamp_rect(
                Rect::new(
                    (base.0 - width as f64 / 2.0).max(0.0) as u16,
                    y.max(0.0) as u16,
                    width,
                    1,
                ),
                area,
            );
            chips.push((Node::Sub(*pane, (*name).clone()), rect));
        }
    }
    chips
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    buf: &mut Buffer,
    rect: Rect,
    agent: &Agent,
    size: Size,
    selected: bool,
    flash: Option<Color>,
    spin: &str,
    theme: Theme,
) {
    let (icon, label, description) = agent.label();
    let working = agent.present && agent.pane.working;
    let state_color = if !agent.present {
        Color::DarkGray
    } else if working {
        Color::Green
    } else {
        Color::Gray
    };
    let border_color = flash.unwrap_or(if selected {
        Color::Cyan
    } else if working {
        Color::Green
    } else {
        Color::DarkGray
    });
    let name = format!("{icon}{}{label}", if icon.is_empty() { "" } else { " " });
    if size == Size::Chip {
        let glyph = if !agent.present {
            "✕"
        } else if working {
            spin
        } else {
            "●"
        };
        let mut style = theme.fg(border_color);
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        // Clear the whole row first: packets fly under chips, and a short name leaves cells.
        buf.set_string(
            rect.x,
            rect.y,
            " ".repeat(rect.width as usize),
            Style::new(),
        );
        buf.set_string(
            rect.x,
            rect.y,
            fit(&format!("{glyph} {name}"), rect.width as usize),
            style,
        );
        return;
    }
    let border = if selected {
        BorderType::Double
    } else if !agent.present {
        BorderType::Plain
    } else {
        BorderType::Rounded
    };
    let tab = agent.pane.tab_name.clone().unwrap_or_else(|| {
        if agent.pane.tab > 0 {
            format!("tab {}", agent.pane.tab)
        } else {
            String::new()
        }
    });
    let mut block = Block::bordered()
        .border_type(border)
        .border_style(theme.fg(border_color))
        .title_top(Line::from(Span::styled(
            format!(" {} ", fit(&name, rect.width as usize - 4)),
            Style::new().add_modifier(Modifier::BOLD),
        )));
    if size == Size::Card && !tab.is_empty() {
        block = block
            .title_top(Line::from(Span::styled(format!(" {tab} "), theme.dim())).right_aligned());
    }
    let inner = block.inner(rect);
    ratatui::widgets::Widget::render(Clear, rect, buf);
    ratatui::widgets::Widget::render(block, rect, buf);
    let width = inner.width as usize;
    let state = state_line(agent, spin, width);
    let mut rows: Vec<Line> = Vec::new();
    // A title without `NAME: description` is all label already; repeating it adds nothing.
    if size == Size::Card && !description.is_empty() {
        rows.push(Line::from(Span::raw(fit(&description, width))));
    }
    rows.push(Line::from(Span::styled(state, theme.fg(state_color))));
    let subs = agent
        .subs
        .values()
        .filter(|s| s.running(App::now_ms()))
        .count();
    let mut place = agent.repo();
    if let Some(cli) = &agent.pane.cli {
        place.push_str(&format!(" · {cli}"));
    }
    if subs > 0 {
        place.push_str(&format!(" · {subs} sub"));
    }
    rows.push(Line::from(Span::styled(fit(&place, width), theme.dim())));
    if size == Size::Card {
        let usage = if agent.tokens > 0 {
            format!("{} tok", tokens(agent.tokens))
        } else {
            String::new()
        };
        let left = match agent.todo {
            Some((done, total)) => format!("{} {done}/{total}", bar(done, total, 5)),
            None => agent
                .model
                .as_deref()
                .map(|m| m.rsplit('/').next().unwrap_or(m).to_string())
                .unwrap_or_default(),
        };
        let gap = width.saturating_sub(left.width() + usage.width());
        rows.push(Line::from(vec![
            Span::styled(
                fit(&left, width.saturating_sub(usage.width() + 1)),
                theme.fg(Color::Blue),
            ),
            Span::raw(" ".repeat(gap.max(1))),
            Span::styled(usage, theme.dim()),
        ]));
    }
    ratatui::widgets::Widget::render(Paragraph::new(rows), inner, buf);
}

fn state_line(agent: &Agent, spin: &str, width: usize) -> String {
    if !agent.present {
        return fit("✕ pane closed", width);
    }
    if !agent.is_agent() {
        return fit(
            &format!("{} shell pane", if agent.pane.working { spin } else { "·" }),
            width,
        );
    }
    if agent.pane.working {
        let doing = match (agent.tool(), &agent.pane.cli) {
            (Some((tool, intent)), _) if !intent.is_empty() => format!("{tool} · {intent}"),
            (Some((tool, _)), _) => tool.to_string(),
            (None, Some(cli)) => format!("{cli} working"),
            (None, None) => "thinking".to_string(),
        };
        return fit(&format!("{spin} {doing}"), width);
    }
    fit(
        &format!("· idle {}", duration(agent.pane.quiet_ms as i64)),
        width,
    )
}

fn draw_chip(buf: &mut Buffer, rect: Rect, app: &App, node: &Node, spin: &str, theme: Theme) {
    let Some(sub) = sub_of(app, node) else { return };
    let running = sub.running(App::now_ms());
    let glyph = if running {
        spin
    } else if sub.done {
        "✓"
    } else {
        "·"
    };
    let name = app.model.name(node);
    let color = app
        .flashes
        .get(node)
        .map(|(_, c)| *c)
        .unwrap_or(if running {
            Color::Green
        } else {
            Color::DarkGray
        });
    buf.set_string(
        rect.x,
        rect.y,
        fit(&format!("{glyph} {name}"), rect.width as usize),
        theme.fg(color),
    );
}

/// Packets in flight, plus a waiting mark on messages sent but not yet seen by the recipient.
fn draw_traffic(
    buf: &mut Buffer,
    area: Rect,
    app: &App,
    placed: &HashMap<Node, Placed>,
    theme: Theme,
) {
    let position = |node: &Node| {
        placed
            .get(node)
            .or_else(|| placed.get(&Node::Pane(node.pane())))
    };
    let now = Instant::now();
    for packet in &app.packets {
        let (Some(from), Some(to)) = (position(&packet.from), position(&packet.to)) else {
            continue;
        };
        let Some((t0, t1)) = clip(from, to) else {
            continue;
        };
        let progress = ((now - packet.start).as_millis() as f64 / PACKET_MS as f64).clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        let t = t0 + (t1 - t0) * eased;
        for k in (1..=3).rev() {
            let back = t - (t1 - t0) * 0.045 * k as f64;
            if back > t0 {
                put(
                    buf,
                    area,
                    lerp(from.center, to.center, back),
                    "·",
                    theme.fg(packet.color),
                );
            }
        }
        put(
            buf,
            area,
            lerp(from.center, to.center, t),
            packet.glyph,
            theme.fg(packet.color).add_modifier(Modifier::BOLD),
        );
    }
    if !matches!(app.mode, Mode::Live) {
        return;
    }
    let wall = App::now_ms();
    let blink = !app.animate || (wall / 500) % 2 == 0;
    for message in app.model.messages.iter().filter(|m| waiting(app, m, wall)) {
        if app.packets.iter().any(|p| p.seq == message.seq) || !blink {
            continue;
        }
        let (Some(from), Some(to)) = (position(&message.from), position(&message.to)) else {
            continue;
        };
        if let Some((t0, t1)) = clip(from, to) {
            put(
                buf,
                area,
                lerp(from.center, to.center, t1 - (t1 - t0) * 0.04),
                "◌",
                theme.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            );
        }
    }
}

/// Sent recently to an omp pane, and not yet in its transcript. Other agents' deliveries cannot
/// be observed, so they never wait.
fn waiting(app: &App, message: &Message, now: i64) -> bool {
    message.kind == Kind::Pane
        && !message.failed
        && message.delivered.is_none()
        && message.sent.is_some_and(|s| now - s < WAIT_MS)
        && observable(app, message)
}

fn observable(app: &App, message: &Message) -> bool {
    app.model
        .agents
        .get(&message.to.pane())
        .is_some_and(Agent::has_transcript)
}

/// The part of the center-to-center segment outside both node rectangles.
fn clip(from: &Placed, to: &Placed) -> Option<(f64, f64)> {
    let inside = |rect: Rect, (x, y): (f64, f64)| {
        rect.contains(Position::new(x.max(0.0) as u16, y.max(0.0) as u16))
    };
    let steps = 240;
    let point = |i: usize| lerp(from.center, to.center, i as f64 / steps as f64);
    let start = (0..=steps).find(|i| !inside(from.rect, point(*i)))?;
    let end = (0..=steps).rev().find(|i| !inside(to.rect, point(*i)))?;
    (start < end).then(|| (start as f64 / steps as f64, end as f64 / steps as f64))
}

fn put(buf: &mut Buffer, area: Rect, (x, y): (f64, f64), glyph: &str, style: Style) {
    let position = Position::new(x.max(0.0) as u16, y.max(0.0) as u16);
    if area.contains(position)
        && let Some(cell) = buf.cell_mut(position)
    {
        cell.set_symbol(glyph).set_style(style);
    }
}

fn draw_panel(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    // Inspector 40%, feed 60%: stacked when the panel is tall enough, else side by side. Stacked,
    // the inspector takes at least 8 rows and the feed needs 5 (3 messages) under it.
    let side_by_side = area.height < 20 && (area.width >= 80 || area.height < 13);
    let (inspector, feed) = if side_by_side {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
                .areas(area);
        (left, right)
    } else {
        let [top, bottom] = Layout::vertical([
            Constraint::Length((area.height * 2 / 5).max(8).min(area.height)),
            Constraint::Min(0),
        ])
        .areas(area);
        (top, bottom)
    };
    draw_inspector(frame, inspector, app, theme);
    draw_feed(frame, feed, app, theme);
}

fn draw_inspector(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    app.hit_inspector = area;
    // A feed row selected with the feed focused shows its details here.
    let message = app
        .feed_selected
        .filter(|seq| app.focus == Focus::Feed && app.model.message(*seq).is_some());
    if app.inspected != message {
        app.inspected = message;
        app.inspector_scroll = 0;
    }
    let focused = app.focus == Focus::Graph;
    let selected = app.selected.and_then(|p| app.model.agents.get(&p));
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            theme.fg(Color::Cyan)
        } else {
            theme.dim()
        })
        .title(Span::styled(
            if message.is_some() {
                " message · Esc: back "
            } else if selected.is_some() {
                " agent "
            } else {
                " swarm "
            },
            Style::new().add_modifier(Modifier::BOLD),
        ));
    if let Some(seq) = message {
        let lines = message_lines(app, seq, theme, block.inner(area).width);
        let (_, scroll) = scrolled(frame, area, block, lines, app.inspector_scroll, theme);
        app.inspector_scroll = scroll;
        return;
    }
    let Some(agent) = selected else {
        let width = block.inner(area).width;
        let lines = overview(app, theme, width);
        let (_, scroll) = scrolled(frame, area, block, lines, app.inspector_scroll, theme);
        app.inspector_scroll = scroll;
        return;
    };
    let (icon, label, description) = agent.label();
    let spin = spinner(app.animate);
    let key = |text: &'static str| Span::styled(format!("{text:<6}"), theme.dim());
    let state = if !agent.present {
        Span::styled("closed", theme.dim())
    } else if agent.pane.working {
        Span::styled(format!("{spin} working"), theme.fg(Color::Green))
    } else {
        Span::styled(
            format!("idle {}", duration(agent.pane.quiet_ms as i64)),
            theme.fg(Color::Gray),
        )
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{icon} {label}  "),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        state,
    ])];
    if !description.is_empty() {
        lines.push(Line::from(description));
    }
    let tab = agent
        .pane
        .tab_name
        .clone()
        .map(|n| format!("tab {} {n}", agent.pane.tab))
        .unwrap_or_else(|| format!("tab {}", agent.pane.tab));
    // Underlined: a click copies the id (see the hit area below).
    let id = agent.pane.id.to_string();
    lines.push(Line::from(vec![
        key("pane"),
        Span::styled(id.clone(), Style::new().add_modifier(Modifier::UNDERLINED)),
        Span::raw(format!(" · {tab} · {}", agent.pane.session)),
    ]));
    if !agent.pane.cwd.is_empty() {
        lines.push(Line::from(vec![
            key("cwd"),
            Span::raw(home(&agent.pane.cwd)),
        ]));
    }
    if let Some(cli) = &agent.pane.cli {
        lines.push(Line::from(vec![
            key("agent"),
            Span::raw(format!(
                "{cli} · no omp transcript, so deliveries to it are not observable"
            )),
        ]));
    }
    if let Some(model) = &agent.model {
        lines.push(Line::from(vec![key("model"), Span::raw(model.clone())]));
    }
    if let Some((tool, intent)) = agent.tool().filter(|_| agent.pane.working) {
        lines.push(Line::from(vec![
            key("tool"),
            Span::styled(format!("{tool} {intent}"), theme.fg(Color::Green)),
        ]));
    }
    if let Some((done, total)) = agent.todo {
        lines.push(Line::from(vec![
            key("todo"),
            Span::styled(
                format!("{} {done}/{total}", bar(done, total, 10)),
                theme.fg(Color::Blue),
            ),
        ]));
    }
    if agent.tokens > 0 {
        let cost = if agent.cost > 0.0 {
            format!(" · ${:.2}", agent.cost)
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            key("usage"),
            Span::raw(format!("{} tokens{cost}", tokens(agent.tokens))),
        ]));
    }
    let id = agent.pane.id;
    let mine: Vec<&Message> = app
        .model
        .messages
        .iter()
        .filter(|m| m.kind == Kind::Pane && !m.failed && app.visible_message(m))
        .filter(|m| m.from.pane() == id || m.to.pane() == id)
        .collect();
    if !mine.is_empty() {
        let sent = mine.iter().filter(|m| m.from.pane() == id).count();
        // Per peer: messages and the latest one's time.
        let mut peers: HashMap<PaneId, (usize, i64)> = HashMap::new();
        for m in &mine {
            let peer = peers
                .entry(if m.from.pane() == id {
                    m.to.pane()
                } else {
                    m.from.pane()
                })
                .or_default();
            peer.0 += 1;
            peer.1 = peer.1.max(m.time());
        }
        // Most messages first, then most recent, then pane id: a stable order across frames.
        let mut peers: Vec<_> = peers.into_iter().collect();
        peers.sort_by_key(|(pane, (n, last))| (Reverse(*n), Reverse(*last), *pane));
        let names: Vec<String> = peers
            .iter()
            .take(4)
            .map(|(p, (n, _))| format!("{} ×{n}", app.model.name(&Node::Pane(*p))))
            .collect();
        lines.push(Line::from(vec![
            key("talk"),
            Span::raw(format!(
                "↑{sent} ↓{} · {}",
                mine.len() - sent,
                names.join(", ")
            )),
        ]));
        let pending = mine
            .iter()
            .filter(|m| m.from.pane() == id && waiting(app, m, App::now_ms()))
            .count();
        if pending > 0 {
            lines.push(Line::from(vec![
                key(""),
                Span::styled(
                    format!("◌ {pending} sent, not yet in the recipient's transcript"),
                    theme.fg(Color::Yellow),
                ),
            ]));
        }
    }
    if !agent.subs.is_empty() {
        let now = App::now_ms();
        let mut subs: Vec<_> = agent.subs.iter().collect();
        subs.sort_by_key(|(_, s)| Reverse((s.running(now), s.last_ms)));
        let running = subs.iter().filter(|(_, s)| s.running(now)).count();
        lines.push(Line::from(vec![
            key("subs"),
            Span::raw(format!("{running} running · {} total", subs.len())),
        ]));
        for (name, sub) in subs.iter().take(4) {
            let glyph = if sub.running(now) {
                spin
            } else if sub.done {
                "✓"
            } else {
                "·"
            };
            let tool = sub
                .tool
                .clone()
                .filter(|_| sub.running(now))
                .unwrap_or_default();
            let style = if sub.running(now) {
                theme.fg(Color::Green)
            } else {
                theme.dim()
            };
            lines.push(Line::from(Span::styled(
                format!("  {glyph} {name} {tool}"),
                style,
            )));
        }
    }
    if let Some((at, text)) = &agent.last_reply {
        let first = text
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            key("said"),
            Span::styled(format!("{} ", clock(*at)), theme.dim()),
            Span::raw(first.to_string()),
        ]));
    }
    let pane = agent.pane.id;
    let (inner, scroll) = scrolled(frame, area, block, lines, app.inspector_scroll, theme);
    app.inspector_scroll = scroll;
    // Lines above may wrap, so find the id's row in what was drawn.
    let buf = frame.buffer_mut();
    let prefix = format!("{:<6}{id}", "pane");
    for y in inner.top()..inner.bottom() {
        let row: String = (inner.left()..inner.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        if row.starts_with(&prefix) {
            let width = (prefix.len() as u16).min(inner.width);
            app.hit_copy = Some((Rect::new(inner.x, y, width, 1), pane));
            break;
        }
    }
}

/// Draws `lines` wrapped in `block`, scrolled down `scroll` rows (clamped to the content), with a
/// scrollbar when they overflow. Returns the inner area and the clamped scroll.
fn scrolled(
    frame: &mut Frame,
    area: Rect,
    block: Block,
    lines: Vec<Line>,
    scroll: u16,
    theme: Theme,
) -> (Rect, u16) {
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner.width);
    let scroll = scroll.min(total.saturating_sub(inner.height as usize) as u16);
    frame.render_widget(block, area);
    frame.render_widget(paragraph.scroll((scroll, 0)), inner);
    scrollbar(
        frame,
        area,
        total,
        inner.height as usize,
        scroll as usize,
        theme,
    );
    (inner, scroll)
}

/// The swarm at a glance, shown when no agent is selected: who works on what, cross-pane sends
/// that need a look, the busiest links, who has gone quiet, and the last message.
fn overview(app: &App, theme: Theme, width: u16) -> Vec<Line<'static>> {
    let now = App::now_ms();
    let spin = spinner(app.animate);
    let key = |text: &'static str| Span::styled(format!("{text:<8}"), theme.dim());
    let name = |pane: PaneId| app.model.name(&Node::Pane(pane));
    let agents: Vec<&Agent> = app
        .visible_agents()
        .iter()
        .filter_map(|p| app.model.agents.get(p))
        .filter(|a| a.present && (a.is_agent() || a.pane.cli.is_some()))
        .collect();
    let mut lines = Vec::new();

    let working: Vec<&&Agent> = agents.iter().filter(|a| a.pane.working).collect();
    if working.is_empty() {
        lines.push(Line::from(vec![
            key("working"),
            Span::styled("nobody", theme.dim()),
        ]));
    }
    for (i, agent) in working.iter().enumerate() {
        lines.push(Line::from(vec![
            if i == 0 { key("working") } else { key("") },
            Span::styled(
                format!("{} ", name(agent.pane.id)),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(state_line(agent, spin, 60), theme.fg(Color::Green)),
        ]));
    }

    // Cross-pane sends from the last hour, by what is known of their delivery (as in the feed).
    let sends: Vec<&Message> = app
        .model
        .messages
        .iter()
        .filter(|m| m.kind == Kind::Pane && app.visible_message(m))
        .collect();
    let (mut arrived, mut pending, mut failed, mut unseen, mut unknown) = (0, 0, 0, 0, 0);
    for m in sends.iter().filter(|m| now - m.time() < 60 * 60_000) {
        if m.failed {
            failed += 1;
        } else if m.delivered.is_some() {
            arrived += 1;
        } else if waiting(app, m, now) {
            pending += 1;
        } else if observable(app, m) {
            unseen += 1;
        } else {
            // The recipient has no omp transcript: delivery cannot be seen either way.
            unknown += 1;
        }
    }
    let mut delivery = vec![key("last 1h")];
    for (count, glyph, text, color) in [
        (arrived, "✓", "arrived", Color::Green),
        (pending, "◌", "on the way", Color::Yellow),
        (failed, "✗", "failed", Color::LightRed),
        (unseen, "?", "never seen", Color::LightRed),
        (unknown, "·", "not observable", Color::DarkGray),
    ] {
        if count > 0 {
            delivery.push(Span::styled(
                format!("{glyph} {count} {text}  "),
                theme.fg(color),
            ));
        }
    }
    if delivery.len() == 1 {
        delivery.push(Span::styled("no cross-pane sends", theme.dim()));
    }
    lines.push(Line::from(delivery));

    // Busiest pairs, either direction.
    let mut links: HashMap<(PaneId, PaneId), (usize, i64)> = HashMap::new();
    for m in sends
        .iter()
        .filter(|m| !m.failed && m.from.pane() != m.to.pane())
    {
        let (a, b) = (m.from.pane(), m.to.pane());
        let link = links.entry((a.min(b), a.max(b))).or_default();
        link.0 += 1;
        link.1 = link.1.max(m.time());
    }
    let mut links: Vec<_> = links.into_iter().collect();
    links.sort_by_key(|(pair, (n, last))| (Reverse(*n), Reverse(*last), *pair));
    for (i, ((a, b), (n, _))) in links.iter().take(3).enumerate() {
        lines.push(Line::from(vec![
            if i == 0 { key("links") } else { key("") },
            Span::raw(format!(
                "{} ↔ {} ×{n}",
                short(&name(*a), 14),
                short(&name(*b), 14)
            )),
        ]));
    }

    let mut quiet: Vec<&&Agent> = agents
        .iter()
        .filter(|a| !a.pane.working && a.pane.quiet_ms >= 30 * 60_000)
        .collect();
    quiet.sort_by_key(|a| Reverse(a.pane.quiet_ms));
    if !quiet.is_empty() {
        let names: Vec<String> = quiet.iter().take(4).map(|a| name(a.pane.id)).collect();
        let more = quiet.len().saturating_sub(4);
        let more = if more > 0 {
            format!(" +{more}")
        } else {
            String::new()
        };
        lines.push(Line::from(vec![
            key("quiet"),
            Span::styled(
                format!("{}{more} · idle 30m+", names.join(", ")),
                theme.dim(),
            ),
        ]));
    }

    if let Some(last) = sends
        .iter()
        .filter(|m| !m.failed)
        .max_by_key(|m| (m.time(), m.seq))
    {
        let text = last.text.lines().next().unwrap_or_default();
        lines.push(Line::from(vec![
            key("last"),
            Span::styled(format!("{} ", clock(last.time())), theme.dim()),
            Span::styled(
                format!(
                    "{}→{} ",
                    short(&app.model.name(&last.from), 10),
                    short(&app.model.name(&last.to), 10)
                ),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            // About two panel rows; selecting the message in the feed shows all of it.
            Span::raw(fit(text, 50)),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Click a card or press ←/→ to see one agent and its messages only. Click a message, or Tab then ↑, to see its details here.",
        theme.dim(),
    ));
    // Rows of two or more spans start with their label; wrap them under the label column.
    lines
        .into_iter()
        .flat_map(|line| match line.spans.len() {
            0 | 1 => vec![line],
            _ => {
                let mut spans = line.spans;
                let label = spans.remove(0);
                hang(label, spans, width)
            }
        })
        .collect()
}

/// `label` then `content`, wrapped by hand at spaces to `width`, with continuation rows indented
/// past the label (a paragraph's own wrap would start them at the left edge).
fn hang(label: Span<'static>, content: Vec<Span<'static>>, width: u16) -> Vec<Line<'static>> {
    let indent = label.width();
    let room = (width as usize).saturating_sub(indent).max(1);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0;
    for span in content {
        for word in span.content.split_inclusive(' ') {
            let mut rest = word;
            while !rest.is_empty() {
                if used > 0 && used + rest.trim_end().width() > room {
                    rows.push(Vec::new());
                    used = 0;
                }
                // As much as fits; a word longer than a whole row is cut by characters.
                let mut end = 0;
                let mut taken = 0;
                for (i, c) in rest.char_indices() {
                    let w = c.width().unwrap_or(0);
                    if used + taken + w > room && end > 0 {
                        break;
                    }
                    taken += w;
                    end = i + c.len_utf8();
                }
                rows.last_mut()
                    .expect("rows starts non-empty")
                    .push(Span::styled(rest[..end].to_string(), span.style));
                used += taken;
                rest = &rest[end..];
            }
        }
    }
    rows.into_iter()
        .enumerate()
        .map(|(i, mut row)| {
            let lead = if i == 0 {
                label.clone()
            } else {
                Span::raw(" ".repeat(indent))
            };
            row.insert(0, lead);
            Line::from(row)
        })
        .collect()
}

fn draw_feed(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    app.hit_feed_area = area;
    let focused = app.focus == Focus::Feed;
    let mut scope = app
        .selected
        .map(|p| format!("{} · Esc: all", app.model.name(&Node::Pane(p))))
        .unwrap_or_else(|| "all".to_string());
    if app.cross_pane_only {
        scope = format!("cross-pane · {scope}");
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            theme.fg(Color::Cyan)
        } else {
            theme.dim()
        })
        .title(Span::styled(
            format!(" messages · {scope} "),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let feed: Vec<Message> = app.feed().into_iter().cloned().collect();
    if feed.is_empty() {
        let text = match app.selected {
            Some(pane) => format!(
                "No messages to or from {}.\nEsc shows every agent's.",
                app.model.name(&Node::Pane(pane))
            ),
            None => "No messages yet. Agents talk with\n`tern run <pane> \"From $TERN_PANE …\"`,\nIRC, and subagents.".to_string(),
        };
        frame.render_widget(Paragraph::new(text).style(theme.dim()), inner);
        return;
    }
    let rows = inner.height as usize;
    let selected = app
        .feed_selected
        .and_then(|s| feed.iter().position(|m| m.seq == s));
    let max_start = feed.len().saturating_sub(rows);
    // Scrolled by the wheel: keep that top row. Else keep the selected row in view, scrolling no
    // further than needed, so a clicked row stays under the pointer; with none, show the newest.
    let start = match app
        .feed_top
        .and_then(|s| feed.iter().position(|m| m.seq == s))
    {
        Some(top) => top.min(max_start),
        None => match selected {
            Some(selected) => app
                .feed_view
                .0
                .max((selected + 1).saturating_sub(rows))
                .min(selected)
                .min(max_start),
            None => max_start,
        },
    };
    app.feed_view = (start, rows);
    let wall = App::now_ms();
    for (row, message) in feed.iter().skip(start).take(rows).enumerate() {
        let y = inner.y + row as u16;
        let (glyph, color) = packet_style(message);
        let status = match message.kind {
            Kind::Pane if message.failed => ("✗", Color::LightRed),
            Kind::Pane if message.delivered.is_some() => ("✓", Color::Green),
            Kind::Pane if waiting(app, message, wall) => ("◌", Color::Yellow),
            Kind::Pane if !observable(app, message) => ("·", Color::DarkGray),
            Kind::Pane => ("?", Color::LightRed),
            _ => (" ", Color::Reset),
        };
        let route = format!(
            "{}→{}",
            short(&app.model.name(&message.from), 9),
            short(&app.model.name(&message.to), 9)
        );
        let text = message.text.lines().next().unwrap_or_default();
        let text = match message.kind {
            Kind::Irc => match (message.outcome.as_deref(), text.is_empty()) {
                (Some(outcome), false) => format!("irc {outcome} · {text}"),
                (Some(outcome), true) => format!("irc {outcome}"),
                (None, false) => format!("irc · {text}"),
                (None, true) => "irc".into(),
            },
            Kind::Spawn => format!("spawn {}", text),
            Kind::Yield => "yield".into(),
            Kind::Pane => text.to_string(),
        };
        let is_selected = selected == Some(start + row);
        let mut line = Line::from(vec![
            Span::styled(format!("{} ", clock(message.time())), theme.dim()),
            Span::styled(glyph, theme.fg(color)),
            Span::styled(status.0, theme.fg(status.1)),
            Span::raw(" "),
            Span::styled(route, Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::raw(text),
        ]);
        if is_selected {
            line = line.style(Style::new().add_modifier(Modifier::REVERSED));
        }
        let rect = Rect::new(inner.x, y, inner.width, 1);
        frame.render_widget(Paragraph::new(line), rect);
        app.hit_feed.push((rect, message.seq));
    }
    scrollbar(frame, area, feed.len(), rows, start, theme);
}

/// A scrollbar on a bordered box's right edge, when its content is taller than its rows.
fn scrollbar(frame: &mut Frame, area: Rect, total: usize, rows: usize, top: usize, theme: Theme) {
    if total <= rows {
        return;
    }
    // Positions run 0..=total-rows, so the thumb reaches the bottom at the last one.
    let mut state = ScrollbarState::new(total - rows + 1)
        .position(top)
        .viewport_content_length(rows);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .style(theme.dim()),
        area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
}

fn draw_help(frame: &mut Frame, area: Rect, app: &mut App, theme: Theme) {
    let rows = [
        (
            "←/→  ↑/↓  h/l",
            "select agent (feed rows when the feed has focus)",
        ),
        ("Tab", "switch focus: graph ↔ message feed"),
        (
            "wheel  PgUp/Dn",
            "scroll the panel under the pointer · the focused panel (a selected message's details)",
        ),
        (
            "Enter",
            "graph: focus that pane in Tern · feed: show the newest message's details",
        ),
        ("m", "type a message into the selected pane's composer"),
        ("r", "replay recorded history ↔ back to live"),
        ("space  + -", "pause replay · replay speed"),
        ("f", "feed: cross-pane messages only, on/off"),
        (
            "Esc",
            "feed: back from a message's details · else deselect the agent",
        ),
        ("a", "show every pane, not only omp agents"),
        (
            "c",
            "copy the selected pane's id, for tern run / send / capture",
        ),
        (
            "click",
            "card: select it and copy its pane id · feed row: select it",
        ),
        ("q  Ctrl-C", "quit"),
    ];
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:<15}"), theme.fg(Color::Cyan)),
                Span::raw(*v),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    for (glyph, color, text) in [
        (
            "◆",
            Color::Cyan,
            "cross-pane message (tern run / tern send)",
        ),
        (
            "●",
            Color::Magenta,
            "reply to the last message the other way",
        ),
        (
            "•",
            Color::Yellow,
            "omp IRC between an agent and its subagents",
        ),
        (
            "✦ ✓",
            Color::LightBlue,
            "subagent spawned · subagent yielded",
        ),
        (
            "◌",
            Color::Yellow,
            "sent, not yet in the recipient's transcript",
        ),
        (
            "✓ ✗ ?",
            Color::Green,
            "delivered · send failed · never seen (3 min)",
        ),
        (
            "·",
            Color::DarkGray,
            "recipient has no omp transcript: delivery not observable",
        ),
        (
            "─ 3",
            Color::Blue,
            "3 messages between two agents; bright <90 s, blue <15 min",
        ),
    ] {
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph:<6}"), theme.fg(color)),
            Span::raw(text),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Delivered means the text reached the recipient's transcript; it does not mean the agent acted on it.", theme.dim()));
    // As tall as its wrapped rows; on a shorter screen it scrolls.
    let width = 76.min(area.width);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let rows = paragraph.line_count(width.saturating_sub(2));
    let height = rows.saturating_add(2).min(area.height as usize) as u16;
    let title = if rows.saturating_add(2) > height as usize {
        " ternwatch keys · ↑↓ scroll · Esc closes "
    } else {
        " ternwatch keys · Esc closes "
    };
    let rect = center_rect(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title);
    let visible = block.inner(rect).height as usize;
    let scroll = (app.help_scroll as usize).min(rows.saturating_sub(visible)) as u16;
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), rect);
    scrollbar(frame, rect, rows, visible, scroll as usize, theme);
    app.help_scroll = scroll;
}

/// A message's details for the inspector: kind, ends, times and delivery, each wrapped under its
/// label, then the full text.
fn message_lines(app: &App, seq: u64, theme: Theme, width: u16) -> Vec<Line<'static>> {
    let Some(message) = app.model.message(seq) else {
        return Vec::new();
    };
    let endpoint = |node: &Node| match node {
        Node::Pane(p) => format!("{} (pane {p})", app.model.name(node)),
        Node::Sub(p, name) => format!("{name} (subagent of {})", app.model.name(&Node::Pane(*p))),
    };
    let kind = match message.kind {
        Kind::Pane => "cross-pane message",
        Kind::Irc => "omp IRC message",
        Kind::Spawn => "subagent spawn",
        Kind::Yield => "subagent yield",
    };
    let mut rows = vec![
        ("kind", Span::raw(kind)),
        ("from", Span::raw(endpoint(&message.from))),
        ("to", Span::raw(endpoint(&message.to))),
        (
            "sent",
            Span::raw(
                message
                    .sent
                    .map(stamp)
                    .unwrap_or_else(|| "not in the sender's transcript".into()),
            ),
        ),
    ];
    if message.kind == Kind::Pane {
        // The feed's status glyphs and colors, spelled out.
        let (text, color) = match (message.delivered, message.sent) {
            (None, _) if message.failed => ("✗ send failed".into(), Color::LightRed),
            (Some(d), Some(s)) => (
                format!("✓ {} (+{})", stamp(d), duration(d - s)),
                Color::Green,
            ),
            (Some(d), None) => (format!("✓ {}", stamp(d)), Color::Green),
            (None, _) if waiting(app, message, App::now_ms()) => (
                "◌ on the way: not in the recipient's transcript yet".into(),
                Color::Yellow,
            ),
            (None, _) if !observable(app, message) => (
                "· not observable: the recipient has no omp transcript".into(),
                Color::DarkGray,
            ),
            (None, _) => (
                "? not seen in the recipient's transcript".into(),
                Color::LightRed,
            ),
        };
        rows.push(("delivered", Span::styled(text, theme.fg(color))));
    }
    if let Some(outcome) = &message.outcome {
        rows.push(("receipt", Span::raw(outcome.clone())));
    }
    if message.reply_to.is_some() {
        rows.push((
            "reply",
            Span::raw("answers the previous message the other way"),
        ));
    } else if message.replied {
        rows.push(("reply", Span::raw("answered")));
    }
    let mut lines: Vec<Line<'static>> = rows
        .into_iter()
        .flat_map(|(key, value)| {
            hang(
                Span::styled(format!("{key:<10}"), theme.dim()),
                vec![value],
                width,
            )
        })
        .collect();
    lines.push(Line::raw(""));
    lines.extend(message.text.lines().map(|line| Line::raw(line.to_string())));
    lines
}

fn sub_of<'a>(app: &'a App, node: &Node) -> Option<&'a crate::model::Sub> {
    match node {
        Node::Sub(pane, name) => app.model.agents.get(pane)?.subs.get(name),
        Node::Pane(_) => None,
    }
}

fn spinner(animate: bool) -> &'static str {
    if !animate {
        return "●";
    }
    SPINNER[((App::now_ms() / 80) % SPINNER.len() as i64) as usize]
}

fn center_of(rect: Rect) -> (f64, f64) {
    (
        rect.x as f64 + rect.width as f64 / 2.0,
        rect.y as f64 + rect.height as f64 / 2.0,
    )
}

fn lerp((x1, y1): (f64, f64), (x2, y2): (f64, f64), t: f64) -> (f64, f64) {
    (x1 + (x2 - x1) * t, y1 + (y2 - y1) * t)
}

fn grow(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_sub(1),
        rect.y,
        rect.width + 2,
        rect.height,
    )
}

fn clamp_rect(rect: Rect, area: Rect) -> Rect {
    let width = rect.width.min(area.width);
    let height = rect.height.min(area.height);
    let x = rect.x.clamp(area.x, area.right() - width);
    let y = rect.y.clamp(area.y, area.bottom() - height);
    Rect::new(x, y, width, height)
}

fn center_rect(area: Rect, width: u16, height: u16) -> Rect {
    let (width, height) = (width.min(area.width), height.min(area.height));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Truncates to `width` terminal cells, ending in `…` when cut.
pub fn fit(text: &str, width: usize) -> String {
    let text = text.replace(['\n', '\r', '\t'], " ");
    if text.width() <= width {
        return text;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn short(text: &str, width: usize) -> String {
    fit(text, width)
}

fn bar(done: u32, total: u32, cells: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        (done as usize * cells).div_ceil(total as usize).min(cells)
    };
    format!("{}{}", "▰".repeat(filled), "▱".repeat(cells - filled))
}

fn tokens(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => format!("{:.1}k", count as f64 / 1e3),
        _ => format!("{:.1}M", count as f64 / 1e6),
    }
}

pub fn duration(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h{:02}", seconds / 3_600, seconds % 3_600 / 60),
        _ => format!("{}d", seconds / 86_400),
    }
}

fn speed(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value}")
    }
}

/// Time of day for today; month, day and minute for other days, so old rows don't read as new.
fn clock(ms: i64) -> String {
    let Some(time) = chrono::Local.timestamp_millis_opt(ms).single() else {
        return "--:--:--".into();
    };
    if time.date_naive() == chrono::Local::now().date_naive() {
        time.format("%H:%M:%S").to_string()
    } else {
        time.format("%m-%d %H:%M").to_string()
    }
}

fn stamp(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if path.starts_with(&home) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clicked_feed_row_stays_under_the_pointer() {
        use crate::transcript::Event;
        use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let row_text = |buffer: &Buffer, rect: Rect| -> String {
            (rect.x..rect.right())
                .map(|x| buffer[(x, rect.y)].symbol())
                .collect()
        };
        // 60x16 is the minimum size; its feed must still show rows to click.
        for (width, height) in [(80, 24), (60, 16)] {
            let mut app = App::new(String::new(), false, false, false);
            let sends = (1..=40)
                .map(|i| Event::ToolCall {
                    ts: i,
                    id: format!("s{i}"),
                    name: "bash".into(),
                    intent: String::new(),
                    args: serde_json::json!({"command": format!("tern run 7 'message {i}'")}),
                })
                .collect();
            app.model.apply(9, None, sends);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut render = |app: &mut App| {
                terminal.draw(|frame| draw(frame, app)).unwrap();
                terminal.backend().buffer().clone()
            };
            let before = render(&mut app);
            assert!(
                app.hit_feed.len() >= 3,
                "{width}x{height}: {} feed rows",
                app.hit_feed.len()
            );
            // Each visible row in turn: the newest, the oldest and those between.
            for i in 0..app.hit_feed.len() {
                let (rect, seq) = app.hit_feed[i];
                let text = row_text(&before, rect);
                app.handle_mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                });
                let after = render(&mut app);
                assert_eq!(app.feed_selected, Some(seq));
                assert_eq!(
                    row_text(&after, rect),
                    text,
                    "{width}x{height}: row {i} moved"
                );
                assert!(
                    after[(rect.x, rect.y)]
                        .modifier
                        .contains(Modifier::REVERSED),
                    "{width}x{height}: row {i} is not highlighted"
                );
            }
        }
    }

    #[test]
    fn page_keys_scroll_a_selected_message_longer_than_the_panel() {
        use crate::transcript::Event;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use ratatui::{Terminal, backend::TestBackend};

        let text: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
        let mut app = App::new(String::new(), false, false, false);
        app.model.apply(
            9,
            None,
            vec![Event::ToolCall {
                ts: 1,
                id: "a".into(),
                name: "bash".into(),
                intent: String::new(),
                args: serde_json::json!({"command": format!("tern run 7 '{}'", text.join("\n"))}),
            }],
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut render = |app: &mut App| {
            terminal.draw(|frame| draw(frame, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .chunks(80)
                .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        app.handle_key(KeyEvent::from(KeyCode::Tab));
        app.handle_key(KeyEvent::from(KeyCode::Up));
        let top = render(&mut app);
        assert!(top.contains("message · Esc: back"), "{top}");
        assert!(top.contains("kind") && !top.contains("line 40"), "{top}");
        for _ in 0..10 {
            app.handle_key(KeyEvent::from(KeyCode::PageDown));
            render(&mut app);
        }
        let bottom = render(&mut app);
        assert!(bottom.contains("message · Esc: back"), "{bottom}");
        assert!(
            bottom.contains("line 40") && !bottom.contains("kind"),
            "{bottom}"
        );
        for _ in 0..10 {
            app.handle_key(KeyEvent::from(KeyCode::PageUp));
            render(&mut app);
        }
        assert_eq!(render(&mut app), top);
    }

    #[test]
    fn small_help_scrolls_to_the_legend_and_disclaimer_then_reopens_at_top() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use ratatui::{Terminal, backend::TestBackend};

        for (width, height) in [(80, 24), (60, 16)] {
            let mut app = App::new(String::new(), false, false, false);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let render = |terminal: &mut Terminal<TestBackend>, app: &mut App| {
                terminal.draw(|frame| draw(frame, app)).unwrap();
                terminal
                    .backend()
                    .buffer()
                    .content()
                    .chunks(width as usize)
                    .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            app.handle_key(KeyEvent::from(KeyCode::Char('?')));
            let top = render(&mut terminal, &mut app);
            assert!(top.contains("↑↓ scroll"));
            assert!(!top.contains("agent acted on it."));
            for _ in 0..10 {
                app.handle_key(KeyEvent::from(KeyCode::PageDown));
                render(&mut terminal, &mut app);
            }
            let bottom = render(&mut terminal, &mut app);
            assert!(bottom.contains("3 messages between two agents"), "{bottom}");
            assert!(
                bottom.contains("Delivered means the text reached"),
                "{bottom}"
            );
            assert!(bottom.contains("agent acted on it."), "{bottom}");
            let end = app.help_scroll;
            app.handle_key(KeyEvent::from(KeyCode::PageDown));
            assert_eq!(render(&mut terminal, &mut app), bottom);
            assert_eq!(app.help_scroll, end);
            app.handle_key(KeyEvent::from(KeyCode::Up));
            render(&mut terminal, &mut app);
            assert_eq!(app.help_scroll, end - 1);
            app.handle_key(KeyEvent::from(KeyCode::Home));
            assert_eq!(render(&mut terminal, &mut app), top);
            app.handle_key(KeyEvent::from(KeyCode::PageDown));
            render(&mut terminal, &mut app);
            app.handle_key(KeyEvent::from(KeyCode::Esc));
            app.handle_key(KeyEvent::from(KeyCode::Char('?')));
            assert_eq!(render(&mut terminal, &mut app), top);
        }
    }

    #[test]
    fn layouts_never_overlap_and_shrink_cards_to_fit() {
        for (width, height) in [(80u16, 24u16), (74, 30), (120, 40), (200, 60)] {
            for count in 1..=24 {
                let area = Rect::new(0, 1, width, height);
                let (rects, _, _) = place(area, count);
                assert_eq!(rects.len(), count);
                for (i, a) in rects.iter().enumerate() {
                    assert!(
                        area.contains(a.as_position())
                            && a.right() <= area.right()
                            && a.bottom() <= area.bottom(),
                        "{width}x{height} n={count}"
                    );
                    for b in &rects[i + 1..] {
                        assert!(!a.intersects(*b), "{width}x{height} n={count}: {a:?} {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn mid_size_graphs_use_a_card_grid_before_one_line_chips() {
        // The graph beside the side panel in a 120x32 terminal, with a dozen agents.
        let (rects, size, ring) = place(Rect::new(0, 1, 74, 30), 12);
        assert_eq!((size, ring), (Size::Small, false));
        assert_eq!(rects.len(), 12);
    }

    #[test]
    fn hang_wraps_under_the_label_and_keeps_every_character() {
        let content = vec![
            Span::raw("TERN ● bash · "),
            Span::raw("an_unbreakable_word_longer_than_a_row and more"),
        ];
        let rows = hang(Span::raw("working "), content.clone(), 24);
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(text.iter().all(|row| row.width() <= 24), "{text:?}");
        assert!(text[0].starts_with("working TERN"), "{text:?}");
        assert!(
            text[1..]
                .iter()
                .all(|row| row.starts_with("        ") && !row[8..].starts_with(' ')),
            "{text:?}"
        );
        let joined: String = text.iter().map(|row| &row[8..]).collect();
        let original: String = content.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, original);
    }

    #[test]
    fn clock_dates_only_messages_from_other_days() {
        let now = chrono::Local::now();
        let today = clock(now.timestamp_millis());
        assert_eq!(today, now.format("%H:%M:%S").to_string());
        let earlier = now - chrono::Duration::days(3);
        assert_eq!(
            clock(earlier.timestamp_millis()),
            earlier.format("%m-%d %H:%M").to_string()
        );
    }

    #[test]
    fn fit_counts_wide_characters() {
        assert_eq!(fit("hello", 5), "hello");
        assert_eq!(fit("hello world", 6), "hello…");
        assert_eq!(fit("代码代码", 5), "代码…");
    }
}

use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::{
    env, fs,
    io::{self, Write, Stdout},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Debug)]
struct Session {
    name: String,
    windows: String,
    attached: bool,
    created: String,
    activity: String,
    active_line: String,
    preview: Vec<String>,
}

#[derive(Clone, Debug)]
struct DirNode {
    path: PathBuf,
    depth: usize,
    expanded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus { Dirs, Sessions }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirInput { Filter, Name }

enum Choice { Dir(PathBuf, String), Session(String) }

struct App {
    focus: Focus,
    dir_idx: usize,
    sess_idx: usize,
    query: String,
    dir_query: String,
    dir_input: DirInput,
    new_session: String,
    dirs: Vec<DirNode>,
    dir_cache: Vec<PathBuf>,
    cache_status: String,
    sessions: Vec<Session>,
    preview_for: Option<String>,
    last_load: Instant,
}

fn run(cmd: &mut Command) -> String {
    String::from_utf8_lossy(&cmd.output().map(|o| o.stdout).unwrap_or_default()).to_string()
}

fn tmux(args: &[&str]) -> String {
    run(Command::new("tmux").args(args))
}

fn sanitize_name(s: &str) -> String {
    let mapped: String = s.chars().map(|c| if ". /:".contains(c) { '_' } else { c }).collect();
    let out: String = mapped.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if out.is_empty() { "session".into() } else { out }
}

fn fmt_time(epoch: &str) -> String {
    if epoch.is_empty() { return String::new(); }
    // Keep this simple for now; the Python implementation formats this nicely.
    epoch.to_string()
}

fn active_pane_id(name: &str) -> Option<String> {
    let win = tmux(&["list-windows", "-t", &format!("={name}"), "-F", "#{?window_active,#{window_id},}"])
        .lines().find(|l| !l.trim().is_empty()).map(str::to_string)?;
    tmux(&["list-panes", "-t", &win, "-F", "#{?pane_active,#{pane_id},}"])
        .lines().find(|l| !l.trim().is_empty()).map(str::to_string)
}

fn capture_preview(name: &str) -> Vec<String> {
    let Some(pane) = active_pane_id(name) else { return vec!["Unable to find active pane.".into()]; };
    let out = tmux(&["capture-pane", "-epJ", "-t", &pane]);
    let mut lines: Vec<String> = out.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
    while lines.first().is_some_and(|l| strip_ansi(l).trim().is_empty()) { lines.remove(0); }
    if lines.is_empty() { vec![String::new()] } else { lines }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for c in s.chars() {
        if esc {
            if c.is_ascii_alphabetic() { esc = false; }
        } else if c == '\x1b' { esc = true; } else { out.push(c); }
    }
    out
}

fn load_sessions(preview_for: Option<&str>) -> Vec<Session> {
    let raw = tmux(&["list-sessions", "-F", "#S|#{session_windows}|#{session_attached}|#{session_created}|#{session_activity}"]);
    let mut sessions = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() < 5 { continue; }
        let name = parts[0].to_string();
        let active = tmux(&["display-message", "-p", "-t", &format!("={}:", name), "#W · #{pane_current_command} · #{pane_current_path}"])
            .trim().to_string();
        let preview = if preview_for == Some(name.as_str()) { capture_preview(&name) } else { Vec::new() };
        sessions.push(Session { name, windows: parts[1].into(), attached: parts[2] != "0", created: fmt_time(parts[3]), activity: fmt_time(parts[4]), active_line: active, preview });
    }
    sessions.sort_by_key(|s| s.name.to_lowercase());
    sessions
}

fn child_dirs(path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(path).ok().into_iter().flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.')))
        .collect();
    out.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    out.truncate(200);
    out
}

fn cache_excluded(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| matches!(n,
        ".git" | "node_modules" | "target" | "dist" | "build" | ".cache" | "Caches" |
        "DerivedData" | "Library" | "Movies" | "Music" | "Pictures"
    ))
}

fn child_dirs_for_cache(path: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(path).ok().into_iter().flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && !p.is_symlink() && !cache_excluded(p) && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.')))
        .collect();
    out.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    out.truncate(200);
    out
}

fn visible_dirs(nodes: &mut Vec<DirNode>) -> Vec<DirNode> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    fn walk(path: PathBuf, depth: usize, nodes: &mut Vec<DirNode>, out: &mut Vec<DirNode>) {
        let pos = nodes.iter().position(|n| n.path == path).unwrap_or_else(|| { nodes.push(DirNode { path: path.clone(), depth, expanded: false }); nodes.len() - 1 });
        nodes[pos].depth = depth;
        let node = nodes[pos].clone();
        out.push(node.clone());
        if node.expanded {
            for child in child_dirs(&node.path) { walk(child, depth + 1, nodes, out); }
        }
    }
    if nodes.is_empty() { nodes.push(DirNode { path: home.clone(), depth: 0, expanded: true }); }
    let mut out = Vec::new();
    walk(home, 0, nodes, &mut out);
    out
}

const MAX_CACHE_DIRS: usize = 25_000;
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

fn cache_path() -> PathBuf {
    let base = env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(".cache"));
    base.join("tmux-session-switch").join("dirs.txt")
}

fn load_dir_cache() -> Vec<PathBuf> {
    fs::read_to_string(cache_path()).ok().into_iter().flat_map(|s| {
        s.lines().take(MAX_CACHE_DIRS).map(PathBuf::from).collect::<Vec<_>>()
    }).filter(|p| p.is_dir()).collect()
}

fn cache_is_stale() -> bool {
    let Ok(meta) = fs::metadata(cache_path()) else { return true; };
    let Ok(modified) = meta.modified() else { return true; };
    SystemTime::now().duration_since(modified).map_or(true, |age| age > CACHE_TTL)
}

fn save_dir_cache(cache: &[PathBuf]) {
    let path = cache_path();
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    let tmp = path.with_extension("tmp");
    if let Ok(mut f) = fs::File::create(&tmp) {
        for p in cache.iter().take(MAX_CACHE_DIRS) {
            let _ = writeln!(f, "{}", p.display());
        }
        let _ = fs::rename(tmp, path);
    }
}

fn build_dir_cache(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if out.len() >= MAX_CACHE_DIRS { break; }
        out.push(path.clone());
        let mut children = child_dirs_for_cache(&path);
        children.reverse();
        stack.extend(children);
    }
    out.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    out.dedup();
    out
}

fn filtered_visible_dirs(nodes: &mut Vec<DirNode>, cache: &[PathBuf], query: &str) -> Vec<DirNode> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return visible_dirs(nodes);
    }

    let path_mode = q.contains('/') || q.starts_with('~');
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let normalized_q = if q.starts_with('~') {
        q.replacen('~', &home.to_string_lossy(), 1)
    } else {
        q.clone()
    };

    let mut matches: Vec<(u8, String, PathBuf)> = cache.iter()
        .filter_map(|p| {
            let full = p.to_string_lossy().to_lowercase();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("~").to_lowercase();
            let rank = if path_mode {
                if full == normalized_q { 0 }
                else if full.contains(&normalized_q) { 1 }
                else { return None }
            } else if name == q { 0 }
                else if name.contains(&q) { 1 }
                else { return None };
            Some((rank, full, p.clone()))
        })
        .collect();
    matches.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    matches.into_iter()
        .take(1000)
        .map(|(_, _, p)| DirNode { path: p, depth: 0, expanded: false })
        .collect()
}

fn switch_session(name: &str) {
    if env::var("TMUX").is_ok() {
        let client = tmux(&["display-message", "-p", "#{client_name}"]).trim().to_string();
        let mut cmd = Command::new("tmux");
        cmd.arg("switch-client");
        if !client.is_empty() { cmd.args(["-c", &client]); }
        let _ = cmd.args(["-t", &format!("={name}")]).status();
    } else { let _ = Command::new("tmux").args(["attach-session", "-t", &format!("={name}")]).status(); }
}

fn create_session(path: &Path, requested: &str) {
    let fallback = path.file_name().and_then(|n| n.to_str()).unwrap_or("home");
    let name = sanitize_name(if requested.trim().is_empty() { fallback } else { requested.trim() });
    let exists = Command::new("tmux").args(["has-session", "-t", &format!("={name}")]).status().is_ok_and(|s| s.success());
    if !exists { let _ = Command::new("tmux").args(["new-session", "-ds", &name, "-c", &path.to_string_lossy()]).status(); }
    switch_session(&name);
}

fn draw(app: &mut App, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let dirs = filtered_visible_dirs(&mut app.dirs, &app.dir_cache, &app.dir_query);
    let mut sessions = app.sessions.clone();
    if !app.query.is_empty() {
        let q = app.query.to_lowercase();
        sessions.retain(|s| s.name.to_lowercase().contains(&q) || s.active_line.to_lowercase().contains(&q));
    }
    app.dir_idx = app.dir_idx.min(dirs.len().saturating_sub(1));
    app.sess_idx = app.sess_idx.min(sessions.len().saturating_sub(1));

    terminal.draw(|f| {
        let root = f.size();
        // Actual Ghostty Cyberdyne palette:
        // background #151144, foreground #00ff92, selection #454d96.
        let bg = Color::Rgb(21, 17, 68);
        let panel = Color::Rgb(21, 17, 68);
        let surface = Color::Rgb(69, 77, 150);
        let text = Color::Rgb(0, 255, 146);
        let muted = Color::Rgb(107, 255, 221);
        let neon = Color::Rgb(0, 255, 156);
        let neon_bright = Color::Rgb(214, 252, 186);
        let neon_dim = Color::Rgb(0, 193, 114);
        let violet = Color::Rgb(255, 144, 254);
        let amber = Color::Rgb(255, 254, 213);
        let danger = Color::Rgb(255, 131, 115);
        let blue = Color::Rgb(194, 227, 255);
        let mauve = violet;
        let green = neon;
        let yellow = amber;
        let peach = muted;
        let red = danger;

        f.render_widget(Block::default().style(Style::default().bg(bg)), root);

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(8), Constraint::Length(1)])
            .split(root);
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(outer[1]);
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Min(8)])
            .split(cols[0]);

        let active_label = match app.focus { Focus::Dirs => "DIRECTORY", Focus::Sessions => "SESSION" };
        let top = Line::from(vec![
            Span::styled(" active ", Style::default().fg(bg).bg(neon).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {active_label} "), Style::default().fg(neon_bright).bg(bg).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {} ", app.cache_status), Style::default().fg(muted).bg(bg)),
        ]);
        f.render_widget(Paragraph::new(top).style(Style::default().bg(bg)), outer[0]);

        let box_style = |focused: bool, color: Color| {
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(if focused { Style::default().fg(color).add_modifier(Modifier::BOLD) } else { Style::default().fg(neon_dim) })
                .style(Style::default().bg(panel).fg(text))
        };

        let dir_block = box_style(app.focus == Focus::Dirs, green)
            .title(Line::from(vec![
                Span::styled("  ", Style::default().fg(green).add_modifier(Modifier::BOLD)),
                Span::styled("project tree", Style::default().fg(text).add_modifier(Modifier::BOLD)),
            ]));
        let sess_block = box_style(app.focus == Focus::Sessions, blue)
            .title(Line::from(vec![
                Span::styled("  ", Style::default().fg(blue).add_modifier(Modifier::BOLD)),
                Span::styled("sessions", Style::default().fg(text).add_modifier(Modifier::BOLD)),
                Span::styled(format!("  {} total", sessions.len()), Style::default().fg(muted)),
            ]));
        let preview_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(neon_bright))
            .style(Style::default().bg(panel).fg(text))
            .title(Line::from(vec![
                Span::styled(" 󰈙 ", Style::default().fg(peach).add_modifier(Modifier::BOLD)),
                Span::styled("preview", Style::default().fg(text).add_modifier(Modifier::BOLD)),
            ]).alignment(Alignment::Left));

        let dir_inner = dir_block.inner(left[0]);
        f.render_widget(dir_block, left[0]);
        let dir_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(dir_inner);
        let dir_view_h = dir_split[0].height as usize;
        let dir_start = if app.dir_idx >= dir_view_h { app.dir_idx + 1 - dir_view_h } else { 0 };
        let dir_items: Vec<ListItem> = dirs.iter().enumerate().skip(dir_start).take(dir_view_h).map(|(i, d)| {
            let kids = !child_dirs(&d.path).is_empty();
            let marker = if kids && d.expanded { "▾" } else if kids { "▸" } else { " " };
            let name = if d.path == dirs::home_dir().unwrap_or_default() { "~".into() } else { d.path.file_name().unwrap_or_default().to_string_lossy().into_owned() };
            let child_count = child_dirs(&d.path).len();
            let count_badge = if child_count > 0 { format!("  {child_count}") } else { String::new() };
            let indent = "  ".repeat(d.depth);
            if app.focus == Focus::Dirs && i == app.dir_idx {
                ListItem::new(Line::from(vec![
                    Span::styled(" 󰮟 ", Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{indent}{marker}  {name}{count_badge}"), Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD)),
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::styled("   ", Style::default().bg(panel)),
                    Span::styled(indent, Style::default().fg(muted).bg(panel)),
                    Span::styled(format!("{marker} "), Style::default().fg(mauve).bg(panel)),
                    Span::styled(" ", Style::default().fg(green).bg(panel)),
                    Span::styled(name, Style::default().fg(text).bg(panel)),
                    Span::styled(count_badge, Style::default().fg(muted).bg(panel)),
                ]))
            }
        }).collect();
        f.render_widget(List::new(dir_items).style(Style::default().bg(panel)), dir_split[0]);
        if dir_start > 0 {
            f.render_widget(Paragraph::new("▲").style(Style::default().fg(neon_bright).bg(panel).add_modifier(Modifier::BOLD)), Rect { x: dir_split[0].x + dir_split[0].width.saturating_sub(2), y: dir_split[0].y, width: 1, height: 1 });
        }
        if dir_start + dir_view_h < dirs.len() {
            f.render_widget(Paragraph::new("▼").style(Style::default().fg(neon_bright).bg(panel).add_modifier(Modifier::BOLD)), Rect { x: dir_split[0].x + dir_split[0].width.saturating_sub(2), y: dir_split[0].y + dir_split[0].height.saturating_sub(1), width: 1, height: 1 });
        }
        let dir_filter = if app.dir_query.is_empty() { "type to filter directories" } else { &app.dir_query };
        let new_name = if app.new_session.is_empty() { "<directory name>" } else { &app.new_session };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  search ", Style::default().fg(bg).bg(if app.focus == Focus::Dirs && app.dir_input == DirInput::Filter { green } else { surface }).add_modifier(Modifier::BOLD)),
                Span::styled(" ", Style::default().fg(if app.focus == Focus::Dirs && app.dir_input == DirInput::Filter { green } else { surface }).bg(panel)),
                Span::styled(dir_filter.to_string(), Style::default().fg(if app.dir_query.is_empty() { muted } else { text }).bg(panel).add_modifier(if app.focus == Focus::Dirs && app.dir_input == DirInput::Filter { Modifier::BOLD } else { Modifier::empty() })),
                Span::styled("  │  ", Style::default().fg(surface).bg(panel)),
                Span::styled("  name ", Style::default().fg(bg).bg(if app.focus == Focus::Dirs && app.dir_input == DirInput::Name { green } else { surface }).add_modifier(Modifier::BOLD)),
                Span::styled(" ", Style::default().fg(if app.focus == Focus::Dirs && app.dir_input == DirInput::Name { green } else { surface }).bg(panel)),
                Span::styled(new_name.to_string(), Style::default().fg(if app.new_session.is_empty() { muted } else { text }).bg(panel).add_modifier(if app.focus == Focus::Dirs && app.dir_input == DirInput::Name { Modifier::BOLD } else { Modifier::empty() })),
            ])).style(Style::default().bg(panel)),
            dir_split[1],
        );

        let sess_inner = sess_block.inner(left[1]);
        f.render_widget(sess_block, left[1]);
        let sess_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(sess_inner);
        let sess_items: Vec<ListItem> = sessions.iter().enumerate().map(|(i, s)| {
            let attached = if s.attached { " 󰈈" } else { "" };
            if app.focus == Focus::Sessions && i == app.sess_idx {
                ListItem::new(Line::from(vec![
                    Span::styled(" 󰮟 ", Style::default().fg(bg).bg(blue).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {}{}", s.name, attached), Style::default().fg(bg).bg(blue).add_modifier(Modifier::BOLD)),
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::styled("    ", Style::default().fg(blue).bg(panel)),
                    Span::styled(s.name.clone(), Style::default().fg(text).bg(panel)),
                    Span::styled(attached, Style::default().fg(yellow).bg(panel)),
                ]))
            }
        }).collect();
        f.render_widget(List::new(sess_items).style(Style::default().bg(panel)), sess_split[0]);
        let filter = if app.query.is_empty() { "type to filter sessions" } else { &app.query };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("  filter ", Style::default().fg(bg).bg(if app.focus == Focus::Sessions { blue } else { surface }).add_modifier(Modifier::BOLD)),
                Span::styled(" ", Style::default().fg(if app.focus == Focus::Sessions { blue } else { surface }).bg(panel)),
                Span::styled(filter.to_string(), Style::default().fg(if app.query.is_empty() { muted } else { text }).bg(panel).add_modifier(if app.focus == Focus::Sessions { Modifier::BOLD } else { Modifier::empty() })),
            ])).style(Style::default().bg(panel)),
            sess_split[1],
        );

        f.render_widget(preview_block, cols[1]);
        let p = Rect { x: cols[1].x + 2, y: cols[1].y + 1, width: cols[1].width.saturating_sub(4), height: cols[1].height.saturating_sub(2) };
        let preview_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(1)])
            .split(p);
        let lines: Vec<Line> = if app.focus == Focus::Dirs {
            let d = dirs.get(app.dir_idx).map(|d| d.path.clone()).unwrap_or_default();
            let target = sanitize_name(if app.new_session.is_empty() { d.file_name().and_then(|n| n.to_str()).unwrap_or("home") } else { &app.new_session });
            let children = child_dirs(&d);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![Span::styled(" CREATE ", Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD)), Span::styled(format!("   {target}"), Style::default().fg(text).add_modifier(Modifier::BOLD))]),
                    Line::from(vec![Span::styled(" cwd     ", Style::default().fg(muted)), Span::styled(d.display().to_string(), Style::default().fg(peach))]),
                    Line::from(vec![
                        Span::styled(" folders ", Style::default().fg(muted)),
                        Span::styled(format!("{}", children.len()), Style::default().fg(green).add_modifier(Modifier::BOLD)),
                        Span::styled("   action ", Style::default().fg(muted)),
                        Span::styled("Enter creates detached tmux session here", Style::default().fg(text)),
                    ]),
                ]).block(Block::default().borders(Borders::BOTTOM).border_style(Style::default().fg(surface))).style(Style::default().bg(panel)),
                preview_chunks[0],
            );
            let mut v = vec![Line::from(vec![
                Span::styled("   ", Style::default().fg(green).add_modifier(Modifier::BOLD)),
                Span::styled("child folders", Style::default().fg(text).add_modifier(Modifier::BOLD)),
            ])];
            v.extend(children.into_iter().map(|p| Line::from(vec![
                Span::styled("   ", Style::default().fg(green)),
                Span::styled(p.file_name().unwrap_or_default().to_string_lossy().to_string(), Style::default().fg(text)),
            ])));
            v
        } else if let Some(s) = sessions.get(app.sess_idx) {
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled(" SWITCH ", Style::default().fg(bg).bg(blue).add_modifier(Modifier::BOLD)),
                        Span::styled(format!("   {}", s.name), Style::default().fg(text).add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  {}w", s.windows), Style::default().fg(mauve)),
                        Span::styled(if s.attached { "  attached" } else { "  detached" }, Style::default().fg(if s.attached { yellow } else { muted })),
                    ]),
                    Line::from(vec![Span::styled(" pane    ", Style::default().fg(muted)), Span::styled(s.active_line.clone(), Style::default().fg(peach))]),
                    Line::from(vec![
                        Span::styled(" active  ", Style::default().fg(muted)),
                        Span::styled(s.activity.clone(), Style::default().fg(yellow)),
                        Span::styled("   created ", Style::default().fg(muted)),
                        Span::styled(s.created.clone(), Style::default().fg(mauve)),
                    ]),
                ]).block(Block::default().borders(Borders::BOTTOM).border_style(Style::default().fg(surface))).style(Style::default().bg(panel)),
                preview_chunks[0],
            );
            s.preview.iter().map(|l| Line::from(strip_ansi(l))).collect()
        } else {
            vec![Line::from(Span::styled("No match", Style::default().fg(red).add_modifier(Modifier::BOLD)))]
        };
        f.render_widget(Paragraph::new(lines).style(Style::default().bg(panel).fg(text)).wrap(Wrap { trim: false }), preview_chunks[1]);

        let help = Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(bg).bg(yellow).add_modifier(Modifier::BOLD)),
            Span::styled(" switch boxes  ", Style::default().fg(text).bg(bg)),
            Span::styled(" ←/→ ", Style::default().fg(bg).bg(mauve).add_modifier(Modifier::BOLD)),
            Span::styled(" dirs  ", Style::default().fg(text).bg(bg)),
            Span::styled(" Enter ", Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD)),
            Span::styled(" open/switch  ", Style::default().fg(text).bg(bg)),
            Span::styled(" Esc ", Style::default().fg(bg).bg(red).add_modifier(Modifier::BOLD)),
            Span::styled(" quit", Style::default().fg(text).bg(bg)),
        ]);
        f.render_widget(Paragraph::new(help).style(Style::default().bg(bg)), outer[2]);
    })?;
    Ok(())
}

fn run_app() -> Result<Option<Choice>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut dir_cache = load_dir_cache();
    let initial_status = if dir_cache.is_empty() { "cache: indexing" } else { "cache: loaded" };
    if dir_cache.is_empty() {
        dir_cache = vec![home.clone()];
    }
    let (cache_tx, cache_rx) = mpsc::channel();
    if cache_is_stale() || initial_status == "cache: indexing" {
        let root = home.clone();
        thread::spawn(move || {
            let fresh = build_dir_cache(&root);
            save_dir_cache(&fresh);
            let _ = cache_tx.send(fresh);
        });
    }
    let mut app = App { focus: Focus::Dirs, dir_idx: 0, sess_idx: 0, query: String::new(), dir_query: String::new(), dir_input: DirInput::Filter, new_session: String::new(), dirs: vec![DirNode { path: home, depth: 0, expanded: true }], dir_cache, cache_status: initial_status.into(), sessions: load_sessions(None), preview_for: None, last_load: Instant::now() };
    let result = loop {
        if let Ok(fresh) = cache_rx.try_recv() {
            app.dir_cache = fresh;
            app.cache_status = format!("cache: {} dirs", app.dir_cache.len());
            app.dir_idx = 0;
        }
        if app.last_load.elapsed() > Duration::from_secs(2) { app.sessions = load_sessions(app.preview_for.as_deref()); app.last_load = Instant::now(); }
        draw(&mut app, &mut terminal)?;
        if !event::poll(Duration::from_millis(60))? { continue; }
        match event::read()? {
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break None,
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if app.focus == Focus::Dirs {
                    let dirs = filtered_visible_dirs(&mut app.dirs, &app.dir_cache, &app.dir_query);
                    if let Some(d) = dirs.get(app.dir_idx) { break Some(Choice::Dir(d.path.clone(), app.new_session.clone())); }
                } else if let Some(s) = app.sessions.get(app.sess_idx) { break Some(Choice::Session(s.name.clone())); }
            }
            Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => if app.focus == Focus::Dirs { app.dir_input = if app.dir_input == DirInput::Filter { DirInput::Name } else { DirInput::Filter }; },
            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => if app.focus == Focus::Sessions && app.sess_idx == 0 { app.focus = Focus::Dirs; app.dir_idx = filtered_visible_dirs(&mut app.dirs, &app.dir_cache, &app.dir_query).len().saturating_sub(1); } else if app.focus == Focus::Sessions { app.sess_idx = app.sess_idx.saturating_sub(1); } else { app.dir_idx = app.dir_idx.saturating_sub(1); },
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => if app.focus == Focus::Dirs { let n = filtered_visible_dirs(&mut app.dirs, &app.dir_cache, &app.dir_query).len(); if app.dir_idx + 1 >= n { app.focus = Focus::Sessions; app.sess_idx = 0; } else { app.dir_idx += 1; } } else { app.sess_idx = (app.sess_idx + 1).min(app.sessions.len().saturating_sub(1)); },
            Event::Key(KeyEvent { code: KeyCode::Right, .. }) => if app.focus == Focus::Dirs { let dirs = filtered_visible_dirs(&mut app.dirs, &app.dir_cache, &app.dir_query); if let Some(d) = dirs.get(app.dir_idx) { if let Some(n) = app.dirs.iter_mut().find(|n| n.path == d.path) { n.expanded = true; } } },
            Event::Key(KeyEvent { code: KeyCode::Left, .. }) => if app.focus == Focus::Dirs { let dirs = filtered_visible_dirs(&mut app.dirs, &app.dir_cache, &app.dir_query); if let Some(d) = dirs.get(app.dir_idx) { if let Some(n) = app.dirs.iter_mut().find(|n| n.path == d.path) { n.expanded = false; } } },
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => if app.focus == Focus::Dirs { if app.dir_input == DirInput::Filter { app.dir_query.pop(); app.dir_idx = 0; } else { app.new_session.pop(); } } else { app.query.pop(); },
            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => if app.focus == Focus::Dirs { if app.dir_input == DirInput::Filter { app.dir_query.push(c); app.dir_idx = 0; } else { app.new_session.push(c); } } else { app.query.push(c); app.sess_idx = 0; },
            Event::Mouse(m) => if matches!(m.kind, MouseEventKind::Down(_)) { app.focus = if m.row < terminal.size()?.height / 2 { Focus::Dirs } else { Focus::Sessions }; },
            _ => {}
        }
        if app.focus == Focus::Sessions {
            if let Some(s) = app.sessions.get(app.sess_idx) {
                if app.preview_for.as_deref() != Some(&s.name) { app.preview_for = Some(s.name.clone()); app.sessions = load_sessions(app.preview_for.as_deref()); }
            }
        }
    };
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(result)
}

fn main() -> Result<()> {
    if env::args().len() > 1 {
        let name = env::args().nth(1).ok_or_else(|| anyhow!("missing session"))?;
        switch_session(&name);
        return Ok(());
    }
    match run_app()? {
        Some(Choice::Dir(path, name)) => create_session(&path, &name),
        Some(Choice::Session(name)) => switch_session(&name),
        None => {}
    }
    Ok(())
}

// This keeps Cargo.toml tiny in the common case, but should become the dirs crate if preferred.
mod dirs {
    use std::{env, path::PathBuf};
    pub fn home_dir() -> Option<PathBuf> { env::var_os("HOME").map(PathBuf::from) }
}

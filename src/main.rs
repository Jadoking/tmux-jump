use anyhow::{anyhow, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind},
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
    collections::HashMap,
    env, fs,
    io::{self, Write, Stdout},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
struct Session {
    name: String,
    windows: String,
    attached: bool,
    created: String,
    activity: String,
    active_line: String,
    active_pane: Option<String>,
    activity_epoch: u64,
    current: bool,
    preview: Vec<String>,
}

#[derive(Clone, Debug)]
struct DirNode {
    path: PathBuf,
    depth: usize,
    expanded: bool,
}

#[derive(Clone, Debug)]
struct IndexedDir {
    path: PathBuf,
    full_lower: String,
    name_lower: String,
}

fn index_dirs(paths: Vec<PathBuf>) -> Vec<IndexedDir> {
    paths.into_iter().map(|path| IndexedDir {
        full_lower: path.to_string_lossy().to_lowercase(),
        name_lower: path.file_name().and_then(|name| name.to_str()).unwrap_or("~").to_lowercase(),
        path,
    }).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus { Dirs, Sessions }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DirInput { Filter, Name }

enum Choice { Dir(PathBuf, String), Session(String) }

#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionAction {
    None,
    Rename { original: String, input: String },
    ConfirmKill { name: String },
}

#[derive(Clone, Copy, Debug, Default)]
struct UiAreas {
    sessions: Rect,
    dirs: Rect,
    session_start: usize,
    dir_start: usize,
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

struct App {
    focus: Focus,
    dir_idx: usize,
    sess_idx: usize,
    query: String,
    dir_query: String,
    dir_input: DirInput,
    new_session: String,
    dirs: Vec<DirNode>,
    dir_children: HashMap<PathBuf, Vec<PathBuf>>,
    dir_cache: Vec<IndexedDir>,
    cache_status: String,
    sessions: Vec<Session>,
    preview_for: Option<String>,
    preview_scroll: u16,
    action: SessionAction,
    notice: String,
    ui: UiAreas,
    last_load: Instant,
}

enum WorkerRequest {
    Refresh { preview_for: Option<String>, current_session: Option<String> },
    Preview { session: String, pane: Option<String> },
}

enum WorkerResponse {
    Sessions(Result<Vec<Session>, String>),
    Preview { session: String, lines: Result<Vec<String>, String> },
}

fn spawn_tmux_worker() -> (mpsc::Sender<WorkerRequest>, mpsc::Receiver<WorkerResponse>) {
    let (request_tx, request_rx) = mpsc::channel();
    let (response_tx, response_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(mut request) = request_rx.recv() {
            while let Ok(newer) = request_rx.try_recv() { request = newer; }
            let response = match request {
                WorkerRequest::Refresh { preview_for, current_session } => {
                    WorkerResponse::Sessions(load_sessions(preview_for.as_deref(), current_session.as_deref()).map_err(|error| error.to_string()))
                }
                WorkerRequest::Preview { session, pane } => {
                    WorkerResponse::Preview { session, lines: capture_preview(pane.as_deref()).map_err(|error| error.to_string()) }
                }
            };
            if response_tx.send(response).is_err() { break; }
        }
    });
    (request_tx, response_rx)
}

struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                return Err(error.into());
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn run(cmd: &mut Command) -> Result<String> {
    let output = cmd.output()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(anyhow!(if stderr.is_empty() { "command failed".into() } else { stderr }))
}

fn tmux(args: &[&str]) -> Result<String> {
    run(Command::new("tmux").args(args))
}

fn sanitize_name(s: &str) -> String {
    let mapped: String = s.chars().map(|c| if ". /:".contains(c) { '_' } else { c }).collect();
    let out: String = mapped.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if out.is_empty() { "session".into() } else { out }
}

fn fuzzy_score(haystack: &str, needle: &str) -> Option<i64> {
    if needle.is_empty() { return Some(0); }
    let haystack = haystack.to_lowercase();
    let needle = needle.to_lowercase();
    if haystack == needle { return Some(10_000); }
    if haystack.starts_with(&needle) { return Some(9_000 - haystack.len() as i64); }
    if let Some(position) = haystack.find(&needle) { return Some(8_000 - position as i64 - haystack.len() as i64); }

    let mut chars = needle.chars();
    let mut wanted = chars.next()?;
    let mut first = None;
    let mut last = 0usize;
    let mut gaps = 0usize;
    for (position, candidate) in haystack.chars().enumerate() {
        if candidate == wanted {
            first.get_or_insert(position);
            gaps += position.saturating_sub(last);
            last = position + 1;
            if let Some(next) = chars.next() { wanted = next; } else {
                return Some(5_000 - first.unwrap_or_default() as i64 - gaps as i64 - haystack.len() as i64);
            }
        }
    }
    None
}

fn fmt_time(epoch: &str) -> String {
    let Ok(epoch) = epoch.parse::<u64>() else { return String::new(); };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(epoch, |d| d.as_secs());
    let age = now.saturating_sub(epoch);
    match age {
        0..=59 => format!("{age}s ago"),
        60..=3_599 => format!("{}m ago", age / 60),
        3_600..=86_399 => format!("{}h ago", age / 3_600),
        86_400..=2_592_000 => format!("{}d ago", age / 86_400),
        _ => format!("{}mo ago", age / 2_592_000),
    }
}

fn current_session_name() -> Option<String> {
    let pane = env::var("TMUX_PANE").ok()?;
    let name = tmux(&["display-message", "-p", "-t", &pane, "#S"]).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn capture_preview(pane: Option<&str>) -> Result<Vec<String>> {
    let Some(pane) = pane else { return Ok(vec!["Unable to find active pane.".into()]); };
    let out = tmux(&["capture-pane", "-epJ", "-t", pane])?;
    let mut lines: Vec<String> = out.lines().map(|l| l.trim_end_matches('\r').to_string()).collect();
    while lines.first().is_some_and(|l| strip_ansi(l).trim().is_empty()) { lines.remove(0); }
    Ok(if lines.is_empty() { vec![String::new()] } else { lines })
}

fn strip_ansi(s: &str) -> String {
    #[derive(Clone, Copy)]
    enum State { Text, Escape, Csi, Osc, OscEscape }
    let mut state = State::Text;
    let mut out = String::new();
    for character in s.chars() {
        state = match state {
            State::Text if character == '\x1b' => State::Escape,
            State::Text => { out.push(character); State::Text }
            State::Escape if character == '[' => State::Csi,
            State::Escape if character == ']' => State::Osc,
            State::Escape => State::Text,
            State::Csi if ('@'..='~').contains(&character) => State::Text,
            State::Csi => State::Csi,
            State::Osc if character == '\x07' => State::Text,
            State::Osc if character == '\x1b' => State::OscEscape,
            State::Osc => State::Osc,
            State::OscEscape if character == '\\' => State::Text,
            State::OscEscape => State::Osc,
        };
    }
    out
}

fn load_sessions(preview_for: Option<&str>, current_session: Option<&str>) -> Result<Vec<Session>> {
    let raw = tmux(&["list-sessions", "-F", "#S|#{session_windows}|#{session_attached}|#{session_created}|#{session_activity}"])?;
    let pane_raw = tmux(&["list-panes", "-a", "-F", "#S\t#{window_active}\t#{pane_active}\t#W · #{pane_current_command} · #{pane_current_path}\t#{pane_id}"])?;
    let active_panes: HashMap<String, (String, String)> = pane_raw.lines().filter_map(|line| {
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() == 5 && parts[1] == "1" && parts[2] == "1" {
            Some((parts[0].to_string(), (parts[3].to_string(), parts[4].to_string())))
        } else {
            None
        }
    }).collect();

    let mut sessions = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() < 5 { continue; }
        let name = parts[0].to_string();
        let (active_line, active_pane) = active_panes.get(&name)
            .map(|(line, pane)| (line.clone(), Some(pane.clone())))
            .unwrap_or_default();
        let preview = if preview_for == Some(name.as_str()) { capture_preview(active_pane.as_deref())? } else { Vec::new() };
        let activity_epoch = parts[4].parse().unwrap_or_default();
        let current = current_session == Some(name.as_str());
        sessions.push(Session { name, windows: parts[1].into(), attached: parts[2] != "0", created: fmt_time(parts[3]), activity: fmt_time(parts[4]), active_line, active_pane, activity_epoch, current, preview });
    }
    sessions.sort_by(|a, b| b.activity_epoch.cmp(&a.activity_epoch).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Ok(sessions)
}

fn load_preview(sessions: &mut [Session], index: usize) -> Result<()> {
    if let Some(session) = sessions.get_mut(index) {
        session.preview = capture_preview(session.active_pane.as_deref())?;
    }
    Ok(())
}

fn read_child_dirs(path: &Path) -> Vec<PathBuf> {
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

fn cached_child_dirs(cache: &mut HashMap<PathBuf, Vec<PathBuf>>, path: &Path) -> Vec<PathBuf> {
    cache.entry(path.to_path_buf()).or_insert_with(|| read_child_dirs(path)).clone()
}

fn visible_dirs(nodes: &mut Vec<DirNode>, children: &mut HashMap<PathBuf, Vec<PathBuf>>) -> Vec<DirNode> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    fn walk(path: PathBuf, depth: usize, nodes: &mut Vec<DirNode>, children: &mut HashMap<PathBuf, Vec<PathBuf>>, out: &mut Vec<DirNode>) {
        let pos = nodes.iter().position(|n| n.path == path).unwrap_or_else(|| { nodes.push(DirNode { path: path.clone(), depth, expanded: false }); nodes.len() - 1 });
        nodes[pos].depth = depth;
        let node = nodes[pos].clone();
        out.push(node.clone());
        if node.expanded {
            for child in cached_child_dirs(children, &node.path) { walk(child, depth + 1, nodes, children, out); }
        }
    }
    if nodes.is_empty() { nodes.push(DirNode { path: home.clone(), depth: 0, expanded: true }); }
    let mut out = Vec::new();
    walk(home, 0, nodes, children, &mut out);
    out
}

const MAX_CACHE_DIRS: usize = 25_000;
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 24);

fn cache_path() -> PathBuf {
    let base = env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(".cache"));
    base.join("tmux-jump").join("dirs.txt")
}

fn load_dir_cache() -> Vec<IndexedDir> {
    index_dirs(fs::read_to_string(cache_path()).ok().into_iter().flat_map(|s| {
        s.lines().take(MAX_CACHE_DIRS).map(PathBuf::from).collect::<Vec<_>>()
    }).filter(|p| p.is_dir()).collect())
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

fn refresh_cache_async(root: PathBuf, tx: mpsc::Sender<Vec<PathBuf>>) {
    thread::spawn(move || {
        let fresh = build_dir_cache(&root);
        save_dir_cache(&fresh);
        let _ = tx.send(fresh);
    });
}

fn filtered_visible_dirs(nodes: &mut Vec<DirNode>, children: &mut HashMap<PathBuf, Vec<PathBuf>>, cache: &[IndexedDir], query: &str) -> Vec<DirNode> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return visible_dirs(nodes, children);
    }

    let path_mode = q.contains('/') || q.starts_with('~');
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let normalized_q = if q.starts_with('~') {
        q.replacen('~', &home.to_string_lossy(), 1)
    } else {
        q.clone()
    };

    let mut matches: Vec<(i64, String, PathBuf)> = cache.iter()
        .filter_map(|entry| {
            let score = if path_mode {
                fuzzy_score(&entry.full_lower, &normalized_q)?
            } else {
                fuzzy_score(&entry.name_lower, &q).or_else(|| fuzzy_score(&entry.full_lower, &q).map(|score| score - 1_000))?
            };
            Some((score, entry.full_lower.clone(), entry.path.clone()))
        })
        .collect();
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    matches.into_iter()
        .take(1000)
        .map(|(_, _, p)| DirNode { path: p, depth: 0, expanded: false })
        .collect()
}

fn state_path() -> PathBuf {
    cache_path().with_file_name("state")
}

fn load_last_session() -> Option<String> {
    fs::read_to_string(state_path()).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

fn save_last_session(name: Option<&str>) {
    let path = state_path();
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    if let Some(name) = name { let _ = fs::write(path, format!("{name}\n")); }
}

fn command_action(command: &mut Command) -> Result<()> {
    let output = command.output()?;
    if output.status.success() { return Ok(()); }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(anyhow!(if stderr.is_empty() { "command failed".into() } else { stderr }))
}

fn tmux_action(args: &[&str]) -> Result<()> {
    command_action(Command::new("tmux").args(args))
}

fn rename_session(original: &str, replacement: &str) -> Result<String> {
    let replacement = sanitize_name(replacement);
    tmux_action(&["rename-session", "-t", &format!("={original}"), &replacement])?;
    Ok(replacement)
}

fn kill_session(name: &str) -> Result<()> {
    tmux_action(&["kill-session", "-t", &format!("={name}")])
}

fn switch_session(name: &str) -> Result<()> {
    if env::var("TMUX").is_ok() {
        let client = tmux(&["display-message", "-p", "#{client_name}"]).unwrap_or_default().trim().to_string();
        let mut command = Command::new("tmux");
        command.arg("switch-client");
        if !client.is_empty() { command.args(["-c", &client]); }
        command_action(command.args(["-t", &format!("={name}")]))
    } else {
        command_action(Command::new("tmux").args(["attach-session", "-t", &format!("={name}")]))
    }
}

fn create_session(path: &Path, requested: &str) -> Result<()> {
    let fallback = path.file_name().and_then(|n| n.to_str()).unwrap_or("home");
    let name = sanitize_name(if requested.trim().is_empty() { fallback } else { requested.trim() });
    let exists = Command::new("tmux").args(["has-session", "-t", &format!("={name}")]).status().is_ok_and(|s| s.success());
    if !exists {
        command_action(Command::new("tmux").args(["new-session", "-ds", &name, "-c", &path.to_string_lossy()]))?;
    }
    switch_session(&name)
}

fn filtered_session_indices(sessions: &[Session], query: &str) -> Vec<usize> {
    if query.is_empty() { return (0..sessions.len()).collect(); }
    let mut matches: Vec<(i64, u64, usize)> = sessions.iter().enumerate().filter_map(|(index, session)| {
        let score = fuzzy_score(&session.name, query)
            .or_else(|| fuzzy_score(&session.active_line, query).map(|score| score - 1_000))?;
        Some((score, session.activity_epoch, index))
    }).collect();
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    matches.into_iter().map(|(_, _, index)| index).collect()
}

fn draw(app: &mut App, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let dirs = filtered_visible_dirs(&mut app.dirs, &mut app.dir_children, &app.dir_cache, &app.dir_query);
    let sessions = filtered_session_indices(&app.sessions, &app.query);
    app.dir_idx = app.dir_idx.min(dirs.len().saturating_sub(1));
    app.sess_idx = app.sess_idx.min(sessions.len().saturating_sub(1));

    terminal.draw(|f| {
        let root = f.size();
        // Color::Reset inherits the terminal's configured foreground and background.
        // Hierarchy comes from bold, dim, and reverse-video rather than a fixed palette.
        let bg = Color::Reset;
        let panel = Color::Reset;
        let surface = Color::Reset;
        let text = Color::Reset;
        let muted = Color::Reset;
        let neon = Color::Reset;
        let neon_bright = Color::Reset;
        let neon_dim = Color::Reset;
        let blue = Color::Reset;
        let mauve = Color::Reset;
        let green = Color::Reset;
        let yellow = Color::Reset;
        let peach = Color::Reset;
        let red = Color::Reset;
        let field_style = |active: bool| {
            Style::default().add_modifier(if active {
                Modifier::BOLD | Modifier::REVERSED
            } else {
                Modifier::DIM
            })
        };

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
            Span::styled(" active ", Style::default().fg(bg).bg(neon).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::styled(format!(" {active_label} "), Style::default().fg(neon_bright).bg(bg).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {} ", app.cache_status), Style::default().fg(muted).bg(bg)),
            Span::styled(format!("  {}", app.notice), Style::default().fg(text).bg(bg).add_modifier(Modifier::BOLD)),
        ]);
        f.render_widget(Paragraph::new(top).style(Style::default().bg(bg)), outer[0]);

        let box_style = |focused: bool, color: Color| {
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(if focused { Style::default().fg(color).add_modifier(Modifier::BOLD) } else { Style::default().fg(neon_dim).add_modifier(Modifier::DIM) })
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

        let dir_inner = dir_block.inner(left[1]);
        f.render_widget(dir_block, left[1]);
        let dir_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(dir_inner);
        let dir_view_h = dir_split[0].height as usize;
        let dir_start = if app.dir_idx >= dir_view_h { app.dir_idx + 1 - dir_view_h } else { 0 };
        app.ui.dirs = dir_split[0];
        app.ui.dir_start = dir_start;
        let dir_items: Vec<ListItem> = dirs.iter().enumerate().skip(dir_start).take(dir_view_h).map(|(i, d)| {
            let child_count = cached_child_dirs(&mut app.dir_children, &d.path).len();
            let kids = child_count > 0;
            let marker = if kids && d.expanded { "▾" } else if kids { "▸" } else { " " };
            let name = if d.path == dirs::home_dir().unwrap_or_default() { "~".into() } else { d.path.file_name().unwrap_or_default().to_string_lossy().into_owned() };
            let count_badge = if child_count > 0 { format!("  {child_count}") } else { String::new() };
            let indent = "  ".repeat(d.depth);
            if app.focus == Focus::Dirs && i == app.dir_idx {
                ListItem::new(Line::from(vec![
                    Span::styled(" 󰮟 ", Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
                    Span::styled(format!("{indent}{marker}  {name}{count_badge}"), Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
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
                Span::styled("  search ", field_style(app.focus == Focus::Dirs && app.dir_input == DirInput::Filter)),
                Span::styled(" ", Style::default().fg(if app.focus == Focus::Dirs && app.dir_input == DirInput::Filter { green } else { surface }).bg(panel)),
                Span::styled(dir_filter.to_string(), Style::default().fg(if app.dir_query.is_empty() { muted } else { text }).bg(panel).add_modifier(if app.focus == Focus::Dirs && app.dir_input == DirInput::Filter { Modifier::BOLD } else { Modifier::empty() })),
                Span::styled("  │  ", Style::default().fg(surface).bg(panel)),
                Span::styled("  name ", field_style(app.focus == Focus::Dirs && app.dir_input == DirInput::Name)),
                Span::styled(" ", Style::default().fg(if app.focus == Focus::Dirs && app.dir_input == DirInput::Name { green } else { surface }).bg(panel)),
                Span::styled(new_name.to_string(), Style::default().fg(if app.new_session.is_empty() { muted } else { text }).bg(panel).add_modifier(if app.focus == Focus::Dirs && app.dir_input == DirInput::Name { Modifier::BOLD } else { Modifier::empty() })),
            ])).style(Style::default().bg(panel)),
            dir_split[1],
        );

        let sess_inner = sess_block.inner(left[0]);
        f.render_widget(sess_block, left[0]);
        let sess_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(sess_inner);
        let sess_view_h = sess_split[0].height as usize;
        let sess_start = if app.sess_idx >= sess_view_h { app.sess_idx + 1 - sess_view_h } else { 0 };
        app.ui.sessions = sess_split[0];
        app.ui.session_start = sess_start;
        let sess_items: Vec<ListItem> = sessions.iter().enumerate().skip(sess_start).take(sess_view_h).map(|(i, session_index)| {
            let s = &app.sessions[*session_index];
            let attached = if s.attached { " 󰈈" } else { "" };
            let current = if s.current { "  ← current" } else { "" };
            if app.focus == Focus::Sessions && i == app.sess_idx {
                ListItem::new(Line::from(vec![
                    Span::styled(" 󰮟 ", Style::default().fg(bg).bg(blue).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
                    Span::styled(format!(" {}{}{}", s.name, attached, current), Style::default().fg(bg).bg(blue).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
                ]))
            } else {
                ListItem::new(Line::from(vec![
                    Span::styled("    ", Style::default().fg(blue).bg(panel)),
                    Span::styled(s.name.clone(), Style::default().fg(text).bg(panel)),
                    Span::styled(attached, Style::default().fg(yellow).bg(panel)),
                    Span::styled(current, Style::default().fg(muted).bg(panel).add_modifier(Modifier::DIM)),
                ]))
            }
        }).collect();
        f.render_widget(List::new(sess_items).style(Style::default().bg(panel)), sess_split[0]);
        let (filter_label, filter, action_active) = match &app.action {
            SessionAction::Rename { original, input } => ("  rename ", format!("{original} → {input}"), true),
            SessionAction::ConfirmKill { name } => ("  confirm ", format!("kill {name}?  y/N"), true),
            SessionAction::None => ("  filter ", if app.query.is_empty() { "type to fuzzy-filter sessions".into() } else { app.query.clone() }, app.focus == Focus::Sessions),
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(filter_label, field_style(action_active)),
                Span::styled("  ", Style::default().bg(panel)),
                Span::styled(filter, Style::default().fg(text).bg(panel).add_modifier(if action_active { Modifier::BOLD } else { Modifier::empty() })),
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
            let children = cached_child_dirs(&mut app.dir_children, &d);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![Span::styled(" CREATE ", Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD | Modifier::REVERSED)), Span::styled(format!("   {target}"), Style::default().fg(text).add_modifier(Modifier::BOLD))]),
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
        } else if let Some(s) = sessions.get(app.sess_idx).map(|index| &app.sessions[*index]) {
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
                        Span::styled(" SWITCH ", Style::default().fg(bg).bg(blue).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
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
        f.render_widget(Paragraph::new(lines).style(Style::default().bg(panel).fg(text)).wrap(Wrap { trim: false }).scroll((app.preview_scroll, 0)), preview_chunks[1]);

        let help = Line::from(vec![
            Span::styled("  ↑/↓ ", Style::default().fg(bg).bg(yellow).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::styled(" switch boxes  ", Style::default().fg(text).bg(bg)),
            Span::styled(" ←/→ ", Style::default().fg(bg).bg(mauve).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::styled(" dirs  ", Style::default().fg(text).bg(bg)),
            Span::styled(" Enter ", Style::default().fg(bg).bg(green).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::styled(" open/switch  ", Style::default().fg(text).bg(bg)),
            Span::styled(" ^R/^D ", Style::default().fg(bg).bg(red).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::styled(" rename/kill  ", Style::default().fg(text).bg(bg)),
            Span::styled(" PgUp/PgDn ", Style::default().fg(bg).bg(red).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::styled(" preview", Style::default().fg(text).bg(bg)),
        ]);
        f.render_widget(Paragraph::new(help).style(Style::default().bg(bg)), outer[2]);
    })?;
    Ok(())
}

fn run_app() -> Result<Option<Choice>> {
    let mut tui = Tui::new()?;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let mut dir_cache = load_dir_cache();
    let initial_status = if dir_cache.is_empty() { "cache: indexing" } else { "cache: loaded" };
    if dir_cache.is_empty() {
        dir_cache = index_dirs(vec![home.clone()]);
    }
    let (cache_tx, cache_rx) = mpsc::channel();
    if cache_is_stale() || initial_status == "cache: indexing" {
        refresh_cache_async(home.clone(), cache_tx.clone());
    }
    let mut current_session = current_session_name();
    let mut sessions = load_sessions(None, current_session.as_deref())?;
    let last_session = load_last_session();
    let sess_idx = last_session.as_deref().and_then(|name| sessions.iter().position(|session| session.name == name)).unwrap_or(0);
    let preview_for = sessions.get(sess_idx).map(|s| s.name.clone());
    if preview_for.is_some() { load_preview(&mut sessions, sess_idx)?; }
    let mut app = App { focus: Focus::Sessions, dir_idx: 0, sess_idx, query: String::new(), dir_query: String::new(), dir_input: DirInput::Filter, new_session: String::new(), dirs: vec![DirNode { path: home.clone(), depth: 0, expanded: true }], dir_children: HashMap::new(), dir_cache, cache_status: initial_status.into(), sessions, preview_for, preview_scroll: 0, action: SessionAction::None, notice: String::new(), ui: UiAreas::default(), last_load: Instant::now() };
    let (worker_tx, worker_rx) = spawn_tmux_worker();
    let mut dirty = true;
    let result = loop {
        while let Ok(response) = worker_rx.try_recv() {
            match response {
                WorkerResponse::Sessions(Ok(sessions)) => {
                    let selected_name = filtered_session_indices(&app.sessions, &app.query)
                        .get(app.sess_idx)
                        .and_then(|index| app.sessions.get(*index))
                        .map(|session| session.name.clone());
                    app.sessions = sessions;
                    if let Some(name) = selected_name {
                        let filtered = filtered_session_indices(&app.sessions, &app.query);
                        app.sess_idx = filtered.iter().position(|index| app.sessions[*index].name == name).unwrap_or(0);
                    }
                }
                WorkerResponse::Sessions(Err(error)) => app.notice = format!("refresh failed: {error}"),
                WorkerResponse::Preview { session, lines: Ok(lines) } => {
                    if app.preview_for.as_deref() == Some(&session) {
                        if let Some(target) = app.sessions.iter_mut().find(|candidate| candidate.name == session) {
                            target.preview = lines;
                        }
                    }
                }
                WorkerResponse::Preview { lines: Err(error), .. } => app.notice = format!("preview failed: {error}"),
            }
            dirty = true;
        }
        if let Ok(fresh) = cache_rx.try_recv() {
            app.dir_cache = index_dirs(fresh);
            app.cache_status = format!("cache: {} dirs", app.dir_cache.len());
            app.dir_idx = 0;
            dirty = true;
        }
        if app.last_load.elapsed() > Duration::from_secs(2) {
            let _ = worker_tx.send(WorkerRequest::Refresh {
                preview_for: app.preview_for.clone(),
                current_session: current_session.clone(),
            });
            app.last_load = Instant::now();
        }
        if dirty {
            draw(&mut app, &mut tui.terminal)?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(60))? { continue; }
        let input_event = event::read()?;
        if let Event::Key(key) = &input_event {
            let action = std::mem::replace(&mut app.action, SessionAction::None);
            match action {
                SessionAction::Rename { original, mut input } => {
                    match key.code {
                        KeyCode::Esc => app.notice = "rename cancelled".into(),
                        KeyCode::Enter if input.trim().is_empty() => {
                            app.notice = "enter a new session name".into();
                            app.action = SessionAction::Rename { original, input };
                        }
                        KeyCode::Enter => match rename_session(&original, &input) {
                            Ok(replacement) => {
                                if current_session.as_deref() == Some(&original) { current_session = Some(replacement.clone()); }
                                app.preview_for = Some(replacement.clone());
                                app.sessions = load_sessions(app.preview_for.as_deref(), current_session.as_deref())?;
                                app.sess_idx = app.sessions.iter().position(|session| session.name == replacement).unwrap_or(0);
                                app.notice = "session renamed".into();
                            }
                            Err(error) => app.notice = format!("rename failed: {error}"),
                        },
                        KeyCode::Backspace => { input.pop(); app.action = SessionAction::Rename { original, input }; }
                        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => { input.push(c); app.action = SessionAction::Rename { original, input }; }
                        _ => app.action = SessionAction::Rename { original, input },
                    }
                    dirty = true;
                    continue;
                }
                SessionAction::ConfirmKill { name } => {
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        match kill_session(&name) {
                            Ok(()) => {
                                app.sessions = load_sessions(None, current_session.as_deref())?;
                                app.sess_idx = app.sess_idx.min(app.sessions.len().saturating_sub(1));
                                app.preview_for = app.sessions.get(app.sess_idx).map(|session| session.name.clone());
                                if app.preview_for.is_some() { load_preview(&mut app.sessions, app.sess_idx)?; }
                                app.notice = "session killed".into();
                            }
                            Err(error) => app.notice = format!("kill failed: {error}"),
                        }
                    } else {
                        app.notice = "kill cancelled".into();
                    }
                    dirty = true;
                    continue;
                }
                SessionAction::None => {}
            }
        }
        match input_event {
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => break None,
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if app.focus == Focus::Dirs {
                    let dirs = filtered_visible_dirs(&mut app.dirs, &mut app.dir_children, &app.dir_cache, &app.dir_query);
                    if let Some(d) = dirs.get(app.dir_idx) { break Some(Choice::Dir(d.path.clone(), app.new_session.clone())); }
                } else {
                    let sessions = filtered_session_indices(&app.sessions, &app.query);
                    if let Some(s) = sessions.get(app.sess_idx).map(|index| &app.sessions[*index]) { break Some(Choice::Session(s.name.clone())); }
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Char('r'), modifiers, .. }) if modifiers.contains(KeyModifiers::CONTROL) && app.focus == Focus::Sessions => {
                let sessions = filtered_session_indices(&app.sessions, &app.query);
                if let Some(session) = sessions.get(app.sess_idx).and_then(|index| app.sessions.get(*index)) {
                    app.action = SessionAction::Rename { original: session.name.clone(), input: String::new() };
                    app.notice.clear();
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Char('d'), modifiers, .. }) if modifiers.contains(KeyModifiers::CONTROL) && app.focus == Focus::Sessions => {
                let sessions = filtered_session_indices(&app.sessions, &app.query);
                if let Some(session) = sessions.get(app.sess_idx).and_then(|index| app.sessions.get(*index)) {
                    if session.current {
                        app.notice = "switch away before killing the current session".into();
                    } else {
                        app.action = SessionAction::ConfirmKill { name: session.name.clone() };
                        app.notice.clear();
                    }
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Char('n'), modifiers, .. }) if modifiers.contains(KeyModifiers::CONTROL) => {
                app.focus = Focus::Dirs;
                app.dir_input = DirInput::Name;
            }
            Event::Key(KeyEvent { code: KeyCode::Char('u'), modifiers, .. }) if modifiers.contains(KeyModifiers::CONTROL) => {
                app.cache_status = "cache: indexing".into();
                app.dir_children.clear();
                refresh_cache_async(home.clone(), cache_tx.clone());
            }
            Event::Key(KeyEvent { code: KeyCode::PageUp, .. }) => app.preview_scroll = app.preview_scroll.saturating_sub(5),
            Event::Key(KeyEvent { code: KeyCode::PageDown, .. }) => app.preview_scroll = app.preview_scroll.saturating_add(5),
            Event::Key(KeyEvent { code: KeyCode::Tab, .. }) => if app.focus == Focus::Dirs { app.dir_input = if app.dir_input == DirInput::Filter { DirInput::Name } else { DirInput::Filter }; },
            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => if app.focus == Focus::Dirs && app.dir_idx == 0 { app.focus = Focus::Sessions; app.sess_idx = filtered_session_indices(&app.sessions, &app.query).len().saturating_sub(1); } else if app.focus == Focus::Dirs { app.dir_idx = app.dir_idx.saturating_sub(1); } else { app.sess_idx = app.sess_idx.saturating_sub(1); },
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => if app.focus == Focus::Sessions { let n = filtered_session_indices(&app.sessions, &app.query).len(); if app.sess_idx + 1 >= n { app.focus = Focus::Dirs; app.dir_idx = 0; } else { app.sess_idx += 1; } } else { app.dir_idx = (app.dir_idx + 1).min(filtered_visible_dirs(&mut app.dirs, &mut app.dir_children, &app.dir_cache, &app.dir_query).len().saturating_sub(1)); },
            Event::Key(KeyEvent { code: KeyCode::Right, .. }) => if app.focus == Focus::Dirs { let dirs = filtered_visible_dirs(&mut app.dirs, &mut app.dir_children, &app.dir_cache, &app.dir_query); if let Some(d) = dirs.get(app.dir_idx) { if let Some(n) = app.dirs.iter_mut().find(|n| n.path == d.path) { n.expanded = true; } } },
            Event::Key(KeyEvent { code: KeyCode::Left, .. }) => if app.focus == Focus::Dirs { let dirs = filtered_visible_dirs(&mut app.dirs, &mut app.dir_children, &app.dir_cache, &app.dir_query); if let Some(d) = dirs.get(app.dir_idx) { if let Some(n) = app.dirs.iter_mut().find(|n| n.path == d.path) { n.expanded = false; } } },
            Event::Key(KeyEvent { code: KeyCode::Backspace, .. }) => if app.focus == Focus::Dirs { if app.dir_input == DirInput::Filter { app.dir_query.pop(); app.dir_idx = 0; } else { app.new_session.pop(); } } else { app.query.pop(); app.sess_idx = 0; },
            Event::Key(KeyEvent { code: KeyCode::Char(c), .. }) => if app.focus == Focus::Dirs { if app.dir_input == DirInput::Filter { app.dir_query.push(c); app.dir_idx = 0; } else { app.new_session.push(c); } } else { app.query.push(c); app.sess_idx = 0; },
            Event::Mouse(m) => if matches!(m.kind, MouseEventKind::Down(_)) {
                if rect_contains(app.ui.sessions, m.column, m.row) {
                    app.focus = Focus::Sessions;
                    let row = app.ui.session_start + (m.row - app.ui.sessions.y) as usize;
                    app.sess_idx = row.min(filtered_session_indices(&app.sessions, &app.query).len().saturating_sub(1));
                } else if rect_contains(app.ui.dirs, m.column, m.row) {
                    app.focus = Focus::Dirs;
                    let row = app.ui.dir_start + (m.row - app.ui.dirs.y) as usize;
                    let count = filtered_visible_dirs(&mut app.dirs, &mut app.dir_children, &app.dir_cache, &app.dir_query).len();
                    app.dir_idx = row.min(count.saturating_sub(1));
                }
            },
            _ => {}
        }
        if app.focus == Focus::Sessions {
            let sessions = filtered_session_indices(&app.sessions, &app.query);
            if let Some(index) = sessions.get(app.sess_idx).copied() {
                let name = app.sessions[index].name.clone();
                if app.preview_for.as_deref() != Some(&name) {
                    app.preview_for = Some(name);
                    app.preview_scroll = 0;
                    let _ = worker_tx.send(WorkerRequest::Preview {
                        session: app.sessions[index].name.clone(),
                        pane: app.sessions[index].active_pane.clone(),
                    });
                }
            }
        }
        dirty = true;
    };
    let selected = filtered_session_indices(&app.sessions, &app.query)
        .get(app.sess_idx)
        .and_then(|index| app.sessions.get(*index))
        .map(|session| session.name.as_str());
    save_last_session(selected);
    Ok(result)
}

fn main() -> Result<()> {
    if env::args().len() > 1 {
        let name = env::args().nth(1).ok_or_else(|| anyhow!("missing session"))?;
        switch_session(&name)?;
        return Ok(());
    }
    match run_app()? {
        Some(Choice::Dir(path, name)) => create_session(&path, &name)?,
        Some(Choice::Session(name)) => switch_session(&name)?,
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, active_line: &str) -> Session {
        Session {
            name: name.into(),
            windows: "1".into(),
            attached: false,
            created: String::new(),
            activity: String::new(),
            active_line: active_line.into(),
            active_pane: None,
            activity_epoch: 0,
            current: false,
            preview: Vec::new(),
        }
    }

    #[test]
    fn session_filter_returns_source_indices() {
        let sessions = vec![
            session("api", "server · cargo · /work/api"),
            session("docs", "editor · nvim · /work/docs"),
            session("web", "server · node · /work/web"),
        ];

        assert_eq!(filtered_session_indices(&sessions, ""), vec![0, 1, 2]);
        assert_eq!(filtered_session_indices(&sessions, "DOC"), vec![1]);
        assert_eq!(filtered_session_indices(&sessions, "node"), vec![2]);
        assert_eq!(filtered_session_indices(&sessions, "edtr"), vec![1]);
    }

    #[test]
    fn fuzzy_score_prefers_exact_prefix_and_subsequence_in_order() {
        assert!(fuzzy_score("tmux-jump", "tmux-jump") > fuzzy_score("tmux-jump-extra", "tmux-jump"));
        assert!(fuzzy_score("tmux-jump", "tmj").is_some());
        assert!(fuzzy_score("tmux-jump", "jtm").is_none());
    }

    #[test]
    fn ansi_stripping_handles_csi_and_osc_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b]0;title\x07prompt"), "prompt");
        assert_eq!(strip_ansi("\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\"), "link");
    }

    #[test]
    fn session_names_are_sanitized_for_tmux() {
        assert_eq!(sanitize_name("my project/api"), "my_project_api");
        assert_eq!(sanitize_name("!!!"), "session");
    }
}

// This keeps Cargo.toml tiny in the common case, but should become the dirs crate if preferred.
mod dirs {
    use std::{env, path::PathBuf};
    pub fn home_dir() -> Option<PathBuf> { env::var_os("HOME").map(PathBuf::from) }
}

//! kale: pick, start, attach and kill tmux sessions per project.
//!
//! State lives in tmux only. A project session carries session options:
//!   @project  project name (from ~/.config/kale/projects.toml)
//!   @slug     slug of the Claude Code /rename name the session was last named after
//!   @manual   set when renamed by hand; disables auto-naming
//! Sessions are always targeted by id ($N), never by name (names prefix-match in tmux).
//!
//! Env overrides (used by tests): KALE_TMUX_SOCKET (tmux -L), KALE_CLAUDE_SESSIONS, KALE_AGY_DIR, KALE_AGY_PANES, KALE_CONFIG.

use anyhow::{bail, Context, Result};
use ratatui::{
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
    DefaultTerminal, Frame,
};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// ---------- config ----------

#[derive(Deserialize, Clone)]
struct Project {
    name: String,
    dir: String,
    /// Optional command typed into a new session (e.g. "claude").
    #[serde(default)]
    start: Option<String>,
    /// Keep a Claude Remote Control server (`<name>-remote` session) running in dir (`kale remote-up`).
    #[serde(default)]
    remote: bool,
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "project", default)]
    projects: Vec<Project>,
}

const DEFAULT_CONFIG: &str = r#"# kale projects. name = session prefix (<name>-1, <name>-2, ...), dir = start folder.
# Optional: start = "claude"  runs that command in every new session of the project.

[[project]]
name = "agents"
dir = "~/agents"

[[project]]
name = "tuccar"
dir = "~/code/tuccar/main"
"#;

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn expand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest).to_string_lossy().into_owned(),
        None if p == "~" => home().to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

fn config_path() -> PathBuf {
    env::var("KALE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config/kale/projects.toml"))
}

fn load_projects() -> Result<Vec<Project>> {
    let p = config_path();
    if !p.exists() {
        if let Some(dir) = p.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&p, DEFAULT_CONFIG)?;
    }
    let text = fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let c: Config = toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
    Ok(c.projects)
}

// ---------- tmux ----------

fn tmux_cmd() -> Command {
    let mut c = Command::new("tmux");
    if let Ok(s) = env::var("KALE_TMUX_SOCKET") {
        c.args(["-L", &s]);
    }
    c
}

fn tmux(args: &[&str]) -> Result<String> {
    let o = tmux_cmd().args(args).stdin(Stdio::null()).output().context("running tmux")?;
    if !o.status.success() {
        bail!("tmux {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentKind {
    Claude,
    Agy,
}

impl AgentKind {
    fn display_name(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude",
            AgentKind::Agy => "Antigravity",
        }
    }
}

#[derive(Clone)]
struct AgentPane {
    kind: AgentKind,
    name: Option<String>, // session rename/title used for autonaming & tab label
    topic: String,        // fallback topic (e.g. Claude pane title without status glyph)
    busy: bool,
}

#[derive(Clone, Default)]
struct Sess {
    id: String,
    name: String,
    project: String,
    slug: String,
    manual: bool,
    remote: bool,
    attached: u32,
    activity: u64,
    windows: u32,
    path: String,
    agents: Vec<AgentPane>,
}

impl Sess {
    fn label(&self) -> String {
        if self.remote {
            return "Remote Control server: new sessions from the Claude app / claude.ai/code".into();
        }
        match self.agents.iter().find_map(|a| a.name.clone()) {
            Some(n) => n,
            None => self.agents.iter().map(|a| a.topic.clone()).find(|t| !t.is_empty()).unwrap_or_default(),
        }
    }

    fn running_agents_label(&self) -> Option<String> {
        let mut names: Vec<&'static str> = vec![];
        for a in &self.agents {
            let n = a.kind.display_name();
            if !names.contains(&n) {
                names.push(n);
            }
        }
        if names.is_empty() {
            None
        } else {
            Some(names.join(" + "))
        }
    }
}

struct ClaudeInfo {
    name: Option<String>,
    busy: bool,
    updated: u64,
}

/// Claude Code writes ~/.claude/sessions/<pid>.json; "tmux" ends in ".%<pane>". Newest file per pane wins.
fn claude_by_pane() -> HashMap<String, ClaudeInfo> {
    let dir = env::var("KALE_CLAUDE_SESSIONS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".claude/sessions"));
    let mut m: HashMap<String, ClaudeInfo> = HashMap::new();
    let Ok(rd) = fs::read_dir(dir) else { return m };
    for e in rd.flatten() {
        if e.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(t) = fs::read_to_string(e.path()) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
        let Some(pane) = v["tmux"].as_str().and_then(|s| s.rsplit_once('.')).map(|(_, p)| p.to_string()) else {
            continue;
        };
        let updated = v["updatedAt"].as_u64().unwrap_or(0);
        if m.get(&pane).is_some_and(|c| c.updated >= updated) {
            continue;
        }
        let name = (v["nameSource"].as_str() == Some("user"))
            .then(|| v["name"].as_str().unwrap_or("").to_string())
            .filter(|s| !s.is_empty());
        m.insert(pane, ClaudeInfo { name, busy: v["status"].as_str() == Some("busy"), updated });
    }
    m
}

struct AgyInfo {
    title: Option<String>,
    busy: bool,
}

fn agy_dir() -> PathBuf {
    env::var("KALE_AGY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".gemini/antigravity-cli"))
}

/// Optional test override: JSON object mapping tmux pane_id ("%1") -> agy conversation_id.
fn agy_pane_overrides() -> HashMap<String, String> {
    let Ok(path) = env::var("KALE_AGY_PANES") else { return HashMap::new() };
    let Ok(text) = fs::read_to_string(path) else { return HashMap::new() };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Read Antigravity CLI conversation summaries (title + running/idle state) from its SQLite WAL DB read-only.
fn agy_summaries(dir: &Path) -> HashMap<String, AgyInfo> {
    let db = dir.join("conversation_summaries.db");
    let mut m = HashMap::new();
    let Ok(con) = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX) else {
        return m;
    };
    let _ = con.busy_timeout(Duration::from_millis(100));
    let Ok(mut stmt) = con.prepare("SELECT conversation_id, title, status, not_fully_idle FROM conversation_summaries") else {
        return m;
    };
    let Ok(rows) = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let title: String = row.get(1)?;
        let status: String = row.get(2)?;
        let not_idle: i64 = row.get(3).unwrap_or(0);
        Ok((id, title, status, not_idle))
    }) else {
        return m;
    };
    for r in rows.flatten() {
        let (id, title, status, not_idle) = r;
        let t = title.trim().to_string();
        let busy = status == "CASCADE_RUN_STATUS_RUNNING" || not_idle != 0;
        m.insert(id, AgyInfo { title: (!t.is_empty()).then_some(t), busy });
    }
    m
}

/// Parse `title:"..."` from Antigravity's annotations/<conv_id>.pbtxt.
fn parse_pbtxt_title(text: &str) -> Option<String> {
    let idx = text.find("title:")?;
    let rest = text[idx + "title:".len()..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                let trimmed = out.trim().to_string();
                return (!trimmed.is_empty()).then_some(trimmed);
            }
            '\\' => match chars.next()? {
                'n' => out.push(' '),
                't' => out.push(' '),
                'r' => {}
                other => out.push(other),
            },
            _ => out.push(c),
        }
    }
    None
}

fn agy_annotation_title(dir: &Path, conv_id: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join("annotations").join(format!("{conv_id}.pbtxt"))).ok()?;
    parse_pbtxt_title(&text)
}

/// Walk descendants of `root_pid` via /proc to find the open Antigravity conversation lock (presence/<id>.lock).
fn agy_conv_id_for_pid(root_pid: u32) -> Option<String> {
    let mut queue = VecDeque::from([(root_pid, 0u8)]);
    let mut seen = HashSet::from([root_pid]);
    let mut fallback: Option<String> = None;

    while let Some((pid, depth)) = queue.pop_front() {
        if let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten() {
                let Ok(target) = fs::read_link(fd.path()) else { continue };
                let s = target.to_string_lossy();
                if let Some(rest) = s.rsplit_once("/presence/").map(|(_, r)| r) {
                    if let Some(id) = rest.strip_suffix(".lock") {
                        if !id.is_empty() && !id.contains('/') {
                            return Some(id.to_string());
                        }
                    }
                } else if fallback.is_none() {
                    if let Some(rest) = s.rsplit_once("/conversations/").map(|(_, r)| r) {
                        if let Some(id) = rest.strip_suffix(".db") {
                            if !id.is_empty() && !id.contains('/') {
                                fallback = Some(id.to_string());
                            }
                        }
                    }
                }
            }
        }
        if depth >= 6 || seen.len() >= 64 {
            continue;
        }
        if let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) {
            for t in tasks.flatten() {
                let Ok(children) = fs::read_to_string(t.path().join("children")) else { continue };
                for tok in children.split_whitespace() {
                    if let Ok(cpid) = tok.parse::<u32>() {
                        if seen.insert(cpid) {
                            queue.push_back((cpid, depth + 1));
                        }
                    }
                }
            }
        }
    }
    fallback
}

fn resolve_agy_pane(
    pane_id: &str,
    pane_pid: u32,
    dir: &Path,
    overrides: &HashMap<String, String>,
    summaries: &HashMap<String, AgyInfo>,
) -> (Option<String>, bool) {
    let conv_id = overrides.get(pane_id).cloned().or_else(|| agy_conv_id_for_pid(pane_pid));
    let Some(cid) = conv_id else { return (None, false) };
    let db = summaries.get(&cid);
    let title = agy_annotation_title(dir, &cid).or_else(|| db.and_then(|d| d.title.clone()));
    let busy = db.is_some_and(|d| d.busy);
    (title, busy)
}

fn truncate_title(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        s.chars().take(max_chars.saturating_sub(1)).chain(['…']).collect()
    } else {
        s.to_string()
    }
}

fn is_no_server(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("no server running") || s.contains("error connecting") || s.contains("No such file")
}

fn list_sessions() -> Result<Vec<Sess>> {
    let out = match tmux(&[
        "list-sessions",
        "-F",
        "#{session_id}\t#{session_name}\t#{@project}\t#{@slug}\t#{@manual}\t#{session_attached}\t#{session_activity}\t#{session_windows}\t#{session_path}\t#{@remote}",
    ]) {
        Ok(o) => o,
        Err(e) if is_no_server(&e) => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut v: Vec<Sess> = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 10).then(|| Sess {
                id: f[0].into(),
                name: f[1].into(),
                project: f[2].into(),
                slug: f[3].into(),
                manual: !f[4].is_empty(),
                remote: !f[9].is_empty(),
                attached: f[5].parse().unwrap_or(0),
                activity: f[6].parse().unwrap_or(0),
                windows: f[7].parse().unwrap_or(1),
                path: f[8].into(),
                agents: vec![],
            })
        })
        .collect();
    let panes = tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{session_id}\t#{pane_id}\t#{pane_current_command}\t#{window_active}#{pane_active}\t#{pane_pid}\t#{pane_title}",
    ])
    .unwrap_or_default();
    let mut rows: Vec<Vec<&str>> = panes.lines().map(|l| l.splitn(6, '\t').collect()).filter(|f: &Vec<&str>| f.len() == 6).collect();
    rows.sort_by_key(|f| f[3] != "11"); // active pane first, so it names the session

    let has_claude = rows.iter().any(|f| f[2] == "claude");
    let has_agy = rows.iter().any(|f| f[2] == "agy");
    let claude_info = if has_claude { claude_by_pane() } else { HashMap::new() };
    let agy_root = agy_dir();
    let agy_overrides = if has_agy { agy_pane_overrides() } else { HashMap::new() };
    let agy_info = if has_agy { agy_summaries(&agy_root) } else { HashMap::new() };

    for f in rows {
        let Some(s) = v.iter_mut().find(|s| s.id == f[0]) else { continue };
        match f[2] {
            "claude" => {
                let ci = claude_info.get(f[1]);
                let mut topic = f[5].trim().to_string();
                if topic.chars().next().is_some_and(|c| !c.is_alphanumeric()) {
                    topic = topic.chars().skip(1).collect::<String>().trim().to_string();
                }
                if topic == "Claude Code" {
                    topic.clear();
                }
                s.agents.push(AgentPane {
                    kind: AgentKind::Claude,
                    name: ci.and_then(|c| c.name.clone()),
                    topic,
                    busy: ci.is_some_and(|c| c.busy),
                });
            }
            "agy" => {
                let pid = f[4].parse::<u32>().unwrap_or(0);
                let (title, busy) = resolve_agy_pane(f[1], pid, &agy_root, &agy_overrides, &agy_info);
                s.agents.push(AgentPane { kind: AgentKind::Agy, name: title, topic: String::new(), busy });
            }
            _ => {}
        }
    }
    Ok(v)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(30).collect();
    out.trim_end_matches('-').to_string()
}

/// Project sessions follow their agent /rename or title: <project>-<slug>, -2/-3 on a clash.
/// Returns true if anything was renamed.
fn autoname(sessions: &[Sess]) -> bool {
    let mut taken: HashSet<String> = sessions.iter().map(|s| s.name.clone()).collect();
    let mut changed = false;
    for s in sessions {
        if s.project.is_empty() || s.manual {
            continue;
        }
        let Some(n) = s.agents.iter().find_map(|a| a.name.clone()) else { continue };
        let sl = slug(&n);
        if sl.is_empty() || sl == s.slug {
            continue;
        }
        let base = format!("{}-{}", s.project, sl);
        let mut want = base.clone();
        let mut i = 2;
        while want != s.name && taken.contains(&want) {
            want = format!("{base}-{i}");
            i += 1;
        }
        if want != s.name && tmux(&["rename-session", "-t", &s.id, &want]).is_err() {
            continue;
        }
        let _ = tmux(&["set-option", "-t", &s.id, "@slug", &sl]);
        taken.remove(&s.name);
        taken.insert(want);
        changed = true;
    }
    changed
}

/// Update @cc_name on Claude and Antigravity tmux panes (only when changed, to avoid redraw storms)
/// and sync project session auto-renames. Called by bin/tmux-claude-title from tmux status-right.
fn tmux_titles() -> Result<()> {
    let out = match tmux(&["list-panes", "-a", "-F", "#{pane_id}\t#{pane_current_command}\t#{pane_pid}\t#{@cc_name}"]) {
        Ok(o) => o,
        Err(e) if is_no_server(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    let rows: Vec<Vec<&str>> = out.lines().map(|l| l.splitn(4, '\t').collect()).filter(|f: &Vec<&str>| f.len() == 4).collect();
    let has_claude = rows.iter().any(|f| f[1] == "claude");
    let has_agy = rows.iter().any(|f| f[1] == "agy");
    let claude_info = if has_claude { claude_by_pane() } else { HashMap::new() };
    let agy_root = agy_dir();
    let agy_overrides = if has_agy { agy_pane_overrides() } else { HashMap::new() };
    let agy_info = if has_agy { agy_summaries(&agy_root) } else { HashMap::new() };

    for f in rows {
        let (pane, cmd, cur) = (f[0], f[1], f[3]);
        let want = match cmd {
            "claude" => claude_info
                .get(pane)
                .and_then(|c| c.name.as_deref())
                .map(|n| truncate_title(n, 32))
                .unwrap_or_else(|| "Claude Code".into()),
            "agy" => {
                let pid = f[2].parse::<u32>().unwrap_or(0);
                let (title, busy) = resolve_agy_pane(pane, pid, &agy_root, &agy_overrides, &agy_info);
                let glyph = if busy { "◆" } else { "◇" };
                let label = title.as_deref().map(|t| truncate_title(t, 32)).unwrap_or_else(|| "Antigravity".into());
                format!("{glyph} {label}")
            }
            _ => continue,
        };
        if want != cur {
            let _ = tmux(&["set", "-p", "-t", pane, "@cc_name", &want]);
        }
    }
    if has_claude || has_agy {
        if let Ok(all) = list_sessions() {
            autoname(&all);
        }
    }
    Ok(())
}

fn sessions() -> Result<Vec<Sess>> {
    let s = list_sessions()?;
    if autoname(&s) {
        list_sessions()
    } else {
        Ok(s)
    }
}

fn new_session(p: &Project, name: Option<&str>, existing: &[Sess]) -> Result<String> {
    let taken: HashSet<&str> = existing.iter().map(|s| s.name.as_str()).collect();
    let n = match name {
        Some(n) => {
            let sl = slug(n.strip_prefix(&format!("{}-", p.name)).unwrap_or(n));
            if sl.is_empty() {
                bail!("bad session name {n:?}");
            }
            format!("{}-{}", p.name, sl)
        }
        None => (1..).map(|i| format!("{}-{i}", p.name)).find(|c| !taken.contains(c.as_str())).unwrap(),
    };
    if taken.contains(n.as_str()) {
        bail!("session {n} already exists");
    }
    let dir = expand(&p.dir);
    let id = tmux(&["new-session", "-d", "-P", "-F", "#{session_id}", "-s", &n, "-c", &dir])?.trim().to_string();
    tmux(&["set-option", "-t", &id, "@project", &p.name])?;
    if name.is_some() {
        tmux(&["set-option", "-t", &id, "@manual", "1"])?;
    }
    if let Some(cmd) = &p.start {
        tmux(&["send-keys", "-t", &id, cmd, "Enter"])?;
    }
    Ok(id)
}

/// Ensure every `remote = true` project has a running Remote Control server session. Idempotent.
fn remote_up(projects: &[Project]) -> Result<()> {
    let all = list_sessions()?;
    for p in projects.iter().filter(|p| p.remote) {
        let cmd = format!(
            "claude remote-control --spawn same-dir --permission-mode default --remote-control-session-name-prefix kale-{}",
            p.name
        );
        if let Some(s) = all.iter().find(|s| s.remote && s.project == p.name) {
            let fg = tmux(&["display", "-p", "-t", &s.id, "#{pane_current_command}"]).unwrap_or_default();
            if fg.trim() == "claude" {
                println!("{}: {} running", p.name, s.name);
            } else {
                tmux(&["send-keys", "-t", &s.id, &cmd, "Enter"])?;
                println!("{}: {} had stopped; restarted", p.name, s.name);
            }
            continue;
        }
        let name = format!("{}-remote", p.name);
        if all.iter().any(|s| s.name == name) {
            println!("{}: a session named {name} exists but is not a remote server; skipped", p.name);
            continue;
        }
        let dir = expand(&p.dir);
        let id = tmux(&["new-session", "-d", "-P", "-F", "#{session_id}", "-s", &name, "-c", &dir])?.trim().to_string();
        for (k, v) in [("@project", p.name.as_str()), ("@manual", "1"), ("@remote", "1")] {
            tmux(&["set-option", "-t", &id, k, v])?;
        }
        tmux(&["send-keys", "-t", &id, &cmd, "Enter"])?;
        println!("{}: started {name}", p.name);
    }
    Ok(())
}

/// Attach (blocking) and return when the client detaches or the session ends.
/// Inside tmux, switch the current client instead.
fn attach(id: &str) -> Result<()> {
    if env::var_os("TMUX").is_some() && env::var_os("KALE_TMUX_SOCKET").is_none() {
        tmux(&["switch-client", "-t", id])?;
        return Ok(());
    }
    let st = tmux_cmd().args(["attach-session", "-t", id]).status().context("running tmux attach")?;
    if !st.success() {
        bail!("tmux attach failed ({st})");
    }
    Ok(())
}

fn find_exact(all: &[Sess], name: &str) -> Option<Sess> {
    all.iter().find(|s| s.name == name).cloned()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", d / 60),
        3600..=86399 => format!("{}h", d / 3600),
        _ => format!("{}d", d / 86400),
    }
}

// ---------- picker ----------

#[derive(Clone)]
enum Row {
    Header(String, String), // title, dir
    Sess(Sess),
    New(usize), // project index
}

enum Mode {
    Normal,
    ConfirmKill(Sess),
    Rename(Sess, String),
    NewNamed(usize, String),
}

struct App {
    projects: Vec<Project>,
    rows: Vec<Row>,
    state: ListState,
    mode: Mode,
    msg: String,
}

enum Action {
    None,
    Quit,
    Shell,
    Attach(String),
}

impl App {
    fn refresh(&mut self) {
        let all = match sessions() {
            Ok(s) => s,
            Err(e) => {
                self.msg = format!("error: {e}");
                return;
            }
        };
        let keep = self.selected_key();
        let mut rows = vec![];
        for (i, p) in self.projects.iter().enumerate() {
            rows.push(Row::Header(p.name.clone(), p.dir.clone()));
            let mut ss: Vec<Sess> = all.iter().filter(|s| s.project == p.name).cloned().collect();
            ss.sort_by(|a, b| a.name.cmp(&b.name));
            rows.extend(ss.into_iter().map(Row::Sess));
            rows.push(Row::New(i));
        }
        let known: HashSet<&str> = self.projects.iter().map(|p| p.name.as_str()).collect();
        let mut other: Vec<Sess> = all.iter().filter(|s| !known.contains(s.project.as_str())).cloned().collect();
        if !other.is_empty() {
            other.sort_by(|a, b| a.name.cmp(&b.name));
            rows.push(Row::Header("other".into(), "sessions outside any project".into()));
            rows.extend(other.into_iter().map(Row::Sess));
        }
        self.rows = rows;
        let pos = keep
            .and_then(|k| self.rows.iter().position(|r| row_key(r).as_deref() == Some(k.as_str())))
            .or_else(|| self.state.selected().filter(|&i| i < self.rows.len() && selectable(&self.rows[i])))
            .or_else(|| self.rows.iter().position(selectable));
        self.state.select(pos);
    }

    fn selected_key(&self) -> Option<String> {
        self.state.selected().and_then(|i| self.rows.get(i)).and_then(row_key)
    }

    fn selected(&self) -> Option<&Row> {
        self.state.selected().and_then(|i| self.rows.get(i))
    }

    fn project_of_selected(&self) -> Option<usize> {
        let i = self.state.selected()?;
        self.rows[..=i].iter().rev().find_map(|r| match r {
            Row::Header(t, _) => self.projects.iter().position(|p| &p.name == t),
            Row::New(pi) => Some(*pi),
            _ => None,
        })
    }

    fn step(&mut self, d: i32) {
        let n = self.rows.len() as i32;
        let Some(mut i) = self.state.selected().map(|i| i as i32) else { return };
        for _ in 0..n {
            i = (i + d).rem_euclid(n);
            if selectable(&self.rows[i as usize]) {
                self.state.select(Some(i as usize));
                return;
            }
        }
    }

    fn create(&mut self, pi: usize, name: Option<&str>) -> Action {
        let all = sessions().unwrap_or_default();
        match new_session(&self.projects[pi], name, &all) {
            Ok(id) => Action::Attach(id),
            Err(e) => {
                self.msg = e.to_string();
                Action::None
            }
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) -> Action {
        self.msg.clear();
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::ConfirmKill(s) => {
                if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    match tmux(&["kill-session", "-t", &s.id]) {
                        Ok(_) => self.msg = format!("killed {}", s.name),
                        Err(e) => self.msg = e.to_string(),
                    }
                    self.refresh();
                } else {
                    self.msg = "kill cancelled".into();
                }
                Action::None
            }
            Mode::Rename(s, mut buf) => {
                match code {
                    KeyCode::Enter => {
                        let sl = slug(&buf);
                        let new = if s.project.is_empty() || sl.is_empty() { buf.trim().to_string() } else { format!("{}-{}", s.project, sl) };
                        if !new.is_empty() {
                            match tmux(&["rename-session", "-t", &s.id, &new]) {
                                Ok(_) => {
                                    let _ = tmux(&["set-option", "-t", &s.id, "@manual", "1"]);
                                    self.msg = format!("renamed to {new}");
                                }
                                Err(e) => self.msg = e.to_string(),
                            }
                        }
                        self.refresh();
                    }
                    KeyCode::Esc => {}
                    KeyCode::Backspace => {
                        buf.pop();
                        self.mode = Mode::Rename(s, buf);
                    }
                    KeyCode::Char(c) => {
                        buf.push(c);
                        self.mode = Mode::Rename(s, buf);
                    }
                    _ => self.mode = Mode::Rename(s, buf),
                }
                Action::None
            }
            Mode::NewNamed(pi, mut buf) => match code {
                KeyCode::Enter => {
                    let n = buf.trim().to_string();
                    self.create(pi, (!n.is_empty()).then_some(n.as_str()))
                }
                KeyCode::Esc => Action::None,
                KeyCode::Backspace => {
                    buf.pop();
                    self.mode = Mode::NewNamed(pi, buf);
                    Action::None
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    self.mode = Mode::NewNamed(pi, buf);
                    Action::None
                }
                _ => {
                    self.mode = Mode::NewNamed(pi, buf);
                    Action::None
                }
            },
            Mode::Normal => match code {
                KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
                KeyCode::Char('s') => Action::Shell,
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => Action::Quit,
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    self.step(1);
                    Action::None
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                    self.step(-1);
                    Action::None
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => match self.selected().cloned() {
                    Some(Row::Sess(s)) => Action::Attach(s.id),
                    Some(Row::New(pi)) => self.create(pi, None),
                    _ => Action::None,
                },
                KeyCode::Char('n') => match self.project_of_selected() {
                    Some(pi) => self.create(pi, None),
                    None => {
                        self.msg = "select a project first".into();
                        Action::None
                    }
                },
                KeyCode::Char('N') => {
                    match self.project_of_selected() {
                        Some(pi) => self.mode = Mode::NewNamed(pi, String::new()),
                        None => self.msg = "select a project first".into(),
                    }
                    Action::None
                }
                KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(Row::Sess(s)) = self.selected().cloned() {
                        self.mode = Mode::ConfirmKill(s);
                    }
                    Action::None
                }
                KeyCode::Char('r') => {
                    if let Some(Row::Sess(s)) = self.selected().cloned() {
                        self.mode = Mode::Rename(s, String::new());
                    }
                    Action::None
                }
                KeyCode::Char(c @ '1'..='9') => {
                    // jump to the n-th session overall
                    let n = c as usize - '0' as usize;
                    if let Some(i) = self.rows.iter().enumerate().filter(|(_, r)| matches!(r, Row::Sess(_))).nth(n - 1).map(|(i, _)| i) {
                        self.state.select(Some(i));
                    }
                    Action::None
                }
                _ => Action::None,
            },
        }
    }
}

fn row_key(r: &Row) -> Option<String> {
    match r {
        Row::Sess(s) => Some(s.id.clone()),
        Row::New(i) => Some(format!("new:{i}")),
        Row::Header(..) => None,
    }
}

fn selectable(r: &Row) -> bool {
    !matches!(r, Row::Header(..))
}

fn draw(f: &mut Frame, app: &mut App) {
    let [top, body, foot] = Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(2)]).areas(f.area());
    let host = fs::read_to_string("/etc/hostname").unwrap_or_else(|_| "kale".into());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" 󰒋 {} ", host.trim()), Style::new().fg(Color::Black).bg(Color::Green).bold()),
            Span::raw("  what do you want to work on?").bold(),
        ])),
        top,
    );
    let w = body.width as usize;
    let mut sess_no = 0;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|r| match r {
            Row::Header(t, d) => ListItem::new(Line::from(vec![
                Span::styled(format!(" {t}"), Style::new().fg(Color::Magenta).bold()),
                Span::styled(format!("  {d}"), Style::new().fg(Color::DarkGray)),
            ])),
            Row::New(_) => ListItem::new(Line::from(Span::styled("   + new session", Style::new().fg(Color::Cyan)))),
            Row::Sess(s) => {
                sess_no += 1;
                let num = if sess_no <= 9 { format!("{sess_no}") } else { " ".into() };
                let (dot, dc) = if s.attached > 0 { ("●", Color::Green) } else { ("○", Color::DarkGray) };
                let busy = s.agents.iter().any(|a| a.busy);
                let mut spans = vec![
                    Span::styled(format!(" {num} "), Style::new().fg(Color::DarkGray)),
                    Span::styled(format!("{dot} "), Style::new().fg(dc)),
                    Span::styled(format!("{:<22}", s.name), Style::new().bold()),
                ];
                if s.remote {
                    spans.push(Span::styled(" ⇄ remote  ", Style::new().fg(Color::Cyan)));
                } else if !s.agents.is_empty() {
                    let (g, c) = if busy { ("◆ working", Color::Yellow) } else { ("◇ idle   ", Color::Blue) };
                    spans.push(Span::styled(format!(" {g} "), Style::new().fg(c)));
                } else {
                    spans.push(Span::styled(" · shell   ", Style::new().fg(Color::DarkGray)));
                }
                let right = format!(
                    "{}{} {}",
                    if s.windows > 1 { format!("{}w ", s.windows) } else { String::new() },
                    if s.attached > 0 { "attached" } else { "detached" },
                    ago(s.activity)
                );
                let used = 4 + 2 + 22 + 11 + right.len() + 2;
                let mut label = s.label();
                if s.project.is_empty() && label.is_empty() {
                    label = s.path.replace(&home().to_string_lossy().into_owned(), "~");
                }
                let room = w.saturating_sub(used);
                let label: String = if label.chars().count() > room { label.chars().take(room.saturating_sub(1)).chain(['…']).collect() } else { label };
                let pad = room.saturating_sub(label.chars().count());
                spans.push(Span::raw(label));
                spans.push(Span::raw(" ".repeat(pad + 1)));
                spans.push(Span::styled(right, Style::new().fg(Color::DarkGray)));
                ListItem::new(Line::from(spans))
            }
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, body, &mut app.state);

    let prompt = match &app.mode {
        Mode::ConfirmKill(s) => {
            let warn = s.running_agents_label().map(|a| format!(" — {a} is running in it")).unwrap_or_default();
            Line::from(Span::styled(format!(" kill {}{warn}? y/N", s.name), Style::new().fg(Color::Red).bold()))
        }
        Mode::Rename(s, b) => Line::from(format!(" rename {} to: {}{}▏", s.name, if s.project.is_empty() { "" } else { &s.project }, if s.project.is_empty() { b.clone() } else { format!("-{b}") })),
        Mode::NewNamed(pi, b) => Line::from(format!(" new {} session named: {}-{b}▏", app.projects[*pi].name, app.projects[*pi].name)),
        Mode::Normal if !app.msg.is_empty() => Line::from(Span::styled(format!(" {}", app.msg), Style::new().fg(Color::Yellow))),
        Mode::Normal => Line::from(""),
    };
    let help = Line::from(Span::styled(
        " ↵ attach/new · n new · N new named · x kill · r rename · 1-9 jump · s shell · q quit · in a session: Alt+q back here",
        Style::new().fg(Color::DarkGray),
    ));
    f.render_widget(Paragraph::new(vec![prompt, help]), foot);
}

fn tui_enter() -> Result<DefaultTerminal> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut t = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;
    t.clear()?;
    Ok(t)
}

fn tui_leave() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, ratatui::crossterm::cursor::Show);
}

fn picker(first: Option<String>) -> Result<()> {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tui_leave();
        prev(info);
    }));
    let mut app = App { projects: load_projects()?, rows: vec![], state: ListState::default(), mode: Mode::Normal, msg: String::new() };
    let mut pending = first;
    loop {
        if let Some(id) = pending.take() {
            if let Err(e) = attach(&id) {
                app.msg = e.to_string();
            }
            if env::var_os("TMUX").is_some() && env::var_os("KALE_TMUX_SOCKET").is_none() {
                return Ok(()); // switched client; nothing to come back to
            }
        }
        let mut term = tui_enter()?;
        app.refresh();
        let mut last = std::time::Instant::now();
        let action = loop {
            term.draw(|f| draw(f, &mut app))?;
            if event::poll(Duration::from_millis(500))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Press {
                        match app.key(k.code, k.modifiers) {
                            Action::None => {}
                            a => break a,
                        }
                    }
                }
            }
            if last.elapsed() >= Duration::from_secs(2) && matches!(app.mode, Mode::Normal) {
                app.refresh();
                last = std::time::Instant::now();
            }
        };
        drop(term);
        tui_leave();
        match action {
            Action::Quit => return Ok(()),
            Action::Shell => std::process::exit(10), // ~/.zshrc login hook: stay in a plain shell
            Action::Attach(id) => pending = Some(id),
            Action::None => {}
        }
    }
}

// ---------- CLI ----------

const HELP: &str = "kale — sessions on kale, one per terminal tab

  kale                     picker: pick / start / kill sessions; detach (Alt+q) returns here
  kale <project> [name]    start a new session in <project> (or attach <project>-<name>)
  kale ls                  list sessions
  kale attach <session>    attach by exact name
  kale kill <session> [-y] kill by exact name
  kale projects            list projects (~/.config/kale/projects.toml)
  kale remote-up           start missing Remote Control servers (projects with remote = true)
  kale tmux-titles         sync tmux tab labels (@cc_name) and session auto-renames for Claude/agy
";

fn print_ls() -> Result<()> {
    let all = sessions()?;
    let projects = load_projects()?;
    if all.is_empty() {
        println!("no sessions");
    }
    let known: HashSet<&str> = projects.iter().map(|p| p.name.as_str()).collect();
    let mut groups: Vec<(String, Vec<&Sess>)> = projects.iter().map(|p| (p.name.clone(), all.iter().filter(|s| s.project == p.name).collect())).collect();
    groups.push(("other".into(), all.iter().filter(|s| !known.contains(s.project.as_str())).collect()));
    for (g, ss) in groups {
        if ss.is_empty() {
            continue;
        }
        println!("{g}");
        for s in ss {
            let c = if s.remote { "remote" } else if s.agents.is_empty() { "shell" } else if s.agents.iter().any(|a| a.busy) { "working" } else { "idle" };
            println!("  {:<24} {:<8} {:<9} {:>4}  {}", s.name, c, if s.attached > 0 { "attached" } else { "detached" }, ago(s.activity), s.label());
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        [] => picker(None),
        ["-h" | "--help" | "help"] => {
            print!("{HELP}");
            Ok(())
        }
        ["ls" | "list"] => print_ls(),
        ["remote-up"] => remote_up(&load_projects()?),
        ["tmux-titles"] => tmux_titles(),
        ["projects"] => {
            for p in load_projects()? {
                println!("{:<12} {}", p.name, p.dir);
            }
            Ok(())
        }
        ["attach" | "a", name] => {
            let s = find_exact(&sessions()?, name).with_context(|| format!("no session named {name} (kale ls)"))?;
            picker(Some(s.id))
        }
        ["kill" | "k", name, rest @ ..] => {
            let s = find_exact(&sessions()?, name).with_context(|| format!("no session named {name}"))?;
            if !rest.contains(&"-y") {
                let warn = s.running_agents_label().map(|a| format!(" ({a} is running in it)")).unwrap_or_default();
                print!("kill {}{warn}? [y/N] ", s.name);
                io::stdout().flush()?;
                let mut l = String::new();
                io::stdin().read_line(&mut l)?;
                if !l.trim().eq_ignore_ascii_case("y") {
                    println!("cancelled");
                    return Ok(());
                }
            }
            tmux(&["kill-session", "-t", &s.id])?;
            println!("killed {}", s.name);
            Ok(())
        }
        [proj, rest @ ..] if rest.len() <= 1 => {
            let projects = load_projects()?;
            let Some(p) = projects.iter().find(|p| p.name == *proj) else {
                eprint!("unknown command or project {proj:?}\n\n{HELP}");
                std::process::exit(2);
            };
            let all = sessions()?;
            if let Some(n) = rest.first() {
                let full = format!("{}-{}", p.name, slug(n.strip_prefix(&format!("{}-", p.name)).unwrap_or(n)));
                if let Some(s) = find_exact(&all, &full) {
                    return picker(Some(s.id));
                }
            }
            let id = new_session(p, rest.first().copied(), &all)?;
            picker(Some(id))
        }
        _ => {
            eprint!("{HELP}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pbtxt_handles_plain_and_escaped() {
        assert_eq!(parse_pbtxt_title(r#"title:"upgrade-merge""#), Some("upgrade-merge".into()));
        assert_eq!(parse_pbtxt_title("title: \"Fix \\\"quoted\\\"\\\\path\"\n"), Some("Fix \"quoted\"\\path".into()));
        assert_eq!(parse_pbtxt_title("title:\"  \""), None);
        assert_eq!(parse_pbtxt_title("other:\"foo\""), None);
    }

    #[test]
    fn truncate_title_preserves_unicode_and_bounds() {
        assert_eq!(truncate_title("short", 32), "short");
        let long = "a".repeat(40);
        let t = truncate_title(&long, 32);
        assert_eq!(t.chars().count(), 32);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn agy_summaries_reads_wal_status_and_title() {
        let dir = env::temp_dir().join(format!("kale-agy-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("annotations")).unwrap();
        let db = dir.join("conversation_summaries.db");
        let con = Connection::open(&db).unwrap();
        con.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE conversation_summaries (
               conversation_id TEXT PRIMARY KEY,
               title TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT '',
               not_fully_idle INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO conversation_summaries VALUES ('c1', 'upgrade-merge', 'CASCADE_RUN_STATUS_IDLE', 0);
             INSERT INTO conversation_summaries VALUES ('c2', 'Casual Greeting', 'CASCADE_RUN_STATUS_RUNNING', 1);
             INSERT INTO conversation_summaries VALUES ('c3', '', 'CASCADE_RUN_STATUS_IDLE', 1);",
        )
        .unwrap();
        fs::write(dir.join("annotations/c1.pbtxt"), r#"title:"override-title""#).unwrap();

        let summaries = agy_summaries(&dir);
        assert_eq!(summaries["c1"].title.as_deref(), Some("upgrade-merge"));
        assert!(!summaries["c1"].busy);
        assert_eq!(summaries["c2"].title.as_deref(), Some("Casual Greeting"));
        assert!(summaries["c2"].busy);
        assert_eq!(summaries["c3"].title, None);
        assert!(summaries["c3"].busy);

        let overrides = HashMap::from([("%1".into(), "c1".into()), ("%2".into(), "c2".into())]);
        assert_eq!(resolve_agy_pane("%1", 0, &dir, &overrides, &summaries), (Some("override-title".into()), false));
        assert_eq!(resolve_agy_pane("%2", 0, &dir, &overrides, &summaries), (Some("Casual Greeting".into()), true));

        drop(con);
        let _ = fs::remove_dir_all(&dir);
    }
}

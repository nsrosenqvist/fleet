//! TUI app state + crossterm event loop.
//!
//! Refresh lives on a background thread so rendering and input never block on
//! the limactl→ao roundtrip (which can be 100–300 ms). The main loop drains
//! updates via `mpsc::try_recv()` between draws.
//!
//! Aesthetic borrows from `example.png` (the `keel` TUI): rounded borders,
//! section labels styled in colored text on the border line, a single
//! dot-separated status bar at the bottom, top breadcrumb.

use anyhow::Result;
use crossterm::{event, execute, terminal};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::ao::{Ao, SessionInfo};
use crate::lima::{Lima, VmStatus};
use crate::process::{ProcessInvoker, RealProcessInvoker};

use super::subprocess::suspend_around;

const REFRESH_INTERVAL: Duration = Duration::from_millis(1500);
const TICK_INTERVAL: Duration = Duration::from_millis(50);
const VM_NAME: &str = "fleet-vm";
const AO_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const VM_PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// Messages sent from the background refresh thread back to the UI thread.
enum RefreshUpdate {
    Sessions(Vec<SessionInfo>),
    PaneCapture { session_id: String, output: String },
    Error(String),
    AoUp(bool),
    VmUp(VmStatus),
}

/// Messages sent from the UI thread to the refresh thread.
enum RefreshCommand {
    ForceRefresh,
    Shutdown,
}

/// Pending destructive action awaiting a y/N confirmation in the status bar.
#[derive(Debug, Clone)]
enum Confirm {
    KillSession(String),
    StopAo,
}

impl Confirm {
    fn prompt(&self) -> String {
        match self {
            Self::KillSession(id) => format!("Kill session {id}? [y/N]"),
            Self::StopAo => "Stop AO orchestrator + dashboard? [y/N]".to_string(),
        }
    }
}

pub struct App {
    repo_root: PathBuf,
    invoker: Arc<dyn ProcessInvoker>,

    // State painted to the screen.
    sessions: Vec<SessionInfo>,
    selected: usize,
    last_error: Option<String>,
    last_info: Option<String>,
    ao_up: bool,
    vm_status: VmStatus,
    confirm: Option<Confirm>,
    /// Most recent tmux pane capture per session, keyed by session id.
    /// Populated by the background refresh thread; rendered in the output panel.
    pane_outputs: std::collections::HashMap<String, String>,

    // Control flow.
    should_quit: bool,

    // Channels to/from the refresh thread (Option so we can take ownership
    // on shutdown).
    refresh_cmd_tx: Option<mpsc::Sender<RefreshCommand>>,
    refresh_update_rx: Option<mpsc::Receiver<RefreshUpdate>>,
    refresh_handle: Option<JoinHandle<()>>,
}

impl App {
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(repo_root: &Path) -> Result<Self> {
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            invoker: Arc::new(RealProcessInvoker),
            sessions: Vec::new(),
            selected: 0,
            last_error: None,
            last_info: None,
            ao_up: false,
            vm_status: VmStatus::Missing,
            confirm: None,
            pane_outputs: std::collections::HashMap::new(),
            should_quit: false,
            refresh_cmd_tx: None,
            refresh_update_rx: None,
            refresh_handle: None,
        })
    }

    pub fn run(mut self) -> Result<i32> {
        let mut stdout = io::stdout();
        terminal::enable_raw_mode()?;
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;
        let backend = CrosstermBackend::new(stdout);
        let mut term = Terminal::new(backend)?;

        self.spawn_refresh_thread();
        let result = self.event_loop(&mut term);
        self.shutdown_refresh_thread();

        execute!(
            io::stdout(),
            terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        )?;
        terminal::disable_raw_mode()?;
        result
    }

    fn spawn_refresh_thread(&mut self) {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let repo_root = self.repo_root.clone();
        let invoker = self.invoker.clone();
        let handle = thread::spawn(move || refresh_loop(repo_root, invoker, cmd_rx, &update_tx));
        self.refresh_cmd_tx = Some(cmd_tx);
        self.refresh_update_rx = Some(update_rx);
        self.refresh_handle = Some(handle);
    }

    fn shutdown_refresh_thread(&mut self) {
        if let Some(tx) = self.refresh_cmd_tx.take() {
            let _ = tx.send(RefreshCommand::Shutdown);
        }
        if let Some(handle) = self.refresh_handle.take() {
            let _ = handle.join();
        }
    }

    fn event_loop(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<i32> {
        while !self.should_quit {
            self.drain_updates();
            term.draw(|f| self.draw(f))?;

            if event::poll(TICK_INTERVAL)?
                && let event::Event::Key(key) = event::read()?
                && key.kind == event::KeyEventKind::Press
            {
                self.handle_key(term, key);
            }
        }
        Ok(0)
    }

    fn drain_updates(&mut self) {
        let Some(rx) = &self.refresh_update_rx else {
            return;
        };
        while let Ok(update) = rx.try_recv() {
            match update {
                RefreshUpdate::Sessions(s) => {
                    // Drop captures for sessions that vanished.
                    let alive: std::collections::HashSet<String> =
                        s.iter().filter_map(|s| s.id.clone()).collect();
                    self.pane_outputs.retain(|k, _| alive.contains(k));
                    self.sessions = s;
                    self.last_error = None;
                    if !self.sessions.is_empty() && self.selected >= self.sessions.len() {
                        self.selected = self.sessions.len() - 1;
                    }
                }
                RefreshUpdate::PaneCapture { session_id, output } => {
                    self.pane_outputs.insert(session_id, output);
                }
                RefreshUpdate::Error(e) => {
                    self.last_error = Some(e);
                }
                RefreshUpdate::AoUp(up) => self.ao_up = up,
                RefreshUpdate::VmUp(s) => self.vm_status = s,
            }
        }
    }

    fn request_refresh(&self) {
        if let Some(tx) = &self.refresh_cmd_tx {
            let _ = tx.send(RefreshCommand::ForceRefresh);
        }
    }

    fn handle_key(
        &mut self,
        term: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key: event::KeyEvent,
    ) {
        use event::{KeyCode, KeyModifiers};

        // If a confirmation is pending, only y/Y accepts; anything else cancels.
        if self.confirm.is_some() {
            if let KeyCode::Char('y' | 'Y') = key.code {
                self.confirm_yes(term);
            } else {
                self.confirm = None;
                self.last_info = Some("cancelled".to_string());
            }
            return;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            (KeyCode::Down | KeyCode::Char('j'), _) => self.nav_down(),
            (KeyCode::Up, _) => self.nav_up(),
            (KeyCode::Char('r'), _) => self.request_refresh(),
            (KeyCode::Enter, _) => self.attach_selected(term),

            // `k` alone navigates up (vim). Holding shift uses `K` to kill.
            (KeyCode::Char('k'), m) if !m.contains(KeyModifiers::SHIFT) => self.nav_up(),
            (KeyCode::Char('K') | KeyCode::Delete, _) => {
                if let Some(id) = self.sessions.get(self.selected).and_then(|s| s.id.clone()) {
                    self.confirm = Some(Confirm::KillSession(id));
                }
            }
            (KeyCode::Char('c'), _) => self.edit_config(term),
            (KeyCode::Char('t'), _) => self.tracker_preview(term),
            (KeyCode::Char('S'), _) => self.start_ao(term),
            (KeyCode::Char('X'), _) => {
                self.confirm = Some(Confirm::StopAo);
            }
            (KeyCode::Char('W'), _) => self.open_web(),
            _ => {}
        }
    }

    fn nav_down(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    fn nav_up(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.sessions.len() - 1);
        }
    }

    fn confirm_yes(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
        let pending = self.confirm.take();
        match pending {
            Some(Confirm::KillSession(id)) => {
                let res = suspend_around(term, || {
                    crate::cli::passthrough::run_with_prefix(
                        &self.repo_root_clone(),
                        "session",
                        &["kill".to_string(), id.clone()],
                    )
                    .map(|_| ())
                });
                self.flash_result(format!("killed {id}"), res);
                self.request_refresh();
            }
            Some(Confirm::StopAo) => {
                let res = suspend_around(term, || {
                    crate::cli::passthrough::run(&self.repo_root_clone(), &["stop".to_string()])
                        .map(|_| ())
                });
                self.flash_result("stopped AO".to_string(), res);
                self.request_refresh();
            }
            None => {}
        }
    }

    fn edit_config(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let yaml_path = self.repo_root.join("agent-orchestrator.yaml");
        let res = suspend_around(term, || {
            crate::process::run_interactive(&editor, &[yaml_path.display().to_string()], &[], &[])
                .map(|_| ())
        });
        self.flash_result("edited config".to_string(), res);
    }

    fn tracker_preview(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
        // the sandbox project uses git-bug. When we add more
        // trackers, dispatch on `project.tracker.plugin` (read from the
        // agent-orchestrator.yaml or AO state).
        let res = suspend_around(term, || {
            crate::cli::passthrough::run(
                &self.repo_root_clone(),
                &[
                    // Use `git-bug` directly via limactl — bypass `ao` since
                    // `ao` doesn't expose a tracker-UI subcommand.
                ],
            )
            // Fall through to the explicit limactl shell call below.
            .map(|_| ())
            .or_else(|_| Ok::<(), anyhow::Error>(()))?;
            crate::process::run_interactive(
                "limactl",
                &[
                    "shell".to_string(),
                    "--workdir".to_string(),
                    "/Users/niklas/Code/agent-team/tmp/git-bug-sandbox".to_string(),
                    "fleet-vm".to_string(),
                    "git-bug".to_string(),
                    "termui".to_string(),
                ],
                &[("TERM", "xterm-256color")],
                &[],
            )
            .map(|_| ())
        });
        self.flash_result("closed tracker preview".to_string(), res);
    }

    fn start_ao(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
        let res = suspend_around(term, || {
            crate::cli::spawn::run_start(&self.repo_root_clone(), false, false).map(|_| ())
        });
        self.flash_result("started AO".to_string(), res);
        self.request_refresh();
    }

    fn open_web(&mut self) {
        let url = self.sessions.get(self.selected).map_or_else(
            || "http://localhost:3000".to_string(),
            |s| {
                let project = s.project_id.as_deref().unwrap_or("default");
                let id = s.id.as_deref().unwrap_or("");
                format!("http://localhost:3000/projects/{project}/sessions/{id}")
            },
        );
        if !self.ao_up {
            self.last_info = Some(format!(
                "AO dashboard not running — start with Shift+S, then retry [Shift+W] (url: {url})"
            ));
            return;
        }
        match webbrowser::open(&url) {
            Ok(()) => self.last_info = Some(format!("opened {url}")),
            Err(e) => self.last_error = Some(format!("open: {e}")),
        }
    }

    /// Clone of `repo_root` to satisfy borrow rules inside `&mut self` callbacks.
    fn repo_root_clone(&self) -> PathBuf {
        self.repo_root.clone()
    }

    fn flash_result(&mut self, ok_msg: String, res: anyhow::Result<()>) {
        match res {
            Ok(()) => {
                self.last_info = Some(ok_msg);
                self.last_error = None;
            }
            Err(e) => {
                self.last_error = Some(format!("{e:#}").lines().next().unwrap_or("").to_string());
            }
        }
    }

    /// Drop into `tmux attach` on the selected session. Suspends the TUI
    /// around the child so claude (or whatever runs in the pane) gets a real
    /// TTY. Re-enters the TUI when the user detaches.
    fn attach_selected(&mut self, term: &mut Terminal<CrosstermBackend<io::Stdout>>) {
        let Some(target) = self.sessions.get(self.selected).and_then(|s| s.id.clone()) else {
            return;
        };
        let repo_root = self.repo_root.clone();
        let res = suspend_around(term, move || {
            crate::cli::attach::run(&repo_root, &target).map(|_| ())
        });
        // Re-render on resume. State will refresh on the next tick anyway.
        if let Err(e) = res {
            self.last_error = Some(format!("attach failed: {e:#}"));
        }
    }

    // ----- Drawing --------------------------------------------------------

    fn draw(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // breadcrumb
                Constraint::Min(0),    // body
                Constraint::Length(1), // status bar
            ])
            .split(area);

        self.draw_breadcrumb(frame, layout[0]);
        self.draw_body(frame, layout[1]);
        self.draw_status_bar(frame, layout[2]);
    }

    fn draw_breadcrumb(&self, frame: &mut Frame<'_>, area: Rect) {
        // Left chunk: project chain. Right chunk: AO/VM badges.
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(18)])
            .split(area);

        let chain = self.sessions.get(self.selected).map_or_else(
            || {
                vec![
                    chip("fleet", Color::Cyan),
                    Span::raw(" | "),
                    Span::raw("(no sessions)"),
                ]
            },
            |s| {
                vec![
                    chip("fleet", Color::Cyan),
                    Span::raw(" | "),
                    Span::raw(s.project_id.clone().unwrap_or_else(|| "?".into())),
                    Span::raw(" | "),
                    Span::raw(s.id.clone().unwrap_or_else(|| "?".into())),
                    Span::raw(" "),
                    Span::styled(
                        s.status.clone().unwrap_or_else(|| "?".into()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]
            },
        );
        frame.render_widget(Paragraph::new(Line::from(chain)), cols[0]);

        let ao_color = if self.ao_up { Color::Green } else { Color::Red };
        let vm_color = match self.vm_status {
            VmStatus::Running => Color::Green,
            _ => Color::Red,
        };
        let badges = Line::from(vec![
            Span::raw(" "),
            Span::styled("AO", Style::default().fg(Color::DarkGray)),
            Span::raw(":"),
            Span::styled("●", Style::default().fg(ao_color)),
            Span::raw("  "),
            Span::styled("VM", Style::default().fg(Color::DarkGray)),
            Span::raw(":"),
            Span::styled("●", Style::default().fg(vm_color)),
        ]);
        frame.render_widget(Paragraph::new(badges).alignment(Alignment::Right), cols[1]);
    }

    fn draw_body(&self, frame: &mut Frame<'_>, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(0)])
            .split(area);

        // Left: sessions list
        let items: Vec<ListItem<'_>> = if self.sessions.is_empty() {
            vec![ListItem::new(Line::from(Span::styled(
                "(no sessions)",
                Style::default().fg(Color::DarkGray),
            )))]
        } else {
            self.sessions
                .iter()
                .map(|s| {
                    let id = s.id.clone().unwrap_or_else(|| "?".into());
                    let activity = s.activity.clone().unwrap_or_default();
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{id:<8}")),
                        Span::styled(activity, Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect()
        };
        let list_count = format!(" sessions ({}) ", self.sessions.len());
        let list = List::new(items)
            .block(framed_block(&list_count))
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = ListState::default();
        if !self.sessions.is_empty() {
            state.select(Some(self.selected));
        }
        frame.render_stateful_widget(list, cols[0], &mut state);

        // Right: details (top) + output (bottom)
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(11), Constraint::Min(0)])
            .split(cols[1]);
        self.draw_details(frame, rows[0]);
        self.draw_output(frame, rows[1]);
    }

    fn draw_details(&self, frame: &mut Frame<'_>, area: Rect) {
        let selected = self.sessions.get(self.selected);
        let body = selected.map_or_else(
            || Text::from("(no session selected)").style(Style::default().fg(Color::DarkGray)),
            |s| {
                let kv = |label: &str, value: &str| {
                    Line::from(vec![
                        Span::styled(
                            format!("  {label:<10}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(value.to_string()),
                    ])
                };
                Text::from(vec![
                    kv("Branch", s.branch.as_deref().unwrap_or("(none)")),
                    kv("Ticket", s.issue_id.as_deref().unwrap_or("(none)")),
                    kv("Project", s.project_id.as_deref().unwrap_or("(none)")),
                    kv("Status", s.status.as_deref().unwrap_or("(none)")),
                    kv("Activity", s.activity.as_deref().unwrap_or("(none)")),
                    kv("Last", s.last_activity.as_deref().unwrap_or("(none)")),
                    kv(
                        "Summary",
                        s.claude_summary
                            .as_deref()
                            .or(s.summary.as_deref())
                            .unwrap_or("(none)"),
                    ),
                ])
            },
        );
        let title = selected
            .and_then(|s| s.id.as_deref())
            .map_or_else(|| " details ".to_string(), |id| format!(" details — {id} "));
        frame.render_widget(
            Paragraph::new(body)
                .block(framed_block(&title))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_output(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = framed_block(" output ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let selected_id = self.sessions.get(self.selected).and_then(|s| s.id.clone());
        let body = selected_id.as_deref().map_or_else(
            || "(no session selected)".to_string(),
            |id| match self.pane_outputs.get(id) {
                Some(pane) if !pane.is_empty() => pane.clone(),
                _ => "(no output captured yet — first refresh after spawn takes ~1.5s)".to_string(),
            },
        );

        // Tail the captured pane so the most recent activity is visible. Pane
        // capture comes in as plain text from `tmux capture-pane -p`; the last
        // `area.height` lines fit. If the body is shorter, just render as is.
        let line_count = body.lines().count();
        let take = inner.height as usize;
        let lines: Vec<Line<'_>> = body
            .lines()
            .skip(line_count.saturating_sub(take))
            .map(Line::raw)
            .collect();
        let style = if selected_id
            .as_deref()
            .and_then(|id| self.pane_outputs.get(id))
            .is_some_and(|s| !s.is_empty())
        {
            Style::default()
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(lines)
                .style(style)
                .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn draw_status_bar(&self, frame: &mut Frame<'_>, area: Rect) {
        // Confirmation prompt takes over the status bar when active.
        if let Some(c) = &self.confirm {
            let prompt = Line::from(vec![
                chip("[confirm]", Color::Yellow),
                Span::raw("  "),
                Span::raw(c.prompt()),
            ]);
            frame.render_widget(
                Paragraph::new(prompt).style(Style::default().fg(Color::Yellow)),
                area,
            );
            return;
        }

        let mut spans: Vec<Span<'_>> = vec![
            chip("[fleet]", Color::Cyan),
            sep(),
            key("↑/↓"),
            Span::raw(" nav"),
            sep(),
            key("Enter"),
            Span::raw(" attach"),
            sep(),
            key("t"),
            Span::raw(" tracker"),
            sep(),
            key("c"),
            Span::raw(" config"),
            sep(),
            key("K"),
            Span::raw(" kill"),
            sep(),
            key("⇧S"),
            Span::raw("/⇧X start/stop"),
            sep(),
            key("⇧W"),
            Span::raw(" web"),
            sep(),
            key("r"),
            Span::raw(" refresh"),
            sep(),
            key("q"),
            Span::raw(" quit"),
        ];
        if let Some(e) = &self.last_error {
            spans.push(sep());
            spans.push(Span::styled(
                format!("[error] {e}"),
                Style::default().fg(Color::Red),
            ));
        } else if let Some(m) = &self.last_info {
            spans.push(sep());
            spans.push(Span::styled(m.clone(), Style::default().fg(Color::Green)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }
}

// ----- Helpers ------------------------------------------------------------

fn framed_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(Color::Cyan),
        ))
}

fn chip(text: &str, color: Color) -> Span<'_> {
    Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn sep() -> Span<'static> {
    Span::styled("  ·  ", Style::default().fg(Color::DarkGray))
}

fn key(k: &str) -> Span<'_> {
    Span::styled(
        k.to_string(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
}

// ----- Background refresh thread -----------------------------------------

// These args are passed by value because the thread takes ownership at spawn;
// clippy::needless_pass_by_value doesn't account for that pattern.
#[allow(clippy::needless_pass_by_value)]
fn refresh_loop(
    repo_root: PathBuf,
    invoker: Arc<dyn ProcessInvoker>,
    cmds: mpsc::Receiver<RefreshCommand>,
    updates: &mpsc::Sender<RefreshUpdate>,
) {
    let lima = Lima::new(invoker.clone(), VM_NAME);
    let mut next_status = Instant::now();
    let mut next_ao_probe = Instant::now();
    let mut next_vm_probe = Instant::now();

    loop {
        let now = Instant::now();
        let wait_for = [next_status, next_ao_probe, next_vm_probe]
            .into_iter()
            .map(|t| t.saturating_duration_since(now))
            .min()
            .unwrap_or(Duration::from_millis(100));

        match cmds.recv_timeout(wait_for) {
            Ok(RefreshCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Ok(RefreshCommand::ForceRefresh) => {
                next_status = Instant::now();
                next_ao_probe = Instant::now();
                next_vm_probe = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        if now >= next_status {
            let ao = Ao::new(&lima, &repo_root);
            match ao.status() {
                Ok(r) => {
                    // Send Sessions first so the UI sees the structure
                    // immediately, then capture each pane sequentially and
                    // send incremental PaneCapture updates as they finish.
                    let session_ids: Vec<String> =
                        r.data.iter().filter_map(|s| s.id.clone()).collect();
                    if updates.send(RefreshUpdate::Sessions(r.data)).is_err() {
                        return;
                    }
                    for id in session_ids {
                        // 200 lines of scrollback is enough to fill any
                        // realistic output panel. tmux capture inside the VM
                        // is fast (~30 ms); we tail at render time.
                        if let Ok(out) = crate::tmux::capture_pane(&lima, &repo_root, &id, 200)
                            && updates
                                .send(RefreshUpdate::PaneCapture {
                                    session_id: id,
                                    output: out,
                                })
                                .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    if updates
                        .send(RefreshUpdate::Error(
                            format!("{e:#}").lines().next().unwrap_or("").to_string(),
                        ))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            next_status = now + REFRESH_INTERVAL;
        }

        if now >= next_ao_probe {
            // Cheap TCP probe — the AO dashboard listens on localhost:3000
            // when `ao start` is up. Two-second timeout so a hung probe
            // doesn't stall the refresh loop.
            let up = std::net::TcpStream::connect_timeout(
                &"127.0.0.1:3000".parse().unwrap(),
                Duration::from_millis(200),
            )
            .is_ok();
            if updates.send(RefreshUpdate::AoUp(up)).is_err() {
                return;
            }
            next_ao_probe = now + AO_PROBE_INTERVAL;
        }

        if now >= next_vm_probe {
            let status = lima.status();
            if updates.send(RefreshUpdate::VmUp(status)).is_err() {
                return;
            }
            next_vm_probe = now + VM_PROBE_INTERVAL;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> SessionInfo {
        SessionInfo {
            id: Some(id.to_string()),
            ..Default::default()
        }
    }

    fn app_with(n: usize) -> App {
        let mut app = App::new(std::path::Path::new(".")).expect("App::new");
        app.sessions = (0..n).map(|i| session(&format!("sb-{}", i + 1))).collect();
        app
    }

    #[test]
    fn initial_state_has_no_selection_marker_but_index_zero() {
        let app = App::new(std::path::Path::new(".")).expect("ok");
        assert!(app.sessions.is_empty());
        assert_eq!(app.selected, 0);
        assert!(!app.should_quit);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn nav_down_wraps_at_end() {
        let mut app = app_with(3);
        app.nav_down();
        app.nav_down();
        assert_eq!(app.selected, 2);
        app.nav_down();
        assert_eq!(app.selected, 0, "should wrap from last back to first");
    }

    #[test]
    fn nav_up_wraps_from_zero() {
        let mut app = app_with(3);
        app.nav_up();
        assert_eq!(app.selected, 2, "should wrap from 0 to last");
    }

    #[test]
    fn nav_is_a_noop_on_empty_list() {
        let mut app = app_with(0);
        app.nav_down();
        app.nav_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn drain_updates_replaces_sessions_and_clamps_selection() {
        let mut app = app_with(3);
        app.selected = 2;
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Sessions(vec![session("only")]))
            .unwrap();
        app.drain_updates();
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.selected, 0, "selection should clamp to new length");
    }

    #[test]
    fn drain_updates_drops_pane_captures_for_dead_sessions() {
        let mut app = app_with(2);
        app.pane_outputs.insert("sb-1".into(), "live output".into());
        app.pane_outputs
            .insert("ghost".into(), "stale output".into());
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Sessions(vec![session("sb-1")]))
            .unwrap();
        app.drain_updates();
        assert!(app.pane_outputs.contains_key("sb-1"));
        assert!(
            !app.pane_outputs.contains_key("ghost"),
            "vanished session's pane capture should be evicted"
        );
    }

    #[test]
    fn drain_updates_applies_pane_capture() {
        let mut app = app_with(1);
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::PaneCapture {
            session_id: "sb-1".into(),
            output: "hello\nworld".into(),
        })
        .unwrap();
        app.drain_updates();
        assert_eq!(
            app.pane_outputs.get("sb-1").map(String::as_str),
            Some("hello\nworld")
        );
    }

    #[test]
    fn drain_updates_records_error() {
        let mut app = app_with(0);
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::Error("ao unreachable".into()))
            .unwrap();
        app.drain_updates();
        assert_eq!(app.last_error.as_deref(), Some("ao unreachable"));
    }

    #[test]
    fn drain_updates_applies_ao_and_vm_status() {
        let mut app = app_with(0);
        let (tx, rx) = mpsc::channel();
        app.refresh_update_rx = Some(rx);
        tx.send(RefreshUpdate::AoUp(true)).unwrap();
        tx.send(RefreshUpdate::VmUp(VmStatus::Running)).unwrap();
        app.drain_updates();
        assert!(app.ao_up);
        assert_eq!(app.vm_status, VmStatus::Running);
    }

    #[test]
    fn confirm_prompts_name_the_action() {
        let kill = Confirm::KillSession("sb-1".into());
        assert!(kill.prompt().contains("sb-1"));
        assert!(kill.prompt().contains("Kill"));

        let stop = Confirm::StopAo;
        assert!(stop.prompt().contains("Stop AO"));
    }
}

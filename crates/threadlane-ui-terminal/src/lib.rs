use std::future::Future;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

mod search;
mod links;
#[cfg(test)]
mod links_tests;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use links::{visible_links, TerminalLink};
pub use links::is_web_url;
use gpui_component::ThemeMode;
use gpui_component::{ActiveTheme, Disableable, ElementExt, Icon, IconName, Sizable, WindowExt};

use search::{
    cue_row, next_find_match, reveal_offset, scan_retained_output, TerminalSearchHit,
    TerminalSearchOutcome,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const DEFAULT_ROWS: u16 = 30;
const DEFAULT_COLS: u16 = 120;
const SCROLLBACK_ROWS: usize = 10_000;
const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
const TERMINAL_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const TERMINAL_FLOOD_FRAME_INTERVAL: Duration = Duration::from_millis(33);
const TERMINAL_READ_CHUNK_BYTES: usize = 8192;
const TERMINAL_OUTPUT_BUFFERED_CHUNKS: usize = 8;
const TERMINAL_PARSE_BUDGET_PER_FRAME: usize = TERMINAL_READ_CHUNK_BYTES * 2;
/// Debounce between the last query keystroke and a worker scan.
const TERMINAL_FIND_DEBOUNCE: Duration = Duration::from_millis(120);
/// Minimum interval between background rescans while find is open and output
/// keeps arriving; scans never run when find is closed.
const TERMINAL_FIND_RESCAN_INTERVAL: Duration = Duration::from_millis(250);

actions!(
    threadlane_terminal,
    [
        FindInTerminalOutput,
        CloseTerminalFind,
        NextTerminalMatch,
        PreviousTerminalMatch
    ]
);

/// Registers the terminal's focus-scoped keybindings: Cmd+F (macOS) or
/// Ctrl+Shift+F opens find from the focused terminal, Escape closes it, and
/// Enter/Shift+Enter navigate while the find input is focused.
pub fn init(cx: &mut App) {
    let find_shortcut = if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        // Ctrl+F stays with the shell: readline and TUI programs own it.
        "ctrl-shift-f"
    };
    cx.bind_keys([
        KeyBinding::new(find_shortcut, FindInTerminalOutput, Some("Terminal")),
        KeyBinding::new("escape", CloseTerminalFind, Some("TerminalFind")),
        KeyBinding::new("enter", NextTerminalMatch, Some("TerminalFind > Input")),
        KeyBinding::new(
            "shift-enter",
            PreviousTerminalMatch,
            Some("TerminalFind > Input"),
        ),
    ]);
}

/// Terminal text metrics. The painted glyph size, row height, hit-testing,
/// and resize math must all agree; they share these constants so a font
/// change cannot drift click-to-select away from what is painted.
/// Row height = selected font size × selected line height.
/// The screen container uses `p_3`, so the content inset is 12px per side.
const TERMINAL_FONT_SIZE: f32 = 13.0;
const TERMINAL_LINE_HEIGHT: f32 = 1.35;
const TERMINAL_COMPACT_LINE_HEIGHT: f32 = 1.15;
const TERMINAL_CONTENT_INSET: f32 = 12.0;
/// Fallback advance width until the text system measures `.ZedMono`.
const TERMINAL_CELL_WIDTH_FALLBACK: f32 = 7.8;

fn terminal_frame_policy(saturated: bool) -> (Duration, usize) {
    if saturated {
        (
            TERMINAL_FLOOD_FRAME_INTERVAL,
            TERMINAL_PARSE_BUDGET_PER_FRAME * 2,
        )
    } else {
        (TERMINAL_FRAME_INTERVAL, TERMINAL_PARSE_BUDGET_PER_FRAME)
    }
}

fn terminal_parse_budget_exhausted(parsed_bytes: usize, parse_budget: usize) -> bool {
    parsed_bytes >= parse_budget
}

struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Box<dyn Child + Send + Sync>,
}

impl PtySession {
    fn write(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            if let Err(error) = writer.write_all(bytes).and_then(|_| writer.flush()) {
                tracing::warn!("failed to write to terminal PTY: {error}");
            }
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        if let Err(error) = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            tracing::warn!("failed to resize terminal PTY: {error}");
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

enum PtyEvent {
    Frame(TerminalFrame),
    Closed,
    Error(String),
    /// Bounded result descriptors for one find generation. `revealed` is set
    /// only when this reply also moved the viewport (settled query or
    /// navigation), so the view can trust it as the selected match.
    SearchResults {
        generation: u64,
        hits: Vec<TerminalSearchHit>,
        total: usize,
        truncated: bool,
        scrollback_len: usize,
        alt_screen: bool,
        revealed: Option<usize>,
    },
}

enum TerminalWake {
    Events(Vec<PtyEvent>),
    Blink,
    Disconnected,
}

async fn next_terminal_wake(
    event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PtyEvent>,
    blink: impl Future<Output = ()>,
) -> TerminalWake {
    tokio::select! {
        biased;
        event = event_rx.recv() => {
            let Some(event) = event else {
                return TerminalWake::Disconnected;
            };
            let mut events = Vec::new();
            let mut pending_frame = None;
            let mut next = Some(event);
            while let Some(event) = next {
                match event {
                    PtyEvent::Frame(frame) => pending_frame = Some(frame),
                    event => {
                        if let Some(frame) = pending_frame.take() {
                            events.push(PtyEvent::Frame(frame));
                        }
                        events.push(event);
                    }
                }
                next = event_rx.try_recv().ok();
            }
            if let Some(frame) = pending_frame {
                events.push(PtyEvent::Frame(frame));
            }
            TerminalWake::Events(events)
        }
        _ = blink => TerminalWake::Blink,
    }
}

enum ParserCommand {
    LinkEpoch(u64),
    Clear,
    Resize(u16, u16),
    SetScrollback(usize),
    /// `Some(query)` opens/updates the search for `generation`; `None` closes
    /// it so the worker stops scanning entirely.
    Find {
        generation: u64,
        query: Option<String>,
    },
    /// Worker-validated navigation: the worker rescans the buffer and
    /// resolves the hit by identity (`row` + `excerpt`) in the fresh result
    /// set — never trusting the index the view navigated from — then scrolls
    /// the viewport to it. When the identity no longer exists the viewport
    /// stays put and `revealed` comes back `None`, so a stale location can
    /// never reveal unrelated output.
    RevealMatch {
        generation: u64,
        row: usize,
        excerpt: String,
    },
}

/// Worker-side find state: the query the retained buffer is searched with.
struct WorkerFind {
    generation: u64,
    query: String,
}

/// Scans the retained buffer unless the terminal is on the alternate screen
/// (full-screen TUIs) or the query is empty. Returns the outcome plus the
/// alternate-screen flag so replies can carry it.
fn worker_find_scan(
    parser: &mut vt100::Parser,
    rows: u16,
    cols: u16,
    query: &str,
) -> (TerminalSearchOutcome, bool) {
    let alt_screen = parser.screen().alternate_screen();
    let outcome = if alt_screen || query.is_empty() {
        TerminalSearchOutcome::default()
    } else {
        scan_retained_output(parser.screen_mut(), rows, cols, query)
    };
    (outcome, alt_screen)
}

fn emit_find_results(
    parser: &mut vt100::Parser,
    rows: u16,
    cols: u16,
    find: &WorkerFind,
    revealed: Option<usize>,
    event_tx: &tokio::sync::mpsc::UnboundedSender<PtyEvent>,
) -> Result<(), tokio::sync::mpsc::error::SendError<PtyEvent>> {
    let (outcome, alt_screen) = worker_find_scan(parser, rows, cols, &find.query);
    event_tx.send(PtyEvent::SearchResults {
        generation: find.generation,
        hits: outcome.hits,
        total: outcome.total,
        truncated: outcome.truncated,
        scrollback_len: outcome.scrollback_len,
        alt_screen,
        revealed,
    })
}

struct TerminalFrame {
    link_epoch: u64,
    links: Vec<TerminalLink>,
    screen: vt100::Screen,
    scrollback: usize,
    /// Total retained scrollback rows; `scrollback` (the view offset) can
    /// range over all of it, so the view needs both numbers.
    scrollback_len: usize,
    /// Alternate-screen state travels with the frame because
    /// `state_formatted` does not reproduce it on the reconstructed screen.
    alt_screen: bool,
}

fn visible_terminal_frame(parser: &mut vt100::Parser, rows: u16, cols: u16) -> TerminalFrame {
    // vt100 clamps deep offsets to the scrollback length, so a MAX probe is
    // the cheap way to read that length without exposing it directly.
    let saved_offset = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX);
    let scrollback_len = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(saved_offset);
    // Look back exactly one row to establish whether the viewport starts mid-token.
    parser
        .screen_mut()
        .set_scrollback(saved_offset.saturating_add(1));
    let first_continues = if parser.screen().scrollback() > saved_offset {
        parser.screen().row_wrapped(0)
    } else {
        // At the retained-history boundary a clipped first token is ambiguous.
        scrollback_len > 0
    };
    parser.screen_mut().set_scrollback(saved_offset);
    let links = visible_links(parser.screen(), first_continues);
    let mut visible = vt100::Parser::new(rows, cols, 0);
    visible.process(&parser.screen().state_formatted());
    TerminalFrame {
        link_epoch: 0,
        links,
        screen: visible.screen().clone(),
        scrollback: parser.screen().scrollback(),
        scrollback_len,
        alt_screen: parser.screen().alternate_screen(),
    }
}

impl TerminalFrame {
    fn with_link_epoch(mut self, epoch: u64) -> Self {
        self.link_epoch = epoch;
        self
    }
}

fn start_parser_worker(
    rows: u16,
    cols: u16,
    event_tx: tokio::sync::mpsc::UnboundedSender<PtyEvent>,
) -> std::io::Result<(mpsc::SyncSender<Vec<u8>>, mpsc::Sender<ParserCommand>)> {
    let (output_tx, output_rx) = mpsc::sync_channel::<Vec<u8>>(TERMINAL_OUTPUT_BUFFERED_CHUNKS);
    let (command_tx, command_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("threadlane-gpui-terminal-parser".into())
        .spawn(move || {
            let mut rows = rows;
            let mut cols = cols;
            let mut parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
            let mut dirty = false;
            let mut parsed_bytes = 0;
            let (mut frame_interval, mut parse_budget) = terminal_frame_policy(false);
            let mut saturated = false;
            let mut next_frame = Instant::now() + frame_interval;
            // Find state lives here: the worker owns the retained buffer, so
            // scans and navigation always run against the real history.
            let mut find: Option<WorkerFind> = None;
            // Deferred rescan while output keeps arriving; scanning on every
            // chunk would stall PTY parsing during output floods.
            let mut find_rescan_at: Option<Instant> = None;
            // After the PTY reader ends the buffer is still searchable, so
            // the worker keeps servicing commands until the view drops the
            // channel instead of exiting with the shell.
            let mut output_disconnected = false;
            let mut link_epoch = 0;

            loop {
                let mut commands_open = true;
                'drain: while commands_open {
                    let command = match command_rx.try_recv() {
                        Ok(command) => command,
                        Err(mpsc::TryRecvError::Empty) => break 'drain,
                        Err(mpsc::TryRecvError::Disconnected) => {
                            commands_open = false;
                            break 'drain;
                        }
                    };
                    match command {
                        ParserCommand::LinkEpoch(epoch) => link_epoch = epoch,
                        ParserCommand::Clear => {
                            parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
                            if find.is_some() {
                                find_rescan_at = Some(Instant::now());
                            }
                        }
                        ParserCommand::Resize(new_rows, new_cols) => {
                            rows = new_rows.max(1);
                            cols = new_cols.max(1);
                            parser.screen_mut().set_size(rows, cols);
                            let offset = parser.screen().scrollback();
                            parser.screen_mut().set_scrollback(offset);
                            if find.is_some() {
                                find_rescan_at = Some(Instant::now());
                            }
                        }
                        ParserCommand::SetScrollback(offset) => {
                            parser.screen_mut().set_scrollback(offset);
                        }
                        ParserCommand::Find { generation, query } => match query {
                            Some(query) => {
                                find = Some(WorkerFind { generation, query });
                                find_rescan_at = None;
                                let find_state = find.as_ref().unwrap();
                                // A settled query reveals its newest match.
                                let (outcome, _) =
                                    worker_find_scan(&mut parser, rows, cols, &find_state.query);
                                let revealed = outcome.hits.len().checked_sub(1);
                                if let Some(index) = revealed {
                                    parser.screen_mut().set_scrollback(reveal_offset(
                                        &outcome.hits[index],
                                        outcome.scrollback_len,
                                    ));
                                }
                                if event_tx
                                    .send(PtyEvent::SearchResults {
                                        generation,
                                        hits: outcome.hits,
                                        total: outcome.total,
                                        truncated: outcome.truncated,
                                        scrollback_len: outcome.scrollback_len,
                                        alt_screen: parser.screen().alternate_screen(),
                                        revealed,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            None => {
                                find = None;
                                find_rescan_at = None;
                            }
                        },
                        ParserCommand::RevealMatch {
                            generation,
                            row,
                            excerpt,
                        } => {
                            if find
                                .as_ref()
                                .is_some_and(|state| state.generation == generation)
                            {
                                let find_state = find.as_ref().unwrap();
                                let (outcome, alt_screen) =
                                    worker_find_scan(&mut parser, rows, cols, &find_state.query);
                                // Resolve the target by identity: a rescan
                                // can renumber matches, and only the same
                                // row + text is the same line.
                                let revealed = outcome.hits.iter().position(|hit| {
                                    hit.absolute_row == row && hit.excerpt == excerpt
                                });
                                if let Some(index) = revealed {
                                    parser.screen_mut().set_scrollback(reveal_offset(
                                        &outcome.hits[index],
                                        outcome.scrollback_len,
                                    ));
                                }
                                if event_tx
                                    .send(PtyEvent::SearchResults {
                                        generation,
                                        hits: outcome.hits,
                                        total: outcome.total,
                                        truncated: outcome.truncated,
                                        scrollback_len: outcome.scrollback_len,
                                        alt_screen,
                                        revealed,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    dirty = true;
                }
                if !commands_open && output_disconnected {
                    break;
                }

                let now = Instant::now();
                if now >= next_frame {
                    if dirty {
                        if event_tx
                            .send(PtyEvent::Frame(
                                visible_terminal_frame(&mut parser, rows, cols)
                                    .with_link_epoch(link_epoch),
                            ))
                            .is_err()
                        {
                            break;
                        }
                        dirty = false;
                    }
                    // Coalesced rescan: new output refreshes results without
                    // ever moving the reading position on its own.
                    if let (Some(due), Some(find_state)) = (find_rescan_at, find.as_mut()) {
                        if now >= due {
                            find_rescan_at = None;
                            if emit_find_results(
                                &mut parser,
                                rows,
                                cols,
                                find_state,
                                None,
                                &event_tx,
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                    }
                    (frame_interval, parse_budget) = terminal_frame_policy(saturated);
                    parsed_bytes = 0;
                    saturated = false;
                    next_frame = now + frame_interval;
                }

                if terminal_parse_budget_exhausted(parsed_bytes, parse_budget) {
                    std::thread::sleep(next_frame.saturating_duration_since(Instant::now()));
                    continue;
                }

                if output_disconnected {
                    // Sleep to the next frame instead of recv_timeout on the
                    // command channel: a recv here would consume a command
                    // without running its match arm, and after shell exit the
                    // buffer still answers Find/Resize commands.
                    std::thread::sleep(next_frame.saturating_duration_since(Instant::now()));
                    continue;
                }

                match output_rx.recv_timeout(next_frame.saturating_duration_since(Instant::now())) {
                    Ok(bytes) => {
                        parsed_bytes = parsed_bytes.saturating_add(bytes.len());
                        saturated = terminal_parse_budget_exhausted(parsed_bytes, parse_budget);
                        parser.process(&bytes);
                        // vt100 bumps the view offset as rows scroll into
                        // history, so the visible content stays put.
                        if find.is_some() && find_rescan_at.is_none() {
                            find_rescan_at =
                                Some(Instant::now() + TERMINAL_FIND_RESCAN_INTERVAL);
                        }
                        dirty = true;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        if dirty {
                            if event_tx
                                .send(PtyEvent::Frame(
                                    visible_terminal_frame(&mut parser, rows, cols)
                                        .with_link_epoch(link_epoch),
                                ))
                                .is_err()
                            {
                                break;
                            }
                            dirty = false;
                        }
                        output_disconnected = true;
                    }
                }
            }
        })?;
    Ok((output_tx, command_tx))
}

fn selection_bounds(
    anchor: (u16, u16),
    head: (u16, u16),
    cols: u16,
) -> Option<((u16, u16), (u16, u16))> {
    if anchor == head {
        return None;
    }
    if head < anchor {
        Some((
            head,
            (
                anchor.0,
                anchor.1.saturating_add(1).min(cols.saturating_sub(1)),
            ),
        ))
    } else {
        Some((anchor, head))
    }
}

fn selection_present(
    anchor: Option<(u16, u16)>,
    head: Option<(u16, u16)>,
    cols: u16,
) -> bool {
    match (anchor, head) {
        (Some(anchor), Some(head)) => selection_bounds(anchor, head, cols).is_some(),
        _ => false,
    }
}

fn selected_excerpt(
    screen: &vt100::Screen,
    anchor: Option<(u16, u16)>,
    head: Option<(u16, u16)>,
    cols: u16,
) -> Option<String> {
    let (anchor, head) = (anchor?, head?);
    let (start, end) = selection_bounds(anchor, head, cols)?;
    Some(screen.contents_between(start.0, start.1, end.0, end.1))
}

fn should_paint_cursor(is_focused: bool, terminal_hides_cursor: bool, blink_visible: bool) -> bool {
    is_focused && !terminal_hides_cursor && blink_visible
}

/// Live state of the inline Find strip: the query input plus the worker's
/// latest bounded results for the current generation. Match locations are
/// worker-owned — the view keeps descriptors (absolute row + excerpt), never
/// terminal text it could navigate to stalely.
struct TerminalFind {
    input: Entity<InputState>,
    previous_focus: Option<FocusHandle>,
    query: String,
    generation: u64,
    pending: bool,
    failed: bool,
    hits: Vec<TerminalSearchHit>,
    total: usize,
    truncated: bool,
    /// Scrollback length the current `hits` were computed against.
    scrollback_len: usize,
    selected: Option<usize>,
    /// Identity of the selected hit (absolute row + excerpt) so a refreshed
    /// result set keeps the selection only while the same line is present.
    selected_hit: Option<TerminalSearchHit>,
    _subscription: Subscription,
}

/// A point-in-time copy of the user's terminal selection, detached from
/// the live screen so later output, resize, or a shell restart can never
/// mutate it.
pub struct TerminalSelection {
    /// Selected text as displayed, whitespace and Unicode preserved.
    pub text: String,
    /// The directory the shell was launched in — honest provenance for
    /// the excerpt, not proof of the shell's current working directory
    /// after a `cd`, and not a session worktree claim.
    pub launched_in: PathBuf,
}

/// Block-relevant selection state without the excerpt payload: whether
/// the covered cells hold any non-whitespace text, and the UTF-8 byte
/// length [`selection_snapshot`](TerminalView::selection_snapshot)
/// would produce. Cheap to poll per frame — it walks cells rather than
/// copying text out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionStatus {
    /// At least one covered cell holds non-whitespace content.
    pub has_text: bool,
    /// UTF-8 bytes of the snapshot excerpt: cell contents plus one line
    /// break per covered row that is not a soft wrap.
    pub excerpt_len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkDestination {
    Threadlane,
    DefaultBrowser,
}

#[derive(Clone, Debug)]
pub struct OpenTerminalLink {
    pub url: String,
    pub destination: LinkDestination,
}

impl EventEmitter<OpenTerminalLink> for TerminalView {}

/// A persistent, focusable project shell backed by a real pseudo-terminal.
///
/// Construct it with `cx.new(|cx| TerminalView::new(project, cx))` and
/// render the resulting `Entity<TerminalView>` directly from its parent view.
pub struct TerminalView {
    links: Vec<TerminalLink>,
    link_press: Option<(TerminalLink, Point<Pixels>)>,
    link_menu: Option<Entity<PopupMenu>>,
    link_menu_focus: Option<FocusHandle>,
    link_menu_subscription: Option<Subscription>,
    link_epoch: u64,
    retry_url: Option<String>,
    // Worker echo rejects frames queued before clear/restart/geometry changes.
    frame_epoch: u64,
    dismissed_link_focus: Option<FocusHandle>,
    context_link: Option<(String, u64)>,
    project: PathBuf,
    focus_handle: FocusHandle,
    screen: vt100::Screen,
    parser_command_tx: Option<mpsc::Sender<ParserCommand>>,
    session: Option<PtySession>,
    event_tx: tokio::sync::mpsc::UnboundedSender<PtyEvent>,
    status: Option<String>,
    rows: u16,
    cols: u16,
    screen_bounds: Option<Bounds<Pixels>>,
    selection_anchor: Option<(u16, u16)>,
    selection_head: Option<(u16, u16)>,
    cell_width: f32,
    font_size: f32,
    compact: bool,
    translucent_background: bool,
    cursor_visible: bool,
    scrollback_offset: usize,
    scroll_accumulator: f32,
    scrollback_len: usize,
    alt_screen: bool,
    find: Option<TerminalFind>,
    /// Monotonic seed for find generations so a generation is never reused
    /// across find instances or shell restarts in this view: a delayed reply
    /// from an older find can then never land under a newer one.
    find_generation_seed: u64,
}

impl TerminalView {
    pub fn new(project: PathBuf, cx: &mut Context<Self>) -> Self {
        Self::new_with_start(project, cx, true)
    }

    /// Same as `new` minus the real PTY: tests construct selection and
    /// screen state by hand, and a live pty-reader thread would trip the
    /// test scheduler's deterministic-thread guard.
    #[cfg(test)]
    fn new_for_test(project: PathBuf, cx: &mut Context<Self>) -> Self {
        Self::new_with_start(project, cx, false)
    }

    fn new_with_start(project: PathBuf, cx: &mut Context<Self>, autostart: bool) -> Self {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        cx.spawn(async move |this, cx| loop {
            let wake = next_terminal_wake(
                &mut event_rx,
                cx.background_executor().timer(CURSOR_BLINK_INTERVAL),
            )
            .await;
            let (events, blink) = match wake {
                TerminalWake::Events(events) => (events, false),
                TerminalWake::Blink => (Vec::new(), true),
                TerminalWake::Disconnected => break,
            };
            if this
                .update(cx, |this, cx| {
                    for event in events {
                        this.apply_event(event);
                    }
                    if blink {
                        this.cursor_visible = !this.cursor_visible;
                    } else {
                        this.cursor_visible = true;
                    }
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        let mut terminal = Self {
            links: Vec::new(),
            link_press: None,
            link_menu: None,
            link_menu_focus: None,
            link_menu_subscription: None,
            link_epoch: 0,
            retry_url: None,
            frame_epoch: 0,
            dismissed_link_focus: None,
            context_link: None,
            project,
            focus_handle: cx.focus_handle(),
            screen: vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLS, 0)
                .screen()
                .clone(),
            parser_command_tx: None,
            session: None,
            event_tx,
            status: None,
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            screen_bounds: None,
            selection_anchor: None,
            selection_head: None,
            cell_width: TERMINAL_CELL_WIDTH_FALLBACK,
            font_size: TERMINAL_FONT_SIZE,
            compact: false,
            translucent_background: false,
            cursor_visible: true,
            scrollback_offset: 0,
            scroll_accumulator: 0.0,
            scrollback_len: 0,
            alt_screen: false,
            find: None,
            find_generation_seed: 0,
        };
        if autostart {
            terminal.start();
        }
        terminal
    }

    /// Sends raw input bytes into the terminal PTY.
    pub fn send_input(&self, input: &str) {
        if let Some(session) = &self.session {
            session.write(input.as_bytes());
        }
    }

    /// Switches the shell to another project, restarting it in the new cwd.
    pub fn set_project(&mut self, project: PathBuf, cx: &mut Context<Self>) {
        if self.project != project {
            self.project = project;
            self.restart(cx);
        }
    }

    /// Terminates the current shell and starts a fresh login-capable interactive shell.
    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.invalidate_links();
        self.session.take();
        self.parser_command_tx = None;
        self.screen = vt100::Parser::new(self.rows, self.cols, 0).screen().clone();
        self.scrollback_offset = 0;
        self.scroll_accumulator = 0.0;
        self.scrollback_len = 0;
        self.alt_screen = false;
        self.find = None;
        self.status = None;
        self.clear_selection();
        self.start();
        cx.notify();
    }

    /// Clears both the emulator scrollback and the visible screen.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.invalidate_links();
        self.screen = vt100::Parser::new(self.rows, self.cols, 0).screen().clone();
        if let Some(parser) = &self.parser_command_tx {
            let _ = parser.send(ParserCommand::Clear);
            self.sync_link_frame();
        }
        self.scrollback_offset = 0;
        self.scroll_accumulator = 0.0;
        self.status = None;
        self.clear_selection();
        cx.notify();
    }

    /// Scrolls the terminal view by a number of lines (positive = into scrollback history, negative = towards bottom).
    fn scroll_by(&mut self, lines: f32, cx: &mut Context<Self>) {
        self.link_press = None;
        self.scroll_accumulator += lines;
        let whole_lines = self.scroll_accumulator.trunc() as isize;
        if whole_lines != 0 {
            let previous_offset = self.scrollback_offset;
            self.scroll_accumulator -= whole_lines as f32;
            let current = self.scrollback_offset as isize;
            let new_offset = (current + whole_lines).max(0) as usize;
            self.set_scrollback(new_offset);
            if self.scrollback_offset != previous_offset {
                self.clear_selection();
            }
            cx.notify();
        }
    }

    /// Resets scrollback to the bottom (live / auto-scroll mode).
    fn scroll_to_bottom(&mut self, cx: &mut Context<Self>) {
        if self.scrollback_offset != 0 {
            self.clear_selection();
        }
        self.scrollback_offset = 0;
        self.scroll_accumulator = 0.0;
        self.set_scrollback(0);
        cx.notify();
    }

    /// Scrolls all the way to the top of available scrollback history.
    fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        let previous_offset = self.scrollback_offset;
        self.scroll_accumulator = 0.0;
        self.set_scrollback(self.scrollback_len);
        if self.scrollback_offset != previous_offset {
            self.clear_selection();
        }
        cx.notify();
    }

    /// Updates the PTY and terminal parser dimensions. Parents can call this when
    /// they have measured cell dimensions for their allocated terminal bounds.
    fn resize(&mut self, rows: u16, cols: u16, cx: &mut Context<Self>) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (rows, cols) == (self.rows, self.cols) {
            return;
        }
        self.invalidate_links();
        self.rows = rows;
        self.cols = cols;
        self.clear_selection();
        if let Some(parser) = &self.parser_command_tx {
            let _ = parser.send(ParserCommand::Resize(rows, cols));
            self.sync_link_frame();
        }
        if let Some(session) = &self.session {
            session.resize(rows, cols);
        }
        cx.notify();
    }

    pub fn project(&self) -> &PathBuf {
        &self.project
    }

    /// Whether the visible screen holds any output. Used to confirm closing
    /// a shell that would discard scrollback/build output with one misclick.
    pub fn has_output(&self) -> bool {
        !self.screen.contents().trim().is_empty()
    }

    fn start(&mut self) {
        let result = start_parser_worker(self.rows, self.cols, self.event_tx.clone())
            .map_err(|e| e.to_string())
            .and_then(|(output_tx, command_tx)| {
                spawn_shell(
                    &self.project,
                    self.rows,
                    self.cols,
                    output_tx,
                    self.event_tx.clone(),
                )
                .map_err(|e| e.to_string())
                .map(|session| (session, command_tx))
            });
        match result {
            Ok((session, command_tx)) => {
                self.session = Some(session);
                let _ = command_tx.send(ParserCommand::LinkEpoch(self.frame_epoch));
                self.parser_command_tx = Some(command_tx);
            }
            Err(error) => {
                self.session = None;
                self.parser_command_tx = None;
                self.status = Some(format!("Unable to start terminal: {error}"));
            }
        }
    }

    fn apply_event(&mut self, event: PtyEvent) {
        match event {
            PtyEvent::Frame(frame) => {
                if frame.link_epoch != self.frame_epoch {
                    return;
                }
                self.link_press = None;
                if self.alt_screen != frame.alt_screen || self.scrollback_offset != frame.scrollback
                {
                    self.invalidate_links();
                    self.sync_link_frame();
                }
                self.links = frame.links;
                self.screen = frame.screen;
                self.scrollback_offset = frame.scrollback;
                self.scrollback_len = frame.scrollback_len;
                self.alt_screen = frame.alt_screen;
            }
            PtyEvent::SearchResults {
                generation,
                hits,
                total,
                truncated,
                scrollback_len,
                alt_screen,
                revealed,
            } => {
                if self
                    .find
                    .as_ref()
                    .is_some_and(|find| find.generation == generation)
                    && self.alt_screen != alt_screen
                {
                    self.invalidate_links();
                    self.sync_link_frame();
                }
                // Replies for an older query generation never land; the
                // generation moves on every query edit and on close.
                let Some(find) = &mut self.find else {
                    return;
                };
                if find.generation != generation {
                    return;
                }
                self.alt_screen = alt_screen;
                find.pending = false;
                find.failed = false;
                find.hits = hits;
                find.total = total;
                find.truncated = truncated;
                find.scrollback_len = scrollback_len;
                if let Some(index) = revealed {
                    find.selected = Some(index);
                    find.selected_hit = find.hits.get(index).cloned();
                } else if let Some(identity) = &find.selected_hit {
                    // Keep the selection glued to the same line; a hit whose
                    // row moved out of view is simply deselected, never
                    // repointed at different output.
                    find.selected = find.hits.iter().position(|hit| {
                        hit.absolute_row == identity.absolute_row
                            && hit.excerpt == identity.excerpt
                    });
                    if find.selected.is_none() {
                        find.selected_hit = None;
                    }
                }
            }
            PtyEvent::Closed => {
                if self.session.is_some() {
                    self.status = Some("Shell exited. Select Restart to open a new shell.".into());
                }
            }
            PtyEvent::Error(error) => self.status = Some(format!("Terminal read failed: {error}")),
        }
    }

    // The worker holds the real emulator and clamps deep offsets to the
    // retained length itself; the view clamps against its last-known length
    // so wheel scrolling can reach any retained row.
    fn set_scrollback(&mut self, offset: usize) {
        self.invalidate_links();
        self.scrollback_offset = offset.min(self.scrollback_len);
        if let Some(parser) = &self.parser_command_tx {
            let _ = parser.send(ParserCommand::SetScrollback(self.scrollback_offset));
            self.sync_link_frame();
        }
    }

    fn send(&self, bytes: &[u8]) {
        if let Some(session) = &self.session {
            session.write(bytes);
        }
    }

    fn paste(&self, text: String) {
        if self.screen.bracketed_paste() {
            self.send(b"\x1b[200~");
            self.send(text.as_bytes());
            self.send(b"\x1b[201~");
        } else {
            self.send(text.as_bytes());
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.is_focused(window) {
            return;
        }
        if let Some(find) = &self.find {
            if find.input.read(cx).focus_handle(cx).is_focused(window) {
                // Keystrokes inside the find input belong to the input: they
                // must not reach the shell as raw PTY bytes.
                return;
            }
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        if modifiers.shift {
            match key {
                "pageup" => {
                    self.scroll_by((self.rows / 2).max(1) as f32, cx);
                    cx.stop_propagation();
                    return;
                }
                "pagedown" => {
                    self.scroll_by(-((self.rows / 2).max(1) as f32), cx);
                    cx.stop_propagation();
                    return;
                }
                "home" => {
                    self.scroll_to_top(cx);
                    cx.stop_propagation();
                    return;
                }
                "end" => {
                    self.scroll_to_bottom(cx);
                    cx.stop_propagation();
                    return;
                }
                "up" => {
                    self.scroll_by(1.0, cx);
                    cx.stop_propagation();
                    return;
                }
                "down" => {
                    self.scroll_by(-1.0, cx);
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        if modifiers.platform && key.eq_ignore_ascii_case("v") {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.paste(text);
            }
            cx.stop_propagation();
            return;
        }

        // Platform copy shortcut: copy the selection when one exists, otherwise
        // fall through so the keystroke still sends ^C to the shell.
        if modifiers.platform && key.eq_ignore_ascii_case("c") {
            if let Some(text) = self.selected_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                cx.stop_propagation();
                return;
            }
            self.send(&[0x03]);
            cx.stop_propagation();
            return;
        }

        let bytes: Option<Vec<u8>> = if modifiers.control {
            match key.to_ascii_lowercase().as_str() {
                "v" if !modifiers.platform => cx
                    .read_from_clipboard()
                    .and_then(|item| item.text())
                    .map(|text| {
                        if self.screen.bracketed_paste() {
                            [
                                b"\x1b[200~".as_slice(),
                                text.as_bytes(),
                                b"\x1b[201~".as_slice(),
                            ]
                            .concat()
                        } else {
                            text.into_bytes()
                        }
                    }),
                letter if letter.len() == 1 && letter.as_bytes()[0].is_ascii_lowercase() => {
                    // Convert Ctrl+<letter> to the standard terminal control character.
                    Some(vec![letter.as_bytes()[0] - b'a' + 1])
                }
                _ => None,
            }
        } else {
            match key {
                "enter" => Some(b"\r".to_vec()),
                "backspace" => Some(vec![0x7f]),
                "tab" => Some(b"\t".to_vec()),
                "up" => Some(self.cursor_key(b'A', modifiers)),
                "down" => Some(self.cursor_key(b'B', modifiers)),
                "right" => Some(self.cursor_key(b'C', modifiers)),
                "left" => Some(self.cursor_key(b'D', modifiers)),
                "home" => Some(b"\x1b[H".to_vec()),
                "end" => Some(b"\x1b[F".to_vec()),
                "delete" => Some(b"\x1b[3~".to_vec()),
                "pageup" => Some(b"\x1b[5~".to_vec()),
                "pagedown" => Some(b"\x1b[6~".to_vec()),
                "f1" => Some(b"\x1bOP".to_vec()),
                "f2" => Some(b"\x1bOQ".to_vec()),
                "f3" => Some(b"\x1bOR".to_vec()),
                "f4" => Some(b"\x1bOS".to_vec()),
                "f5" => Some(b"\x1b[15~".to_vec()),
                "f6" => Some(b"\x1b[17~".to_vec()),
                "f7" => Some(b"\x1b[18~".to_vec()),
                "f8" => Some(b"\x1b[19~".to_vec()),
                "f9" => Some(b"\x1b[20~".to_vec()),
                "f10" => Some(b"\x1b[21~".to_vec()),
                "f11" => Some(b"\x1b[23~".to_vec()),
                "f12" => Some(b"\x1b[24~".to_vec()),
                "escape" => Some(vec![0x1b]),
                _ if !modifiers.platform => event.keystroke.key_char.as_ref().map(|text| {
                    let mut bytes = Vec::with_capacity(text.len() + usize::from(modifiers.alt));
                    if modifiers.alt {
                        bytes.push(0x1b);
                    }
                    bytes.extend_from_slice(text.as_bytes());
                    bytes
                }),
                _ => None,
            }
        };

        if let Some(bytes) = bytes {
            self.send(&bytes);
            cx.stop_propagation();
        }
    }

    fn cursor_key(&self, key: u8, modifiers: Modifiers) -> Vec<u8> {
        let modifier = 1
            + u8::from(modifiers.shift)
            + 2 * u8::from(modifiers.alt)
            + 4 * u8::from(modifiers.control);
        if modifier == 1 {
            if self.screen.application_cursor() {
                vec![0x1b, b'O', key]
            } else {
                vec![0x1b, b'[', key]
            }
        } else {
            format!("\x1b[1;{modifier}{}", key as char).into_bytes()
        }
    }

    /// Dismiss transient destinations when the owner changes the displayed terminal.
    pub fn dismiss_links(&mut self, cx: &mut Context<Self>) {
        self.context_link = None;
        self.retry_url = None;
        self.link_press = None;
        self.dismissed_link_focus = self.link_menu_focus.take();
        self.link_menu = None;
        self.link_menu_subscription = None;
        self.link_epoch = self.link_epoch.wrapping_add(1);
        cx.notify();
    }

    fn invalidate_links(&mut self) {
        self.frame_epoch = self.frame_epoch.wrapping_add(1);
        self.context_link = None;
        self.retry_url = None;
        self.link_press = None;
        self.links.clear();
        self.dismissed_link_focus = self.link_menu_focus.take();
        self.link_menu = None;
        self.link_menu_subscription = None;
        self.link_epoch = self.link_epoch.wrapping_add(1);
    }

    fn sync_link_frame(&self) {
        if let Some(parser) = &self.parser_command_tx {
            let _ = parser.send(ParserCommand::LinkEpoch(self.frame_epoch));
        }
    }

    fn link_at(&self, position: Point<Pixels>) -> Option<&TerminalLink> {
        let bounds = self.screen_bounds?;
        let x = (position.x - bounds.left()).as_f32() - TERMINAL_CONTENT_INSET;
        let y = (position.y - bounds.top()).as_f32() - TERMINAL_CONTENT_INSET;
        // Unlike selection drags, links must not clamp padding to a nearby cell.
        if !bounds.contains(&position) || x < 0.0 || y < 0.0
            || x >= f32::from(self.cols) * self.cell_width
            || y >= f32::from(self.rows) * self.row_height()
        {
            return None;
        }
        let cell = self.cell_at(position)?;
        self.links.iter().find(|link| link.cells.contains(&cell))
    }

    fn activate_link(
        &mut self,
        url: String,
        destination: LinkDestination,
        epoch: u64,
        cx: &mut Context<Self>,
    ) {
        if epoch == self.link_epoch && !self.alt_screen && is_web_url(&url) {
            cx.emit(OpenTerminalLink { url, destination });
        }
    }

    fn link_commands(
        menu: PopupMenu,
        url: String,
        terminal: WeakEntity<Self>,
        epoch: u64,
    ) -> PopupMenu {
        let mut menu = menu;
        if cfg!(target_os = "macos") {
            let url = url.clone();
            let terminal = terminal.clone();
            menu = menu.item(
                PopupMenuItem::new(format!("Open in Threadlane browser — {url}")).on_click(
                    move |_, _, cx| {
                        let _ = terminal.update(cx, |terminal, cx| {
                            terminal.activate_link(
                                url.clone(),
                                LinkDestination::Threadlane,
                                epoch,
                                cx,
                            );
                        });
                    },
                ),
            );
        }
        menu.item(
            PopupMenuItem::new(format!("Open in default browser — {url}")).on_click(
                move |_, _, cx| {
                    let _ = terminal.update(cx, |terminal, cx| {
                        terminal.activate_link(
                            url.clone(),
                            LinkDestination::DefaultBrowser,
                            epoch,
                            cx,
                        );
                    });
                },
            ),
        )
    }

    pub fn retry_link(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        self.retry_url = Some(url);
        self.open_links(window, cx);
    }

    /// A standard menu owns keyboard navigation and a frozen, deduplicated viewport snapshot.
    pub fn open_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        let terminal = cx.weak_entity();
        let epoch = self.link_epoch;
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            let mut menu = menu
                .label("Links in visible output")
                .action_context(self.focus_handle.clone())
                .scrollable(true);
            if self.alt_screen {
                return menu.label("Links unavailable in full-screen terminal applications");
            }
            if !cfg!(target_os = "macos") {
                menu = menu.label("Threadlane browser is available on macOS only");
            }
            if let Some(url) = &self.retry_url {
                menu = menu.label("Navigation could not start — retry or open externally");
                menu = Self::link_commands(menu, url.clone(), terminal.clone(), epoch).separator();
            }
            if self.links.is_empty() && self.retry_url.is_none() {
                return menu
                    .label("No web links in visible output")
                    .label("Scroll older output into view to find links");
            }
            let mut seen = std::collections::HashSet::new();
            for link in &self.links {
                if seen.insert(&link.url) {
                    menu = Self::link_commands(menu, link.url.clone(), terminal.clone(), epoch);
                }
            }
            menu
        });
        self.link_menu_subscription = Some(cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.link_menu_focus = None;
            this.link_menu = None;
            cx.notify();
        }));
        self.link_menu_focus = Some(menu.read(cx).focus_handle(cx));
        menu.read(cx).focus_handle(cx).focus(window, cx);
        self.link_menu = Some(menu);
        cx.notify();
    }

    fn screen_text(&self) -> String {
        let mut text = self.screen.contents();
        if let Some(status) = &self.status {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(status);
        }
        text
    }

    fn line_height(&self) -> f32 {
        if self.compact {
            TERMINAL_COMPACT_LINE_HEIGHT
        } else {
            TERMINAL_LINE_HEIGHT
        }
    }

    fn row_height(&self) -> f32 {
        self.font_size * self.line_height()
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let bounds = self.screen_bounds?;
        let x = ((position.x - bounds.left()).as_f32() - TERMINAL_CONTENT_INSET) / self.cell_width;
        let y = ((position.y - bounds.top()).as_f32() - TERMINAL_CONTENT_INSET) / self.row_height();
        Some((
            y.floor()
                .max(0.0)
                .min(f32::from(self.rows.saturating_sub(1))) as u16,
            x.floor()
                .max(0.0)
                .min(f32::from(self.cols.saturating_sub(1))) as u16,
        ))
    }

    fn selected_text(&self) -> Option<String> {
        selected_excerpt(
            &self.screen,
            self.selection_anchor,
            self.selection_head,
            self.cols,
        )
    }

    /// Whether a real selection currently covers at least one cell. A
    /// cheap presence check for affordances tracking selection state —
    /// read the text with [`selection_snapshot`](Self::selection_snapshot).
    pub fn has_selection(&self) -> bool {
        selection_present(self.selection_anchor, self.selection_head, self.cols)
    }

    /// Read-only copy of the current selection for cross-surface
    /// handoffs. `None` while no real selection exists (no anchors, or a
    /// zero-width drag). The text is copied out of the live screen at
    /// call time, so coordinates are never reinterpreted against older
    /// frames and later output, resize, or a restart cannot change the
    /// snapshot.
    pub fn selection_snapshot(&self) -> Option<TerminalSelection> {
        let text = self.selected_text()?;
        Some(TerminalSelection {
            text,
            launched_in: self.project.clone(),
        })
    }

    /// Availability facts about the current selection, derived by
    /// walking the covered cells instead of copying the excerpt —
    /// affordances that refresh on selection changes can evaluate it on
    /// every notify without paying for a string copy. Mirrors the
    /// accounting `contents_between` performs.
    pub fn selection_status(&self) -> Option<SelectionStatus> {
        let (anchor, head) = (self.selection_anchor?, self.selection_head?);
        let (start, end) = selection_bounds(anchor, head, self.cols)?;
        let mut has_text = false;
        let mut excerpt_len = 0usize;
        for row in start.0..=end.0 {
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 { end.1 } else { self.cols };
            for col in from..to {
                if let Some(cell) = self.screen.cell(row, col) {
                    let contents = cell.contents();
                    excerpt_len += contents.len();
                    has_text |= !contents.trim().is_empty();
                }
            }
            if row != end.0 && !self.screen.row_wrapped(row) {
                excerpt_len += 1;
            }
        }
        Some(SelectionStatus {
            has_text,
            excerpt_len,
        })
    }

    fn clear_selection(&mut self) {
        self.link_press = None;
        self.selection_anchor = None;
        self.selection_head = None;
    }

    fn begin_selection(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.link_press = None;
        let modifier = if cfg!(target_os = "macos") {
            event.modifiers.platform
        } else {
            event.modifiers.control
        };
        if modifier && event.click_count == 1 {
            self.link_press = self
                .link_at(event.position)
                .cloned()
                .map(|link| (link, event.position));
        }
        self.selection_anchor = self.cell_at(event.position);
        self.selection_head = self.selection_anchor;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn extend_selection(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.link_press.as_ref().is_some_and(|(_, start)| {
            (event.position.x - start.x).abs() > px(3.)
                || (event.position.y - start.y).abs() > px(3.)
        }) {
            self.link_press = None;
        }
        if event.dragging() && self.selection_anchor.is_some() {
            self.selection_head = self.cell_at(event.position);
            cx.notify();
        }
    }

    fn end_selection(
        &mut self,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((link, start)) = self.link_press.take() {
            let modifier = if cfg!(target_os = "macos") {
                event.modifiers.platform
            } else {
                event.modifiers.control
            };
            if modifier
                && (event.position.x - start.x).abs() <= px(3.)
                && (event.position.y - start.y).abs() <= px(3.)
                && self.link_at(event.position) == Some(&link)
            {
                let destination = if cfg!(target_os = "macos") {
                    LinkDestination::Threadlane
                } else {
                    LinkDestination::DefaultBrowser
                };
                self.activate_link(link.url, destination, self.link_epoch, cx);
                self.clear_selection();
                return;
            }
        }
        self.selection_head = self.cell_at(event.position).or(self.selection_head);
        cx.notify();
    }
    fn is_cell_selected(&self, row: u16, col: u16) -> bool {
        let (Some(anchor), Some(head)) = (self.selection_anchor, self.selection_head) else {
            return false;
        };
        let Some((start, end)) = selection_bounds(anchor, head, self.cols) else {
            return false;
        };
        let pos = (row, col);
        pos >= start && pos <= end
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection_anchor = Some((0, 0));
        self.selection_head = Some((self.rows.saturating_sub(1), self.cols.saturating_sub(1)));
        cx.notify();
    }

    fn paste_from_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste(text);
        }
    }

    /// Opens the inline find strip (or re-selects the query when it is
    /// already open). Called by the FindInTerminalOutput action, the toolbar
    /// button, and the context-menu item — one command, one path.
    pub fn open_find(
        &mut self,
        _: &FindInTerminalOutput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            cx.propagate();
            return;
        }
        if self.find.is_none() {
            let generation = self.next_find_generation();
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder("Find in retained output…")
            });
            let subscription = cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        let query = input.read(cx).value().to_string();
                        this.update_find_query(query, cx);
                    }
                },
            );
            self.find = Some(TerminalFind {
                input,
                previous_focus: window.focused(cx),
                query: String::new(),
                generation,
                pending: false,
                failed: false,
                hits: Vec::new(),
                total: 0,
                truncated: false,
                scrollback_len: self.scrollback_len,
                selected: None,
                selected_hit: None,
                _subscription: subscription,
            });
            self.send_find_query();
        }
        let Some(find) = &self.find else {
            return;
        };
        find.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.stop_propagation();
        cx.notify();
    }

    fn close_find(
        &mut self,
        _: &CloseTerminalFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(find) = self.find.take() else {
            cx.propagate();
            return;
        };
        if window.has_active_dialog(cx) {
            self.find = Some(find);
            cx.propagate();
            return;
        }
        // Stop worker-side scanning: no background work while find is closed.
        if let Some(parser) = &self.parser_command_tx {
            let _ = parser.send(ParserCommand::Find {
                generation: find.generation,
                query: None,
            });
        }
        let focus = find
            .previous_focus
            .unwrap_or_else(|| self.focus_handle.clone());
        window.focus(&focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn next_find_generation(&mut self) -> u64 {
        self.find_generation_seed += 1;
        self.find_generation_seed
    }

    fn update_find_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self
            .find
            .as_ref()
            .is_none_or(|find| find.query == query)
        {
            return;
        }
        let generation = self.next_find_generation();
        let find = self.find.as_mut().unwrap();
        find.query = query;
        find.generation = generation;
        find.failed = false;
        find.hits.clear();
        find.total = 0;
        find.selected = None;
        find.selected_hit = None;
        find.pending = !find.query.is_empty();
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TERMINAL_FIND_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                let Some(find) = &mut this.find else {
                    return;
                };
                if find.generation != generation {
                    return;
                }
                this.send_find_query();
                cx.notify();
            });
        })
        .detach();
    }

    /// Sends the current query to the parser worker, or marks the strip
    /// failed when the worker is gone (dead session, closed channel).
    fn send_find_query(&mut self) {
        let Some(find) = &mut self.find else {
            return;
        };
        let sent = self.parser_command_tx.as_ref().is_some_and(|parser| {
            parser
                .send(ParserCommand::Find {
                    generation: find.generation,
                    query: Some(find.query.clone()),
                })
                .is_ok()
        });
        if !sent {
            find.failed = true;
            find.pending = false;
        }
    }

    fn retry_find(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        find.failed = false;
        find.pending = !find.query.is_empty();
        self.send_find_query();
        cx.notify();
    }

    fn navigate_find(&mut self, previous: bool, cx: &mut Context<Self>) {
        let alt_screen = self.alt_screen;
        let Some(find) = &mut self.find else {
            return;
        };
        if find.pending || find.failed || find.hits.is_empty() || alt_screen {
            return;
        }
        let Some(index) = next_find_match(find.selected, find.hits.len(), previous) else {
            return;
        };
        find.selected = Some(index);
        find.selected_hit = find.hits.get(index).cloned();
        let generation = find.generation;
        // Navigation asks the worker to reveal by hit identity; the worker
        // resolves it in a fresh scan so output churn cannot reveal a
        // different line.
        if let (Some(hit), Some(parser)) = (&find.selected_hit, &self.parser_command_tx) {
            let _ = parser.send(ParserCommand::RevealMatch {
                generation,
                row: hit.absolute_row,
                excerpt: hit.excerpt.clone(),
            });
        }
        cx.notify();
    }

    fn next_terminal_match(
        &mut self,
        _: &NextTerminalMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_find(false, cx);
        cx.stop_propagation();
    }

    fn previous_terminal_match(
        &mut self,
        _: &PreviousTerminalMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_find(true, cx);
        cx.stop_propagation();
    }

    fn find_status_text(&self) -> String {
        let Some(find) = &self.find else {
            return String::new();
        };
        if self.alt_screen {
            "Find unavailable in full-screen terminal applications".to_owned()
        } else if find.failed {
            "Couldn't search terminal output".to_owned()
        } else if find.query.is_empty() {
            "Type to find output".to_owned()
        } else if find.pending {
            "Searching…".to_owned()
        } else if find.hits.is_empty() {
            "No matching lines in retained output".to_owned()
        } else if let Some(index) = find.selected {
            // `hits` holds the newest descriptors when truncated, so a
            // visible index maps to a global match position.
            let position = find.total - find.hits.len() + index + 1;
            format!("{position} of {} matching lines", find.total)
        } else {
            format!("{} matching lines · Choose Previous or Next", find.total)
        }
    }

    fn render_find_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(find) = &self.find else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let status = self.find_status_text();
        let nav_disabled =
            find.pending || find.failed || find.hits.is_empty() || self.alt_screen;
        div()
            .key_context("TerminalFind")
            .flex()
            .flex_col()
            .gap_1p5()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div().flex_1().min_w(rems(10.)).child(
                            Input::new(&find.input)
                                .small()
                                .aria_label("Find in terminal output"),
                        ),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Match case"),
                    )
                    .child(
                        Button::new("terminal-find-previous")
                            .debug_selector(|| "terminal-find-previous".into())
                            .label("Previous")
                            .small()
                            .ghost()
                            .accessibility_label("Previous matching line (Shift+Enter)")
                            .tooltip("Previous matching line (Shift+Enter)")
                            .disabled(nav_disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.navigate_find(true, cx)
                            })),
                    )
                    .child(
                        Button::new("terminal-find-next")
                            .debug_selector(|| "terminal-find-next".into())
                            .label("Next")
                            .small()
                            .ghost()
                            .accessibility_label("Next matching line (Enter)")
                            .tooltip("Next matching line (Enter)")
                            .disabled(nav_disabled)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.navigate_find(false, cx)
                            })),
                    )
                    .children(find.failed.then(|| {
                        Button::new("terminal-find-retry")
                            .label("Retry")
                            .small()
                            .ghost()
                            .accessibility_label("Retry terminal output search")
                            .tooltip("Retry terminal output search")
                            .on_click(cx.listener(|this, _, _, cx| this.retry_find(cx)))
                    }))
                    .children((self.scrollback_offset > 0).then(|| {
                        Button::new("terminal-find-jump-to-live")
                            .label("Jump to live output")
                            .small()
                            .ghost()
                            .accessibility_label("Jump to live output")
                            .tooltip("Jump to live output")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.scroll_to_bottom(cx)
                            }))
                    }))
                    .child(
                        Button::new("terminal-find-close")
                            .debug_selector(|| "terminal-find-close".into())
                            .label("Close")
                            .small()
                            .ghost()
                            .accessibility_label("Close find in terminal output (Escape)")
                            .tooltip("Close find (Escape)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_find(&CloseTerminalFind, window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("terminal-find-status")
                            .role(Role::Status)
                            .aria_label(status.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(status),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Retained output · current terminal"),
                    ),
            )
            .children(find.selected.and_then(|index| {
                find.hits.get(index).map(|hit| {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Match: {}", hit.excerpt))
                })
            }))
            .into_any_element()
    }
}

fn rgb_to_hsla(r: u8, g: u8, b: u8) -> Hsla {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-4 {
        return hsla(0.0, 0.0, l, 1.0);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-4 {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() < 1e-4 {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    hsla(h, s, l, 1.0)
}

fn ansi_index_to_hsla(idx: u8, is_light_theme: bool) -> Hsla {
    // The base palette is tuned for dark backgrounds. On light themes the
    // achromatic entries are inverted and bright chromatic colors are darkened
    // so they stay legible against a light terminal background.
    let adjust = |color: Hsla| -> Hsla {
        if is_light_theme {
            hsla(color.h, color.s, (color.l - 0.18).max(0.0), color.a)
        } else {
            color
        }
    };
    match idx {
        // Standard 16 ANSI colors
        0 => hsla(0.0, 0.0, if is_light_theme { 0.95 } else { 0.15 }, 1.0), // Black
        1 => adjust(hsla(0.0, 0.75, 0.60, 1.0)),                            // Red
        2 => adjust(hsla(0.35, 0.65, 0.55, 1.0)),                           // Green
        3 => adjust(hsla(0.12, 0.80, 0.60, 1.0)),                           // Yellow
        4 => adjust(hsla(0.60, 0.75, 0.65, 1.0)),                           // Blue
        5 => adjust(hsla(0.82, 0.65, 0.65, 1.0)),                           // Magenta
        6 => adjust(hsla(0.50, 0.75, 0.60, 1.0)),                           // Cyan
        7 => hsla(0.0, 0.0, if is_light_theme { 0.25 } else { 0.85 }, 1.0), // White (Dim)
        8 => hsla(0.0, 0.0, if is_light_theme { 0.55 } else { 0.45 }, 1.0), // Bright Black (Gray)
        9 => adjust(hsla(0.0, 0.85, 0.70, 1.0)),                            // Bright Red
        10 => adjust(hsla(0.35, 0.75, 0.65, 1.0)),                          // Bright Green
        11 => adjust(hsla(0.12, 0.90, 0.70, 1.0)),                          // Bright Yellow
        12 => adjust(hsla(0.60, 0.85, 0.75, 1.0)),                          // Bright Blue
        13 => adjust(hsla(0.82, 0.75, 0.75, 1.0)),                          // Bright Magenta
        14 => adjust(hsla(0.50, 0.85, 0.70, 1.0)),                          // Bright Cyan
        15 => hsla(0.0, 0.0, if is_light_theme { 0.05 } else { 0.98 }, 1.0), // Bright White
        // 216 Color cube: 16..=231
        16..=231 => {
            let n = idx - 16;
            let levels = [0, 95, 135, 175, 215, 255];
            let b = levels[(n % 6) as usize];
            let g = levels[((n / 6) % 6) as usize];
            let r = levels[(n / 36) as usize];
            rgb_to_hsla(r, g, b)
        }
        // 24 Grayscale ramp: 232..=255
        232..=255 => {
            let gray = (idx - 232) as f32 / 23.0 * 0.9 + 0.05;
            hsla(0.0, 0.0, gray, 1.0)
        }
    }
}

fn ansi_to_hsla(color: vt100::Color, default_fg: Hsla, is_light_theme: bool) -> Option<Hsla> {
    match color {
        vt100::Color::Default => Some(default_fg),
        vt100::Color::Idx(idx) => Some(ansi_index_to_hsla(idx, is_light_theme)),
        vt100::Color::Rgb(r, g, b) => Some(rgb_to_hsla(r, g, b)),
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(focus) = self.dismissed_link_focus.take() {
            if focus.contains_focused(window, cx) {
                self.focus_handle.focus(window, cx);
            }
        }
        let theme = cx.theme().colors;
        let is_light_theme = cx.theme().mode == ThemeMode::Light;
        let terminal_resize = cx.entity().clone();
        let terminal_actions = cx.entity().clone();
        let is_focused = self.focus_handle.is_focused(window);
        let font_id = window.text_system().resolve_font(&font(".ZedMono"));
        let measured_cell_width = window
            .text_system()
            .layout_width(font_id, px(self.font_size), '0')
            .as_f32();
        if measured_cell_width > 0.0 {
            self.cell_width = measured_cell_width;
        }
        let cell_width = self.cell_width;
        let font_size = self.font_size;
        let line_height = self.line_height();
        let row_height = self.row_height();

        let screen = &self.screen;
        let (cursor_row, cursor_col) = screen.cursor_position();
        let hide_cursor = screen.hide_cursor();

        // The selected find hit paints as a row-level background cue: it
        // sits behind the per-cell ANSI colors and is never part of copied
        // text, so a selection cannot change what Copy produces.
        let find_cue_row = self.find.as_ref().and_then(|find| {
            let hit = find.selected.and_then(|index| find.hits.get(index))?;
            cue_row(hit, self.scrollback_len, self.scrollback_offset, self.rows)
        });

        let mut screen_lines = Vec::with_capacity(self.rows as usize);
        let link_cells: std::collections::HashMap<_, _> = self
            .links
            .iter()
            .enumerate()
            .flat_map(|(index, link)| link.cells.iter().map(move |cell| (*cell, index)))
            .collect();
        for row in 0..self.rows {
            let mut row_spans = Vec::new();
            let mut current_span_text = String::new();
            let mut current_style: Option<(Option<Hsla>, Option<Hsla>, bool, bool, Option<usize>)> =
                None;

            // Find the rightmost non-empty column or cursor column
            let mut max_col = 0;
            for col in (0..self.cols).rev() {
                if let Some(cell) = screen.cell(row, col) {
                    let contents = cell.contents();
                    if (!contents.is_empty() && contents != " ")
                        || (row == cursor_row && col == cursor_col)
                    {
                        max_col = col + 1;
                        break;
                    }
                }
            }

            for col in 0..max_col {
                let is_cursor = should_paint_cursor(is_focused, hide_cursor, self.cursor_visible)
                    && row == cursor_row
                    && col == cursor_col;
                let is_selected = self.is_cell_selected(row, col);

                if let Some(cell) = screen.cell(row, col) {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let cell_content = cell.contents();
                    let char_str = if cell_content.is_empty() {
                        " "
                    } else {
                        cell_content
                    };

                    let fg = if is_selected {
                        Some(theme.accent_foreground)
                    } else if is_cursor {
                        Some(theme.background)
                    } else {
                        ansi_to_hsla(cell.fgcolor(), theme.foreground, is_light_theme)
                    };

                    let bg = if is_selected {
                        Some(theme.accent)
                    } else if is_cursor {
                        Some(theme.primary)
                    } else {
                        match cell.bgcolor() {
                            vt100::Color::Default => None,
                            other => ansi_to_hsla(other, theme.background, is_light_theme),
                        }
                    };

                    let link = link_cells.get(&(row, col)).copied();
                    let style = (fg, bg, cell.bold(), is_cursor, link);

                    if current_style == Some(style) {
                        current_span_text.push_str(char_str);
                    } else {
                        if let Some((cfg, cbg, bold, _cur, link)) = current_style {
                            if !current_span_text.is_empty() {
                                let mut span = div().child(current_span_text.clone());
                                if let Some(c) = cfg {
                                    span = span.text_color(c);
                                }
                                if let Some(c) = cbg {
                                    span = span.bg(c);
                                }
                                if bold {
                                    span = span.font_weight(FontWeight::BOLD);
                                }
                                if let Some(index) = link {
                                    let gesture = if cfg!(target_os = "macos") {
                                        "Cmd-click to open in Threadlane browser"
                                    } else {
                                        "Ctrl-click to open in default browser"
                                    };
                                    let tooltip = format!("{} — {gesture}", self.links[index].url);
                                    span = span.underline().cursor_pointer();
                                    row_spans.push(
                                        span.id((
                                            "terminal-link",
                                            row as usize * self.cols as usize + col as usize,
                                        ))
                                        .tooltip(move |window, cx| {
                                            gpui_component::tooltip::Tooltip::new(tooltip.clone())
                                                .build(window, cx)
                                        })
                                        .into_any_element(),
                                    );
                                } else {
                                    row_spans.push(span.into_any_element());
                                }
                            }
                        }
                        current_span_text.clear();
                        current_span_text.push_str(char_str);
                        current_style = Some(style);
                    }
                }
            }

            if let Some((cfg, cbg, bold, _cur, link)) = current_style {
                if !current_span_text.is_empty() {
                    let mut span = div().child(current_span_text);
                    if let Some(c) = cfg {
                        span = span.text_color(c);
                    }
                    if let Some(c) = cbg {
                        span = span.bg(c);
                    }
                    if bold {
                        span = span.font_weight(FontWeight::BOLD);
                    }
                    if let Some(index) = link {
                        let gesture = if cfg!(target_os = "macos") {
                            "Cmd-click to open in Threadlane browser"
                        } else {
                            "Ctrl-click to open in default browser"
                        };
                        let tooltip = format!("{} — {gesture}", self.links[index].url);
                        span = span.underline().cursor_pointer();
                        row_spans.push(
                            span.id(("terminal-link-end", row as usize))
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(tooltip.clone())
                                        .build(window, cx)
                                })
                                .into_any_element(),
                        );
                    } else {
                        row_spans.push(span.into_any_element());
                    }
                }
            }

            if row_spans.is_empty() {
                row_spans.push(div().child(" ").into_any_element());
            }

            let find_cue = find_cue_row == Some(row);
            screen_lines.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(row_height))
                    .when(find_cue, |row| row.bg(theme.accent.opacity(0.18)))
                    .children(row_spans),
            );
        }

        let status_banner = self.status.as_ref().map(|status_text| {
            let restart_handle = terminal_actions.clone();
            let is_error = status_text.starts_with("Terminal read failed")
                || status_text.starts_with("Unable to start terminal");
            let status_color = if is_error {
                theme.danger
            } else {
                theme.warning
            };
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .bg(status_color.opacity(0.1))
                .border_1()
                .border_color(status_color.opacity(0.3))
                .rounded_md()
                .mt_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            Icon::new(if is_error {
                                IconName::CircleX
                            } else {
                                IconName::Info
                            })
                            .xsmall()
                            .text_color(status_color),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.foreground)
                                .child(status_text.clone()),
                        ),
                )
                .child(
                    Button::new("terminal-restart-banner-btn")
                        .label("Restart shell")
                        .icon(IconName::Redo)
                        .ghost()
                        .xsmall()
                        .on_click(move |_event, _window, cx| {
                            restart_handle.update(cx, |t, cx| t.restart(cx));
                        }),
                )
        });

        let autoscroll_pill = if self.scrollback_offset > 0 {
            let scroll_to_bottom_handle = terminal_actions.clone();
            Some(
                // Lifted clear of the last output lines with a scrimmed,
                // outlined pill so it never covers PTY text nor mis-taps
                // during selection.
                div()
                    .absolute()
                    .bottom_7()
                    .right_3()
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.title_bar)
                    .shadow_md()
                    .child(
                        Button::new("terminal-autoscroll-pill")
                            .label(format!(
                                "↓ Scroll to Bottom ({} lines up)",
                                self.scrollback_offset
                            ))
                            .icon(IconName::ChevronDown)
                            .tooltip("Jump to live output")
                            .xsmall()
                            .ghost()
                            .on_click(move |_event, _window, cx| {
                                scroll_to_bottom_handle.update(cx, |t, cx| t.scroll_to_bottom(cx));
                            }),
                    ),
            )
        } else {
            None
        };

        let find_strip = self.find.as_ref().map(|_| self.render_find_strip(cx));

        div()
            .id("pty-terminal-root")
            .key_context("Terminal")
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::next_terminal_match))
            .on_action(cx.listener(Self::previous_terminal_match))
            .bg(theme.background.opacity(if self.translucent_background {
                0.92
            } else {
                1.0
            }))
            .rounded_md()
            .border_1()
            .border_color(gpui::transparent_black())
            .focus(|style| style.border_color(theme.ring))
            .track_focus(&self.focus_handle)
            .role(Role::Terminal)
            .on_key_down(cx.listener(Self::key_down))
            .children(find_strip)
            .children(
                self.link_menu
                    .as_ref()
                    .map(|menu| deferred(anchored().child(menu.clone())).with_priority(1)),
            )
            .child(
                div()
                    .id(("pty-terminal-screen", self.link_epoch as usize))
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .font_family(".ZedMono")
                    // Raster-bound: glyph size must match row_height
                    // and the measured cell width; not a type-scale step.
                    .text_size(px(font_size))
                    .line_height(relative(line_height))
                    .cursor_text()
                    .on_scroll_wheel(cx.listener(
                        move |this, event: &ScrollWheelEvent, _window, cx| {
                            let delta = match event.delta {
                                ScrollDelta::Lines(lines) => lines.y * 2.0,
                                ScrollDelta::Pixels(pixels) => pixels.y.as_f32() / row_height,
                            };
                            if delta.abs() > 0.01 {
                                this.scroll_by(delta, cx);
                            }
                        },
                    ))
                    .on_prepaint(move |bounds, _, cx| {
                        let rows = ((bounds.size.height.as_f32() - TERMINAL_CONTENT_INSET * 2.0)
                            / row_height)
                            .floor() as u16;
                        let cols = ((bounds.size.width.as_f32() - TERMINAL_CONTENT_INSET * 2.0)
                            / cell_width)
                            .floor() as u16;
                        terminal_resize.update(cx, |terminal, cx| {
                            if terminal.screen_bounds != Some(bounds) {
                                terminal.link_press = None;
                            }
                            terminal.screen_bounds = Some(bounds);
                            terminal.resize(rows, cols, cx);
                        });
                    })
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, _, _| {
                            this.context_link = this
                                .link_at(event.position)
                                .map(|link| (link.url.clone(), this.link_epoch));
                        }),
                    )
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::begin_selection))
                    .on_mouse_move(cx.listener(Self::extend_selection))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::end_selection))
                    .children(screen_lines)
                    .children(status_banner)
                    .children(autoscroll_pill)
                    .context_menu({
                        let terminal = terminal_actions.clone();
                        move |menu, _window, cx| {
                            let (output, selection) = {
                                let terminal = terminal.read(cx);
                                (terminal.screen_text(), terminal.selected_text())
                            };
                            let mut menu = menu;
                            if let Some((url, epoch)) = terminal.read(cx).context_link.clone() {
                                menu = Self::link_commands(menu, url, terminal.downgrade(), epoch)
                                    .separator();
                            }
                            if let Some(selection) = &selection {
                                let selection = selection.clone();
                                menu = menu.item(PopupMenuItem::new("Copy Selection").on_click(
                                    move |_event, _window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            selection.clone(),
                                        ));
                                    },
                                ));
                            }
                            let t_paste = terminal.clone();
                            let t_select = terminal.clone();
                            let t_clear = terminal.clone();
                            let t_restart = terminal.clone();
                            let (font_size, compact, translucent_background) = {
                                let view = terminal.read(cx);
                                (view.font_size, view.compact, view.translucent_background)
                            };
                            for (label, size) in
                                [("Small", 11.0), ("Medium", 13.0), ("Large", 16.0)]
                            {
                                let target = terminal.clone();
                                menu = menu.item(
                                    PopupMenuItem::new(format!("Font size: {label}"))
                                        .checked(font_size == size)
                                        .on_click(move |_, _, cx| {
                                            target.update(cx, |view, cx| {
                                                view.invalidate_links();
                                                view.sync_link_frame();
                                                view.font_size = size;
                                                cx.notify();
                                            });
                                        }),
                                );
                            }
                            let compact_target = terminal.clone();
                            menu = menu.item(
                                PopupMenuItem::new("Compact lines")
                                    .checked(compact)
                                    .on_click(move |_, _, cx| {
                                        compact_target.update(cx, |view, cx| {
                                            view.invalidate_links();
                                            view.sync_link_frame();
                                            view.compact = !view.compact;
                                            cx.notify();
                                        });
                                    }),
                            );
                            let background_target = terminal.clone();
                            menu = menu.item(
                                PopupMenuItem::new("Blend background")
                                    .checked(translucent_background)
                                    .on_click(move |_, _, cx| {
                                        background_target.update(cx, |view, cx| {
                                            view.translucent_background =
                                                !view.translucent_background;
                                            cx.notify();
                                        });
                                    }),
                            );
                            let t_find = terminal.clone();
                            menu = menu.item(
                                PopupMenuItem::new("Find in Terminal Output…").on_click(
                                    move |_event, window, cx| {
                                        t_find.update(cx, |terminal, cx| {
                                            terminal.open_find(&FindInTerminalOutput, window, cx)
                                        });
                                    },
                                ),
                            );
                            menu.item(PopupMenuItem::new("Copy Terminal Output").on_click(
                                move |_event, _window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        output.clone(),
                                    ));
                                },
                            ))
                            .item(PopupMenuItem::new("Paste").on_click(
                                move |_event, _window, cx| {
                                    t_paste.update(cx, |terminal, cx| {
                                        terminal.paste_from_clipboard(cx)
                                    });
                                },
                            ))
                            .item(PopupMenuItem::new("Select All").on_click(
                                move |_event, _window, cx| {
                                    t_select.update(cx, |terminal, cx| terminal.select_all(cx));
                                },
                            ))
                            .item(PopupMenuItem::new("Clear Terminal").on_click(
                                move |_event, _window, cx| {
                                    t_clear.update(cx, |terminal, cx| terminal.clear(cx));
                                },
                            ))
                            .item(
                                PopupMenuItem::new("Restart Terminal").on_click(
                                    move |_event, _window, cx| {
                                        t_restart.update(cx, |terminal, cx| terminal.restart(cx));
                                    },
                                ),
                            )
                        }
                    }),
            )
    }
}

fn spawn_shell(
    project: &PathBuf,
    rows: u16,
    cols: u16,
    output_tx: mpsc::SyncSender<Vec<u8>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<PtyEvent>,
) -> Result<PtySession, Box<dyn std::error::Error + Send + Sync>> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut command = CommandBuilder::new(shell);
    command.cwd(project);
    command.env("TERM", "xterm-256color");
    command.arg("-i");

    let child = pair.slave.spawn_command(command)?;
    let mut reader = pair.master.try_clone_reader()?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer()?));
    drop(pair.slave);

    std::thread::Builder::new()
        .name("threadlane-gpui-pty-reader".into())
        .spawn(move || {
            let mut buffer = [0_u8; TERMINAL_READ_CHUNK_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = event_tx.send(PtyEvent::Closed);
                        break;
                    }
                    Ok(read) => {
                        if output_tx.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(PtyEvent::Error(error.to_string()));
                        break;
                    }
                }
            }
        })?;

    Ok(PtySession {
        master: pair.master,
        writer,
        child,
    })
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};
    use std::time::Duration;

    use super::{
        ansi_index_to_hsla, next_terminal_wake, rgb_to_hsla, selected_excerpt, selection_bounds,
        selection_present, should_paint_cursor, start_parser_worker, terminal_frame_policy,
        terminal_parse_budget_exhausted, ParserCommand, PtyEvent, TerminalWake,
        TERMINAL_FIND_RESCAN_INTERVAL, TERMINAL_PARSE_BUDGET_PER_FRAME,
    };

    /// Receives worker events until a `SearchResults` event arrives.
    async fn next_search_results(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PtyEvent>,
    ) -> PtyEvent {
        for _ in 0..200 {
            let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .expect("timed out waiting for terminal search results")
                .expect("terminal worker channel closed");
            if matches!(event, PtyEvent::SearchResults { .. }) {
                return event;
            }
        }
        panic!("expected terminal search results");
    }

    /// Waits for the next painted frame; used to know previously sent output
    /// bytes were parsed before issuing a command that scans the buffer.
    async fn next_frame(
        event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<PtyEvent>,
    ) {
        for _ in 0..200 {
            let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
                .await
                .expect("timed out waiting for a terminal frame")
                .expect("terminal worker channel closed");
            if matches!(event, PtyEvent::Frame(_)) {
                return;
            }
        }
        panic!("expected a terminal frame");
    }

    #[test]
    fn terminal_selection_excerpt_follows_the_drag_endpoints() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"build failed\r\nsecond line");
        let screen = parser.screen();

        // No anchors — nothing selected.
        assert!(selected_excerpt(screen, None, None, 20).is_none());
        // A zero-width drag is not a selection.
        assert!(!selection_present(Some((0, 0)), Some((0, 0)), 20));
        assert!(selected_excerpt(screen, Some((0, 0)), Some((0, 0)), 20).is_none());
        // Forward and reversed drags cover the same cells: a forward
        // drag's head cell is exclusive, a reversed drag's anchor is
        // extended by one to compensate.
        assert!(selection_present(Some((0, 0)), Some((0, 12)), 20));
        assert_eq!(
            selected_excerpt(screen, Some((0, 0)), Some((0, 12)), 20).as_deref(),
            Some("build failed")
        );
        assert_eq!(
            selected_excerpt(screen, Some((0, 11)), Some((0, 0)), 20).as_deref(),
            Some("build failed")
        );
        // Cross-line selections keep the line break.
        assert_eq!(
            selected_excerpt(screen, Some((0, 6)), Some((1, 6)), 20).as_deref(),
            Some("failed\nsecond")
        );
        // Snapshots are recomputed against the current screen, never a
        // remembered copy — output landing after the drag changes the
        // next snapshot rather than being silently re-read as the old text.
        parser.process(b"\x1b[Hchanged");
        assert_ne!(
            selected_excerpt(parser.screen(), Some((0, 6)), Some((1, 6)), 20).as_deref(),
            Some("failed\nsecond")
        );
    }

    #[gpui::test]
    fn screen_resets_invalidate_a_selection_instead_of_reinterpreting_it(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        let terminal = cx.update(|cx| {
            cx.new(|cx| super::TerminalView::new_for_test(std::path::PathBuf::from("/tmp"), cx))
        });
        terminal.update(cx, |terminal, cx| {
            let mut parser = vt100::Parser::new(4, 20, 0);
            parser.process(b"select me");
            terminal.apply_event(super::PtyEvent::Frame(super::TerminalFrame {
                link_epoch: 0,
                links: Vec::new(),
                screen: parser.screen().clone(),
                scrollback: 0,
                scrollback_len: 0,
                alt_screen: false,
            }));
            terminal.selection_anchor = Some((0, 0));
            terminal.selection_head = Some((0, 9));
            assert!(terminal.has_selection());
            let snapshot = terminal.selection_snapshot().expect("selection snapshot");
            assert_eq!(snapshot.text, "select me");
            assert_eq!(snapshot.launched_in, std::path::PathBuf::from("/tmp"));
            let status = terminal.selection_status().expect("selection status");
            assert!(status.has_text);
            assert_eq!(status.excerpt_len, 9);
            // Geometry changes clear the selection rather than re-mapping
            // the old cell coordinates onto the resized screen.
            terminal.resize(8, 40, cx);
            assert!(!terminal.has_selection());
            assert!(terminal.selection_snapshot().is_none());
            assert!(terminal.selection_status().is_none());
        });
        terminal.update(cx, |terminal, cx| {
            // Clearing the screen invalidates the selection the same way:
            // the retained coordinates must not be re-read against a
            // blank or restarted screen.
            terminal.selection_anchor = Some((0, 0));
            terminal.selection_head = Some((0, 9));
            assert!(terminal.has_selection());
            terminal.clear(cx);
            assert!(!terminal.has_selection());
            assert!(terminal.selection_snapshot().is_none());
        });
    }

    #[test]
    fn terminal_parser_yields_at_its_frame_budget() {
        assert!(!terminal_parse_budget_exhausted(
            TERMINAL_PARSE_BUDGET_PER_FRAME - 1,
            TERMINAL_PARSE_BUDGET_PER_FRAME,
        ));
        assert!(terminal_parse_budget_exhausted(
            TERMINAL_PARSE_BUDGET_PER_FRAME,
            TERMINAL_PARSE_BUDGET_PER_FRAME,
        ));
    }

    #[test]
    fn saturated_terminal_halves_redraws_and_approximately_preserves_parse_throughput() {
        let (normal_interval, normal_budget) = terminal_frame_policy(false);
        let (flood_interval, flood_budget) = terminal_frame_policy(true);

        assert_eq!(normal_interval, Duration::from_millis(16));
        assert_eq!(flood_interval, Duration::from_millis(33));
        assert_eq!(flood_budget, normal_budget * 2);
    }

    #[tokio::test]
    async fn terminal_wake_coalesces_queued_frames_immediately() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"old");
        event_tx
            .send(PtyEvent::Frame(super::TerminalFrame {
                link_epoch: 0,
                links: Vec::new(),
                screen: parser.screen().clone(),
                scrollback: 0,
                scrollback_len: 0,
                alt_screen: false,
            }))
            .unwrap();
        parser.process(b"\rnew");
        event_tx
            .send(PtyEvent::Frame(super::TerminalFrame {
                link_epoch: 0,
                links: Vec::new(),
                screen: parser.screen().clone(),
                scrollback: 0,
                scrollback_len: 0,
                alt_screen: false,
            }))
            .unwrap();
        event_tx.send(PtyEvent::Error("closed".into())).unwrap();

        let TerminalWake::Events(events) = next_terminal_wake(&mut event_rx, pending()).await
        else {
            panic!("expected terminal events");
        };
        assert_eq!(events.len(), 2);
        let PtyEvent::Frame(frame) = &events[0] else {
            panic!("expected latest terminal frame");
        };
        assert_eq!(frame.screen.contents(), "new");
        assert!(matches!(events[1], PtyEvent::Error(_)));
    }

    #[tokio::test]
    async fn terminal_wake_uses_the_cursor_timer_when_idle() {
        let (_event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        assert!(matches!(
            next_terminal_wake(&mut event_rx, ready(())).await,
            TerminalWake::Blink
        ));
    }

    #[tokio::test]
    async fn parser_worker_publishes_output_and_clear_frames() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(2, 8, event_tx).unwrap();

        output_tx.send(b"hello".to_vec()).unwrap();
        let PtyEvent::Frame(frame) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected parsed terminal frame");
        };
        assert_eq!(frame.screen.contents(), "hello");

        command_tx.send(ParserCommand::Clear).unwrap();
        let PtyEvent::Frame(frame) = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("expected cleared terminal frame");
        };
        assert_eq!(frame.screen.contents(), "");
    }

    #[tokio::test]
    async fn parser_worker_find_covers_retained_scrollback_and_reveals_newest() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        let mut input = String::from("needle deep\r\n");
        for line in 0..40 {
            input.push_str(&format!("line {line}\r\n"));
        }
        input.push_str("needle live");
        output_tx.send(input.into_bytes()).unwrap();
        next_frame(&mut event_rx).await;

        command_tx
            .send(ParserCommand::Find { generation: 7, query: Some("needle".to_string()) })
            .unwrap();
        let PtyEvent::SearchResults {
            generation, hits, total, scrollback_len, alt_screen, revealed, ..
        } = next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert_eq!(generation, 7);
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].absolute_row, 0);
        assert!(hits[0].excerpt.contains("needle deep"));
        assert_eq!(scrollback_len, 38);
        assert!(!alt_screen);
        // A settled query reveals the newest match — here the live row.
        assert_eq!(revealed, Some(1));
    }

    #[tokio::test]
    async fn parser_worker_reveal_moves_the_view_to_a_scrollback_match() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        let mut input = String::from("needle deep\r\n");
        for line in 0..40 {
            input.push_str(&format!("line {line}\r\n"));
        }
        output_tx.send(input.into_bytes()).unwrap();
        next_frame(&mut event_rx).await;

        command_tx
            .send(ParserCommand::Find { generation: 1, query: Some("needle".to_string()) })
            .unwrap();
        let _ = next_search_results(&mut event_rx).await;

        // The worker resolves the hit by identity in a fresh scan.
        command_tx
            .send(ParserCommand::RevealMatch {
                generation: 1,
                row: 0,
                excerpt: "needle deep".to_string(),
            })
            .unwrap();
        let PtyEvent::SearchResults { revealed, .. } = next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert_eq!(revealed, Some(0));

        // The reveal moved the parser's view into deep scrollback; the next
        // painted frame carries that offset to the view.
        let mut saw_deep_frame = false;
        for _ in 0..200 {
            let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("timed out waiting for a terminal frame")
                .expect("terminal worker channel closed");
            if let PtyEvent::Frame(frame) = event {
                if frame.scrollback > 0 {
                    saw_deep_frame = true;
                    break;
                }
            }
        }
        assert!(saw_deep_frame, "expected a frame scrolled into scrollback");
    }

    #[tokio::test]
    async fn parser_worker_stale_reveal_generation_is_ignored() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        command_tx
            .send(ParserCommand::Find { generation: 4, query: Some("needle".to_string()) })
            .unwrap();
        let _ = next_search_results(&mut event_rx).await;

        command_tx
            .send(ParserCommand::RevealMatch {
                generation: 999,
                row: 0,
                excerpt: "needle".to_string(),
            })
            .unwrap();
        // No rescan is scheduled for a mismatched generation: draining the
        // channel for a while must not surface another SearchResults.
        let saw_results = tokio::time::timeout(Duration::from_millis(400), async {
            loop {
                match event_rx.recv().await {
                    Some(PtyEvent::SearchResults { .. }) => break true,
                    Some(_) | None => {}
                }
            }
        })
        .await;
        assert!(saw_results.is_err(), "stale generation produced results");
    }

    #[tokio::test]
    async fn parser_worker_reveal_refuses_a_stale_hit_identity() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle one\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        command_tx
            .send(ParserCommand::Find { generation: 8, query: Some("needle".to_string()) })
            .unwrap();
        let _ = next_search_results(&mut event_rx).await;

        // A clear + rewrite removes the hit the view navigated to. The
        // identity can no longer be confirmed, so the worker must NOT reveal
        // a different line in its place.
        command_tx.send(ParserCommand::Clear).unwrap();
        let _ = next_search_results(&mut event_rx).await;
        output_tx.send(b"needle other\r\n".to_vec()).unwrap();
        let _ = next_search_results(&mut event_rx).await;

        command_tx
            .send(ParserCommand::RevealMatch {
                generation: 8,
                row: 0,
                excerpt: "needle one".to_string(),
            })
            .unwrap();
        let PtyEvent::SearchResults { revealed, .. } = next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert_eq!(revealed, None);
    }

    #[tokio::test]
    async fn parser_worker_suspends_and_resumes_find_on_alternate_screen() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle top\r\nmore\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        output_tx.send(b"\x1b[?1049hfull screen app".to_vec()).unwrap();
        next_frame(&mut event_rx).await;

        command_tx
            .send(ParserCommand::Find { generation: 2, query: Some("needle".to_string()) })
            .unwrap();
        let PtyEvent::SearchResults { alt_screen, hits, total, .. } =
            next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert!(alt_screen);
        assert!(hits.is_empty());
        assert_eq!(total, 0);

        // Leaving the alternate screen rescans and reports the restored
        // primary buffer on the next coalesced rescan tick.
        output_tx.send(b"\x1b[?1049l".to_vec()).unwrap();
        let PtyEvent::SearchResults { alt_screen, total, .. } =
            next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert!(!alt_screen);
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn parser_worker_refreshes_results_as_output_arrives() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle one\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        command_tx
            .send(ParserCommand::Find { generation: 3, query: Some("needle".to_string()) })
            .unwrap();
        let PtyEvent::SearchResults { total, revealed, .. } =
            next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert_eq!(total, 1);
        assert_eq!(revealed, Some(0));

        // New output updates the result list via a coalesced rescan without
        // moving the reading position (revealed is None on refreshes).
        output_tx.send(b"needle two\r\n".to_vec()).unwrap();
        let PtyEvent::SearchResults { total, revealed, .. } =
            next_search_results(&mut event_rx).await
        else {
            panic!("expected search results");
        };
        assert_eq!(total, 2);
        assert_eq!(revealed, None);
    }

    #[tokio::test]
    async fn parser_worker_invalidates_results_on_clear_and_resize() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        command_tx
            .send(ParserCommand::Find { generation: 5, query: Some("needle".to_string()) })
            .unwrap();
        let _ = next_search_results(&mut event_rx).await;

        command_tx.send(ParserCommand::Clear).unwrap();
        let PtyEvent::SearchResults { total, .. } = next_search_results(&mut event_rx).await else {
            panic!("expected search results");
        };
        assert_eq!(total, 0);

        output_tx.send(b"needle again\r\n".to_vec()).unwrap();
        let _ = next_search_results(&mut event_rx).await;
        command_tx.send(ParserCommand::Resize(2, 8)).unwrap();
        let PtyEvent::SearchResults { total, .. } = next_search_results(&mut event_rx).await else {
            panic!("expected search results");
        };
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn parser_worker_keeps_retained_output_searchable_after_output_ends() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle retained\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        drop(output_tx);
        // Let the worker observe the output channel closing.
        tokio::time::sleep(Duration::from_millis(100)).await;

        command_tx
            .send(ParserCommand::Find { generation: 6, query: Some("needle".to_string()) })
            .unwrap();
        let PtyEvent::SearchResults { total, .. } = next_search_results(&mut event_rx).await else {
            panic!("expected search results");
        };
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn parser_worker_does_not_scan_while_find_is_closed() {
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (output_tx, command_tx) = start_parser_worker(4, 16, event_tx).unwrap();

        output_tx.send(b"needle\r\n".to_vec()).unwrap();
        next_frame(&mut event_rx).await;
        command_tx
            .send(ParserCommand::Find { generation: 1, query: Some("needle".to_string()) })
            .unwrap();
        let _ = next_search_results(&mut event_rx).await;

        command_tx
            .send(ParserCommand::Find { generation: 1, query: None })
            .unwrap();
        // Keep output flowing well past the rescan interval: no search may
        // run after the strip closed.
        let window = TERMINAL_FIND_RESCAN_INTERVAL + Duration::from_millis(400);
        let mut produced_results = false;
        let _ = tokio::time::timeout(window, async {
            for _ in 0..8 {
                output_tx.send(b"needle more\r\n".to_vec()).unwrap();
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            loop {
                match event_rx.recv().await {
                    Some(PtyEvent::SearchResults { .. }) => produced_results = true,
                    Some(_) => {}
                    None => break,
                }
            }
        })
        .await;
        assert!(!produced_results);
    }

    #[test]
    fn xterm_color_cube_uses_standard_channel_levels() {
        assert_eq!(ansi_index_to_hsla(17, false), rgb_to_hsla(0, 0, 95));
        assert_eq!(ansi_index_to_hsla(67, false), rgb_to_hsla(95, 135, 175));
    }

    #[test]
    fn backward_selection_includes_its_anchor_cell() {
        assert_eq!(selection_bounds((0, 8), (0, 0), 20), Some(((0, 0), (0, 9))));
        assert_eq!(selection_bounds((0, 0), (0, 8), 20), Some(((0, 0), (0, 8))));
    }

    #[test]
    fn cursor_blink_only_controls_a_focused_visible_cursor() {
        assert!(should_paint_cursor(true, false, true));
        assert!(!should_paint_cursor(true, false, false));
        assert!(!should_paint_cursor(false, false, true));
        assert!(!should_paint_cursor(true, true, true));
    }
}

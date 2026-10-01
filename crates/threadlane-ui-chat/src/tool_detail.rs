//! Purpose-built detail cards for expanded tool activities: a terminal-style
//! card for command tools and a diff-styled code card for edit/write tools.

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::scroll::{Scrollable, ScrollableElement};
use gpui_component::tag::{Tag, TagVariant};
use gpui_component::theme::ActiveTheme;
use gpui_component::{Icon, IconName, Sizable};

use threadlane_ui_state::actions::AppAction;
use threadlane_ui_state::{controller, AppState, ToolActivityInfo};

/// Lines of synthesized content kept when an edit call has no result diff
/// yet; long write payloads collapse behind a trailing-count row.
const MAX_SYNTHESIZED_ROWS: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffRowKind {
    Add,
    Remove,
    Context,
    Hunk,
    FileHeader,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiffRow {
    kind: DiffRowKind,
    /// Old-file line number for context/removed rows.
    old_no: Option<usize>,
    /// New-file line number for context/added rows.
    new_no: Option<usize>,
    text: String,
}

impl DiffRow {
    fn changed(kind: DiffRowKind, line: &str, no: usize) -> Self {
        let (old_no, new_no) = match kind {
            DiffRowKind::Add => (None, Some(no)),
            DiffRowKind::Remove => (Some(no), None),
            _ => (Some(no), Some(no)),
        };
        DiffRow {
            kind,
            old_no,
            new_no,
            text: line.to_string(),
        }
    }

    fn marker(kind: DiffRowKind, text: impl Into<String>) -> Self {
        DiffRow {
            kind,
            old_no: None,
            new_no: None,
            text: text.into(),
        }
    }
}

/// `run_command` output rendered by `dispatch.rs`:
/// `Exit Status: {status}\n--- STDOUT ---\n{stdout}\n--- STDERR ---\n{stderr}`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct CommandOutput {
    status: Option<String>,
    stdout: String,
    stderr: String,
}

#[derive(Clone, Debug, Default)]
struct CommandDetail {
    command: Option<String>,
    cwd: Option<String>,
    pending: bool,
    output: CommandOutput,
}

fn normalized_tool_name(title: &str) -> String {
    title.trim().to_lowercase().replace(' ', "_")
}

fn is_command_tool(title: &str) -> bool {
    let name = normalized_tool_name(title);
    if matches!(
        name.as_str(),
        "run_command"
            | "run_terminal_command"
            | "shell_command"
            | "execute_command"
            | "terminal"
            | "bash"
            | "shell"
    ) {
        return true;
    }
    // ACP agents pass human-facing titles like "Execute npm test".
    matches!(
        name.split('_').next().unwrap_or_default(),
        "execute" | "run"
    )
}

fn is_edit_tool(title: &str) -> bool {
    let name = normalized_tool_name(title);
    if matches!(
        name.as_str(),
        "replace_file_content"
            | "multi_replace_file_content"
            | "apply_workspace_edit_plan"
            | "apply_patch"
            | "str_replace"
            | "str_replace_editor"
            | "edit_file_hashline"
            | "edit_files_hashline"
    ) {
        return true;
    }
    matches!(
        name.split('_').next().unwrap_or_default(),
        "edit" | "write" | "create" | "patch"
    )
}

fn args_json(arguments: &str) -> Option<serde_json::Value> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .ok()
        .filter(|value| value.is_object())
}

fn args_str<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(|value| value.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn args_path(args: &serde_json::Value) -> Option<String> {
    args_str(
        args,
        &[
            "path",
            "file_path",
            "FilePath",
            "TargetFile",
            "target_file",
            "filename",
            "file",
        ],
    )
    .map(str::to_owned)
}

/// Parses a `@@ -old_start[,old_count] +new_start[,new_count] @@` header into
/// the first old/new line numbers it covers.
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.trim().strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(' ')?;
    let new_part = rest.split_once(" @@")?.0.strip_prefix('+')?;
    let old_start = old_part.split(',').next()?.parse().ok()?;
    let new_start = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

/// Extracts unified-diff rows from a tool result body. Handles the
/// `Diff:`/`Updated Line Hashes:` sections emitted by the hashline edit tools
/// (including multi-file transactions, where each `Diff:` block is preceded by
/// its file path) and bare `@@` hunks from other edit tools.
fn parse_unified_diff(text: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut in_hunk = false;
    let mut in_anchors = false;
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let mut prev_line = String::new();

    for line in text.lines() {
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            in_hunk = true;
            in_anchors = false;
            old_no = old_start;
            new_no = new_start;
            rows.push(DiffRow::marker(DiffRowKind::Hunk, line.trim()));
            continue;
        }
        if line.trim() == "Diff:" {
            in_anchors = false;
            in_hunk = false;
            // Multi-file transactions print `<path>\nDiff:`; single-file
            // results print a "Successfully applied…" sentence instead, which
            // the space filter rejects.
            if !prev_line.is_empty() && !prev_line.contains(' ') {
                rows.push(DiffRow::marker(DiffRowKind::FileHeader, prev_line.clone()));
            }
            continue;
        }
        if line.trim() == "Updated Line Hashes:" {
            in_hunk = false;
            in_anchors = true;
            continue;
        }
        if in_anchors {
            let is_anchor_row = line
                .split('|')
                .next()
                .is_some_and(|head| {
                    let mut parts = head.trim().split(':');
                    matches!(parts.next(), Some(no) if no.chars().all(|c| c.is_ascii_digit()))
                        && parts.next().is_some_and(|hash| {
                            !hash.is_empty() && hash.chars().all(|c| c.is_ascii_alphanumeric())
                        })
                        && parts.next().is_none()
                });
            // The first non-anchor line (blank separator or the next file's
            // path in multi-file output) ends the anchor block.
            if line.trim().is_empty() || is_anchor_row {
                continue;
            }
            in_anchors = false;
        }
        if in_hunk {
            match line.chars().next() {
                Some('+') => {
                    rows.push(DiffRow::changed(DiffRowKind::Add, &line[1..], new_no));
                    new_no += 1;
                    continue;
                }
                Some('-') => {
                    rows.push(DiffRow::changed(DiffRowKind::Remove, &line[1..], old_no));
                    old_no += 1;
                    continue;
                }
                Some(' ') => {
                    rows.push(DiffRow::changed(
                        DiffRowKind::Context,
                        &line[1..],
                        new_no,
                    ));
                    old_no += 1;
                    new_no += 1;
                    continue;
                }
                // "\ No newline at end of file" markers carry no line.
                Some('\\') => continue,
                _ => in_hunk = false,
            }
        }
        if !line.trim().is_empty() {
            prev_line = line.trim().to_string();
        }
    }
    rows
}

fn anchor_line(anchor: Option<&serde_json::Value>) -> Option<usize> {
    anchor
        .and_then(|value| value.as_str())
        .and_then(|anchor| anchor.split(':').next())
        .and_then(|line| line.parse().ok())
        .and_then(|line| (line > 0).then_some(line))
}

fn push_content_rows(rows: &mut Vec<DiffRow>, kind: DiffRowKind, content: &str, start: usize) {
    for (offset, line) in content.lines().enumerate() {
        if rows.len() >= MAX_SYNTHESIZED_ROWS {
            let remaining = content.lines().count().saturating_sub(offset);
            if remaining > 0 {
                rows.push(DiffRow::marker(
                    DiffRowKind::Context,
                    format!("… {remaining} more lines"),
                ));
            }
            return;
        }
        rows.push(DiffRow::changed(kind, line, start + offset));
    }
}

/// Synthesizes diff rows from hashline-style `edits` arrays (the
/// `edit_file_hashline`/`edit_files_hashline` argument shape) for calls whose
/// result has not landed yet.
fn push_hashline_edit_rows(rows: &mut Vec<DiffRow>, edits: &[serde_json::Value]) {
    for edit in edits {
        if rows.len() >= MAX_SYNTHESIZED_ROWS {
            rows.push(DiffRow::marker(DiffRowKind::Context, "… more edits"));
            return;
        }
        let start = anchor_line(edit.get("start_anchor"));
        let end = anchor_line(edit.get("end_anchor")).or(start);
        let action = edit
            .get("action")
            .and_then(|value| value.as_str())
            .unwrap_or("replace");
        match action {
            "delete" => {
                let range = match (start, end) {
                    (Some(start), Some(end)) if end > start => format!("lines {start}–{end}"),
                    (Some(start), _) => format!("line {start}"),
                    _ => "selected lines".to_string(),
                };
                rows.push(DiffRow::marker(
                    DiffRowKind::Remove,
                    format!("− {range} deleted"),
                ));
            }
            _ => {
                let content = edit
                    .get("new_content")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if action == "insert_after" {
                    rows.push(DiffRow::marker(
                        DiffRowKind::Hunk,
                        match start {
                            Some(line) => format!("insert after line {line}"),
                            None => "insert".to_string(),
                        },
                    ));
                }
                push_content_rows(rows, DiffRowKind::Add, content, start.unwrap_or(1));
            }
        }
    }
}

/// Synthesizes diff rows for old/new-style edit tools
/// (`replace_file_content`, `multi_replace_file_content`, ACP "edit" calls)
/// from common argument spellings.
fn push_old_new_rows(rows: &mut Vec<DiffRow>, edit: &serde_json::Value) {
    const OLD_KEYS: &[&str] = &[
        "old_str",
        "old_string",
        "oldString",
        "old_text",
        "oldText",
        "old",
    ];
    const NEW_KEYS: &[&str] = &[
        "new_str",
        "new_string",
        "newString",
        "new_text",
        "newText",
        "new",
    ];
    let Some(old) = args_str(edit, OLD_KEYS) else {
        return;
    };
    let new = args_str(edit, NEW_KEYS).unwrap_or_default();
    push_content_rows(rows, DiffRowKind::Remove, old, 1);
    push_content_rows(rows, DiffRowKind::Add, new, 1);
}

/// Builds diff rows for an edit/write activity: the result's unified diff
/// when present, otherwise a synthesized view of the call's arguments.
fn diff_rows(activity: &ToolActivityInfo) -> Vec<DiffRow> {
    let parsed = parse_unified_diff(&activity.detail);
    if parsed
        .iter()
        .any(|row| matches!(row.kind, DiffRowKind::Add | DiffRowKind::Remove))
    {
        return parsed;
    }
    let Some(args) = args_json(&activity.arguments) else {
        return Vec::new();
    };
    let name = normalized_tool_name(&activity.title);
    let mut rows = Vec::new();

    // Whole-file writes render as a pure addition.
    if name.starts_with("write") || name.starts_with("create") {
        if let Some(content) = args_str(&args, &["content", "file_text", "new_content"]) {
            push_content_rows(&mut rows, DiffRowKind::Add, content, 1);
        }
        return rows;
    }

    if let Some(files) = args.get("files").and_then(|value| value.as_array()) {
        for file in files {
            if let Some(path) = args_str(file, &["path", "file_path", "TargetFile"]) {
                rows.push(DiffRow::marker(DiffRowKind::FileHeader, path));
            }
            if let Some(edits) = file.get("edits").and_then(|value| value.as_array()) {
                push_hashline_edit_rows(&mut rows, edits);
            }
        }
        return rows;
    }

    if let Some(edits) = args.get("edits").and_then(|value| value.as_array()) {
        // Hashline edits carry `start_anchor`; old/new edits carry `old_str`
        // spellings.
        let hashline = edits
            .iter()
            .any(|edit| edit.get("start_anchor").is_some() || edit.get("action").is_some());
        if hashline {
            push_hashline_edit_rows(&mut rows, edits);
        } else {
            for edit in edits {
                push_old_new_rows(&mut rows, edit);
            }
        }
        return rows;
    }

    push_old_new_rows(&mut rows, &args);
    rows
}

fn diff_stats(rows: &[DiffRow]) -> (usize, usize) {
    rows.iter().fold((0, 0), |(added, removed), row| {
        match row.kind {
            DiffRowKind::Add => (added + 1, removed),
            DiffRowKind::Remove => (added, removed + 1),
            _ => (added, removed),
        }
    })
}

/// Parses the `Exit Status:`/`--- STDOUT ---`/`--- STDERR ---` envelope
/// `run_command` returns, tolerating missing or reordered sections.
fn parse_command_output(detail: &str) -> CommandOutput {
    let Some(rest) = detail.trim_start().strip_prefix("Exit Status:") else {
        return CommandOutput {
            status: None,
            stdout: detail.to_string(),
            stderr: String::new(),
        };
    };
    let (status, rest) = rest
        .split_once('\n')
        .map(|(status, rest)| (status.trim(), rest))
        .unwrap_or((rest.trim(), ""));
    let (stdout, stderr) = match rest.split_once("--- STDERR ---") {
        Some((stdout, stderr)) => (stdout, stderr),
        None => (rest, ""),
    };
    let stdout = stdout.trim_start_matches("--- STDOUT ---");
    CommandOutput {
        status: Some(status.to_string()),
        stdout: stdout.trim_matches('\n').to_string(),
        stderr: stderr.trim_matches('\n').to_string(),
    }
}

fn command_detail(activity: &ToolActivityInfo) -> CommandDetail {
    let mut detail = CommandDetail::default();
    if let Some(args) = args_json(&activity.arguments) {
        detail.command = args_str(
            &args,
            &["command", "CommandLine", "cmd", "shell_command", "input"],
        )
        .map(str::to_owned);
        detail.cwd = args_str(&args, &["cwd", "workdir", "working_dir"]).map(str::to_owned);
    }
    // While the call is in flight `detail` still holds the raw arguments;
    // surface that as a pending body instead of dumping JSON into the card.
    let body = activity.detail.trim();
    let body_is_args = body.is_empty() || body.starts_with('{');
    detail.pending = activity.category == "Working" || (detail.command.is_some() && body_is_args);
    if detail.pending && body_is_args {
        return detail;
    }
    detail.output = parse_command_output(&activity.detail);
    detail
}

fn exit_status_label(status: &str) -> Option<(String, bool)> {
    let code = status
        .rsplit(|c: char| !c.is_ascii_digit() && c != '-')
        .find(|tail| !tail.is_empty())
        .and_then(|tail| tail.parse::<i32>().ok())?;
    Some((format!("exit {code}"), code == 0))
}

fn card_container(theme: &gpui_component::theme::ThemeColor) -> Div {
    div()
        .w_full()
        .min_w_0()
        .rounded_lg()
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .bg(theme.title_bar)
        .overflow_hidden()
}

fn card_header(theme: &gpui_component::theme::ThemeColor) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .px_3()
        .py_1p5()
        .bg(theme.background.opacity(0.35))
        .border_b_1()
        .border_color(theme.border.opacity(0.3))
}

fn card_body(theme: &gpui_component::theme::ThemeColor, id: &str) -> Scrollable<Div> {
    div()
        .w_full()
        .p_2p5()
        .max_h(rems(15.0))
        .font_family("monospace")
        .text_xs()
        .overflow_scrollbar()
        .id(SharedString::from(format!("tool-detail-scroll-{id}")))
        .text_color(theme.foreground)
}

/// Renders a command activity as a small terminal: a `$` prompt line carrying
/// the command (and `cwd` when reported) plus its stdout/stderr, with an exit
/// status badge once the result lands.
fn render_command_card(
    activity: &ToolActivityInfo,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme().colors;
    let detail = command_detail(activity);
    let is_error = activity.category == "Error";

    let command_text = detail
        .command
        .clone()
        .or_else(|| {
            let args = args_json(&activity.detail)?;
            args_str(&args, &["command", "CommandLine", "cmd"]).map(str::to_owned)
        })
        .unwrap_or_else(|| activity.display_summary.clone());

    let mut header = card_header(&theme)
        .child(
            div()
                .flex_none()
                .font_family("monospace")
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.primary)
                .child("$"),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .font_family("monospace")
                .text_xs()
                .text_color(theme.foreground)
                .child(command_text),
        );
    if let Some(cwd) = detail.cwd.clone() {
        header = header.child(
            div()
                .id(SharedString::from(format!("tool-cwd-{}", activity.id)))
                .flex_none()
                .truncate()
                .text_xs()
                .text_color(theme.muted_foreground)
                .tooltip({
                    let cwd = cwd.clone();
                    move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(cwd.clone()).build(window, cx)
                    }
                })
                .child(format!("in {cwd}")),
        );
    }
    if detail.pending {
        header = header.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Running…"),
        );
    } else if let Some((label, ok)) = detail
        .output
        .status
        .as_deref()
        .and_then(exit_status_label)
    {
        header = header.child(
            Tag::new()
                .child(label)
                .with_variant(TagVariant::Secondary)
                .small()
                .text_color(if is_error || !ok {
                    theme.danger
                } else {
                    theme.success
                }),
        );
    }

    let mut body = card_body(&theme, &activity.id).flex().flex_col();
    if detail.pending {
        body = body.child(
            div()
                .text_color(theme.muted_foreground)
                .child("Waiting for output…"),
        );
    } else {
        let stdout = detail.output.stdout.trim_end_matches('\n');
        let stderr = detail.output.stderr.trim_end_matches('\n');
        if stdout.is_empty() && stderr.is_empty() {
            body = body.child(
                div()
                    .text_color(theme.muted_foreground)
                    .child("(no output)"),
            );
        } else {
            if !stdout.is_empty() {
                body = body.child(
                    div()
                        .w_full()
                        .whitespace_nowrap()
                        .text_color(theme.foreground)
                        .child(stdout.to_string()),
                );
            }
            if !stderr.is_empty() {
                body = body.child(
                    div()
                        .w_full()
                        .whitespace_nowrap()
                        .text_color(if is_error {
                            theme.danger
                        } else {
                            theme.warning
                        })
                        .child(stderr.to_string()),
                );
            }
        }
    }

    card_container(&theme)
        .child(header)
        .child(body)
        .into_any_element()
}

fn diff_row_element(row: &DiffRow, theme: &gpui_component::theme::ThemeColor) -> AnyElement {
    match row.kind {
        DiffRowKind::FileHeader => div()
            .w_full()
            .px_2p5()
            .py_1()
            .bg(theme.muted.opacity(0.25))
            .border_t_1()
            .border_color(theme.border.opacity(0.3))
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.muted_foreground)
            .whitespace_nowrap()
            .child(row.text.clone())
            .into_any_element(),
        DiffRowKind::Hunk => div()
            .w_full()
            .px_2p5()
            .py_0p5()
            .bg(theme.info.opacity(0.10))
            .text_color(theme.info)
            .whitespace_nowrap()
            .child(row.text.clone())
            .into_any_element(),
        _ => {
            let (marker, tint, text_color) = match row.kind {
                DiffRowKind::Add => ("+", theme.success, theme.success),
                DiffRowKind::Remove => ("−", theme.danger, theme.danger),
                _ => (" ", theme.border, theme.muted_foreground),
            };
            let number = row.new_no.or(row.old_no);
            div()
                .w_full()
                .flex()
                .items_start()
                .bg(tint.opacity(if row.kind == DiffRowKind::Context {
                    0.0
                } else {
                    0.10
                }))
                .whitespace_nowrap()
                .child(
                    div()
                        .w(rems(2.0))
                        .flex_none()
                        .pr_2()
                        .text_right()
                        .text_color(theme.muted_foreground.opacity(0.7))
                        .child(number.map(|no| no.to_string()).unwrap_or_default()),
                )
                .child(
                    div()
                        .w(rems(1.0))
                        .flex_none()
                        .text_center()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(text_color)
                        .child(marker),
                )
                .child(
                    div()
                        .min_w_0()
                        .pr_2()
                        .text_color(if row.kind == DiffRowKind::Context {
                            theme.muted_foreground
                        } else {
                            theme.foreground
                        })
                        .child(row.text.clone()),
                )
                .into_any_element()
        }
    }
}

/// Renders an edit/write activity as a small code card: a file-path header
/// with add/remove counts, then the unified diff (or the pending change
/// synthesized from the call's arguments) with tinted +/- rows.
fn render_diff_card(
    activity: &ToolActivityInfo,
    model: &Entity<AppState>,
    cx: &mut App,
) -> Option<AnyElement> {
    let rows = diff_rows(activity);
    if rows.is_empty() {
        return None;
    }
    let theme = cx.theme().colors;
    let path = args_json(&activity.arguments).and_then(|args| args_path(&args));
    let (added, removed) = diff_stats(&rows);

    let mut header = card_header(&theme)
        .child(
            Icon::new(IconName::File)
                .xsmall()
                .flex_none()
                .text_color(theme.muted_foreground),
        )
        .child(
            div()
                .id(SharedString::from(format!("tool-path-{}", activity.id)))
                .min_w_0()
                .flex_1()
                .truncate()
                .text_xs()
                .text_color(theme.foreground)
                .when_some(path.clone(), |el, path| {
                    el.tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(path.clone()).build(window, cx)
                    })
                })
                .child(
                    path.clone()
                        .unwrap_or_else(|| activity.display_summary.clone()),
                ),
        );
    if added + removed > 0 {
        header = header.child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1p5()
                .text_xs()
                .font_family("monospace")
                .child(
                    div()
                        .text_color(theme.success)
                        .child(format!("+{added}")),
                )
                .child(
                    div()
                        .text_color(theme.danger)
                        .child(format!("−{removed}")),
                ),
        );
    }
    if let Some(path) = path {
        let model = model.clone();
        let activity_id = activity.id.clone();
        header = header.child(
            Button::new(SharedString::from(format!("diff-open-{activity_id}")))
                .icon(IconName::ExternalLink)
                .accessibility_label("Open file in central editor")
                .xsmall()
                .ghost()
                .tooltip("Open file in central editor")
                .on_click(move |_event, _window, cx| {
                    let path = path.clone();
                    model.update(cx, |state, cx| {
                        controller::dispatch(state, AppAction::OpenFileInEditor(path));
                        cx.notify();
                    });
                }),
        );
    }

    let body = card_body(&theme, &activity.id)
        .flex()
        .flex_col()
        .children(rows.iter().map(|row| diff_row_element(row, &theme)));

    Some(
        card_container(&theme)
            .child(header)
            .child(body)
            .into_any_element(),
    )
}

/// Whether the rich detail card can render for this activity even when
/// `detail` is still empty — used to decide if the row is expandable.
pub(crate) fn expandable(activity: &ToolActivityInfo) -> bool {
    if is_command_tool(&activity.title) {
        return !activity.arguments.trim().is_empty();
    }
    is_edit_tool(&activity.title) && !activity.arguments.trim().is_empty()
}

/// Purpose-built detail for known tool kinds; `None` keeps the generic
/// detail box for everything else.
pub(crate) fn render_activity_detail_card(
    activity: &ToolActivityInfo,
    model: &Entity<AppState>,
    cx: &mut App,
) -> Option<AnyElement> {
    if is_command_tool(&activity.title) {
        return Some(render_command_card(activity, cx));
    }
    if is_edit_tool(&activity.title) {
        return render_diff_card(activity, model, cx);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        command_detail, diff_rows, diff_stats, exit_status_label, parse_command_output,
        parse_unified_diff, DiffRow, DiffRowKind,
    };
    use threadlane_ui_state::ToolActivityInfo;

    fn activity(title: &str, arguments: &str, detail: &str, category: &str) -> ToolActivityInfo {
        ToolActivityInfo {
            id: "tool-1".into(),
            category: category.into(),
            title: title.into(),
            display_summary: String::new(),
            detail: detail.into(),
            arguments: arguments.into(),
            is_expanded: true,
        }
    }

    #[test]
    fn command_output_parses_status_and_streams() {
        let output = parse_command_output(
            "Exit Status: exit status: 0\n--- STDOUT ---\nhello\n--- STDERR ---\nwarn me\n",
        );
        assert_eq!(output.status.as_deref(), Some("exit status: 0"));
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.stderr, "warn me");
        assert_eq!(
            exit_status_label(output.status.as_deref().unwrap()),
            Some(("exit 0".to_string(), true))
        );
    }

    #[test]
    fn command_output_without_envelope_is_stdout() {
        let output = parse_command_output("plain output\nsecond line");
        assert_eq!(output.status, None);
        assert_eq!(output.stdout, "plain output\nsecond line");
    }

    #[test]
    fn unified_diff_tracks_line_numbers() {
        let detail = "Successfully applied 1 hashline edit(s) to 'src/lib.rs'\n\nDiff:\n@@ -2,2 +2,2 @@\n fn a() {}\n-old\n+new\n\nUpdated Line Hashes:\n2:a3f| fn a() {}\n";
        let rows = parse_unified_diff(detail);
        assert_eq!(
            rows,
            vec![
                DiffRow::marker(DiffRowKind::Hunk, "@@ -2,2 +2,2 @@"),
                DiffRow::changed(DiffRowKind::Context, "fn a() {}", 2),
                DiffRow::changed(DiffRowKind::Remove, "old", 3),
                DiffRow::changed(DiffRowKind::Add, "new", 3),
            ]
        );
    }

    #[test]
    fn multi_file_diff_emits_file_headers() {
        let detail = "Successfully committed 2 files atomically.\n\nsrc/a.rs\nDiff:\n@@ -1,1 +1,1 @@\n-a\n+b\nUpdated Line Hashes:\n1:x| b\n\nsrc/b.rs\nDiff:\n@@ -5,1 +5,1 @@\n-c\n+d\nUpdated Line Hashes:\n5:y| d\n";
        let rows = parse_unified_diff(detail);
        let headers: Vec<_> = rows
            .iter()
            .filter(|row| row.kind == DiffRowKind::FileHeader)
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(headers, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(
            diff_stats(&rows),
            (2, 2)
        );
    }

    #[test]
    fn write_file_args_synthesize_all_added_rows() {
        let act = activity(
            "write_file",
            r#"{"path":"src/new.rs","content":"line one\nline two"}"#,
            "Successfully wrote 18 bytes to 'src/new.rs'",
            "Completed",
        );
        let rows = diff_rows(&act);
        assert_eq!(
            rows,
            vec![
                DiffRow::changed(DiffRowKind::Add, "line one", 1),
                DiffRow::changed(DiffRowKind::Add, "line two", 2),
            ]
        );
    }

    #[test]
    fn hashline_args_synthesize_pending_diff() {
        let act = activity(
            "edit_file_hashline",
            r#"{"path":"src/lib.rs","edits":[{"start_anchor":"12:a3f","action":"replace","new_content":"fn b() {}"},{"start_anchor":"20:q9z","end_anchor":"22:k2d","action":"delete"}]}"#,
            "",
            "Working",
        );
        let rows = diff_rows(&act);
        assert!(rows
            .iter()
            .any(|row| row.kind == DiffRowKind::Add && row.text == "fn b() {}"));
        assert!(rows
            .iter()
            .any(|row| row.kind == DiffRowKind::Remove && row.text.contains("20–22")));
    }

    #[test]
    fn command_detail_detects_pending_and_parses_result() {
        let pending = activity(
            "run_command",
            r#"{"command":"cargo check","cwd":"crates"}"#,
            r#"{"command":"cargo check","cwd":"crates"}"#,
            "Working",
        );
        let detail = command_detail(&pending);
        assert!(detail.pending);
        assert_eq!(detail.command.as_deref(), Some("cargo check"));
        assert_eq!(detail.cwd.as_deref(), Some("crates"));

        let done = activity(
            "run_command",
            r#"{"command":"cargo check"}"#,
            "Exit Status: exit status: 1\n--- STDOUT ---\n\n--- STDERR ---\nerror: failed\n",
            "Error",
        );
        let detail = command_detail(&done);
        assert!(!detail.pending);
        assert_eq!(detail.output.stderr, "error: failed");
        assert_eq!(
            exit_status_label(detail.output.status.as_deref().unwrap()),
            Some(("exit 1".to_string(), false))
        );
    }
}

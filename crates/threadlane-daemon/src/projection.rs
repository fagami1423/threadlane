use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use threadlane_protocol::{AgentMessage, TokenUsage};
use threadlane_runtime::harness::{tool_activity_display_summary, JsonlStore, SessionStore};

use crate::types::{
    ChatMessageInfo, ContextWindowInfo, MessageRole, SessionMetricsInfo, SessionProjectionResult,
    SubagentActivityInfo, SubagentActivityStatus, ToolActivityInfo, TrajectoryDiagnostics,
    TrajectoryEntry,
};

pub fn load_session_messages(session_file: &Path) -> Vec<ChatMessageInfo> {
    compute_session_messages(session_file).unwrap_or_default()
}

pub fn compute_session_messages(session_file: &Path) -> Result<Vec<ChatMessageInfo>, String> {
    use threadlane_runtime::harness::{read_transcript_page, TranscriptItem};

    // The durable pager is the single transcript source, but exhaust it here:
    // GPUI state continues to expose complete chronological history.
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let page =
            read_transcript_page(session_file, cursor, 40).map_err(|error| error.to_string())?;
        let has_older = page.has_older;
        cursor = page.next_cursor;
        pages.push(page.items);
        if !has_older {
            break;
        }
    }
    pages.reverse();
    let items = pages.into_iter().flatten().collect::<Vec<_>>();
    let mut rows = Vec::new();
    let mut messages = Vec::new();
    let mut segment_start = 0usize;
    let flush = |messages: &mut Vec<AgentMessage>, rows: &mut Vec<ChatMessageInfo>, start| {
        for (index, mut row) in project_agent_messages(std::mem::take(messages))
            .into_iter()
            .enumerate()
        {
            row.id = format!("history-{start}-{index}-{}", row.id);
            rows.push(row);
        }
    };
    for (item_index, item) in items.into_iter().enumerate() {
        match item {
            TranscriptItem::Message(message) => {
                if messages.is_empty() {
                    segment_start = item_index;
                }
                messages.push(message);
            }
            TranscriptItem::ContextCompacted(marker) => {
                flush(&mut messages, &mut rows, segment_start);
                rows.push(ChatMessageInfo {
                    id: format!("history-context-{}", marker.seq),
                    role: MessageRole::ContextMarker,
                    content: format!(
                        "Context compacted · {} → {}",
                        format_context_marker_tokens(marker.pre_tokens),
                        format_context_marker_tokens(marker.post_tokens),
                    ),
                    tool_activities: Vec::new(),
                    streaming: false,
                    reasoning_content: None,
                    reasoning_expanded: false,
                });
            }
        }
    }
    flush(&mut messages, &mut rows, segment_start);
    Ok(rows)
}

/// Opens a session JSONL once and builds every UI projection required after hydration.
pub fn compute_full_session_projection(
    session_file: &Path,
) -> Result<SessionProjectionResult, String> {
    let store = JsonlStore::open_read_only(session_file).map_err(|error| error.to_string())?;
    let diagnostics = threadlane_runtime::harness::project_session_diagnostics(&store, "main")
        .map_err(|error| error.to_string())?;
    let (trajectory, metrics, token_usage, context_window) =
        project_trajectory_from_store(&store);
    let subagents = project_subagents_from_store(&store);
    Ok(SessionProjectionResult {
        run_timing: project_run_timing(&store),
        plan: store.plan(),
        trajectory,
        subagents,
        diagnostics,
        metrics,
        token_usage,
        context_window,
    })
}

pub fn project_run_timing(store: &impl SessionStore) -> Option<crate::types::RunTiming> {
    use threadlane_runtime::harness::{AbortObservation, OperationIntent, Record};
    let (id, start_seq, started_at_ms) =
        store
            .records()
            .iter()
            .rev()
            .find_map(|record| match record {
                Record::OperationStarted {
                    id,
                    seq,
                    lane,
                    intent: OperationIntent::Run,
                    wall_time_ms,
                    ..
                } if lane == "main" => Some((id, *seq, *wall_time_ms)),
                _ => None,
            })?;
    let finish = store
        .records()
        .iter()
        .rev()
        .find_map(|record| match record {
            Record::OperationFinished {
                run_id,
                lane,
                seq,
                wall_time_ms,
                ..
            } if lane == "main" && run_id == id => Some((*seq, *wall_time_ms)),
            _ => None,
        });
    // Abort acknowledgement precedes reconciliation, which may happen only
    // when the session is reopened. Do not count that intervening idle time.
    let abort = store.records().iter().find_map(|record| match record {
        Record::AbortObserved {
            run_id,
            lane,
            seq,
            wall_time_ms,
            observation: AbortObservation::SignalSent,
            acknowledged: true,
            ..
        } if lane == "main" && run_id == id => Some((*seq, *wall_time_ms)),
        _ => None,
    });
    let terminal = abort.into_iter().chain(finish).min_by_key(|(seq, _)| *seq);
    Some(crate::types::RunTiming {
        start_seq,
        source_seq: finish
            .into_iter()
            .chain(abort)
            .map(|(seq, _)| seq)
            .max()
            .unwrap_or(start_seq),
        started_at_ms,
        finished_at_ms: terminal.and_then(|(_, time)| time),
        finished: terminal.is_some(),
        suppressed: false,
    })
}

pub fn project_subagents_from_store(store: &impl SessionStore) -> Vec<SubagentActivityInfo> {
    use threadlane_runtime::harness::{Record, SubagentLifecyclePhase};

    let mut rows = Vec::new();
    for lane in store.lanes().into_iter().filter(|lane| lane != "main") {
        let has_subagent_lifecycle = store.records().iter().any(|record| {
            matches!(
                record,
                Record::SubagentLifecycle { subagent_lane, .. }
                    if subagent_lane.as_str() == lane
            )
        });
        let transcript = store.transcript(&lane);
        let has_subagent_marker = transcript.entries.iter().any(|entry| {
            matches!(
                &entry.message,
                AgentMessage::Custom { custom_type, .. } if custom_type == "subagent_lane"
            )
        });
        if !has_subagent_lifecycle && !has_subagent_marker {
            continue;
        }
        let mut run_id = String::new();
        let mut agent = lane.clone();
        let mut task = String::new();
        let mut model = None;
        let mut status = SubagentActivityStatus::Running;
        let mut error = None;
        let mut messages = Vec::new();
        for entry in transcript.entries {
            match entry.message {
                AgentMessage::Custom {
                    custom_type,
                    payload,
                } if custom_type == "subagent_lane" => {
                    run_id = payload
                        .get("run_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    agent = payload
                        .get("agent")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(&agent)
                        .to_owned();
                    task = payload
                        .get("task")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    model = payload
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    error = payload
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    status = match payload.get("status").and_then(serde_json::Value::as_str) {
                        Some("completed") => SubagentActivityStatus::Completed,
                        Some("failed") => SubagentActivityStatus::Failed,
                        _ => SubagentActivityStatus::Running,
                    };
                }
                message => messages.push(message),
            }
        }
        let latest = store
            .records()
            .iter()
            .filter_map(|record| match record {
                Record::SubagentLifecycle {
                    seq,
                    child_run_id,
                    agent_id,
                    subagent_lane,
                    phase,
                    error,
                    ..
                } if subagent_lane.as_str() == lane => Some((
                    *seq,
                    child_run_id.as_str(),
                    agent_id.as_str(),
                    phase,
                    error.as_ref().map(|error| error.as_str()),
                )),
                _ => None,
            })
            .max_by_key(|item| item.0);
        if let Some((_, durable_run_id, durable_agent, phase, durable_error)) = latest {
            run_id = durable_run_id.to_owned();
            if agent == lane {
                agent = durable_agent.to_owned();
            }
            status = match phase {
                SubagentLifecyclePhase::Spawned => SubagentActivityStatus::Queued,
                SubagentLifecyclePhase::Started => SubagentActivityStatus::Running,
                SubagentLifecyclePhase::Completed => SubagentActivityStatus::Completed,
                SubagentLifecyclePhase::Failed => SubagentActivityStatus::Failed,
                SubagentLifecyclePhase::Cancelled => SubagentActivityStatus::Cancelled,
            };
            if durable_error.is_some() {
                error = durable_error.map(str::to_owned);
            }
        }
        if run_id.is_empty() {
            run_id = lane.clone();
        }
        rows.push(SubagentActivityInfo {
            batch_run_id: 0,
            task_index: rows.len(),
            journal_run_id: Some(run_id),
            lane: Some(lane),
            agent,
            task,
            model,
            status,
            messages: project_agent_messages(messages),
            error,
            isolation: None,
        });
    }
    rows
}

pub fn format_context_marker_tokens(tokens: usize) -> String {
    let formatted = crate::catalog::format_tokens(tokens.min(u32::MAX as usize) as u32);
    formatted.replace(".0k", "k").replace(".0M", "M")
}

pub fn project_agent_messages(agent_messages: Vec<AgentMessage>) -> Vec<ChatMessageInfo> {
    threadlane_runtime::harness::project_chat_messages(&agent_messages)
        .into_iter()
        .map(|msg| ChatMessageInfo {
            id: msg.id,
            role: match msg.role {
                threadlane_runtime::harness::UiMessageRole::User => MessageRole::User,
                threadlane_runtime::harness::UiMessageRole::Assistant => MessageRole::Assistant,
                threadlane_runtime::harness::UiMessageRole::System => MessageRole::System,
                threadlane_runtime::harness::UiMessageRole::Error => MessageRole::Error,
            },
            content: msg.content,
            tool_activities: msg
                .tool_activities
                .into_iter()
                .map(|act| {
                    let display_summary = tool_activity_display_summary(&act.summary);
                    ToolActivityInfo {
                        id: act.id,
                        category: act.category,
                        title: act.title,
                        display_summary,
                        detail: act.detail,
                        is_expanded: false,
                    }
                })
                .collect(),
            streaming: false,
            reasoning_content: msg.reasoning_content,
            reasoning_expanded: false,
        })
        .collect()
}

pub fn coding_agent_options(
    work_dir: PathBuf,
    session_file: PathBuf,
    model: String,
    model_roles: threadlane_runtime::ModelRoles,
    browser: threadlane_protocol::browser::BrowserBridge,
) -> threadlane_coding_agent::CodingAgentOptions {
    let (api_key, account_id) = threadlane_coding_agent::credentials::provider_credentials(&model);
    let mut agent_config = threadlane_runtime::AgentConfig::default();
    agent_config.model_roles = model_roles;
    let subagent_settings = threadlane_project::subagent_settings::load(&work_dir);
    agent_config.model_roles.fast = subagent_settings.fast_model;
    agent_config.fast_reasoning_effort = subagent_settings.fast_reasoning_effort;
    agent_config.orchestrator_mode = subagent_settings.orchestrator_mode;

    threadlane_coding_agent::CodingAgentOptions {
        api_key,
        account_id,
        model,
        work_dir,
        session_file: Some(session_file),
        system_prompt: Default::default(),
        agent_config: Some(agent_config),
        coding_config: None,
        browser,
    }
}


/// Projects trajectory entries, token usage, and metrics from an already-open store.
pub fn project_trajectory_from_store(
    store: &JsonlStore,
) -> (
    Vec<TrajectoryEntry>,
    SessionMetricsInfo,
    TokenUsage,
    Option<ContextWindowInfo>,
) {
    let mut trajectory: Vec<TrajectoryEntry> = Vec::new();
    let mut metrics = SessionMetricsInfo::default();
    let mut durable_usage = TokenUsage::default();

    let mut tool_starts =
        HashMap::<(String, String), (String, String, String, serde_json::Value)>::new();
    let mut tool_finishes = HashMap::<String, (String, String, String)>::new();
    let provider_usage_keys = store
        .records()
        .iter()
        .filter_map(|record| match record {
            threadlane_runtime::harness::Record::Usage {
                run_id: Some(run_id),
                attempt: Some(attempt),
                cause: threadlane_runtime::harness::UsageCause::Provider,
                ..
            } => Some((run_id.clone(), *attempt)),
            _ => None,
        })
        .collect::<HashSet<_>>();

    for record in store.records() {
        use threadlane_runtime::harness::Record;
        let entry = match record {
            Record::OperationStarted {
                seq,
                lane,
                id,
                intent,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(id.clone()),
                turn: None,
                request: None,
                category: "Operation".into(),
                summary: format!("{intent:?} started"),
                detail: String::new(),
                lane: Some(lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::OperationFinished {
                seq,
                lane,
                run_id,
                outcome,
                error,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: None,
                request: None,
                category: "Operation".into(),
                summary: format!("Operation {outcome:?}"),
                detail: error.clone().unwrap_or_default(),
                lane: Some(lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::StepAttempt {
                seq,
                lane,
                run_id,
                attempt,
                ..
            } => {
                metrics.turns = metrics.turns.saturating_add(1);
                Some(TrajectoryEntry {
                    seq: Some(*seq),
                    run_id: Some(run_id.clone()),
                    turn: Some(*attempt),
                    request: None,
                    category: "Step".into(),
                    summary: format!("Step {attempt} started"),
                    detail: format!("lane {}", lane.as_str()),
                    lane: Some(lane.clone()),
                    correlation_id: None,
                    diagnostics: TrajectoryDiagnostics::default(),
                })
            }
            Record::RetryScheduled {
                seq,
                lane,
                run_id,
                attempt,
                reason,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: Some(*attempt),
                request: None,
                category: "Retry".into(),
                summary: format!("Retry {attempt} scheduled"),
                detail: reason.clone(),
                lane: Some(lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::RetryConsumed {
                seq,
                lane,
                run_id,
                attempt,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: Some(*attempt),
                request: None,
                category: "Retry".into(),
                summary: format!("Retry {attempt} consumed"),
                detail: String::new(),
                lane: Some(lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::LaneMoved {
                seq,
                lane,
                run_id,
                target_leaf_id,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: None,
                request: None,
                category: "Lane".into(),
                summary: format!("Lane moved to {target_leaf_id}"),
                detail: format!("target: {target_leaf_id}"),
                lane: Some(lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::Usage {
                seq,
                lane,
                run_id,
                attempt,
                cause,
                usage,
                ..
            } => {
                if *cause == threadlane_runtime::harness::UsageCause::Provider {
                    metrics.accumulate_usage(usage);
                    durable_usage.accumulate(usage);
                }
                Some(TrajectoryEntry {
                    seq: Some(*seq),
                    run_id: run_id.clone(),
                    turn: *attempt,
                    request: None,
                    category: "Usage".into(),
                    summary: format!("Usage: {} total tokens ({cause:?})", usage.total_tokens),
                    detail: format!(
                        "input: {}, output: {}, cache read: {}, cache write: {}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_read_tokens,
                        usage.cache_write_tokens
                    ),
                    lane: Some(lane.clone()),
                    correlation_id: None,
                    diagnostics: TrajectoryDiagnostics::default(),
                })
            }
            Record::RunContextCaptured {
                seq,
                lane,
                run_id,
                model,
                provider,
                reasoning_effort,
                prompt_cache_enabled,
                work_dir,
                system_prompt,
                tool_schema_sha256,
                enabled_tool_names,
                ..
            } => {
                let prompt_text = match system_prompt {
                    threadlane_runtime::harness::PromptSnapshot::Full { sha256, content } => {
                        format!(
                            "### System Prompt (SHA256 `{}`)\n\n```markdown\n{}\n```",
                            sha256.as_str(),
                            content.as_str()
                        )
                    }
                    threadlane_runtime::harness::PromptSnapshot::Redacted {
                        sha256,
                        byte_len,
                        reason,
                    } => format!(
                        "### System Prompt (Redacted)\n\n- Size: {byte_len} bytes\n- SHA256: `{}`\n- Reason: {}",
                        sha256.as_str(),
                        reason.as_str()
                    ),
                };
                let tools_list = if enabled_tool_names.is_empty() {
                    "None".to_string()
                } else {
                    enabled_tool_names
                        .iter()
                        .map(|t| format!("`{}`", t.as_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let detail = format!(
                    "**Model**: `{}`\n\n**Provider**: `{}`\n\n**Reasoning Effort**: `{:?}`\n\n**Prompt Cache**: `{}`\n\n**Work Dir**: `{}`\n\n**Enabled Tools ({})**:\n{}\n\n**Tool Schema SHA256**: `{}`\n\n{}",
                    model.as_str(),
                    provider.as_str(),
                    reasoning_effort,
                    prompt_cache_enabled,
                    work_dir.as_str(),
                    enabled_tool_names.len(),
                    tools_list,
                    tool_schema_sha256.as_str(),
                    prompt_text
                );
                Some(TrajectoryEntry {
                    seq: Some(*seq),
                    run_id: Some(run_id.clone()),
                    turn: None,
                    request: None,
                    category: "Context".into(),
                    summary: format!(
                        "{} via {} ({reasoning_effort:?})",
                        model.as_str(),
                        provider.as_str()
                    ),
                    detail,
                    lane: Some(lane.clone()),
                    correlation_id: None,
                    diagnostics: TrajectoryDiagnostics {
                        model_visible: true,
                        source: Some("Run context captured".into()),
                        ..Default::default()
                    },
                })
            }
            Record::ContextManifestCaptured {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                total_estimated_tokens,
                items,
                ..
            } => {
                let items_summary = items
                    .iter()
                    .map(|item| {
                        let digest_prefix = if item.digest_sha256.as_str().len() >= 8 {
                            &item.digest_sha256.as_str()[..8]
                        } else {
                            item.digest_sha256.as_str()
                        };
                        format!(
                            "- [{:?}] `{}` (~{} tokens, sha256: `{}`)",
                            item.source,
                            item.role.as_str(),
                            item.token_estimate,
                            digest_prefix,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(TrajectoryEntry {
                    seq: Some(*seq),
                    run_id: Some(run_id.clone()),
                    turn: Some(*attempt),
                    request: None,
                    category: "Context Manifest".into(),
                    summary: format!(
                        "Context manifest ({} items, ~{} tokens)",
                        items.len(),
                        total_estimated_tokens.unwrap_or(0)
                    ),
                    detail: format!(
                        "**Request ID**: `{}`\n\n**Turn / Attempt**: `{}`\n\n**Context Items ({} total)**:\n{}",
                        request_id.as_str(),
                        attempt,
                        items.len(),
                        items_summary
                    ),
                    lane: Some(lane.clone()),
                    correlation_id: Some(request_id.as_str().to_owned()),
                    diagnostics: TrajectoryDiagnostics {
                        source: Some("Context manifest captured".into()),
                        raw: Some(format!(
                            "items={}; total_tokens={:?}; request_id={}",
                            items.len(),
                            total_estimated_tokens,
                            request_id.as_str()
                        )),
                        items_count: Some(items.len()),
                        token_estimate: *total_estimated_tokens,
                        model_visible: true,
                        ..Default::default()
                    },
                })
            }
            Record::ProviderRequestStarted {
                seq,
                lane,
                run_id,
                attempt,
                provider,
                model,
                request_id,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: Some(*attempt),
                request: None,
                category: "Provider".into(),
                summary: format!("{} request started", provider.as_str()),
                detail: format!(
                    "**Provider**: `{}`\n\n**Model**: `{}`\n\n**Turn / Attempt**: `{}`\n\n**Request ID**: `{}`",
                    provider.as_str(),
                    model.as_str(),
                    attempt,
                    request_id.as_ref().map(|r| r.as_str()).unwrap_or("none")
                ),
                lane: Some(lane.clone()),
                correlation_id: request_id.as_ref().map(|id| id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics {
                    status: Some("started".into()),
                    source: Some("Provider request lifecycle".into()),
                    raw: Some(format!(
                        "provider={} model={} request_id={}",
                        provider.as_str(),
                        model.as_str(),
                        request_id.as_ref().map(|id| id.as_str()).unwrap_or("none")
                    )),
                    ..Default::default()
                },
            }),
            Record::ProviderRequestFinished {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                outcome,
                error,
                duration_ms,
                usage,
                ..
            } => {
                if !provider_usage_keys.contains(&(run_id.clone(), *attempt)) {
                    if let Some(usage) = usage {
                        metrics.accumulate_usage(usage);
                        durable_usage.accumulate(usage);
                    }
                }
                let mut detail_lines = Vec::new();
                detail_lines.push(format!("**Outcome**: `{:?}`", outcome));
                if let Some(duration) = duration_ms {
                    detail_lines.push(format!("**Duration**: {duration} ms"));
                }
                if let Some(req_id) = request_id {
                    detail_lines.push(format!("**Request ID**: `{}`", req_id.as_str()));
                }
                if let Some(usage) = usage {
                    detail_lines.push(format!(
                        "**Tokens**: input={}, output={}, total={}",
                        usage.input_tokens, usage.output_tokens, usage.total_tokens
                    ));
                }
                if let Some(err) = error.as_ref() {
                    detail_lines.push(format!("**Category**: `{:?}`", err.category));
                    detail_lines.push(format!("**Retryable**: `{}`", err.retryable));
                    if let Some(code) = err.code.as_ref() {
                        detail_lines
                            .push(format!("**Error Details**:\n```\n{}\n```", code.as_str()));
                    }
                }
                Some(TrajectoryEntry {
                    seq: Some(*seq),
                    run_id: Some(run_id.clone()),
                    turn: Some(*attempt),
                    request: None,
                    category: "Provider".into(),
                    summary: format!("Provider request {outcome:?}"),
                    detail: detail_lines.join("\n\n"),
                    lane: Some(lane.clone()),
                    correlation_id: request_id.as_ref().map(|id| id.as_str().to_owned()),
                    diagnostics: TrajectoryDiagnostics {
                        status: Some(format!("{outcome:?}")),
                        duration_ms: *duration_ms,
                        source: Some("Provider request lifecycle".into()),
                        raw: Some(format!(
                            "outcome={outcome:?}; request_id={}",
                            request_id.as_ref().map(|id| id.as_str()).unwrap_or("none")
                        )),
                        ..Default::default()
                    },
                })
            }
            Record::ProviderResponseAttached {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                entry_id,
                reasoning_entry_id,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: Some(*attempt),
                request: None,
                category: "Provider".into(),
                summary: "Provider response attached".into(),
                detail: format!(
                    "entry {}{}",
                    entry_id,
                    reasoning_entry_id
                        .as_deref()
                        .map(|id| format!(", thinking {id}"))
                        .unwrap_or_default()
                ),
                lane: Some(lane.clone()),
                correlation_id: request_id.as_ref().map(|id| id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::PermissionRequested {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                capability,
                scopes,
                detail_sha256,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: run_id.clone(),
                turn: *attempt,
                request: None,
                category: "Permission".into(),
                summary: format!("{} permission requested", capability.as_str()),
                detail: format!(
                    "scopes {scopes:?}; detail sha256 {}",
                    detail_sha256.as_str()
                ),
                lane: Some(lane.clone()),
                correlation_id: Some(request_id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::PermissionResolved {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                decision,
                source,
                remembered,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: run_id.clone(),
                turn: *attempt,
                request: None,
                category: "Permission".into(),
                summary: format!("Permission {decision:?}"),
                detail: format!("source {source:?}; remembered {remembered}"),
                lane: Some(lane.clone()),
                correlation_id: Some(request_id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::ToolStarted {
                lane,
                run_id,
                assistant_entry_id,
                tool_call_id,
                tool_name,
                effective_args,
                ..
            } => {
                tool_starts.insert(
                    (assistant_entry_id.clone(), tool_call_id.clone()),
                    (
                        run_id.clone(),
                        lane.clone(),
                        tool_name.clone(),
                        effective_args.clone(),
                    ),
                );
                None
            }
            Record::ToolFinished {
                lane,
                run_id,
                tool_call_id,
                result_entry_id,
                ..
            } => {
                tool_finishes.insert(
                    result_entry_id.clone(),
                    (run_id.clone(), lane.clone(), tool_call_id.clone()),
                );
                None
            }
            Record::ToolExecutionObserved {
                seq,
                lane,
                run_id,
                attempt,
                tool_call_id,
                tool_name,
                executor_kind,
                phase,
                duration_ms,
                outcome,
                cancelled,
                exit_code,
                output_bytes,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: *attempt,
                request: None,
                category: "Tool runtime".into(),
                summary: format!("{} {phase:?}", tool_name.as_str()),
                detail: format!(
                    "executor {}; outcome {outcome:?}; duration {duration_ms:?} ms; cancelled {cancelled}",
                    executor_kind.as_str()
                ),
                lane: Some(lane.clone()),
                correlation_id: Some(tool_call_id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics {
                    duration_ms: *duration_ms,
                    status: outcome
                        .as_ref()
                        .map(|o| format!("{o:?}"))
                        .or_else(|| Some(format!("{phase:?}"))),
                    exit_code: *exit_code,
                    output_bytes: *output_bytes,
                    source: Some(format!("Tool Executor ({})", executor_kind.as_str())),
                    raw: Some(format!(
                        "tool={}; phase={:?}; outcome={:?}; duration={:?}ms; exit_code={:?}; output_bytes={:?}",
                        tool_name.as_str(),
                        phase,
                        outcome,
                        duration_ms,
                        exit_code,
                        output_bytes
                    )),
                    ..Default::default()
                },
            }),
            Record::AbortObserved {
                seq,
                lane,
                run_id,
                attempt,
                observation,
                initiator,
                target,
                acknowledged,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: *attempt,
                request: None,
                category: "Cancellation".into(),
                summary: format!("{observation:?} for {target:?}"),
                detail: format!("initiator {initiator:?}; acknowledged {acknowledged}"),
                lane: Some(lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::SubagentLifecycle {
                seq,
                lane,
                run_id,
                attempt,
                child_run_id,
                agent_id,
                subagent_lane,
                phase,
                error,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: run_id.clone(),
                turn: *attempt,
                request: None,
                category: "Subagent".into(),
                summary: format!("{} {phase:?}", agent_id.as_str()),
                detail: format!(
                    "child {}; lane {}{}",
                    child_run_id.as_str(),
                    subagent_lane.as_str(),
                    error
                        .as_ref()
                        .map(|error| format!("; {}", error.as_str()))
                        .unwrap_or_default()
                ),
                lane: Some(lane.clone()),
                correlation_id: Some(child_run_id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            Record::StreamCheckpoint {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                text,
                reasoning,
                checkpoint_index,
                byte_count,
                fingerprint,
                ..
            } => Some(TrajectoryEntry {
                seq: Some(*seq),
                run_id: Some(run_id.clone()),
                turn: *attempt,
                request: None,
                category: "Incomplete stream".into(),
                summary: format!("Incomplete stream checkpoint {checkpoint_index}"),
                detail: format!(
                    "{byte_count} bytes; text {} bytes; reasoning {} bytes; sha256 {}",
                    text.as_ref().map_or(0, |text| text.as_str().len()),
                    reasoning
                        .as_ref()
                        .map_or(0, |reasoning| reasoning.as_str().len()),
                    fingerprint.as_str()
                ),
                lane: Some(lane.clone()),
                correlation_id: Some(request_id.as_str().to_owned()),
                diagnostics: TrajectoryDiagnostics::default(),
            }),
            _ => None,
        };
        if let Some(entry) = entry {
            trajectory.push(entry);
        }
    }

    let mut request_number = 0u32;
    for entry in store.entries() {
        if matches!(
            &entry.message,
            AgentMessage::User { .. } | AgentMessage::UserWithImages { .. }
        ) {
            request_number = request_number.saturating_add(1);
        }
        let request = (request_number > 0).then_some(request_number);
        if let AgentMessage::Assistant {
            tool_calls: Some(calls),
            ..
        } = &entry.message
        {
            for call in calls {
                metrics.tool_calls = metrics.tool_calls.saturating_add(1);
                let durable = tool_starts.get(&(entry.id.clone(), call.id.clone()));
                let run_id = durable.map(|(run_id, _, _, _)| run_id.clone());
                let lane = durable
                    .map(|(_, lane, _, _)| lane.clone())
                    .unwrap_or_else(|| entry.lane.clone());
                let name = durable
                    .map(|(_, _, name, _)| name.as_str())
                    .unwrap_or(call.function.name.as_str());
                let detail = durable
                    .map(|(_, _, _, args)| args.to_string())
                    .unwrap_or_else(|| call.function.arguments.clone());
                trajectory.push(TrajectoryEntry {
                    seq: Some(entry.seq),
                    run_id,
                    turn: None,
                    request,
                    category: "Tool".into(),
                    summary: format!("{name} running"),
                    detail,
                    lane: Some(lane),
                    correlation_id: Some(call.id.clone()),
                    diagnostics: TrajectoryDiagnostics {
                        model_visible: true,
                        source: Some("Assistant tool call".into()),
                        ..Default::default()
                    },
                });
            }
        }
        if let AgentMessage::Tool {
            tool_call_id,
            name,
            content,
            is_error,
            ..
        } = &entry.message
        {
            let durable = tool_finishes.get(&entry.id);
            trajectory.push(TrajectoryEntry {
                seq: Some(entry.seq),
                run_id: durable.map(|(run_id, _, _)| run_id.clone()),
                turn: None,
                request,
                category: "Tool".into(),
                summary: format!("{name} {}", if *is_error { "failed" } else { "finished" }),
                detail: content.clone(),
                lane: Some(
                    durable
                        .map(|(_, lane, _)| lane.clone())
                        .unwrap_or_else(|| entry.lane.clone()),
                ),
                correlation_id: Some(
                    durable
                        .map(|(_, _, call_id)| call_id.clone())
                        .unwrap_or_else(|| tool_call_id.clone()),
                ),
                diagnostics: TrajectoryDiagnostics {
                    model_visible: true,
                    source: Some("Tool result".into()),
                    error_summary: if *is_error {
                        Some("Tool failed".into())
                    } else {
                        None
                    },
                    ..Default::default()
                },
            });
            continue;
        }
        let projected = match &entry.message {
            AgentMessage::User { content } | AgentMessage::UserWithImages { content, .. } => {
                Some((
                    "Input".to_string(),
                    "User input".to_string(),
                    content.clone(),
                ))
            }
            AgentMessage::Assistant {
                content: Some(content),
                ..
            } if !content.trim().is_empty() => Some((
                "Assistant".to_string(),
                "Assistant response".to_string(),
                content.clone(),
            )),
            AgentMessage::Custom {
                custom_type,
                payload,
            } if matches!(
                custom_type.as_str(),
                "thinking" | "goal_round" | "agent_error"
            ) =>
            {
                let (category, summary, detail) = if custom_type == "agent_error" {
                    let err_msg = payload
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("agent error");
                    (
                        "Error".to_string(),
                        "Agent Error".to_string(),
                        format!("### Error Details\n\n```\n{}\n```", err_msg),
                    )
                } else {
                    (
                        "Context".to_string(),
                        custom_type.to_string(),
                        serde_json::to_string_pretty(payload)
                            .unwrap_or_else(|_| payload.to_string()),
                    )
                };
                Some((category, summary, detail))
            }
            _ => None,
        };
        if let Some((category, summary, detail)) = projected {
            trajectory.push(TrajectoryEntry {
                seq: Some(entry.seq),
                run_id: None,
                turn: None,
                request,
                category: category.into(),
                summary: summary.into(),
                detail,
                lane: Some(entry.lane.clone()),
                correlation_id: None,
                diagnostics: TrajectoryDiagnostics {
                    model_visible: true,
                    ..Default::default()
                },
            });
        }
    }

    // Anomaly items from typed trajectory pass
    let typed_traj = threadlane_runtime::harness::project_trajectory(store);
    for anomaly in typed_traj.anomalies {
        trajectory.push(TrajectoryEntry {
            seq: anomaly.related_refs.first().map(|r| r.seq),
            run_id: None,
            turn: None,
            request: None,
            category: "Anomaly".into(),
            summary: anomaly.summary.clone(),
            detail: anomaly.description.clone(),
            lane: Some("main".into()),
            correlation_id: None,
            diagnostics: TrajectoryDiagnostics {
                status: Some("Warning".into()),
                model_visible: false,
                source: Some("Diagnostic Engine".into()),
                is_anomaly: true,
                ..Default::default()
            },
        });
    }

    trajectory.sort_by_key(|entry| entry.seq.unwrap_or(u64::MAX));

    if request_number > 0 {
        for entry in &mut trajectory {
            if entry.request.is_none() && entry.seq.is_some() {
                entry.request = Some(1);
            }
        }
    }

    let context_window = project_context_window(store);
    (trajectory, metrics, durable_usage, context_window)
}

pub fn project_context_window(store: &JsonlStore) -> Option<ContextWindowInfo> {
    use threadlane_runtime::harness::Record;
    let manifest = store
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::ContextManifestCaptured {
                seq,
                lane,
                run_id,
                attempt,
                request_id,
                total_estimated_tokens,
                effective_model,
                context_limit,
                context_limit_is_estimate,
                compaction_generation,
                ..
            } if lane == "main" => Some((
                *seq,
                run_id,
                *attempt,
                request_id.as_str(),
                *total_estimated_tokens,
                effective_model.as_ref().map(|value| value.as_str()),
                *context_limit,
                *context_limit_is_estimate,
                *compaction_generation,
            )),
            _ => None,
        })
        .max_by_key(|value| value.0)?;
    let compaction = store
        .records()
        .iter()
        .filter_map(|record| match record {
            Record::ContextCompacted {
                seq,
                lane,
                timestamp,
                generation,
                effective_model,
                context_limit,
                context_limit_is_estimate,
                post_tokens,
                ..
            } if lane == "main" => Some((
                *generation,
                *seq,
                *timestamp,
                effective_model.as_str(),
                *context_limit,
                *context_limit_is_estimate,
                *post_tokens,
            )),
            _ => None,
        })
        .max_by_key(|value| (value.0, value.1));
    let (
        manifest_seq,
        run_id,
        attempt,
        request_id,
        token_estimate,
        persisted_model,
        persisted_limit,
        persisted_limit_estimate,
        manifest_generation,
    ) = manifest;
    let effective_model = persisted_model
        .map(str::to_owned)
        .or_else(|| {
            store.records().iter().find_map(|record| match record {
                Record::ProviderRequestStarted {
                    run_id: candidate_run,
                    attempt: candidate_attempt,
                    request_id: Some(candidate_request),
                    model,
                    ..
                } if candidate_run == run_id
                    && *candidate_attempt == attempt
                    && candidate_request.as_str() == request_id =>
                {
                    Some(model.as_str().to_owned())
                }
                _ => None,
            })
        })
        .unwrap_or_default();
    let estimating = store.records().iter().any(|record| match record {
        Record::ProviderRequestStarted {
            seq,
            lane,
            run_id: started_run_id,
            attempt: started_attempt,
            request_id: started_request_id,
            ..
        } if lane == "main" && *seq > manifest_seq => {
            started_run_id != run_id
                || *started_attempt != attempt
                || started_request_id.as_ref().map(|value| value.as_str()) != Some(request_id)
        }
        _ => false,
    });
    let mut info = ContextWindowInfo {
        current_tokens: u64::from(token_estimate.unwrap_or_default()),
        context_limit: persisted_limit
            .map(|value| value.min(u64::MAX as usize) as u64)
            .unwrap_or_else(|| {
                u64::from(threadlane_context::model_context_window(&effective_model))
            }),
        context_limit_is_estimate: persisted_limit.is_none() || persisted_limit_estimate,
        effective_model,
        compaction_generation: manifest_generation,
        last_compaction_seq: compaction.map(|value| value.2),
        provisional: false,
        estimating,
    };
    if let Some((generation, _, _, model, limit, estimated, post_tokens)) = compaction {
        if generation > manifest_generation {
            info.current_tokens = post_tokens.min(u64::MAX as usize) as u64;
            info.context_limit = limit.min(u64::MAX as usize) as u64;
            info.context_limit_is_estimate = estimated;
            info.effective_model = model.to_owned();
            info.compaction_generation = generation;
            info.provisional = true;
            info.estimating = false;
        }
    }
    Some(info)
}

use super::*;
use serde::{Deserialize, Serialize};

const FUSION_LANE_CONTRACT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FusionLaneContract {
    version: u32,
    agent: String,
    tools: Option<Vec<String>>,
    system_prompt: PromptSnapshot,
}

impl FusionLaneContract {
    pub(crate) fn restore_into(&self, config: &mut threadlane_skills::agents::AgentDefinition) {
        if let PromptSnapshot::Full { content, .. } = &self.system_prompt {
            config.name = self.agent.clone();
            config.tools = self.tools.clone();
            config.system_prompt = content.as_str().to_owned();
        }
    }
}

fn fusion_lane_contract_key(lane: &str) -> String {
    format!("fusion_lane_contract:{lane}")
}

impl CodingSessionHarness {
    pub(crate) fn capture_fusion_lane_contract(
        &mut self,
        lane: &str,
        agent: &str,
        tools: Option<Vec<String>>,
        system_prompt: &str,
    ) -> Result<(), String> {
        let system_prompt = crate::durable::durable_prompt_snapshot(system_prompt);
        let restorable = matches!(system_prompt, PromptSnapshot::Full { .. });
        let contract = FusionLaneContract {
            version: FUSION_LANE_CONTRACT_VERSION,
            agent: agent.to_owned(),
            tools,
            system_prompt,
        };
        let value = serde_json::to_string(&contract)
            .map_err(|error| format!("Failed to encode Fusion lane contract: {error}"))?;
        self.set_fact(lane, &fusion_lane_contract_key(lane), value)?;
        if restorable {
            Ok(())
        } else {
            Err(format!(
                "Fusion lane contract for {lane} has a redacted system prompt; start a new child after making its prompt restorable"
            ))
        }
    }

    pub(crate) fn load_fusion_lane_contract(
        &mut self,
        lane: &str,
        expected_agent: Option<&str>,
    ) -> Result<Option<FusionLaneContract>, String> {
        self.ensure_fresh()?;
        let facts = self.store.facts();
        let Some(value) = facts.get(&fusion_lane_contract_key(lane)) else {
            return Ok(None);
        };
        let contract = serde_json::from_str::<FusionLaneContract>(value).map_err(|_| {
            format!("Fusion lane contract for {lane} is invalid; start a new child")
        })?;
        if contract.version != FUSION_LANE_CONTRACT_VERSION
            || expected_agent.is_some_and(|agent| contract.agent != agent)
            || contract.agent.is_empty()
        {
            return Err(format!(
                "Fusion lane contract for {lane} does not match this agent; start a new child"
            ));
        }
        if !matches!(contract.system_prompt, PromptSnapshot::Full { .. }) {
            return Err(format!(
                "Fusion lane contract for {lane} has no restorable system prompt; start a new child"
            ));
        }
        Ok(Some(contract))
    }

    pub(crate) fn start_subagent_lane(
        &mut self,
        lane_hint: &str,
        task: &str,
        source_leaf_id: Option<&str>,
    ) -> Result<StartedSubagentLane, SubagentStartError> {
        if self.cancellation.load(Ordering::SeqCst) {
            return Err(SubagentStartError {
                identity: None,
                error: "Subagent start rejected because the parent is cancelling".into(),
            });
        }
        static START_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _start_lock = START_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .map_err(|error| SubagentStartError {
                identity: None,
                error: error.to_string(),
            })?;
        let mut attempt_idx = 0;
        let identity = loop {
            self.ensure_fresh().map_err(|error| SubagentStartError {
                identity: None,
                error: error.to_string(),
            })?;
            let used_ids = self
                .store
                .entries()
                .iter()
                .map(|entry| entry.id.clone())
                .chain(
                    self.store
                        .records()
                        .iter()
                        .flat_map(|record| [record.id().to_owned(), record.lane().to_owned()]),
                )
                .collect::<Vec<_>>();
            let generator = SessionIdGenerator::new(self.store.session_id());
            let base_run_id = generator.next("subagent-run", &used_ids);
            let run_id = if attempt_idx == 0 {
                base_run_id
            } else {
                format!("{base_run_id}-{attempt_idx}")
            };
            let mut lane_ids = used_ids.clone();
            lane_ids.push(run_id.clone());
            let base_lane = generator.next(lane_hint, &lane_ids);
            let lane_name = if attempt_idx == 0 {
                base_lane
            } else {
                format!("{base_lane}-{attempt_idx}")
            };
            let mut identity = SubagentLaneIdentity {
                lane_name: lane_name.clone(),
                run_id: run_id.clone(),
                source_leaf_id: source_leaf_id.map(str::to_owned),
                started_seq: 0,
            };
            if let Err(error) = self.store.start_operation_on_lane(
                &lane_name,
                &run_id,
                source_leaf_id.map(str::to_owned),
                OperationIntent::Run,
            ) {
                let err_str = error.to_string();
                if err_str.contains("DuplicateId") {
                    attempt_idx += 1;
                    continue;
                }
                if source_leaf_id.is_some()
                    && (err_str.contains("source leaf does not exist")
                        || err_str.contains("MissingParent"))
                {
                    if let Err(retry_err) = self.store.start_operation_on_lane(
                        &lane_name,
                        &run_id,
                        None,
                        OperationIntent::Run,
                    ) {
                        if retry_err.to_string().contains("DuplicateId") {
                            attempt_idx += 1;
                            continue;
                        }
                        return Err(SubagentStartError {
                            identity: None,
                            error: retry_err.to_string(),
                        });
                    }
                    identity.source_leaf_id = None;
                } else {
                    return Err(SubagentStartError {
                        identity: None,
                        error: err_str,
                    });
                }
            }
            break identity;
        };
        self.store
            .drive_to_completion()
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        let prompt_message = AgentMessage::user(task.to_owned(), Vec::new());
        let prompt_entry_id = format!("entry-{}-user", identity.run_id);
        let effective_parent_id = source_leaf_id
            .filter(|id| self.store.entries().iter().any(|e| e.id == *id))
            .map(str::to_owned);
        self.store
            .append_entry_gated(HarnessEntry {
                id: prompt_entry_id,
                parent_id: effective_parent_id,
                lane: identity.lane_name.clone(),
                seq: harness_next_seq(self.store.store()),
                timestamp: timestamp(),
                message: prompt_message,
                surface_op: threadlane_runtime::harness::SurfaceOperation::Append,
                terminate: false,
            })
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        self.store
            .drive_to_completion()
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        self.store
            .append_record_gated(HarnessRecord::StepAttempt {
                id: format!("assistant-attempt-action-{}-1", identity.run_id),
                seq: harness_next_seq(self.store.store()),
                lane: identity.lane_name.clone(),
                timestamp: timestamp(),
                run_id: identity.run_id.clone(),
                attempt: 1,
                result_entry_id: format!("entry-{}-assistant-1", identity.run_id),
                compaction_reason: None,
            })
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        self.store
            .drive_to_completion()
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        let state = Reducer::reduce(self.store.store()).map_err(|error| SubagentStartError {
            identity: Some(identity.clone()),
            error: error.to_string(),
        })?;
        let parent_run_id = state
            .lane("main")
            .and_then(|lane| lane.open_operation.clone());
        let parent_attempt = state.lane("main").map(|lane| lane.attempts);
        let seq = harness_next_seq(self.store.store());
        self.store
            .append_record_gated(HarnessRecord::SubagentLifecycle {
                id: format!("subagent-started-{}-{seq}", identity.run_id),
                seq,
                lane: "main".into(),
                timestamp: timestamp(),
                run_id: parent_run_id,
                attempt: parent_attempt,
                child_run_id: TraceString::new(identity.run_id.clone()).map_err(|error| {
                    SubagentStartError {
                        identity: Some(identity.clone()),
                        error,
                    }
                })?,
                parent_tool_call_id: None,
                task_index: None,
                agent_id: TraceString::new(lane_hint).map_err(|error| SubagentStartError {
                    identity: Some(identity.clone()),
                    error,
                })?,
                subagent_lane: TraceString::new(identity.lane_name.clone()).map_err(|error| {
                    SubagentStartError {
                        identity: Some(identity.clone()),
                        error,
                    }
                })?,
                phase: SubagentLifecyclePhase::Started,
                result_entry_id: None,
                error: None,
            })
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        self.store
            .drive_to_completion()
            .map_err(|error| SubagentStartError {
                identity: Some(identity.clone()),
                error: error.to_string(),
            })?;
        let identity = SubagentLaneIdentity {
            started_seq: self
                .store
                .records()
                .iter()
                .find_map(|record| match record {
                    HarnessRecord::OperationStarted { id, seq, .. } if id == &identity.run_id => {
                        Some(*seq)
                    }
                    _ => None,
                })
                .unwrap_or(0),
            ..identity
        };
        let accepted =
            self.accepted_subagent_run(&identity)
                .map_err(|error| SubagentStartError {
                    identity: Some(identity.clone()),
                    error,
                })?;
        Ok(StartedSubagentLane { identity, accepted })
    }

    pub(crate) fn accepted_subagent_run(
        &self,
        identity: &SubagentLaneIdentity,
    ) -> Result<AcceptedRun, String> {
        let accepted = AcceptedRun {
            session_id: self.store.session_id().to_owned(),
            run_id: identity.run_id.clone(),
            lane: identity.lane_name.clone(),
            prompt_entry_id: format!("entry-{}-user", identity.run_id),
            assistant_entry_id: format!("entry-{}-assistant-1", identity.run_id),
            accepted_through_seq: self
                .store
                .entries()
                .iter()
                .map(|entry| entry.seq)
                .chain(self.store.records().iter().map(HarnessRecord::seq))
                .max()
                .unwrap_or(0),
        };
        self.store
            .validate_accepted_run(&accepted)
            .map_err(|error| error.to_string())?;
        Ok(accepted)
    }

    /// Start a follow-up operation on an already-settled subagent lane
    /// (`hub revive` parity with oh-my-pi's parked-agent revive).
    ///
    /// The lane keeps its history: the child syncs the lane context, so the
    /// revived run continues where the previous turn left off. Fails when
    /// the lane is missing or still has an open operation (use `hub send`).
    pub(crate) fn resume_subagent_lane(
        &mut self,
        lane: &str,
        prompt: &str,
    ) -> Result<(SubagentLaneIdentity, AcceptedRun), String> {
        if prompt.trim().is_empty() {
            return Err("revive prompt must be non-empty".into());
        }
        self.ensure_fresh()?;
        let state = Reducer::reduce(self.store.store()).map_err(|error| error.to_string())?;
        let lane_state = state
            .lane(lane)
            .ok_or_else(|| format!("unknown subagent lane: {lane}"))?;
        if lane_state.open_operation.is_some() {
            return Err(format!(
                "lane {lane} is still live; use `hub send` to steer it"
            ));
        }
        let run_id = self.unique_run_id("subagent-run")?;
        let source_leaf_id = lane_state.leaf_id.clone();
        // Prompt acceptance commits the operation and prompt together.
        let prompt_message = AgentMessage::user(prompt.to_owned(), Vec::new());
        let assistant_entry_id = self
            .store
            .accept_prompt_on_lane(lane, &run_id, prompt_message)
            .map_err(|error| error.to_string())?;
        self.store
            .drive_to_completion()
            .map_err(|error| error.to_string())?;
        let started_seq = self
            .store
            .records()
            .iter()
            .find_map(|record| match record {
                HarnessRecord::OperationStarted { id, seq, .. } if id == &run_id => Some(*seq),
                _ => None,
            })
            .unwrap_or(0);
        let identity = SubagentLaneIdentity {
            lane_name: lane.to_owned(),
            run_id: run_id.clone(),
            source_leaf_id,
            started_seq,
        };
        let accepted = AcceptedRun {
            session_id: self.store.session_id().to_owned(),
            run_id,
            lane: lane.to_owned(),
            prompt_entry_id: format!("entry-{}-user", identity.run_id),
            assistant_entry_id,
            accepted_through_seq: self
                .store
                .entries()
                .iter()
                .map(|entry| entry.seq)
                .chain(self.store.records().iter().map(HarnessRecord::seq))
                .max()
                .unwrap_or(0),
        };
        self.store
            .validate_accepted_run(&accepted)
            .map_err(|error| error.to_string())?;
        Ok((identity, accepted))
    }

    pub(crate) fn append_subagent_context(
        &mut self,
        lane: &str,
        run_id: &str,
        message: String,
    ) -> Result<(), String> {
        self.ensure_fresh()?;
        let prompt_entry_id = format!("entry-{run_id}-user");
        if self
            .store
            .entry(&prompt_entry_id)
            .is_none_or(|entry| entry.lane != lane)
        {
            return Err(format!("Missing accepted subagent task for lane {lane}"));
        }
        self.store
            .append_entry_gated(HarnessEntry {
                id: format!("entry-{run_id}-context-1"),
                parent_id: Some(prompt_entry_id),
                lane: lane.into(),
                seq: harness_next_seq(self.store.store()),
                timestamp: timestamp(),
                message: AgentMessage::user(message, Vec::new()),
                surface_op: threadlane_runtime::harness::SurfaceOperation::Append,
                terminate: false,
            })
            .map_err(|error| error.to_string())?;
        self.store
            .drive_to_completion()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn finish_subagent_lane(
        &mut self,
        lane: &str,
        run_id: &str,
        outcome: OperationOutcome,
        error: Option<String>,
    ) -> Result<(), String> {
        self.ensure_fresh()?;
        let is_open = Reducer::reduce(self.store.store()).ok().map(|state| {
            state
                .lanes
                .iter()
                .any(|l| l.open_operation.as_deref() == Some(run_id))
        }) == Some(true);
        if !is_open {
            return Ok(());
        }

        if outcome == OperationOutcome::Aborted {
            let mut any_provisioned = false;
            if let Ok(state) = Reducer::reduce(self.store.store()) {
                if let Some(l) = state
                    .lanes
                    .iter()
                    .find(|l| l.open_operation.as_deref() == Some(run_id))
                {
                    for tool in &l.tools {
                        if !tool.completed
                            && tool.run_id == run_id
                            && self.store.entry(&tool.result_entry_id).is_none()
                        {
                            self.append_message_to_lane(
                                &l.name,
                                run_id,
                                AgentMessage::Tool {
                                    tool_call_id: tool.tool_call_id.clone(),
                                    name: tool.tool_name.clone(),
                                    content: error
                                        .clone()
                                        .unwrap_or_else(|| "Tool execution cancelled.".into()),
                                    is_error: true,
                                    terminate: false,
                                    images: Vec::new(),
                                },
                            )?;
                            any_provisioned = true;
                        }
                    }
                }
            }
            if any_provisioned {
                self.refresh().map_err(|error| error.to_string())?;
            }
            // Best-effort abort request; reconcile errors are observed below
            // but must not skip the terminal lifecycle record.
            let _ = self.store.request_abort(run_id);
            let _ = self.store.drive_to_completion();
            let _ = self.refresh();
            if self.store.reconcile_abort_run(run_id).is_ok() {
                let _ = self.store.drive_to_completion();
            }
        }

        self.store
            .finish_operation(run_id, outcome.clone(), error.clone())
            .map_err(|error| error.to_string())?;
        self.store
            .drive_to_completion()
            .map_err(|error| error.to_string())?;

        let phase = match outcome {
            OperationOutcome::Completed => SubagentLifecyclePhase::Completed,
            OperationOutcome::Failed => SubagentLifecyclePhase::Failed,
            OperationOutcome::Aborted | OperationOutcome::Declined => {
                SubagentLifecyclePhase::Cancelled
            }
        };
        let seq = harness_next_seq(self.store.store());
        self.store
            .append_record_gated(HarnessRecord::SubagentLifecycle {
                id: format!("subagent-finished-{run_id}-{seq}"),
                seq,
                lane: "main".into(),
                timestamp: timestamp(),
                run_id: None,
                attempt: None,
                child_run_id: TraceString::new(run_id.to_owned())?,
                parent_tool_call_id: None,
                task_index: None,
                agent_id: TraceString::new(lane.to_owned())?,
                subagent_lane: TraceString::new(lane.to_owned())?,
                phase,
                result_entry_id: None,
                error: error.map(TraceString::new).transpose()?,
            })
            .map_err(|error| error.to_string())?;
        self.store
            .drive_to_completion()
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod fusion_contract_tests {
    use super::*;

    #[test]
    fn fusion_contract_restores_prompt_and_tool_scope_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let original_prompt = "Use the custom project workflow.";
        let original_tools = Some(vec!["read_file".into(), "grep".into()]);
        let mut harness = CodingSessionHarness::open(&path).unwrap();
        harness
            .capture_fusion_lane_contract(
                "worker-lane",
                "worker",
                original_tools.clone(),
                original_prompt,
            )
            .unwrap();
        drop(harness);

        let mut harness = CodingSessionHarness::open(&path).unwrap();
        let contract = harness
            .load_fusion_lane_contract("worker-lane", Some("worker"))
            .unwrap()
            .unwrap();
        let mut changed_definition = threadlane_skills::agents::AgentDefinition {
            name: "worker".into(),
            description: "Changed agent file".into(),
            tools: None,
            model: None,
            system_prompt: "New definition prompt".into(),
            source: threadlane_skills::agents::AgentSource::Project,
            file_path: dir.path().to_path_buf(),
        };
        contract.restore_into(&mut changed_definition);

        assert_eq!(changed_definition.system_prompt, original_prompt);
        assert_eq!(changed_definition.tools, original_tools);
    }

    #[test]
    fn invalid_mismatched_and_redacted_fusion_contracts_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        let mut harness = CodingSessionHarness::open(&path).unwrap();
        harness
            .set_fact("bad", "fusion_lane_contract:bad", "not-json".into())
            .unwrap();
        let error = harness
            .load_fusion_lane_contract("bad", Some("worker"))
            .unwrap_err();
        assert!(error.contains("start a new child"), "{error}");

        harness
            .capture_fusion_lane_contract("mismatch", "worker", None, "custom prompt")
            .unwrap();
        let error = harness
            .load_fusion_lane_contract("mismatch", Some("other-agent"))
            .unwrap_err();
        assert!(error.contains("does not match"), "{error}");

        let oversized_prompt = "x".repeat(crate::durable::MAX_PERSISTED_SYSTEM_PROMPT_BYTES + 1);
        let error = harness
            .capture_fusion_lane_contract("redacted-new", "worker", None, &oversized_prompt)
            .unwrap_err();
        assert!(error.contains("redacted"), "{error}");
        let redacted_prompt = crate::durable::durable_prompt_snapshot(&oversized_prompt);
        let redacted_contract = FusionLaneContract {
            version: FUSION_LANE_CONTRACT_VERSION,
            agent: "worker".into(),
            tools: Some(vec!["read_file".into()]),
            system_prompt: redacted_prompt,
        };
        harness
            .set_fact(
                "redacted",
                "fusion_lane_contract:redacted",
                serde_json::to_string(&redacted_contract).unwrap(),
            )
            .unwrap();
        let error = harness
            .load_fusion_lane_contract("redacted", Some("worker"))
            .unwrap_err();
        assert!(error.contains("restorable system prompt"), "{error}");
        assert!(error.contains("start a new child"), "{error}");
    }
}

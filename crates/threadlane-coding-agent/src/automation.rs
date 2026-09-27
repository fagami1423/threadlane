//! Preparation for a fresh automation chat. Never selects a foreground session.
use crate::{
    controller::{spawn_session_runtime_construction, SessionController},
    harness::CodingSessionHarness,
    CodingAgentOptions,
};
use std::{path::PathBuf, sync::Arc};
use threadlane_automation::Run;
use threadlane_protocol::{OrchestratorMode, ReasoningEffort};

pub struct PreparedRun {
    pub runtime: Arc<SessionController>,
    pub work_dir: PathBuf,
    pub effort: ReasoningEffort,
}

pub async fn prepare(run: Run) -> Result<PreparedRun, String> {
    let (options, effort) = threadlane_provider::exec::get_runtime()
        .spawn_blocking(move || prepare_options(&run))
        .await
        .map_err(|e| e.to_string())??;
    let work_dir = options.work_dir.clone();
    let runtime = spawn_session_runtime_construction(options)
        .await
        .map_err(|e| e.to_string())?;
    if let Some(error) = runtime.harness_error() {
        return Err(format!(
            "Could not open automation runtime in {}: {error}",
            work_dir.display()
        ));
    }
    Ok(PreparedRun {
        runtime,
        work_dir,
        effort,
    })
}

fn prepare_options(run: &Run) -> Result<(CodingAgentOptions, ReasoningEffort), String> {
    run.definition.validate()?;
    if !run
        .session_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("Invalid automation session identity".into());
    }
    let d = &run.definition;
    let project =
        std::fs::canonicalize(&d.project).map_err(|e| format!("Project unavailable: {e}"))?;
    if !threadlane_project::load_project_registry()
        .iter()
        .any(|p| p.path == project)
    {
        return Err("Attach this automation's project before running it".into());
    }
    let (api_key, account_id) = crate::credentials::provider_credentials(&d.model);
    if api_key.is_empty() {
        return Err("Provider credentials unavailable. Sign in in Settings".into());
    }
    let (work_dir, session_file) = prepare_session(run, &project)?;
    let effort = threadlane_provider::model_registry::effective_effort(
        &d.model,
        ReasoningEffort::from_label(&d.effort).ok_or("Invalid reasoning effort")?,
        Some(&project),
    );
    let mut config = threadlane_runtime::AgentConfig::default();
    config.orchestrator_mode = OrchestratorMode::Normal;
    Ok((
        CodingAgentOptions {
            api_key,
            account_id,
            model: d.model.clone(),
            work_dir,
            session_file: Some(session_file),
            system_prompt: Default::default(),
            agent_config: Some(config),
            coding_config: None,
            browser: Default::default(),
        },
        effort,
    ))
}

fn prepare_session(run: &Run, project: &std::path::Path) -> Result<(PathBuf, PathBuf), String> {
    let d = &run.definition;
    let stub = project
        .join(".threadlane/sessions")
        .join(format!("{}.jsonl", run.session_id));
    if stub.exists() {
        return Err(
            "Automation session already exists; review it before starting another run".into(),
        );
    }
    let mut work_dir = project.to_path_buf();
    let mut facts = vec![
        ("automation_id", d.id.clone()),
        ("automation_run_id", run.id.clone()),
        ("automation_revision", d.revision.to_string()),
        ("name", format!("{} · automation", d.name)),
        ("model", d.model.clone()),
        ("reasoning_effort", d.effort.clone()),
        ("orchestrator_mode", "normal".into()),
    ];
    if d.worktree {
        if !threadlane_git::is_git_repo(project) {
            return Err("This automation requires a Git repository for its worktree".into());
        }
        work_dir = project.join(".threadlane/worktrees").join(&run.session_id);
        let branch = format!("automation/{}", run.id);
        threadlane_git::create_worktree(project, &work_dir, &branch).map_err(|e| {
            format!(
                "Could not create automation worktree {}: {e}",
                work_dir.display()
            )
        })?;
        facts.extend([
            ("is_worktree", "true".into()),
            ("worktree_path", work_dir.to_string_lossy().into_owned()),
            ("git_branch", branch),
        ]);
    }
    let session_file = work_dir
        .join(".threadlane/sessions")
        .join(format!("{}.jsonl", run.session_id));
    // Metadata stubs and the actual transcript use the existing discovery contract.
    // Setup failures retain created worktrees for inspection instead of deleting possible work.
    for path in if stub == session_file {
        vec![&stub]
    } else {
        vec![&stub, &session_file]
    } {
        for (key, value) in &facts {
            CodingSessionHarness::append_fact_to_path(path, "main", key, value, None).map_err(
                |e| format!("Could not prepare automation chat {}: {e}", path.display()),
            )?;
        }
    }
    Ok((work_dir, session_file))
}

pub fn record_outcome(runtime: &SessionController, outcome: &str) -> Result<(), String> {
    CodingSessionHarness::append_fact_to_path(
        runtime.session_file(),
        "main",
        "automation_outcome",
        outcome,
        None,
    )
}

/// Inspect the first foreground operation, not a later interactive follow-up in this chat.
pub fn durable_status(path: &std::path::Path) -> Option<threadlane_automation::RunStatus> {
    use threadlane_automation::RunStatus;
    use threadlane_runtime::harness::{JsonlStore, OperationOutcome, Record};
    let store = JsonlStore::open_read_only(path).ok()?;
    let id = store.records().iter().find_map(|r| match r {
        Record::OperationStarted { id, lane, .. } if lane == "main" => Some(id),
        _ => None,
    })?;
    store
        .records()
        .iter()
        .rev()
        .find_map(|record| match record {
            Record::OperationFinished {
                run_id, outcome, ..
            } if run_id == id => Some(match outcome {
                OperationOutcome::Completed => RunStatus::Succeeded,
                OperationOutcome::Aborted => RunStatus::Cancelled,
                OperationOutcome::Failed | OperationOutcome::Declined => RunStatus::Failed,
            }),
            Record::AbortRequested { run_id, .. } if run_id == id => Some(RunStatus::Cancelled),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::prepare_session;
    use threadlane_automation::{Definition, Run, RunStatus, Schedule};
    use threadlane_runtime::harness::{JsonlStore, SessionStore};

    #[test]
    fn automation_prepares_local_and_worktree_chats_without_touching_project_files() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().canonicalize().unwrap();
        let mut run = Run {
            id: "local".into(),
            session_id: "automation_local".into(),
            definition: Definition {
                id: "research".into(),
                revision: 1,
                name: "Research".into(),
                prompt: "Research without editing files".into(),
                project: project.clone(),
                model: "model".into(),
                effort: "medium".into(),
                worktree: false,
                schedule: Schedule::Manual,
                enabled: false,
                notify_all: false,
                anchor: 0,
                next_at: None,
                failures: 0,
                paused_reason: None,
            },
            scheduled_for: None,
            created_at: 0,
            finished_at: None,
            status: RunStatus::Starting,
            session_file: None,
            error: None,
            reviewed: false,
        };
        std::fs::write(project.join("notes.txt"), "keep my research").unwrap();
        let (checkout, chat) = prepare_session(&run, &project).unwrap();
        assert_eq!(checkout, project);
        assert!(chat.exists());
        assert!(!project.join(".threadlane/worktrees").exists());
        assert_eq!(
            JsonlStore::open_read_only(&chat).unwrap().facts()["automation_id"],
            "research"
        );
        assert!(prepare_session(&run, &project)
            .unwrap_err()
            .contains("already exists"));
        run.id = "code".into();
        run.session_id = "automation_code".into();
        run.definition.worktree = true;
        assert!(prepare_session(&run, &project)
            .unwrap_err()
            .contains("Git repository"));
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        ] {
            let result = std::process::Command::new("git")
                .args(args)
                .current_dir(&project)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        let (checkout, chat) = prepare_session(&run, &project).unwrap();
        assert_ne!(checkout, project);
        assert!(checkout.join(".git").exists());
        assert!(chat.starts_with(&checkout));
        assert!(project
            .join(".threadlane/sessions/automation_code.jsonl")
            .exists());
        let facts = JsonlStore::open_read_only(&chat).unwrap().facts();
        assert_eq!(facts["worktree_path"], checkout.to_string_lossy());
        assert_eq!(
            std::fs::read_to_string(project.join("notes.txt")).unwrap(),
            "keep my research"
        );
        let broken = tempfile::tempdir().unwrap();
        std::fs::write(broken.path().join(".threadlane"), "keep this file").unwrap();
        run.definition.worktree = false;
        let error = prepare_session(&run, broken.path()).unwrap_err();
        assert!(error.contains("Could not prepare automation chat"), "{error}");
        assert!(error.contains("automation_code.jsonl"), "{error}");
    }
}

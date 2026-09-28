//! First-send worktree preparation, off the UI thread.
use std::{path::PathBuf, sync::Arc, time::Duration};

use threadlane_coding_agent::controller::SessionRuntime;
use threadlane_protocol::{ImageAttachment, ReasoningEffort};
use threadlane_runtime::harness::{JsonlStore, SessionStore};

use crate::{ChatStreamEvent, SessionInfo};

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SetupStage {
    Naming,
    Creating,
    Starting,
}

impl SetupStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Naming => "Naming the worktree",
            Self::Creating => "Creating the worktree",
            Self::Starting => "Starting the session",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WorktreeSetup {
    pub project: PathBuf,
    pub session_id: String,
    pub session_file: PathBuf,
    pub worktree: PathBuf,
    pub base: String,
    pub stage: SetupStage,
    pub branch: Option<String>,
    pub error: Option<String>,
    #[serde(skip)]
    pub(crate) cancelled: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) text: String,
    pub(crate) images: Vec<ImageAttachment>,
    pub(crate) model: String,
    pub(crate) effort: ReasoningEffort,
    pub(crate) acp_config: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct PreparedWorktree {
    pub(crate) session: SessionInfo,
    pub(crate) runtime: Arc<SessionRuntime>,
}

impl std::fmt::Debug for PreparedWorktree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedWorktree")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Model output is untrusted: allow only a bounded ASCII slug and add a unique suffix.
pub(crate) fn branch_name(raw: &str, session_id: &str) -> String {
    let slug = raw
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_ascii_lowercase()
        .chars()
        .take(48)
        .collect::<String>();
    let slug = slug.trim_matches('-');
    let suffix = session_id.strip_prefix("session_").unwrap_or(session_id);
    format!(
        "worktree/{}-{suffix}",
        if slug.is_empty() { "task" } else { slug }
    )
}

pub(crate) fn start(
    setup: WorktreeSetup,
    mut options: threadlane_coding_agent::CodingAgentOptions,
    tx: tokio::sync::mpsc::UnboundedSender<ChatStreamEvent>,
) -> Result<(), String> {
    crate::chat::executor()?.spawn(async move {
        let progress = |stage, branch| {
            let _ = tx.send(ChatStreamEvent::WorktreeProgress {
                session_id: setup.session_id.clone(),
                stage,
                branch,
            });
        };
        let ensure_active = || {
            if setup.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                Err("Worktree setup cancelled".to_string())
            } else {
                Ok(())
            }
        };
        let result = async {
            ensure_active()?;
            // A retry after runtime construction failed reuses the recorded checkout.
            if !setup.worktree.exists() {
                let model = options.model.clone();
                let prompt = format!(
                    "Name this coding task in 3 to 6 words. Return only the name. Task:\n{}",
                    setup.text.chars().take(4_000).collect::<String>()
                );
                let generated = tokio::time::timeout(Duration::from_secs(20), async {
                    if let Some(agent) = threadlane_acp_engine::acp_agent_id(&model) {
                        threadlane_acp_engine::generate_title(
                            threadlane_project::default_global_threadlane_dir(),
                            setup.project.clone(),
                            agent,
                            &prompt,
                        )
                        .await
                    } else {
                        let (key, account) =
                            threadlane_coding_agent::credentials::provider_credentials(&model);
                        threadlane_coding_agent::credentials::provider_client_for(key, account)
                            .generate_title(&model, &prompt)
                            .await
                    }
                })
                .await
                .ok()
                .and_then(Result::ok);
                let branch = branch_name(
                    generated.as_deref().unwrap_or(&setup.text),
                    &setup.session_id,
                );
                ensure_active()?;
                progress(SetupStage::Creating, Some(branch.clone()));
                let request = setup.clone();
                tokio::task::spawn_blocking(move || {
                    threadlane_git::create_worktree_from(
                        &request.project,
                        &request.worktree,
                        &branch,
                        &request.base,
                    )
                    .map_err(|e| e.to_string())?;
                    for (key, value) in [("git_branch", branch), ("worktree_base", request.base)] {
                        threadlane_coding_agent::harness::CodingSessionHarness::append_fact_to_path(
                            &request.session_file, "main", key, &value, None,
                        ).map_err(|e| e.to_string())?;
                    }
                    Ok::<_, String>(())
                })
                .await
                .map_err(|e| e.to_string())??;
            }
            let store = JsonlStore::open(&setup.session_file).map_err(|e| e.to_string())?;
            let branch =
                threadlane_git::current_branch(&setup.worktree).map_err(|e| e.to_string())?;
            if branch.as_deref() != store.facts().get("git_branch").map(String::as_str)
                || branch.is_none()
            {
                return Err("The prepared checkout changed; inspect it before continuing".into());
            }
            ensure_active()?;
            progress(SetupStage::Starting, branch);
            options.work_dir = setup.worktree.clone();
            options.session_file = Some(setup.session_file.clone());
            let runtime =
                threadlane_coding_agent::controller::spawn_session_runtime_construction(options)
                    .await
                    .map_err(|e| e.to_string())?;
            let project = setup.project.clone();
            let id = setup.session_id.clone();
            let session = tokio::task::spawn_blocking(move || {
                crate::discovery::discover_sessions_in_project(&project)
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| "Could not reload the prepared session".to_string())
            })
            .await
            .map_err(|e| e.to_string())??;
            Ok(PreparedWorktree { session, runtime })
        }
        .await;
        let _ = tx.send(ChatStreamEvent::WorktreePrepared {
            session_id: setup.session_id,
            result,
        });
    });
    Ok(())
}

/// Save the unsent request with the session so an interrupted setup retains the user's input.
pub(crate) fn persist_request(setup: &WorktreeSetup) -> Result<(), String> {
    for (key, value) in [
        ("is_worktree", "true".to_string()),
        (
            "worktree_path",
            setup.worktree.to_string_lossy().into_owned(),
        ),
        ("reasoning_effort", setup.effort.label().to_string()),
        (
            "worktree_setup",
            serde_json::to_string(setup).map_err(|e| e.to_string())?,
        ),
    ] {
        threadlane_coding_agent::harness::CodingSessionHarness::append_fact_to_path(
            &setup.session_file,
            "main",
            key,
            &value,
            None,
        )
        .map_err(|e| e.to_string())?;
    }
    let mut store = JsonlStore::open(&setup.session_file).map_err(|e| e.to_string())?;
    store.set_model(&setup.model).map_err(|e| e.to_string())?;
    let title = threadlane_runtime::titles::normalize_session_title(&setup.text);
    store
        .set_name(if title.is_empty() {
            "New worktree task"
        } else {
            &title
        })
        .map_err(|e| e.to_string())
}

/// Recover an interrupted first-send without automatically replaying any accepted turn.
pub(crate) fn recover(session: &SessionInfo) -> Option<WorktreeSetup> {
    let store = JsonlStore::open_read_only(&session.session_file).ok()?;
    if !store.entries().is_empty() {
        return None;
    }
    let facts = store.facts();
    let mut setup: WorktreeSetup = serde_json::from_str(facts.get("worktree_setup")?).ok()?;
    if setup.session_id != session.id
        || setup.worktree
            != setup
                .project
                .join(".threadlane/worktrees")
                .join(&setup.session_id)
        || std::fs::canonicalize(&setup.project).ok().as_ref() != Some(&session.work_dir)
        || std::fs::canonicalize(&setup.session_file).ok().as_ref() != Some(&session.session_file)
    {
        return None;
    }
    setup.project = session.work_dir.clone();
    setup.session_file = session.session_file.clone();
    setup.worktree = session.runtime_work_dir.clone();
    setup.error =
        Some("Setup was interrupted. Retry to continue, or cancel to recover your message.".into());
    Some(setup)
}

pub(crate) fn clear_request(setup: &WorktreeSetup) {
    if let Err(error) = threadlane_coding_agent::harness::CodingSessionHarness::append_fact_to_path(
        &setup.session_file,
        "main",
        "worktree_setup",
        "",
        None,
    ) {
        tracing::warn!("Could not clear worktree setup request: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::branch_name;
    #[test]
    fn generated_worktree_name_is_bounded_and_cannot_inject_git_options() {
        assert_eq!(
            branch_name("Fix login expiry", "session_123"),
            "worktree/fix-login-expiry-123"
        );
        assert_eq!(
            branch_name("../ --force / 🦀", "session_123"),
            "worktree/force-123"
        );
        assert_eq!(branch_name("🦀", "session_123"), "worktree/task-123");
        assert!(branch_name(&"a".repeat(500), "session_123").len() < 70);
    }
}

//! Credential-aware, uncached operations for the agent's PR follow-through loop.
use super::{
    comment_on_pull_request, current_branch, execute_gh, execute_gh_json, github_api_args,
    github_graphql_args, graphql_errors, invalidate_github_cache, parse_pull_request_url,
    reply_to_pull_request_review_comment, validate_github_number, validated_text,
};
use crate::GitError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PrWorkflowAction {
    Status,
    Feedback,
    Logs,
    Comment,
    Reply,
    Ready,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrWorkflowRequest {
    action: PrWorkflowAction,
    number: u64,
    body: Option<String>,
    comment_id: Option<u64>,
    run_id: Option<u64>,
}

impl PrWorkflowRequest {
    pub fn mutates(&self) -> bool {
        matches!(
            self.action,
            PrWorkflowAction::Comment | PrWorkflowAction::Reply | PrWorkflowAction::Ready
        )
    }

    fn validate(&self) -> Result<(), String> {
        validate_github_number(self.number, "pull request")?;
        if matches!(
            self.action,
            PrWorkflowAction::Comment | PrWorkflowAction::Reply
        ) {
            validated_text(self.body.as_deref().unwrap_or(""), "reply body")?;
        }
        if matches!(self.action, PrWorkflowAction::Reply) {
            validate_github_number(self.comment_id.unwrap_or(0), "review comment")?;
        }
        if matches!(self.action, PrWorkflowAction::Logs) {
            validate_github_number(self.run_id.unwrap_or(0), "workflow run")?;
        }
        Ok(())
    }
}

const STATUS_FIELDS: &str = "number,url,state,isDraft,headRefName,headRefOid,baseRefName,mergeable,mergeStateStatus,reviewDecision,reviewRequests,statusCheckRollup";
const THREADS_QUERY: &str = "query($owner: String!, $repo: String!, $number: Int!, $cursor: String) { \
    repository(owner: $owner, name: $repo) { pullRequest(number: $number) { \
        reviewThreads(first: 100, after: $cursor) { \
            pageInfo { hasNextPage endCursor } \
            nodes { id isResolved isOutdated comments(first: 1) { nodes { databaseId url } } } \
        } \
    } } \
}";

fn parse_json(work_dir: &Path, text: &str) -> Result<Value, GitError> {
    serde_json::from_str(text)
        .map_err(|error| GitError::new(work_dir, format!("Invalid GitHub response: {error}")))
}

fn status(work_dir: &Path, number: u64) -> Result<Value, GitError> {
    let output = execute_gh(
        work_dir,
        &[
            "pr".into(),
            "view".into(),
            number.to_string(),
            "--json".into(),
            STATUS_FIELDS.into(),
        ],
    )?;
    let value = parse_json(work_dir, &output)?;
    if value["number"].as_u64() != Some(number)
        || value["headRefOid"].as_str().unwrap_or("").is_empty()
    {
        return Err(GitError::new(
            work_dir,
            "GitHub did not confirm the PR identity and head SHA",
        ));
    }
    Ok(value)
}

fn collect_threads(
    work_dir: &Path,
    mut fetch: impl FnMut(Option<&str>) -> Result<Value, GitError>,
) -> Result<Vec<Value>, GitError> {
    let mut threads = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..20 {
        let value = fetch(cursor.as_deref())?;
        if let Some(message) = graphql_errors(&value) {
            return Err(GitError::new(work_dir, message));
        }
        let connection = &value["data"]["repository"]["pullRequest"]["reviewThreads"];
        let nodes = connection["nodes"]
            .as_array()
            .ok_or_else(|| GitError::new(work_dir, "Missing review threads"))?;
        threads.extend(nodes.iter().cloned());
        match connection["pageInfo"]["hasNextPage"].as_bool() {
            Some(false) => return Ok(threads),
            Some(true) => {
                let next = connection["pageInfo"]["endCursor"]
                    .as_str()
                    .filter(|next| !next.is_empty() && Some(*next) != cursor.as_deref())
                    .ok_or_else(|| {
                        GitError::new(work_dir, "Review thread pagination did not advance")
                    })?;
                cursor = Some(next.to_owned());
            }
            None => {
                return Err(GitError::new(
                    work_dir,
                    "Missing review thread pagination state",
                ));
            }
        }
    }
    Err(GitError::new(
        work_dir,
        "Review thread page limit reached; feedback is incomplete",
    ))
}

pub fn execute_pr_workflow(
    work_dir: &Path,
    request: PrWorkflowRequest,
) -> Result<String, GitError> {
    request
        .validate()
        .map_err(|message| GitError::new(work_dir, message))?;
    let snapshot = status(work_dir, request.number)?;
    let url = snapshot["url"]
        .as_str()
        .ok_or_else(|| GitError::new(work_dir, "Missing PR URL"))?;
    let (repository, number) =
        parse_pull_request_url(url).map_err(|message| GitError::new(work_dir, message))?;
    if number != request.number {
        return Err(GitError::new(
            work_dir,
            "PR URL does not match the requested number",
        ));
    }
    match request.action {
        PrWorkflowAction::Status => Ok(snapshot.to_string()),
        PrWorkflowAction::Feedback => {
            let repo = format!("repos/{}/{}", repository.owner, repository.repo);
            let mut feedback = json!({"status": snapshot});
            for (key, endpoint) in [
                (
                    "conversation_comments",
                    format!("{repo}/issues/{number}/comments"),
                ),
                ("reviews", format!("{repo}/pulls/{number}/reviews")),
                ("inline_comments", format!("{repo}/pulls/{number}/comments")),
            ] {
                let output = execute_gh(
                    work_dir,
                    &github_api_args(&repository.host, &[&endpoint, "--paginate", "--slurp"]),
                )?;
                let pages = parse_json(work_dir, &output)?;
                let pages = pages
                    .as_array()
                    .ok_or_else(|| GitError::new(work_dir, "Missing feedback pages"))?;
                let mut items = Vec::new();
                for page in pages {
                    items.extend(
                        page.as_array()
                            .ok_or_else(|| GitError::new(work_dir, "Invalid feedback page"))?
                            .iter()
                            .cloned(),
                    );
                }
                feedback[key] = Value::Array(items);
            }
            let threads = collect_threads(work_dir, |cursor| {
                let payload = json!({"query": THREADS_QUERY, "variables": {
                    "owner": repository.owner, "repo": repository.repo, "number": number, "cursor": cursor,
                }});
                let output =
                    execute_gh_json(work_dir, &github_graphql_args(&repository.host), &payload)?;
                parse_json(work_dir, &output)
            })?;
            feedback["threads"] = Value::Array(threads);
            Ok(feedback.to_string())
        }
        PrWorkflowAction::Logs => execute_gh(
            work_dir,
            &[
                "run".into(),
                "view".into(),
                request.run_id.unwrap().to_string(),
                "--log-failed".into(),
                "--repo".into(),
                format!(
                    "{}/{}/{}",
                    repository.host, repository.owner, repository.repo
                ),
            ],
        ),
        PrWorkflowAction::Comment => {
            comment_on_pull_request(work_dir, number, request.body.as_deref().unwrap())
        }
        PrWorkflowAction::Reply => reply_to_pull_request_review_comment(
            work_dir,
            url,
            request.comment_id.unwrap(),
            request.body.as_deref().unwrap(),
        ),
        PrWorkflowAction::Ready => {
            let branch = current_branch(work_dir)?;
            if branch.as_deref() != snapshot["headRefName"].as_str() || branch.is_none() {
                return Err(GitError::new(
                    work_dir,
                    "Check out the PR head branch before marking it ready",
                ));
            }
            if snapshot["state"].as_str() != Some("OPEN") {
                return Err(GitError::new(
                    work_dir,
                    "Only an open PR can be marked ready",
                ));
            }
            match snapshot["isDraft"].as_bool() {
                Some(false) => return Ok(format!("PR is already ready for review: {url}")),
                Some(true) => {}
                None => return Err(GitError::new(work_dir, "Missing PR draft state")),
            }
            let result = execute_gh(work_dir, &["pr".into(), "ready".into(), number.to_string()]);
            invalidate_github_cache(work_dir);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PrWorkflowRequest, collect_threads};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn workflow_requests_validate_before_accessing_git() {
        for args in [
            json!({"action":"status", "number":0}),
            json!({"action":"reply", "number":1, "body":"fix"}),
            json!({"action":"comment", "number":1, "body":" "}),
            json!({"action":"logs", "number":1, "run_id":0}),
        ] {
            assert!(
                serde_json::from_value::<PrWorkflowRequest>(args)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<PrWorkflowRequest>(json!({"action":"merge", "number":1}))
                .is_err()
        );
        for (action, mutates) in [
            ("status", false),
            ("feedback", false),
            ("logs", false),
            ("comment", true),
            ("reply", true),
            ("ready", true),
        ] {
            let request: PrWorkflowRequest =
                serde_json::from_value(json!({"action":action, "number":1})).unwrap();
            assert_eq!(request.mutates(), mutates);
        }
    }

    #[test]
    fn workflow_threads_follow_pages_and_fail_closed() {
        let mut calls = 0;
        let threads = collect_threads(Path::new("."), |cursor| {
            calls += 1;
            assert_eq!(cursor, (calls == 2).then_some("next"));
            Ok(
                json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
                    "nodes":[{"id":calls}], "pageInfo":{"hasNextPage":calls == 1,"endCursor":"next"}
                }}}}}),
            )
        })
        .unwrap();
        assert_eq!(threads.len(), 2);
        for value in [
            json!({"errors":[{"message":"denied"}]}),
            json!({"data":null}),
            json!({"data":{"repository":{"pullRequest":{"reviewThreads":{
                "nodes":[], "pageInfo":{"hasNextPage":true,"endCursor":null}
            }}}}}),
        ] {
            assert!(collect_threads(Path::new("."), |_| Ok(value.clone())).is_err());
        }
    }
}

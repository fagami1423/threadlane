use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrField {
    Title,
    Description,
}

pub(super) fn generation_prompt(field: PrField, diff: &str) -> String {
    let request = match field {
        PrField::Title => "Return only a concise pull request title (maximum 72 characters).",
        PrField::Description => "Return a concise Markdown pull request description covering the change and tests. Do not include a title. Do not claim tests passed without evidence.",
    };
    let diff: String = diff.chars().take(24_000).collect();
    format!("{request}\n\nPR base-to-HEAD diff:\n{diff}")
}

pub(super) fn current_diff(work_dir: &Path, base: &str) -> Result<String, String> {
    threadlane_git::draft_pr_diff(work_dir, base)
        .map_err(|error| error.to_string())
        .map(|diff| diff.chars().take(24_000).collect())
}

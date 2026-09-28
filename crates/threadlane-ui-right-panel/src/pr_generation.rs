use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrField {
    Title,
    Description,
}

pub(super) fn generation_prompt(field: PrField, diff: &str) -> String {
    let (request, limit) = match field {
        PrField::Title => (
            "Return only a concise pull request title (maximum 72 characters).",
            72,
        ),
        PrField::Description => (
            "Return a concise Markdown pull request description covering the change and tests. Do not include a title.",
            12_000,
        ),
    };
    let diff: String = diff.chars().take(limit * 2).collect();
    format!("{request}\n\nCurrent working-tree diff:\n{diff}")
}

pub(super) fn current_diff(work_dir: &Path) -> Result<String, String> {
    threadlane_git::commit_message_diff(work_dir)
        .map_err(|error| error.to_string())
        .map(|diff| diff.chars().take(24_000).collect())
}

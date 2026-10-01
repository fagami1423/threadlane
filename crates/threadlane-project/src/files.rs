//! Project file browsing and confined single-file I/O for the project root.
//!
//! Canonical home of the Files-surface tree scan (previously in
//! `threadlane-ui-right-panel`) plus the path confinement a remote daemon
//! needs to serve `ReadProjectFile`/`WriteProjectFile`/`ProjectFileExists`
//! without letting a client-supplied relative path escape `work_dir`.

use std::path::{Component, Path, PathBuf};

use threadlane_protocol::repo::ProjectFileNode;

/// Maximum depth `scan_project_tree` descends from the project root.
const TREE_MAX_DEPTH: usize = 6;
/// Largest file `read_project_file` will serve as one UTF-8 payload.
const READ_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Resolve a client-supplied relative path against `root`, refusing any
/// escape: absolute paths, `..`/`.` components, and (when the target
/// exists) symlinks that resolve outside `root`.
pub fn resolve_project_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(relative);
    let mut parts = Vec::new();
    for component in rel_path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_os_string()),
            _ => return Err(format!("Path {relative:?} escapes the project root")),
        }
    }
    if parts.is_empty() {
        return Err("Empty path".to_string());
    }
    let candidate = parts.iter().collect::<PathBuf>();
    let joined = root.join(&candidate);
    if let (Ok(real_root), Ok(real_joined)) =
        (root.canonicalize(), joined.canonicalize())
    {
        if !real_joined.starts_with(&real_root) {
            return Err(format!("Path {relative:?} escapes the project root"));
        }
    }
    Ok(joined)
}

/// Read `relative` under `root` as UTF-8 text.
pub fn read_project_file(root: &Path, relative: &str) -> Result<String, String> {
    let path = resolve_project_path(root, relative)?;
    let metadata = std::fs::metadata(&path).map_err(|error| error.to_string())?;
    if metadata.len() > READ_MAX_BYTES {
        return Err(format!(
            "File is {} bytes; the {}-byte limit keeps replies bounded",
            metadata.len(),
            READ_MAX_BYTES
        ));
    }
    std::fs::read_to_string(&path).map_err(|error| error.to_string())
}

/// Overwrite `relative` under `root` with UTF-8 `content`.
pub fn write_project_file(root: &Path, relative: &str, content: &str) -> Result<(), String> {
    let path = resolve_project_path(root, relative)?;
    std::fs::write(&path, content).map_err(|error| error.to_string())
}

/// Whether `relative` names an existing file entry under `root`.
pub fn project_file_exists(root: &Path, relative: &str) -> Result<bool, String> {
    let path = resolve_project_path(root, relative)?;
    Ok(std::fs::symlink_metadata(&path).is_ok())
}

/// Enumerate the project's file tree for the Files surface.
///
/// Skips `.git`, `target`, and `.threadlane`, stops at `limit` entries and
/// `TREE_MAX_DEPTH` deep; directories sort before files, then by name.
pub fn scan_project_tree(root: &Path, limit: usize) -> Vec<ProjectFileNode> {
    fn visit(
        root: &Path,
        relative: &Path,
        depth: usize,
        limit: usize,
        count: &mut usize,
    ) -> Vec<ProjectFileNode> {
        if *count >= limit || depth > TREE_MAX_DEPTH {
            return Vec::new();
        }
        let Ok(read_dir) = std::fs::read_dir(root.join(relative)) else {
            return Vec::new();
        };
        let mut children = read_dir
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" || name == "target" || name == ".threadlane" {
                    return None;
                }
                Some((name, entry.file_type().ok()?.is_dir()))
            })
            .collect::<Vec<_>>();
        children.sort_by_key(|(name, is_dir)| (!*is_dir, name.to_ascii_lowercase()));

        let mut nodes = Vec::new();
        for (name, is_dir) in children {
            if *count >= limit {
                break;
            }
            *count += 1;
            let path = relative.join(&name);
            let rel_str = path.to_string_lossy().into_owned();
            nodes.push(ProjectFileNode {
                relative_path: rel_str,
                name,
                is_dir,
                children: Vec::new(),
            });
        }
        for node in &mut nodes {
            if node.is_dir {
                node.children = visit(
                    root,
                    Path::new(&node.relative_path),
                    depth + 1,
                    limit,
                    count,
                );
            }
        }
        nodes
    }

    let mut count = 0;
    visit(root, Path::new(""), 0, limit, &mut count)
}

#[cfg(test)]
mod tests {
    use super::{
        project_file_exists, read_project_file, resolve_project_path, scan_project_tree,
        write_project_file,
    };
    use std::path::Path;

    #[test]
    fn resolve_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for bad in ["../outside", "a/../../outside", "./x", "", "/etc/passwd"] {
            assert!(
                resolve_project_path(root, bad).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn resolve_accepts_nested_relative() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_project_path(dir.path(), "src/main.rs").unwrap();
        assert_eq!(resolved, dir.path().join("src/main.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "leak").unwrap();
        std::os::unix::fs::symlink(&secret, dir.path().join("link.txt")).unwrap();
        assert!(resolve_project_path(dir.path(), "link.txt").is_err());
        assert!(read_project_file(dir.path(), "link.txt").is_err());
        assert!(project_file_exists(dir.path(), "link.txt").is_err());
    }

    #[test]
    fn read_write_exists_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        write_project_file(dir.path(), "sub/a.txt", "hello").unwrap();
        assert_eq!(
            read_project_file(dir.path(), "sub/a.txt").unwrap(),
            "hello"
        );
        assert!(project_file_exists(dir.path(), "sub/a.txt").unwrap());
        assert!(!project_file_exists(dir.path(), "sub/missing.txt").unwrap());
    }

    #[test]
    fn scan_skips_noise_and_sorts_dirs_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("b_dir")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".threadlane")).unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();

        let nodes = scan_project_tree(root, 100);
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["b_dir", "a.txt"]);
        assert!(nodes[0].is_dir);
        assert!(!nodes[1].is_dir);
    }

    #[test]
    fn scan_respects_limit_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        std::fs::write(root.join("b.txt"), "x").unwrap();
        assert_eq!(scan_project_tree(root, 1).len(), 1);

        // Beyond TREE_MAX_DEPTH the subtree is not descended.
        let mut deep = root.to_path_buf();
        for _ in 0..8 {
            deep = deep.join("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.txt"), "x").unwrap();
        let tree = scan_project_tree(root, 10_000);
        fn contains(node_path: &str, nodes: &[super::ProjectFileNode]) -> bool {
            nodes.iter().any(|n| {
                n.relative_path == node_path || contains(node_path, &n.children)
            })
        }
        let rel = Path::new("d/d/d/d/d/d/d/d/deep.txt")
            .to_string_lossy()
            .into_owned();
        assert!(!contains(&rel, &tree));
    }
}

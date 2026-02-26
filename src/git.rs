//! Git integration: walk the HEAD tree to build a set of all tracked file paths.
//!
//! Used to validate links to files that live outside the typedown project walk
//! scope (e.g. cross-project links like `../../meow/README.md`).
//!
//! Validation stays pure — this module is called once by the orchestrator
//! (`format.rs`) and the resulting `HashSet<PathBuf>` is passed in as data.
//! No I/O happens inside `validate.rs`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use git2::Repository;

/// Build the set of all absolute paths tracked in HEAD.
///
/// Returns `None` if `root` is not inside a git repository, or if HEAD has
/// no commits yet (empty repo). Uses libgit2; no subprocess.
pub fn list_git_paths(root: &Path) -> Option<HashSet<PathBuf>> {
    let repo = Repository::discover(root).ok()?;
    let repo_root = repo.workdir()?.to_path_buf();
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;

    let mut paths = HashSet::new();
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if entry.kind() == Some(git2::ObjectType::Blob) {
            if let Some(name) = entry.name() {
                let rel = if dir.is_empty() {
                    name.to_string()
                } else {
                    format!("{dir}{name}")
                };
                paths.insert(repo_root.join(rel));
            }
        }
        git2::TreeWalkResult::Ok
    })
    .ok()?;

    Some(paths)
}

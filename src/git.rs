//! Git integration: walk the HEAD tree to build the set of tracked paths.
//!
//! Used to validate links to files that live outside the typedown project walk
//! scope (e.g. cross-project links like `../../sibling-project/README.md`).
//!
//! Validation stays pure — this module is called once by the orchestrator
//! (`format.rs`) and the resulting [`GitTree`] is passed in as data. No I/O
//! happens inside `validate.rs`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use git2::Repository;

/// The paths HEAD tracks, split into files and the directories holding them.
///
/// Both sets hold absolute paths. `dirs` exists because `files` alone cannot
/// tell "this one file is missing" from "this whole subtree isn't in the
/// checkout" — a distinction link validation needs when a link leaves the
/// project. `dirs` always contains the repository root itself.
#[derive(Debug, Default)]
pub struct GitTree {
    /// Every blob in HEAD.
    pub files: HashSet<PathBuf>,
    /// Every tree in HEAD, plus the repository root.
    pub dirs: HashSet<PathBuf>,
}

/// Return the absolute path of the git working directory containing `root`.
///
/// Returns `None` if `root` is not inside a git repository.
pub fn git_repo_root(root: &Path) -> Option<PathBuf> {
    let repo = Repository::discover(root).ok()?;
    Some(repo.workdir()?.to_path_buf())
}

/// Build the set of all absolute paths tracked in HEAD.
///
/// Returns `None` if `root` is not inside a git repository, or if HEAD has
/// no commits yet (empty repo). Uses libgit2; no subprocess.
pub fn read_head_tree(root: &Path) -> Option<GitTree> {
    let repo = Repository::discover(root).ok()?;
    let repo_root = repo.workdir()?.to_path_buf();
    let head = repo.head().ok()?;
    let tree = head.peel_to_tree().ok()?;

    let mut out = GitTree::default();
    out.dirs.insert(repo_root.clone());
    tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
        if let Some(name) = entry.name() {
            let rel = if dir.is_empty() {
                name.to_string()
            } else {
                format!("{dir}{name}")
            };
            match entry.kind() {
                Some(git2::ObjectType::Blob) => {
                    out.files.insert(repo_root.join(rel));
                }
                Some(git2::ObjectType::Tree) => {
                    out.dirs.insert(repo_root.join(rel));
                }
                _ => {}
            }
        }
        git2::TreeWalkResult::Ok
    })
    .ok()?;

    Some(out)
}

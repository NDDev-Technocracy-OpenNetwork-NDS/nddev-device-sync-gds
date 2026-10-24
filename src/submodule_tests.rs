//! Real initialized hierarchy; every remote and credential is generated here.
use super::*;

struct Hierarchy {
    root: Fixture,
    _child: Fixture,
    _grandchild: Fixture,
}
fn add_child(parent: &Path, endpoint: &str, path: &str) {
    git(
        parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            endpoint,
            path,
        ],
    );
    git(parent, &["add", ".gitmodules", path]);
    git(parent, &["commit", "-m", "generated pinned hierarchy"]);
}
fn initialize(parent: &Path) {
    git(
        parent,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    );
}
impl Hierarchy {
    fn new() -> Self {
        let root = Fixture::new();
        let child = Fixture::new();
        let grandchild = Fixture::new();
        add_child(&child.first, &grandchild.endpoint, "inner/grandchild");
        git(&child.first, &["push", "origin", "main"]);
        add_child(&root.first, &child.endpoint, "modules/child");
        initialize(&root.first);
        git(&root.first, &["push", "origin", "main"]);
        git(&root.second, &["pull", "--ff-only"]);
        initialize(&root.second);
        Self {
            root,
            _child: child,
            _grandchild: grandchild,
        }
    }
    fn child(&self, root: &Path) -> PathBuf {
        root.join("modules/child")
    }
    fn grandchild(&self, root: &Path) -> PathBuf {
        self.child(root).join("inner/grandchild")
    }
}

fn child_state(child: &Path) -> (String, Vec<u8>) {
    let admin = git(child, &["rev-parse", "--absolute-git-dir"]);
    (
        git(child, &["rev-parse", "HEAD"]),
        std::fs::read(Path::new(&admin).join("index")).unwrap(),
    )
}

#[tokio::test]
async fn initialized_nested_pins_allow_parent_push_and_fast_forward_without_child_writes() {
    let hierarchy = Hierarchy::new();
    let fixture = &hierarchy.root;
    let children = [&fixture.first, &fixture.second]
        .into_iter()
        .flat_map(|root| [hierarchy.child(root), hierarchy.grandchild(root)])
        .collect::<Vec<_>>();
    let before = children
        .iter()
        .map(|path| child_state(path))
        .collect::<Vec<_>>();
    // Host configuration cannot opt the parent back into child mutation.
    git(&fixture.second, &["config", "submodule.recurse", "true"]);
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::UpToDate
    );
    let published = commit(&fixture.first, "parent.txt", "existing parent commit");
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::Published
    );
    assert_eq!(fixture.remote_head(), published);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::FastForwarded
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), published);
    let returned = commit(&fixture.second, "other-parent.txt", "other existing commit");
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Published
    );
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::FastForwarded
    );
    assert_eq!(fixture.remote_head(), returned);
    for (path, state) in children.iter().zip(before) {
        assert_eq!(child_state(path), state);
    }
}

#[tokio::test]
async fn nested_dirty_work_and_staged_pins_refuse_parent_cycle_without_touching_any_index() {
    let hierarchy = Hierarchy::new();
    let fixture = &hierarchy.root;
    let child = hierarchy.child(&fixture.second);
    let grandchild = hierarchy.grandchild(&fixture.second);
    let before_head = git(&fixture.second, &["rev-parse", "HEAD"]);
    commit(&fixture.first, "remote.txt", "remote parent commit");
    git(&fixture.first, &["push", "origin", "main"]);
    std::fs::write(grandchild.join("initial.txt"), "staged user work").unwrap();
    git(&grandchild, &["add", "initial.txt"]);
    std::fs::write(grandchild.join("initial.txt"), "unstaged user work").unwrap();
    std::fs::write(grandchild.join("new.txt"), "untracked user work").unwrap();
    let before = child_state(&grandchild);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::Dirty)
    );
    assert_eq!(child_state(&grandchild), before);
    assert_eq!(
        std::fs::read_to_string(grandchild.join("initial.txt")).unwrap(),
        "unstaged user work"
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), before_head);
    // A staged child pin must remain visible even though Git status itself
    // ignores submodules. No child checkout needs to be read in this case.
    let different = git(&fixture.second, &["rev-parse", "HEAD~1"]);
    git(
        &fixture.second,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{different},modules/child"),
        ],
    );
    let index = std::fs::read(fixture.second.join(".git/index")).unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::Dirty)
    );
    assert_eq!(
        std::fs::read(fixture.second.join(".git/index")).unwrap(),
        index
    );
    assert!(child.exists());
}

#[tokio::test]
async fn changed_committed_pin_is_refused_for_both_publication_and_fast_forward() {
    let hierarchy = Hierarchy::new();
    let fixture = &hierarchy.root;
    let child = hierarchy.child(&fixture.first);
    let second_child = hierarchy.child(&fixture.second);
    let remote_before = fixture.remote_head();
    let second_before = git(&fixture.second, &["rev-parse", "HEAD"]);
    let child_before = child_state(&second_child);
    commit(&child, "child.txt", "new external child commit");
    git(&fixture.first, &["add", "modules/child"]);
    git(&fixture.first, &["commit", "-m", "external changed pin"]);
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert_eq!(fixture.remote_head(), remote_before);
    git(&fixture.first, &["push", "origin", "main"]);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), second_before);
    assert_eq!(child_state(&second_child), child_before);
}

#[tokio::test]
async fn child_gitdir_escape_and_symlinks_are_refused_before_child_filters() {
    let hierarchy = Hierarchy::new();
    let fixture = &hierarchy.root;
    let child = hierarchy.child(&fixture.second);
    let marker = fixture._directory.path().join("untrusted-filter");
    git(
        &hierarchy._child.second,
        &[
            "config",
            "filter.probe.clean",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(
        child.join(".git"),
        format!(
            "gitdir: {}\n",
            hierarchy._child.second.join(".git").display()
        ),
    )
    .unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert!(!marker.exists());
    #[cfg(unix)]
    {
        let original = child.with_file_name("saved-child");
        std::fs::rename(&child, &original).unwrap();
        std::os::unix::fs::symlink(&original, &child).unwrap();
        assert_eq!(
            fixture.synchronize(&fixture.second).await.unwrap_err(),
            ModuleError::Unavailable
        );
        assert!(!marker.exists());
    }
}

#[tokio::test]
async fn nested_active_writer_blocks_parent_and_uninitialized_child_is_never_created() {
    let hierarchy = Hierarchy::new();
    let fixture = &hierarchy.root;
    let grandchild = hierarchy.grandchild(&fixture.second);
    let admin = PathBuf::from(git(&grandchild, &["rev-parse", "--absolute-git-dir"]));
    std::fs::write(admin.join("index.lock"), "external writer").unwrap();
    let before = child_state(&grandchild);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::ActiveWriter)
    );
    assert_eq!(child_state(&grandchild), before);
    std::fs::remove_file(admin.join("index.lock")).unwrap();
    git(&grandchild, &["switch", "-c", "fixture-writer"]);
    let branch_lock = admin.join("refs/heads/fixture-writer.lock");
    std::fs::write(&branch_lock, "external ref writer").unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::ActiveWriter)
    );
    std::fs::remove_file(branch_lock).unwrap();
    std::fs::remove_file(grandchild.join(".git")).unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert!(!grandchild.join(".git").exists());
}

#[tokio::test]
async fn changed_gitmodules_mapping_and_uncommitted_child_head_never_authorize_parent_writes() {
    let hierarchy = Hierarchy::new();
    let fixture = &hierarchy.root;
    let second_before = git(&fixture.second, &["rev-parse", "HEAD"]);
    let child = hierarchy.child(&fixture.second);
    let child_before = child_state(&child);
    git(
        &fixture.first,
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.modules/child.url",
            "https://github.com/generated-fixture/other.git",
        ],
    );
    git(&fixture.first, &["add", ".gitmodules"]);
    git(
        &fixture.first,
        &["commit", "-m", "external changed mapping"],
    );
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::Unavailable
    );
    git(&fixture.first, &["push", "origin", "main"]);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), second_before);
    assert_eq!(child_state(&child), child_before);
    commit(
        &child,
        "local-child.txt",
        "user child commit not recorded by parent",
    );
    let changed_child = child_state(&child);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::Dirty)
    );
    assert_eq!(child_state(&child), changed_child);
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), second_before);
}

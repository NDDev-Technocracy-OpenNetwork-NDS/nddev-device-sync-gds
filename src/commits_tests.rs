//! Actual isolated Git repositories, objects, ref transactions and worktrees.
//! Local transport does not establish positive GitHub credential acceptance.
#[path = "submodule_tests.rs"]
mod submodule_tests;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "workspace_tests.rs"]
mod workspace_tests;
use super::*;
use nddev_device_sync_adapter_io::NativeIo;
use nddev_device_sync_domain::repositories::workspace::SyncBlock;
use std::path::Path;
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Fixture {
    _directory: tempfile::TempDir,
    remote: PathBuf,
    endpoint: String,
    first: PathBuf,
    second: PathBuf,
    workspace: GitWorkspace,
}
fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=NDS generated fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated Git fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn commit(directory: &Path, name: &str, body: &str) -> String {
    std::fs::write(directory.join(name), body).unwrap();
    git(directory, &["add", "--", name]);
    git(
        directory,
        &["commit", "--quiet", "-m", "fixture existing commit"],
    );
    git(directory, &["rev-parse", "HEAD"])
}

#[test]
fn native_cycle_future_is_send_for_the_existing_owned_background_task() {
    fn require_send<T: std::future::Future + Send>(_: T) {}
    let fixture = Fixture::new();
    require_send(fixture.workspace.run(
        fixture.first.clone(),
        fixture.endpoint.clone(),
        false,
        "generated-invalid-token",
        &CancellationToken::new(),
        || async { Ok(()) },
    ));
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("Developer/NDS");
        std::fs::create_dir_all(&root).unwrap();
        // macOS exposes temporary storage through /var -> /private/var.
        // Match the production constructor's canonical workspace root.
        let root = root.canonicalize().unwrap();
        let remote = directory.path().join("remote.git");
        let endpoint = remote.to_str().unwrap().replace('\\', "/");
        git(
            directory.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        let first = root.join("personal/first");
        let second = root.join("organization/second");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        git(
            directory.path(),
            &["clone", &endpoint, first.to_str().unwrap()],
        );
        commit(&first, "initial.txt", "initial");
        git(&first, &["push", "--set-upstream", "origin", "main"]);
        git(
            directory.path(),
            &["clone", &endpoint, second.to_str().unwrap()],
        );
        let executable = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join(if cfg!(windows) { "git.exe" } else { "git" }))
            .find(|p| p.is_file())
            .unwrap();
        let workspace = GitWorkspace::new(
            NativeIo::new(4).unwrap().process_handle(),
            executable,
            root,
            WorkspacePrincipal {
                origin: "https://nds.example.invalid".into(),
                tenant: "company".into(),
                user: "owner".into(),
                device: "device".into(),
            },
        )
        .unwrap();
        Self {
            _directory: directory,
            remote,
            endpoint,
            first,
            second,
            workspace,
        }
    }
    async fn synchronize(&self, checkout: &Path) -> Result<CommitOutcome, ModuleError> {
        self.workspace
            .run(
                checkout.to_path_buf(),
                self.endpoint.clone(),
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
            )
            .await
    }
    fn remote_head(&self) -> String {
        git(&self.remote, &["rev-parse", "refs/heads/main"])
    }
}

#[tokio::test]
async fn actual_existing_commits_converge_both_directions_without_new_commits() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::UpToDate
    );
    let first = commit(
        &fixture.first,
        "from-first.txt",
        "first device existing commit",
    );
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::Published
    );
    assert_eq!(fixture.remote_head(), first);
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::FastForwarded
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), first);
    assert_eq!(
        std::fs::read_to_string(fixture.second.join("from-first.txt")).unwrap(),
        "first device existing commit"
    );
    let second = commit(
        &fixture.second,
        "from-second.txt",
        "second device existing commit",
    );
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Published
    );
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::FastForwarded
    );
    for checkout in [&fixture.first, &fixture.second] {
        assert_eq!(git(checkout, &["rev-parse", "HEAD"]), second);
        assert!(git(checkout, &["status", "--porcelain"]).is_empty());
        assert_eq!(
            fixture.synchronize(checkout).await.unwrap(),
            CommitOutcome::UpToDate
        );
    }
    assert_eq!(git(&fixture.remote, &["rev-list", "--count", "main"]), "3");
}

#[tokio::test]
async fn acknowledged_push_accepts_a_tracking_ref_already_advanced_by_git() {
    let fixture = Fixture::new();
    let local = commit(&fixture.first, "publication.txt", "existing commit");
    let advanced = Arc::new(AtomicUsize::new(0));
    let counted = advanced.clone();
    let result = fixture
        .workspace
        .run(
            fixture.first.clone(),
            fixture.endpoint.clone(),
            false,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async {
                // A real separate Git ref writer at the publication boundary. Git2.56
                // also performs this tracking update itself; older Git can leave it old.
                if fixture.remote_head() == local && counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    git(
                        &fixture.first,
                        &["update-ref", "refs/remotes/origin/main", &local],
                    );
                }
                Ok(())
            },
        )
        .await;
    assert_eq!(result.unwrap(), CommitOutcome::Published);
    assert!(advanced.load(Ordering::SeqCst) > 0);
    assert_eq!(fixture.remote_head(), local);
    assert_eq!(
        git(&fixture.first, &["rev-parse", "refs/remotes/origin/main"]),
        local
    );
}

#[tokio::test]
async fn publication_cannot_overwrite_an_unrelated_tracking_ref_change() {
    let fixture = Fixture::new();
    git(&fixture.first, &["switch", "-c", "independent"]);
    let independent = commit(&fixture.first, "independent.txt", "retained other branch");
    git(&fixture.first, &["switch", "main"]);
    let local = commit(&fixture.first, "publication.txt", "published branch");
    let changed = Arc::new(AtomicUsize::new(0));
    let count = changed.clone();
    let result = fixture
        .workspace
        .run(
            fixture.first.clone(),
            fixture.endpoint.clone(),
            false,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async {
                if fixture.remote_head() == local && count.fetch_add(1, Ordering::SeqCst) == 0 {
                    git(
                        &fixture.first,
                        &["update-ref", "refs/remotes/origin/main", &independent],
                    );
                }
                Ok(())
            },
        )
        .await;
    assert_eq!(result.unwrap_err(), ModuleError::Busy);
    assert_eq!(
        git(&fixture.first, &["rev-parse", "refs/remotes/origin/main"]),
        independent
    );
    assert_eq!(git(&fixture.first, &["rev-parse", "HEAD"]), local);
    assert_eq!(fixture.remote_head(), local);
}

#[tokio::test]
async fn archived_checkout_fetches_and_fast_forwards_but_cannot_publish() {
    let fixture = Fixture::new();
    let remote = commit(
        &fixture.first,
        "last-before-archive.txt",
        "readable archived data",
    );
    fixture.synchronize(&fixture.first).await.unwrap();
    let archived = async |path: PathBuf| {
        fixture
            .workspace
            .run(
                path,
                fixture.endpoint.clone(),
                true,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
            )
            .await
    };
    assert_eq!(
        archived(fixture.second.clone()).await.unwrap(),
        CommitOutcome::FastForwarded
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), remote);
    let local = commit(
        &fixture.second,
        "retained.txt",
        "must not publish to archive",
    );
    assert_eq!(
        archived(fixture.second.clone()).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::Archived)
    );
    assert_eq!(fixture.remote_head(), remote);
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), local);
}

#[tokio::test]
async fn dirty_diverged_and_locked_repositories_keep_their_work_and_refs() {
    let fixture = Fixture::new();
    let original = git(&fixture.second, &["rev-parse", "HEAD"]);
    commit(&fixture.first, "remote-change.txt", "remote");
    fixture.synchronize(&fixture.first).await.unwrap();
    let old_tracking = git(&fixture.second, &["rev-parse", "refs/remotes/origin/main"]);
    std::fs::write(fixture.second.join("untracked.txt"), "must remain").unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::Dirty)
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), original);
    assert_eq!(
        git(&fixture.second, &["rev-parse", "refs/remotes/origin/main"]),
        old_tracking
    );
    assert_eq!(
        std::fs::read_to_string(fixture.second.join("untracked.txt")).unwrap(),
        "must remain"
    );
    // Turn the preserved work into a real existing commit, creating divergence.
    git(&fixture.second, &["add", "untracked.txt"]);
    git(
        &fixture.second,
        &["commit", "-m", "independent second commit"],
    );
    let local = git(&fixture.second, &["rev-parse", "HEAD"]);
    let remote = fixture.remote_head();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::Diverged)
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), local);
    assert_eq!(fixture.remote_head(), remote);
    std::fs::write(fixture.second.join(".git/index.lock"), b"active writer").unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap(),
        CommitOutcome::Blocked(SyncBlock::ActiveWriter)
    );
    assert_eq!(
        std::fs::read(fixture.second.join(".git/index.lock")).unwrap(),
        b"active writer"
    );
}

#[tokio::test]
async fn real_checkout_lease_and_cancellation_cannot_start_a_second_writer() {
    let fixture = Fixture::new();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(fixture.first.join(".git/nds-sync.lock"))
        .unwrap();
    file.try_lock().unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::Busy
    );
    drop(file);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let original = fixture.remote_head();
    assert_eq!(
        fixture
            .workspace
            .run(
                fixture.first.clone(),
                fixture.endpoint.clone(),
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                move || {
                    let counted = counted.clone();
                    async move {
                        if counted.fetch_add(1, Ordering::SeqCst) >= 3 {
                            Err(ModuleError::Cancelled)
                        } else {
                            Ok(())
                        }
                    }
                }
            )
            .await
            .unwrap_err(),
        ModuleError::Cancelled
    );
    assert!(calls.load(Ordering::SeqCst) >= 4);
    assert_eq!(fixture.remote_head(), original);
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap(),
        CommitOutcome::UpToDate
    );
}

#[tokio::test]
async fn filters_includes_and_hooks_are_refused_without_execution() {
    let fixture = Fixture::new();
    let marker = fixture.first.join("must-not-exist");
    git(
        &fixture.first,
        &[
            "config",
            "filter.untrusted.smudge",
            &format!("touch {}", marker.display()),
        ],
    );
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert!(!marker.exists());
    git(
        &fixture.first,
        &["config", "--remove-section", "filter.untrusted"],
    );
    std::fs::write(
        fixture.first.join(".git/hooks/pre-push"),
        "untrusted hook must not execute",
    )
    .unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn unqualified_submodules_are_refused_before_worktree_filters_or_fast_forward() {
    let fixture = Fixture::new();
    let before = git(&fixture.second, &["rev-parse", "HEAD"]);
    git(
        &fixture.first,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            &fixture.endpoint,
            "child",
        ],
    );
    git(
        &fixture.first,
        &["commit", "-m", "external submodule commit"],
    );
    // The remote change is authored and published externally, not by NDS.
    git(&fixture.first, &["push", "origin", "main"]);
    let marker = fixture._directory.path().join("filter-must-not-run");
    let child = fixture.first.join("child");
    git(
        &child,
        &[
            "config",
            "filter.probe.clean",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(child.join(".gitattributes"), "initial.txt filter=probe\n").unwrap();
    std::fs::write(child.join("initial.txt"), "modified submodule work").unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert!(!marker.exists());
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), before);
    assert!(!fixture.second.join("child").exists());
}

#[tokio::test]
async fn public_workspace_checks_owner_layout_and_exact_remote_before_network() {
    use nddev_device_sync_domain::repositories::catalog::*;
    let fixture = Fixture::new();
    let repository = RepositorySummary {
        id: RepositoryId::new(11).unwrap(),
        owner: GithubOwner {
            id: GithubAccountId::new(5).unwrap(),
            kind: GithubOwnerKind::User,
            login: "personal".into(),
        },
        name: "first".into(),
        visibility: RepositoryVisibility::Private,
        archived: false,
        default_branch: Some("main".into()),
    };
    let location = CheckoutLocation::from_repository(&repository).unwrap();
    let before = fixture.remote_head();
    // Existing directories are never implicitly bound from their slug/URL.
    assert_eq!(
        fixture
            .workspace
            .synchronize(
                &location,
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) }
            )
            .await
            .unwrap_err(),
        ModuleError::Unavailable
    );
    std::fs::write(
        fixture.first.join(".git/nds-binding.json"),
        serde_json::to_vec(&CheckoutBinding {
            id: "10000000-0000-4000-8000-000000000001".into(),
            principal: fixture.workspace.principal.clone(),
            location: location.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    // This actual checkout has an isolated local origin; an NDS GitHub binding
    // must reject it rather than fetching/pushing to the caller-supplied origin.
    assert_eq!(
        fixture
            .workspace
            .synchronize(
                &location,
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        CommitOutcome::Blocked(SyncBlock::WrongRemote)
    );
    assert_eq!(fixture.remote_head(), before);
    let mut different = repository.clone();
    different.id = RepositoryId::new(12).unwrap();
    let different = CheckoutLocation::from_repository(&different).unwrap();
    assert_eq!(
        fixture
            .workspace
            .synchronize(
                &different,
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        CommitOutcome::Blocked(SyncBlock::WrongRemote)
    );
    assert_eq!(fixture.remote_head(), before);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            &fixture.first,
            fixture.workspace.root.join("personal/alias"),
        )
        .unwrap();
        let mut alias = repository;
        alias.name = "alias".into();
        let alias = CheckoutLocation::from_repository(&alias).unwrap();
        assert_eq!(
            fixture
                .workspace
                .synchronize(
                    &alias,
                    false,
                    "generated-invalid-token",
                    &CancellationToken::new(),
                    || async { Ok(()) }
                )
                .await
                .unwrap_err(),
            ModuleError::InvalidInput
        );
        assert_eq!(fixture.remote_head(), before);
    }
}

#[tokio::test]
async fn symbolic_tracking_ref_cannot_redirect_fetch_into_another_branch() {
    let fixture = Fixture::new();
    git(&fixture.second, &["branch", "preserve"]);
    git(
        &fixture.second,
        &[
            "symbolic-ref",
            "refs/remotes/origin/main",
            "refs/heads/preserve",
        ],
    );
    let before = git(&fixture.second, &["rev-parse", "refs/heads/preserve"]);
    commit(
        &fixture.first,
        "new.txt",
        "must not overwrite another local branch",
    );
    fixture.synchronize(&fixture.first).await.unwrap();
    assert!(matches!(
        fixture.synchronize(&fixture.second).await,
        Err(ModuleError::Unavailable) | Ok(CommitOutcome::Blocked(SyncBlock::WrongRemote))
    ));
    assert_eq!(
        git(&fixture.second, &["rev-parse", "refs/heads/preserve"]),
        before
    );
    assert_eq!(git(&fixture.second, &["rev-parse", "HEAD"]), before);
}

#[cfg(unix)]
#[tokio::test]
async fn git_metadata_symlink_is_refused_without_writing_the_external_directory() {
    let fixture = Fixture::new();
    let original = fixture.second.join(".git/refs/remotes/origin");
    let outside = fixture._directory.path().join("outside-tracking");
    // A clone may pack its remote ref. Materialize that same valid Git ref so
    // the external target's bytes can be compared before and after refusal.
    let head = git(&fixture.second, &["rev-parse", "refs/remotes/origin/main"]);
    std::fs::write(original.join("main"), format!("{head}\n")).unwrap();
    std::fs::rename(&original, &outside).unwrap();
    std::os::unix::fs::symlink(&outside, &original).unwrap();
    let before = std::fs::read(outside.join("main")).unwrap();
    commit(&fixture.first, "new.txt", "must remain outside the symlink");
    fixture.synchronize(&fixture.first).await.unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.second).await.unwrap_err(),
        ModuleError::Unavailable
    );
    assert_eq!(std::fs::read(outside.join("main")).unwrap(), before);
}

#[cfg(unix)]
#[tokio::test]
async fn actual_server_refusal_keeps_local_commit_and_remote_head() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let remote = fixture.remote_head();
    let local = commit(&fixture.first, "local.txt", "retained local commit");
    let hook = fixture.remote.join("hooks/pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        fixture.synchronize(&fixture.first).await.unwrap_err(),
        ModuleError::ProviderFailed
    );
    assert_eq!(fixture.remote_head(), remote);
    assert_eq!(git(&fixture.first, &["rev-parse", "HEAD"]), local);
    assert!(git(&fixture.first, &["status", "--porcelain"]).is_empty());
}

#[tokio::test]
async fn refused_dirty_checkout_does_not_prevent_the_next_repository_from_publishing() {
    let blocked = Fixture::new();
    let eligible = Fixture::new();
    let original_head = git(&blocked.first, &["rev-parse", "HEAD"]);
    let original_index = std::fs::read(blocked.first.join(".git/index")).unwrap();
    std::fs::write(blocked.first.join("initial.txt"), b"uncommitted work\n").unwrap();
    let published = commit(
        &eligible.first,
        "existing.txt",
        "existing commit to publish",
    );
    let mut outcomes = Vec::new();
    for fixture in [&blocked, &eligible] {
        outcomes.push(fixture.synchronize(&fixture.first).await.unwrap());
    }
    assert_eq!(
        outcomes,
        [
            CommitOutcome::Blocked(SyncBlock::Dirty),
            CommitOutcome::Published
        ]
    );
    assert_eq!(eligible.remote_head(), published);
    assert_eq!(git(&blocked.first, &["rev-parse", "HEAD"]), original_head);
    assert_eq!(
        std::fs::read(blocked.first.join(".git/index")).unwrap(),
        original_index
    );
    assert_eq!(
        std::fs::read(blocked.first.join("initial.txt")).unwrap(),
        b"uncommitted work\n"
    );
}

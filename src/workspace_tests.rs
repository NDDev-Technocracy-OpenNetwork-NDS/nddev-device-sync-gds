//! Actual local Git and filesystem acceptance. Provider identity facts here are
//! generated inputs; these tests do not qualify positive GitHub credentials.
use super::*;
use crate::commits::ConnectionOutcome;
use nddev_device_sync_domain::repositories::{catalog::*, workspace::ConnectionBlock};
use std::os::unix::fs::MetadataExt;

fn fixture_location(owner: &str, name: &str) -> CheckoutLocation {
    CheckoutLocation::from_repository(&RepositorySummary {
        id: RepositoryId::new(19).unwrap(),
        owner: GithubOwner {
            id: GithubAccountId::new(7).unwrap(),
            kind: GithubOwnerKind::Organization,
            login: owner.into(),
        },
        name: name.into(),
        visibility: RepositoryVisibility::Private,
        archived: false,
        default_branch: Some("main".into()),
    })
    .unwrap()
}
fn stages(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".nds-stage-")
        })
        .collect()
}
fn published_binding(outcome: ConnectionOutcome) -> CheckoutBinding {
    match outcome {
        ConnectionOutcome::Connected { binding, .. } => binding,
        ConnectionOutcome::Blocked(_) => panic!("unexpected connection refusal"),
    }
}

#[tokio::test]
async fn provider_identity_refusal_after_clone_cannot_publish_the_worktree() {
    let mut f = Fixture::new();
    f.workspace.fixture_remote = Some(f.endpoint.clone());
    let location = fixture_location("company", "third");
    let result = f
        .workspace
        .connect(
            &location,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async { Ok(()) },
            || async { Err(ModuleError::InvalidResponse) },
        )
        .await;
    assert!(matches!(result, Err(ModuleError::InvalidResponse)));
    assert!(!f.workspace.root.join(location.relative_path()).exists());
    assert!(!f.workspace.root.join(location.owner()).exists());
    assert!(stages(&f.workspace.root).is_empty());
}

#[tokio::test]
async fn already_cancelled_connection_never_waits_on_an_authority_callback() {
    let f = Fixture::new();
    let location = fixture_location("company", "third");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let calls = AtomicUsize::new(0);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        f.workspace.connect(
            &location,
            "generated-invalid-token",
            &cancellation,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending().await
            },
            || async { Ok(()) },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(ModuleError::Cancelled)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!f.workspace.root.join(".nds-workspace.lock").exists());
}
#[tokio::test]
async fn real_clone_is_scoped_idempotent_and_supports_existing_commit_publication() {
    let mut f = Fixture::new();
    f.workspace.fixture_remote = Some(f.endpoint.clone());
    let location = fixture_location("company", "third");
    let result = f
        .workspace
        .connect(
            &location,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async { Ok(()) },
            || async { Ok(()) },
        )
        .await
        .unwrap();
    assert!(matches!(
        &result,
        ConnectionOutcome::Connected { created: true, .. }
    ));
    let binding = published_binding(result);
    let checkout = f.workspace.root.join(location.relative_path());
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), f.remote_head());
    assert_eq!(
        git(&checkout, &["config", "--get", "remote.origin.url"]),
        location.https_url()
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("initial.txt")).unwrap(),
        "initial"
    );
    assert!(git(&checkout, &["status", "--porcelain"]).is_empty());
    assert!(binding.principal == f.workspace.principal && binding.location == location);
    assert!(stages(&f.workspace.root).is_empty());
    let repeated = published_binding(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) },
            )
            .await
            .unwrap(),
    );
    assert_eq!(binding.id, repeated.id);
    let head = commit(&checkout, "published.txt", "existing user commit");
    assert_eq!(
        f.workspace
            .synchronize(
                &location,
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        CommitOutcome::Published
    );
    assert_eq!(head, f.remote_head());
    assert_eq!(git(&checkout, &["rev-list", "--count", "HEAD"]), "2");
}

#[tokio::test]
async fn explicit_adoption_preserves_ssh_origin_dirty_work_index_and_branches() {
    let f = Fixture::new();
    let location = fixture_location("personal", "first");
    git(
        &f.first,
        &[
            "config",
            "remote.origin.url",
            "git@github.com:personal/first.git",
        ],
    );
    std::fs::write(f.first.join("initial.txt"), "staged user work").unwrap();
    git(&f.first, &["add", "initial.txt"]);
    std::fs::write(f.first.join("initial.txt"), "unstaged user work").unwrap();
    std::fs::write(f.first.join("untracked.txt"), "untracked user work").unwrap();
    let index = std::fs::read(f.first.join(".git/index")).unwrap();
    let config = std::fs::read(f.first.join(".git/config")).unwrap();
    let head = git(&f.first, &["rev-parse", "HEAD"]);
    let binding = published_binding(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) },
            )
            .await
            .unwrap(),
    );
    assert!(binding.location == location);
    assert_eq!(std::fs::read(f.first.join(".git/index")).unwrap(), index);
    assert_eq!(std::fs::read(f.first.join(".git/config")).unwrap(), config);
    assert_eq!(git(&f.first, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(f.first.join("initial.txt")).unwrap(),
        "unstaged user work"
    );
    assert_eq!(
        std::fs::read_to_string(f.first.join("untracked.txt")).unwrap(),
        "untracked user work"
    );
}

#[tokio::test]
async fn active_writers_and_a_different_nds_account_never_receive_a_binding() {
    let mut f = Fixture::new();
    let location = fixture_location("personal", "first");
    git(
        &f.first,
        &["config", "remote.origin.url", &location.https_url()],
    );
    std::fs::write(f.first.join(".git/index.lock"), "user-owned writer").unwrap();
    assert!(matches!(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        ConnectionOutcome::Blocked(ConnectionBlock::ActiveWriter)
    ));
    assert!(!f.first.join(".git/nds-binding.json").exists());
    std::fs::remove_file(f.first.join(".git/index.lock")).unwrap();
    published_binding(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) },
            )
            .await
            .unwrap(),
    );
    let before = std::fs::read(f.first.join(".git/nds-binding.json")).unwrap();
    f.workspace.principal.user = "different-owner".into();
    assert!(matches!(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        ConnectionOutcome::Blocked(ConnectionBlock::DifferentAccount)
    ));
    assert_eq!(
        std::fs::read(f.first.join(".git/nds-binding.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn occupied_paths_symlinks_and_case_collisions_preserve_user_content() {
    let f = Fixture::new();
    let location = fixture_location("company", "third");
    let owner = f.workspace.root.join("company");
    std::fs::create_dir(&owner).unwrap();
    let destination = owner.join("third");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("user.txt"), "preserve").unwrap();
    assert!(matches!(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied)
    ));
    assert_eq!(
        std::fs::read_to_string(destination.join("user.txt")).unwrap(),
        "preserve"
    );
    let alias = fixture_location("company", "alias");
    std::os::unix::fs::symlink(&f.first, owner.join("alias")).unwrap();
    assert!(matches!(
        f.workspace
            .connect(
                &alias,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied)
    ));
    let case = fixture_location("Company", "different");
    assert!(matches!(
        f.workspace
            .connect(
                &case,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) }
            )
            .await
            .unwrap(),
        ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied)
    ));
}

#[tokio::test]
async fn publication_never_replaces_even_an_empty_directory_created_during_clone() {
    let mut f = Fixture::new();
    f.workspace.fixture_remote = Some(f.endpoint.clone());
    let location = fixture_location("company", "third");
    let destination = f.workspace.root.join(location.relative_path());
    let created = std::sync::Mutex::new(None);
    let outcome = f
        .workspace
        .connect(
            &location,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async {
                if stages(&f.workspace.root)
                    .iter()
                    .any(|path| path.join("checkout/.git/nds-binding.json").is_file())
                    && !destination.exists()
                {
                    std::fs::create_dir(&destination).unwrap();
                    *created.lock().unwrap() = Some(std::fs::metadata(&destination).unwrap().ino());
                }
                Ok(())
            },
            || async { Ok(()) },
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied)
    ));
    assert_eq!(
        std::fs::metadata(&destination).unwrap().ino(),
        created.lock().unwrap().unwrap()
    );
    assert!(std::fs::read_dir(&destination).unwrap().next().is_none());
    assert!(stages(&f.workspace.root).is_empty());
}

#[tokio::test]
async fn cancellation_before_publication_cleans_only_the_owned_staging_checkout() {
    let mut f = Fixture::new();
    f.workspace.fixture_remote = Some(f.endpoint.clone());
    let location = fixture_location("company", "third");
    let before = std::fs::read(f.first.join("initial.txt")).unwrap();
    let result = f
        .workspace
        .connect(
            &location,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async {
                if stages(&f.workspace.root)
                    .iter()
                    .any(|path| path.join("checkout/.git/nds-binding.json").is_file())
                {
                    return Err(ModuleError::Cancelled);
                }
                Ok(())
            },
            || async { Ok(()) },
        )
        .await;
    assert!(matches!(result, Err(ModuleError::Cancelled)));
    assert!(!f.workspace.root.join(location.relative_path()).exists());
    assert!(stages(&f.workspace.root).is_empty());
    assert_eq!(std::fs::read(f.first.join("initial.txt")).unwrap(), before);
}

#[tokio::test]
async fn object_alternates_are_refused_without_reading_an_external_repository() {
    let f = Fixture::new();
    let location = fixture_location("personal", "first");
    git(
        &f.first,
        &["config", "remote.origin.url", &location.https_url()],
    );
    std::fs::create_dir_all(f.first.join(".git/objects/info")).unwrap();
    std::fs::write(
        f.first.join(".git/objects/info/alternates"),
        f.remote.join("objects").to_str().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        f.workspace
            .connect(
                &location,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async { Ok(()) },
                || async { Ok(()) }
            )
            .await,
        Err(ModuleError::Unavailable)
    ));
    assert!(!f.first.join(".git/nds-binding.json").exists());
}

#[tokio::test]
async fn moved_owner_directory_cannot_receive_a_clone_or_remove_its_replacement() {
    let mut f = Fixture::new();
    f.workspace.fixture_remote = Some(f.endpoint.clone());
    let location = fixture_location("company", "third");
    let owner = f.workspace.root.join("company");
    let moved = f.workspace.root.join("company-moved");
    let result = f
        .workspace
        .connect(
            &location,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async { Ok(()) },
            || {
                std::fs::rename(&owner, &moved).unwrap();
                std::fs::create_dir(&owner).unwrap();
                async { Ok(()) }
            },
        )
        .await;
    assert!(matches!(result, Err(ModuleError::Busy)));
    assert!(owner.is_dir(), "replacement directory must survive cleanup");
    assert!(!owner.join("third").exists());
    assert!(!moved.join("third").exists());
    assert!(stages(&f.workspace.root).is_empty());
}

#[tokio::test]
async fn adoption_rejects_a_git_directory_replaced_during_confirmation() {
    let f = Fixture::new();
    let location = fixture_location("personal", "first");
    git(
        &f.first,
        &["config", "remote.origin.url", &location.https_url()],
    );
    let directory = f.first.join(".git");
    let moved = f.first.parent().unwrap().join("first-git-held");
    let result = f
        .workspace
        .connect(
            &location,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async { Ok(()) },
            || {
                std::fs::rename(&directory, &moved).unwrap();
                std::os::unix::fs::symlink(&moved, &directory).unwrap();
                async { Ok(()) }
            },
        )
        .await;
    assert!(matches!(result, Err(ModuleError::Busy)));
    assert!(!moved.join("nds-binding.json").exists());
    assert!(
        directory.is_symlink(),
        "concurrent user path must be preserved"
    );
}

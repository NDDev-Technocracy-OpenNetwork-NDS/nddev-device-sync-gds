//! Real Git and native filesystem ownership, including a completed remote
//! effect whose final local proof is lost. No mock executor or fake provider.
use super::*;
use std::sync::atomic::AtomicBool;

fn replacement(fixture: &Fixture) -> PathBuf {
    use nddev_device_sync_domain::repositories::catalog::{GithubAccountId, RepositoryId};
    let path = fixture._directory.path().join("replacement");
    git(
        fixture._directory.path(),
        &[
            "clone",
            "--no-local",
            &fixture.endpoint,
            path.to_str().unwrap(),
        ],
    );
    // Identical portable binding bytes are intentionally insufficient to
    // replace the physical owner captured before the authorization await.
    let binding = serde_json::to_vec(&CheckoutBinding {
        id: "10000000-0000-4000-8000-000000000001".into(),
        principal: fixture.workspace.principal.clone(),
        location: CheckoutLocation::new(
            RepositoryId::new(11).unwrap(),
            GithubAccountId::new(5).unwrap(),
            "personal".into(),
            "first".into(),
        )
        .unwrap(),
    })
    .unwrap();
    for checkout in [&fixture.first, &path] {
        std::fs::write(checkout.join(".git/nds-binding.json"), &binding).unwrap();
    }
    path
}

#[tokio::test]
async fn replacing_checkout_across_authority_await_preserves_both_repositories() {
    for before_lease in [true, false] {
        let fixture = Fixture::new();
        let remote = fixture.remote_head();
        let local = commit(&fixture.first, "retained.txt", "original existing commit");
        let replacement = replacement(&fixture);
        let retained = fixture._directory.path().join("retained-checkout");
        let original_index = std::fs::read(fixture.first.join(".git/index")).unwrap();
        let replacement_index = std::fs::read(replacement.join(".git/index")).unwrap();
        let changed = AtomicBool::new(false);
        let result = fixture
            .workspace
            .run(
                fixture.first.clone(),
                fixture.endpoint.clone(),
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async {
                    if (before_lease || fixture.first.join(".git/nds-sync.lock").exists())
                        && !changed.swap(true, Ordering::SeqCst)
                    {
                        std::fs::rename(&fixture.first, &retained).unwrap();
                        std::fs::rename(&replacement, &fixture.first).unwrap();
                    }
                    Ok(())
                },
            )
            .await;
        assert!(changed.load(Ordering::SeqCst));
        assert_eq!(result.unwrap_err(), ModuleError::Busy);
        assert_eq!(fixture.remote_head(), remote);
        assert_eq!(git(&retained, &["rev-parse", "HEAD"]), local);
        assert_eq!(git(&fixture.first, &["rev-parse", "HEAD"]), remote);
        assert_eq!(
            std::fs::read(retained.join(".git/index")).unwrap(),
            original_index
        );
        assert_eq!(
            std::fs::read(fixture.first.join(".git/index")).unwrap(),
            replacement_index
        );
        assert_eq!(
            std::fs::read_to_string(retained.join("retained.txt")).unwrap(),
            "original existing commit"
        );
        if before_lease {
            assert!(!fixture.first.join(".git/nds-sync.lock").exists());
            assert!(!retained.join(".git/nds-sync.lock").exists());
        }
    }
}

#[tokio::test]
async fn replacing_only_git_directory_cannot_borrow_the_retained_checkout_lease() {
    let fixture = Fixture::new();
    let remote = fixture.remote_head();
    commit(&fixture.first, "retained.txt", "original working files");
    let replacement = replacement(&fixture);
    let retained = fixture._directory.path().join("retained-git");
    let original_index = std::fs::read(fixture.first.join(".git/index")).unwrap();
    let replacement_index = std::fs::read(replacement.join(".git/index")).unwrap();
    let changed = AtomicBool::new(false);
    let result = fixture
        .workspace
        .run(
            fixture.first.clone(),
            fixture.endpoint.clone(),
            false,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async {
                if fixture.first.join(".git/nds-sync.lock").exists()
                    && !changed.swap(true, Ordering::SeqCst)
                {
                    std::fs::rename(fixture.first.join(".git"), &retained).unwrap();
                    std::fs::rename(replacement.join(".git"), fixture.first.join(".git")).unwrap();
                }
                Ok(())
            },
        )
        .await;
    assert!(changed.load(Ordering::SeqCst));
    assert_eq!(result.unwrap_err(), ModuleError::Busy);
    assert_eq!(fixture.remote_head(), remote);
    assert_eq!(
        std::fs::read(retained.join("index")).unwrap(),
        original_index
    );
    assert_eq!(
        std::fs::read(fixture.first.join(".git/index")).unwrap(),
        replacement_index
    );
    assert_eq!(
        std::fs::read_to_string(fixture.first.join("retained.txt")).unwrap(),
        "original working files"
    );
}

#[tokio::test]
async fn replaced_missing_or_symlinked_lease_refuses_before_git_publication() {
    for kind in ["replacement", "missing", "symlink"] {
        let fixture = Fixture::new();
        let remote = fixture.remote_head();
        let local = commit(
            &fixture.first,
            "retained.txt",
            "unpublished existing commit",
        );
        let lease = fixture.first.join(".git/nds-sync.lock");
        let retained = fixture.first.join(".git/retained-sync.lock");
        let external = fixture._directory.path().join("foreign-lock");
        std::fs::write(&external, "foreign owner bytes").unwrap();
        let changed = AtomicBool::new(false);
        let result = fixture
            .workspace
            .run(
                fixture.first.clone(),
                fixture.endpoint.clone(),
                false,
                "generated-invalid-token",
                &CancellationToken::new(),
                || async {
                    if lease.exists() && !changed.swap(true, Ordering::SeqCst) {
                        std::fs::rename(&lease, &retained).unwrap();
                        match kind {
                            "replacement" => std::fs::write(&lease, "replacement owner").unwrap(),
                            "symlink" => std::os::unix::fs::symlink(&external, &lease).unwrap(),
                            "missing" => {}
                            _ => unreachable!(),
                        }
                    }
                    Ok(())
                },
            )
            .await;
        assert!(changed.load(Ordering::SeqCst));
        assert_eq!(result.unwrap_err(), ModuleError::Busy);
        assert_eq!(fixture.remote_head(), remote);
        assert_eq!(git(&fixture.first, &["rev-parse", "HEAD"]), local);
        assert_eq!(
            std::fs::read_to_string(&external).unwrap(),
            "foreign owner bytes"
        );
        assert!(retained.is_file());
        // The original flock is explicitly released on this terminal refusal.
        let original_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&retained)
            .unwrap();
        original_lock.try_lock().unwrap();
        original_lock.unlock().unwrap();
        match kind {
            "replacement" => assert_eq!(
                std::fs::read_to_string(&lease).unwrap(),
                "replacement owner"
            ),
            "symlink" => assert!(lease.is_symlink()),
            "missing" => assert!(!lease.exists()),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn lost_local_lease_after_real_push_keeps_the_effect_and_refuses_success() {
    let fixture = Fixture::new();
    let local = commit(&fixture.first, "published.txt", "completed remote effect");
    let lease = fixture.first.join(".git/nds-sync.lock");
    let retained = fixture.first.join(".git/retained-sync.lock");
    let changed = AtomicBool::new(false);
    let result = fixture
        .workspace
        .run(
            fixture.first.clone(),
            fixture.endpoint.clone(),
            false,
            "generated-invalid-token",
            &CancellationToken::new(),
            || async {
                if fixture.remote_head() == local && !changed.swap(true, Ordering::SeqCst) {
                    std::fs::rename(&lease, &retained).unwrap();
                    std::fs::write(&lease, "new native owner").unwrap();
                }
                Ok(())
            },
        )
        .await;
    assert!(
        changed.load(Ordering::SeqCst),
        "the real push must complete first"
    );
    assert_eq!(result.unwrap_err(), ModuleError::Busy);
    // Refusal after an effect is not rollback or proof that nothing was sent.
    assert_eq!(fixture.remote_head(), local);
    assert_eq!(git(&fixture.first, &["rev-parse", "HEAD"]), local);
    assert_eq!(std::fs::read_to_string(&lease).unwrap(), "new native owner");
    assert!(retained.is_file());
}

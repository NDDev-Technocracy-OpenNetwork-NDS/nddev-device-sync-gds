//! Actual admission refusal inside an owned Git session. Coordination supplies
//! no authority: only the same original proof can permit its next command.
use super::*;
use std::sync::atomic::Ordering;

async fn phase(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    io: Arc<FixtureIo>,
    database: PathBuf,
    cancel: bool,
) -> Result<Value> {
    settle(&vault).await?;
    let source = vault.github_source().await?;
    let consent = vault.module_configuration("gds").await?;
    let selected = Arc::new(runtime(
        identity,
        vault.clone(),
        io.clone(),
        (source.operation_id, consent.operation_id),
    ));
    let token = selected.collection.lock().unwrap().clone();
    let captures = io.captured_reads();
    selected.collect_commits(&token).await?;
    let (original, target) = {
        let state = selected.shared.commits.lock().unwrap();
        (
            state.guard.clone().ok_or("missing original retry proof")?,
            state
                .queue
                .front()
                .and_then(|item| item.location.clone())
                .ok_or("missing retry target")?,
        )
    };
    let attempts = io.git_attempts.load(Ordering::Relaxed);
    let retries = io.proof_retry_reads.load(Ordering::Relaxed);
    let git_pause = ProofPause::new();
    let retry_pause = ProofPause::new();
    *io.git_validation_pause.lock().unwrap() = Some(git_pause.clone());
    let collecting_owner = selected.clone();
    let mut collecting =
        tokio::spawn(async move { collecting_owner.collect_commits(&token).await });
    let mut collected = false;
    let mut locked = None;
    let mut writing = None;
    let outcome = async {
        // The second Git callback follows checkout-lease acquisition and is
        // before its first native command. This is not a replay of an effect.
        tokio::time::timeout(Duration::from_secs(20), git_pause.entered.notified()).await?;
        let lease_path = io.root.join(target.relative_path()).join(".git/nds-sync.lock");
        let lease = std::fs::OpenOptions::new().read(true).write(true).open(lease_path)?;
        let held = match lease.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(_) => false,
            Ok(()) => { lease.unlock()?; false }
        };
        assert!(held, "the original Git session must still own its checkout lease");
        *io.proof_retry_pause.lock().unwrap() = Some(retry_pause.clone());
        locked = Some(tokio::task::spawn_blocking(move || guard_acceptance::LockedDatabase::acquire(database)).await??);
        let writer = vault.clone();
        writing = Some(tokio::spawn(async move {
            writer.save_record(Some(format!("proof-retry-pressure-{cancel}")), 0, None,
                "Generated original proof pressure".into(), String::new(),
                Zeroizing::new("SYNTHETIC_PROOF_RETRY_PRESSURE".into())).await
        }));
        tokio::time::sleep(Duration::from_millis(10)).await;
        let actual_busy = matches!(vault.github_source().await, Err(nddev_device_sync_vault::Error::Busy));
        assert!(actual_busy, "counterexample requires actual native admission refusal");
        git_pause.release.notify_one();
        tokio::time::timeout(Duration::from_secs(20), retry_pause.entered.notified()).await?;
        let same_original = selected.shared.commits.lock().unwrap().guard.as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &original));
        assert!(same_original && !collecting.is_finished());
        if cancel { selected.cancel(); }
        locked.take().unwrap().release().await?;
        writing.take().unwrap().await??;
        retry_pause.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(30), &mut collecting).await??;
        collected = true;
        let positive = if cancel {
            matches!(result, Err(view::GithubCatalogError::Cancelled))
        } else {
            assert!(matches!(result, Ok(true)));
            let state = selected.shared.commits.lock().unwrap();
            state.guard.as_ref().is_some_and(|current| Arc::ptr_eq(current, &original))
                && state.epoch.as_ref().and_then(|epoch| state.observation(epoch, &target.repository_id.value().to_string()))
                    .is_some_and(|view| view.outcome == "up_to_date")
        };
        let one_attempt = io.git_attempts.load(Ordering::Relaxed) - attempts == 1;
        let one_capture = io.captured_reads() - captures == 1;
        let retry_observed = io.proof_retry_reads.load(Ordering::Relaxed) > retries;
        lease.try_lock()?;
        lease.unlock()?;
        assert!(positive && one_attempt && one_capture && retry_observed);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(json!({
            "phase":if cancel {"cancel_during_original_read_retry"} else {"recover_original_read_inside_git"},
            "passed":true,"actual_sqlite_admission_busy":actual_busy,"original_checkout_lease_held":held,
            "same_original_proof":same_original,"scope_captured_reads":1,"git_attempts":1,
            "proof_read_retry_observed":retry_observed,"no_git_attempt_replayed":one_attempt,
            "cancelled_result_refused":cancel,"checkout_lease_released_after_join":true
        }))
    }.await;
    // Every path retains and joins the existing operation. Never abandon a
    // paused Git future or a real pressure writer after a diagnostic failure.
    selected.cancel();
    git_pause.release.notify_one();
    retry_pause.release.notify_one();
    io.git_validation_pause.lock().unwrap().take();
    io.proof_retry_pause.lock().unwrap().take();
    if let Some(lock) = locked {
        lock.release().await?;
    }
    if let Some(writer) = writing {
        let _ = writer.await?;
    }
    if !collected {
        let _ = collecting.await?;
    }
    outcome
}

pub(super) async fn probe(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    io: Arc<FixtureIo>,
    database: PathBuf,
) -> Result<Value> {
    let mut phases = Vec::new();
    for cancel in [false, true] {
        phases.push(
            phase(
                identity.clone(),
                vault.clone(),
                io.clone(),
                database.clone(),
                cancel,
            )
            .await?,
        );
    }
    phases.push(authority_publication(identity, vault, io).await?);
    Ok(
        json!({"passed":true,"phases":phases,"same_guard_read_only_retry":true,
        "no_new_fence_or_scope":true,"no_native_effect_replay":true,"all_original_futures_joined":true}),
    )
}

async fn authority_publication(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    io: Arc<FixtureIo>,
) -> Result<Value> {
    settle(&vault).await?;
    let source = vault.github_source().await?;
    let consent = vault.module_configuration("gds").await?;
    let selected = Arc::new(runtime(
        identity.clone(),
        vault,
        io.clone(),
        (source.operation_id, consent.operation_id),
    ));
    let token = selected.collection.lock().unwrap().clone();
    let captures = io.captured_reads();
    selected.collect_commits(&token).await?;
    let original = selected
        .shared
        .commits
        .lock()
        .unwrap()
        .guard
        .clone()
        .ok_or("missing authority publication proof")?;
    let endpoint = identity.origin();
    let view = identity.view();
    let mut publication = tokio::spawn(async move {
        for _ in 0..16 {
            // Restore the SAME existing signed session. No sign-in, device
            // creation or scope capture is used to replace the reader's proof.
            let writer = IdentityClient::new(&endpoint)?;
            let restored = writer.refresh().await?;
            assert!(restored.authenticated && restored.offline_authorized);
            assert_eq!(restored.user_id, view.user_id);
            assert_eq!(restored.current_device_id, view.current_device_id);
            writer.maintain_authorization().await?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let mut busy = 0usize;
    let mut reads = 0usize;
    let observation = tokio::time::timeout(Duration::from_secs(120), async {
        while !publication.is_finished() {
            match original.validate_original_identity_for_fixture().await {
                Ok(()) => {}
                Err(view::GithubCatalogError::Busy) => {
                    busy += 1;
                }
                Err(error) => {
                    return Err::<(), Box<dyn std::error::Error + Send + Sync>>(error.into());
                }
            }
            reads += 1;
        }
        Ok(())
    })
    .await;
    // A failed observation cannot abandon a publishing identity operation.
    let published = (&mut publication).await?;
    let result = async {
        observation??;
        published?;
        original.validate().await?;
        assert!(busy > 0, "actual same-authority publication must reproduce a protected-read Busy");
        assert!(reads > busy && io.captured_reads() - captures == 1);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
        json!({"phase":"actual_authority_slot_publication","passed":true,
        "actual_identity_read_busy":busy,"protected_reads":reads,"same_existing_session_restored":true,
        "source":"Accounts557 cached_at_fence compares the actual slot before/after protected I/O",
        "original_proof_valid_after_publication":true,"scope_captured_reads":1,"no_signin_or_new_device":true,"writer_joined":true}),
        )
    }.await;
    selected.cancel();
    result
}

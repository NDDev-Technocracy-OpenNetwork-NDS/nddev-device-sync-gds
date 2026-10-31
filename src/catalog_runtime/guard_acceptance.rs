//! Actual SQLite contention against the original native catalog proof. The
//! timing barrier supplies no authority and does not fabricate a guard result.
use super::*;
use std::io::{BufRead, Write};
use std::process::{Child, Stdio};

pub(super) struct LockedDatabase(Option<Child>);
impl LockedDatabase {
    pub(super) fn acquire(database: PathBuf) -> Result<Self> {
        let mut held = Self(Some(Command::new("python3")
            .args(["-I", "-c", "import sqlite3,sys; c=sqlite3.connect(sys.argv[1],timeout=.5); c.execute('BEGIN IMMEDIATE'); print('locked',flush=True); sys.stdin.buffer.read(1); c.rollback()"])
            .arg(database).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?));
        let mut ready = String::new();
        std::io::BufReader::new(held.0.as_mut().unwrap().stdout.take().unwrap())
            .read_line(&mut ready)?;
        if ready != "locked\n" {
            return Err("owned SQLite lock did not initialize".into());
        }
        Ok(held)
    }
    pub(super) async fn release(mut self) -> Result<()> {
        self.0
            .as_mut()
            .unwrap()
            .stdin
            .take()
            .unwrap()
            .write_all(b"\n")?;
        assert!(
            tokio::task::spawn_blocking(move || self.0.as_mut().unwrap().wait())
                .await??
                .success()
        );
        Ok(())
    }
}
impl Drop for LockedDatabase {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn phase(
    runtime: Arc<CatalogRuntime>,
    vault: Arc<VaultClient>,
    io: Arc<FixtureIo>,
    database: PathBuf,
    after: bool,
) -> Result<Value> {
    let (original, cursor, queue, previous, current) = {
        let state = runtime.shared.commits.lock().unwrap();
        (
            state.guard.clone().ok_or("missing original proof")?,
            state.after.clone(),
            state.queue.len(),
            state
                .observation(state.epoch.as_ref().unwrap(), "1")
                .ok_or("missing actual first outcome")?
                .outcome,
            state
                .queue
                .front()
                .ok_or("missing next repository")?
                .location
                .as_ref()
                .unwrap()
                .repository_id
                .value()
                .to_string(),
        )
    };
    let pause = Arc::new(ValidationPause {
        after,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    *io.validation_pause.lock().unwrap() = Some(pause.clone());
    let token = runtime.collection.lock().unwrap().clone();
    let selected = runtime.clone();
    let collecting = tokio::spawn(async move { selected.collect_commits(&token).await });
    tokio::time::timeout(Duration::from_secs(20), pause.entered.notified()).await?;
    let locked = tokio::task::spawn_blocking(move || LockedDatabase::acquire(database)).await??;
    let writing = {
        let vault = vault.clone();
        tokio::spawn(async move {
            vault
                .save_record(
                    Some(format!("guard-pressure-{after}")),
                    0,
                    None,
                    "Generated guard contention".into(),
                    String::new(),
                    Zeroizing::new("SYNTHETIC_GUARD_PRESSURE".into()),
                )
                .await
        })
    };
    // Let the real write enter its 500ms SQLite busy wait. The following actual
    // admission result, not this short coordination delay, proves contention.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let busy = matches!(
        vault.github_source().await,
        Err(nddev_device_sync_vault::Error::Busy)
    );
    pause.release.notify_one();
    let collected = tokio::time::timeout(Duration::from_secs(10), collecting).await??;
    let (same, retained_cursor, retained_prior, refused) = {
        let state = runtime.shared.commits.lock().unwrap();
        (
            state
                .guard
                .as_ref()
                .is_some_and(|g| Arc::ptr_eq(g, &original)),
            state.after == cursor && state.queue.len() == queue - usize::from(after),
            state
                .epoch
                .as_ref()
                .and_then(|e| state.observation(e, "1"))
                .is_some_and(|v| v.outcome == previous),
            state
                .epoch
                .as_ref()
                .and_then(|e| state.observation(e, &current))
                .is_some_and(|v| v.outcome == "refused" && v.reason == "operation_busy"),
        )
    };
    locked.release().await?;
    writing.await??;
    settle(&vault).await?;
    let revalidated = original.validate().await.is_ok();
    Ok(
        json!({"phase":if after {"after_git"} else {"before_git"},"actual_vault_busy":busy,
        "original_proof_retained":same,"cursor_retained":retained_cursor,"prior_outcome_retained":retained_prior,
        "current_refusal_recorded":!after || refused,"original_proof_valid_after_unlock":revalidated,
        "returned_busy":matches!(collected,Err(view::GithubCatalogError::Busy))}),
    )
}

pub(super) async fn probe(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    io: Arc<FixtureIo>,
    database: PathBuf,
) -> Result<Value> {
    settle(&vault).await?;
    let source = vault.github_source().await?;
    let consent = vault.module_configuration("github").await?;
    let selected = Arc::new(runtime(
        identity.clone(),
        vault.clone(),
        io.clone(),
        (source.operation_id, consent.operation_id),
    ));
    let token = selected.collection.lock().unwrap().clone();
    let before = io.captured_reads();
    selected.collect_commits(&token).await?;
    let original = selected
        .shared
        .commits
        .lock()
        .unwrap()
        .guard
        .clone()
        .unwrap();
    let other_facade = selected.foreground(identity);
    let foreign_rejected = matches!(
        other_facade
            .synchronize_connection(&location(1), Some(original.clone()), &token)
            .await,
        Err(view::GithubCatalogError::Cancelled)
    );
    assert!(
        foreign_rejected,
        "another facade cannot borrow a retained read"
    );
    let mut phases = Vec::new();
    for after in [false, true] {
        let facts = phase(
            selected.clone(),
            vault.clone(),
            io.clone(),
            database.clone(),
            after,
        )
        .await?;
        let passed = [
            "actual_vault_busy",
            "original_proof_retained",
            "cursor_retained",
            "prior_outcome_retained",
            "current_refusal_recorded",
            "original_proof_valid_after_unlock",
            "returned_busy",
        ]
        .iter()
        .all(|key| facts[*key] == true);
        phases.push(facts);
        if !passed {
            return Ok(json!({"passed":false,"phases":phases}));
        }
    }
    let same_scope_captures = io.captured_reads() - before;
    assert_eq!(
        same_scope_captures, 1,
        "contention retries cannot recapture the scope"
    );
    let setting = vault.module_configuration("github").await?;
    vault
        .set_module_configuration(
            "github".into(),
            setting.revision,
            setting.pending_id,
            Configuration {
                enabled: false,
                permissions: Default::default(),
            },
        )
        .await?;
    settle(&vault).await?;
    assert!(!selected.collect_commits(&token).await?);
    let cleared = {
        let state = selected.shared.commits.lock().unwrap();
        state.guard.is_none()
            && state.queue.is_empty()
            && state
                .observation(state.epoch.as_ref().unwrap(), "1")
                .is_none()
    };
    let setting = vault.module_configuration("github").await?;
    vault
        .set_module_configuration(
            "github".into(),
            setting.revision,
            setting.pending_id,
            Configuration {
                enabled: true,
                permissions: write_permissions(),
            },
        )
        .await?;
    settle(&vault).await?;
    assert!(original.validate().await.is_err());
    assert!(selected.collect_commits(&token).await?);
    let replacement = selected
        .shared
        .commits
        .lock()
        .unwrap()
        .guard
        .clone()
        .unwrap();
    let renewed = !Arc::ptr_eq(&original, &replacement)
        && replacement.validate().await.is_ok()
        && io.captured_reads() - before == 2;
    assert!(renewed, "a new consent scope requires its own fresh read");
    selected.cancel();
    let cancelled_rejected = matches!(
        selected
            .background
            .synchronize_connection(&location(1), Some(replacement), &CancellationToken::new())
            .await,
        Err(view::GithubCatalogError::Cancelled)
    );
    assert!(
        cancelled_rejected,
        "a new flight cannot revive a cancelled read"
    );
    Ok(
        json!({"passed":cleared && renewed && foreign_rejected && cancelled_rejected,"phases":phases,"actual_disable_clears_scope":cleared,
        "same_scope_captured_reads":same_scope_captures,"original_read_retained_for_effects":true,
        "original_proof_rejected_after_reenable":true,"reenabled_scope_captures_new_read":renewed,
        "other_facade_read_rejected":foreign_rejected,"cancelled_lifetime_read_rejected":cancelled_rejected}),
    )
}

//! Bounded native foreground canary beside the actual periodic collector.
//! Retry only Vault's known pre-commit Busy refusal; unknown writes are never
//! replayed. The acknowledgement still requires a real committed record.
use super::*;
use guard_acceptance::LockedDatabase;
use nddev_device_sync_vault::Error;

async fn probe(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    index: usize,
    first_busy: Option<Arc<tokio::sync::Notify>>,
) -> Result<Value> {
    let started = std::time::Instant::now();
    let lifetime = identity.local_use_lifetime();
    let mut write_busy = 0;
    let mut read_busy = 0;
    let mut phase = "original_authority";
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let fence = identity.local_use_fence_from(&lifetime).await?;
        let id = format!("online-canary-{index}");
        phase = "write";
        loop {
            identity.check_captured_local_use_lifetime(&lifetime)?;
            identity.validate_local_use(&fence).await?;
            match vault
                .save_record(
                    Some(id.clone()),
                    0,
                    None,
                    "Generated concurrent Vault canary".into(),
                    String::new(),
                    Zeroizing::new("SYNTHETIC_ONLINE_FLEET_VALUE".into()),
                )
                .await
            {
                Ok(_) => break,
                // Pinned Vault joins native I/O, returns Busy before commit,
                // and has no fallible read after its durable enqueue receipt.
                // Keep the same ID/CAS; never retry Unavailable or a deadline.
                Err(Error::Busy) => write_busy += 1,
                Err(error) => return Err(error.into()),
            }
            if let Some(signal) = &first_busy {
                signal.notify_one();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        phase = "ack_read";
        loop {
            identity.check_captured_local_use_lifetime(&lifetime)?;
            identity.validate_local_use(&fence).await?;
            match vault.records(String::new()).await {
                Ok(page)
                    if page.items.iter().any(|record| {
                        record.id == id && record.pending_id.is_none() && record.revision > 0
                    }) =>
                {
                    identity.validate_local_use(&fence).await?;
                    identity.check_captured_local_use_lifetime(&lifetime)?;
                    return Ok::<(), Box<dyn std::error::Error + Send + Sync>>(());
                }
                Ok(_) => {}
                Err(Error::Busy) => {
                    read_busy += 1;
                    if let Some(signal) = &first_busy {
                        signal.notify_one();
                    }
                }
                Err(error) => return Err(error.into()),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    eprintln!(
        "{}",
        json!({"canary_progress":{"index":index,"phase":phase,"write_busy":write_busy,
            "read_busy":read_busy,"milliseconds":started.elapsed().as_millis(),
            "completed":matches!(&result, Ok(Ok(())))}})
    );
    result.map_err(|_| {
        format!("online canary45s deadline in {phase}; unknown mutation is not replayed")
    })??;
    Ok(
        json!({"acknowledged":true,"milliseconds":started.elapsed().as_millis(),
        "write_busy":write_busy,"read_busy":read_busy,"original_authority_valid":true,
        "mutation_replayed_after_commit":false,"actual_sqlite_pressure":false}),
    )
}

pub(super) async fn run(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    index: usize,
    database: Option<PathBuf>,
) -> Result<Value> {
    let Some(database) = database else {
        return probe(identity, vault, index, None).await;
    };
    let locked = tokio::task::spawn_blocking(move || LockedDatabase::acquire(database)).await??;
    let pressure = {
        let vault = vault.clone();
        tokio::spawn(async move {
            vault
                .save_record(
                    Some("online-canary-pressure".into()),
                    0,
                    None,
                    "Generated canary contention".into(),
                    String::new(),
                    Zeroizing::new("SYNTHETIC_CANARY_PRESSURE".into()),
                )
                .await
        })
    };
    // Only a coordination delay. An actual Busy reply below proves contention.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let signal = Arc::new(tokio::sync::Notify::new());
    let observed = signal.notified();
    let canary = tokio::spawn(probe(identity, vault, index, Some(signal.clone())));
    let noticed = tokio::time::timeout(Duration::from_secs(10), observed).await;
    // Join both admitted tasks even when the contention expectation fails.
    let released = locked.release().await;
    let pressure = pressure.await;
    let canary = canary.await;
    released?;
    assert!(matches!(pressure?, Ok(_) | Err(Error::Busy)));
    let mut facts = canary??;
    noticed?;
    assert!(facts["write_busy"].as_u64().unwrap() + facts["read_busy"].as_u64().unwrap() > 0);
    facts["actual_sqlite_pressure"] = json!(true);
    Ok(facts)
}

//! Provider I/O sequencing over Core decisions and Vault's immutable captures.
//! A resolution pass never counts as an accepted catalog traversal page.
use super::*;
use catalog::observations::PreparedPage;
use nddev_device_sync_domain::repositories::catalog::observations::{
    ObservationContext, RepositoryObservation,
};
use nddev_device_sync_vault::RepositoryResolution;
use std::collections::{BTreeMap, VecDeque};

pub(super) struct ScopeResolution {
    pub snapshot: RepositoryResolution,
    cancellation: CancellationToken,
}
impl Drop for ScopeResolution {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) struct ResolutionChoice {
    snapshot: RepositoryResolution,
    observation: RepositoryObservation,
    observed_at_ms: u64,
}
pub(super) struct ResolutionBatch {
    pub context: ObservationContext,
    pub choices: Mutex<VecDeque<Arc<ResolutionChoice>>>,
}

async fn capture(
    vault: &VaultClient,
    read: Arc<RepositoryRead>,
    key: &ObservationKey,
) -> Result<RepositoryResolution, GithubCatalogError> {
    let snapshot = vault.repository_resolution(read, key).await?;
    // Let an existing ordinary queued operation reach its actual outcome.
    // Dispatched/Unknown work is already refused by Vault's capture boundary.
    if snapshot.pending().is_some() && !snapshot.pending_is_blocked()
        || snapshot.fresh_read()?.is_none()
    {
        return Err(GithubCatalogError::Busy);
    }
    Ok(snapshot)
}

pub(super) async fn capture_scope(
    port: &CatalogPort,
    page: &PreparedPage,
) -> Result<Option<Arc<ScopeResolution>>, GithubCatalogError> {
    let key = ObservationKey::Scope {
        principal: page.context.principal,
        record_ref: page.context.selection.record_ref().into(),
    };
    if !port
        .read
        .baselines()
        .iter()
        .any(|(id, v)| *id == key.entity_id() && v.conflicted)
    {
        return Ok(None);
    }
    // This lifetime spans the full traversal, independently of any one page.
    let cancellation = port.guard.lifetime.child_token();
    let mut read = port
        .guard
        .vault
        .capture_configured_repository_observations(
            &super::super::required_permissions(),
            &port.source_operation,
            port.guard.fence.clone(),
            cancellation.clone(),
        )
        .await?;
    port.guard
        .vault
        .bind_repository_caller(&mut read, port.caller.clone(), port.caller_fence.clone())
        .await?;
    let read = Arc::new(read);
    let snapshot = capture(&port.guard.vault, read, &key).await?;
    Ok(Some(Arc::new(ScopeResolution {
        snapshot,
        cancellation,
    })))
}

pub(super) async fn prepare_batch(
    port: &CatalogPort,
    page: &PreparedPage,
    after: Option<&CatalogCursor>,
    cancellation: &CancellationToken,
) -> Result<Option<ResolutionBatch>, GithubCatalogError> {
    let mut snapshots = BTreeMap::new();
    for candidate in &page.candidates {
        if candidate.expected().conflicted {
            let key = candidate.observation().key();
            let snapshot = capture(&port.guard.vault, port.read.clone(), &key).await?;
            snapshots.insert(key, snapshot);
        }
    }
    if snapshots.is_empty() {
        return Ok(None);
    }
    // All captures precede this real read. The initial page is not relabelled
    // with a new marker, and no original operation is rebased/requalified.
    let fresh = catalog::observations::observation_page(
        port,
        port.guard.selection.clone(),
        after,
        uuid::Uuid::new_v4().to_string(),
        cancellation,
    )
    .await?;
    port.guard.validate().await?;
    let mut choices = VecDeque::new();
    for candidate in fresh.candidates {
        let observation = candidate.observation();
        if let Some(snapshot) = snapshots.remove(&observation.key()) {
            choices.push_back(Arc::new(ResolutionChoice {
                snapshot,
                observation: observation.clone(),
                observed_at_ms: fresh.observed_at_ms,
            }));
        }
    }
    // Missing keys may have moved between provider pages. Their absence does
    // not resolve/delete anything; the normal traversal still has to reread.
    Ok(Some(ResolutionBatch {
        context: fresh.context,
        choices: Mutex::new(choices),
    }))
}

pub(super) async fn submit(
    vault: &VaultClient,
    snapshot: &RepositoryResolution,
    observation: &RepositoryObservation,
    context: &ObservationContext,
    observed_at_ms: u64,
) -> Result<RepositoryPublication, GithubCatalogError> {
    // Reconstruct the pure guard from the SAME immutable pre-provider snapshot
    // on retry. This performs no new read and cannot acquire a newer baseline.
    let fresh = snapshot.fresh_read()?.ok_or(GithubCatalogError::Busy)?;
    let result = vault
        .resolve_repository_observation(
            snapshot,
            fresh,
            observation.clone(),
            Some(context),
            observed_at_ms,
        )
        .await?;
    if matches!(result, RepositoryPublication::Queued { .. }) {
        tracing::info!(
            event.name = "gds.catalog.conflict.queued",
            module = "gds",
            outcome = "ok"
        );
    }
    Ok(result)
}

pub(super) async fn submit_choice(
    vault: &VaultClient,
    choice: &ResolutionChoice,
    context: &ObservationContext,
) -> Result<RepositoryPublication, GithubCatalogError> {
    submit(
        vault,
        &choice.snapshot,
        &choice.observation,
        context,
        choice.observed_at_ms,
    )
    .await
}

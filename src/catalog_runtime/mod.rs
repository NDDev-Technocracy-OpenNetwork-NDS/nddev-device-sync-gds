//! Shared Rust catalog composition. Core owns traversal/publication decisions,
//! Vault owns encrypted state and fences, and GDS owns the fixed provider I/O.
mod consent;
mod driver;
mod sync_state;
pub mod view;
pub use driver::{CatalogClient, caller_error as classify_error};
use nddev_device_sync_accounts::IdentityClient;
use nddev_device_sync_domain::{Permission, modules::ModuleObservation};
use nddev_device_sync_vault::VaultClient;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};
use tracing::Instrument;

pub fn required_permissions() -> BTreeSet<Permission> {
    BTreeSet::from([
        Permission::ReadOnly,
        Permission::CredentialStore,
        Permission::LocalNetwork,
    ])
}

pub fn workspace_supported() -> bool {
    cfg!(all(
        feature = "commit-sync",
        any(target_os = "linux", target_os = "macos")
    ))
}

pub(super) type Observations = Arc<Mutex<Option<Observed>>>;
pub(super) struct Observed {
    pub epoch: (Option<String>, Option<String>),
    pub value: ModuleObservation,
}

/// One provider admission and observation, with separate bounded cursor
/// lifetimes for presentation and the retained replica collector.
pub struct CatalogRuntime {
    identity: Arc<IdentityClient>,
    authority: Arc<nddev_device_sync_accounts::LocalUseLifetime>,
    shared: Arc<CatalogResources>,
    background: Arc<CatalogClient>,
    vault: Arc<VaultClient>,
    work: Mutex<WorkSchedule>,
    collection: Mutex<tokio_util::sync::CancellationToken>,
}
pub(super) struct CatalogResources {
    observation: Observations,
    admission: Arc<tokio::sync::Semaphore>,
    cache: Arc<crate::github::CatalogCache>,
    commits: Mutex<sync_state::CommitState>,
}
struct WorkSchedule {
    epoch: Option<(Option<String>, Option<String>)>,
    next: Option<nddev_device_sync_accounts::runtime::Instant>,
}
impl CatalogRuntime {
    pub fn new(identity: Arc<IdentityClient>, vault: Arc<VaultClient>) -> Self {
        let authority = Arc::new(identity.local_use_lifetime());
        let observation = Arc::new(Mutex::new(None));
        let admission = Arc::new(tokio::sync::Semaphore::new(1));
        let cache = Arc::new(crate::github::CatalogCache::default());
        let shared = Arc::new(CatalogResources {
            observation,
            admission,
            cache,
            commits: Mutex::default(),
        });
        Self {
            identity: identity.clone(),
            authority: authority.clone(),
            shared: shared.clone(),
            background: Arc::new(CatalogClient::new(
                identity.clone(),
                authority,
                identity,
                vault.clone(),
                shared,
            )),
            vault,
            work: Mutex::new(WorkSchedule {
                epoch: None,
                next: Some(nddev_device_sync_accounts::runtime::Instant::now()),
            }),
            collection: Mutex::default(),
        }
    }
    pub fn foreground(&self, caller: Arc<IdentityClient>) -> Arc<CatalogClient> {
        Arc::new(CatalogClient::new(
            self.identity.clone(),
            self.authority.clone(),
            caller,
            self.vault.clone(),
            self.shared.clone(),
        ))
    }
    pub fn cancel(&self) {
        self.background.cancel_github_catalog();
        let mut collection = self.collection.lock().unwrap_or_else(|e| e.into_inner());
        collection.cancel();
        *collection = Default::default();
        self.shared
            .commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancel();
    }
    /// Health belongs to the exact source and consent that were actually read.
    /// A new selection/configuration cannot inherit another probe's readiness.
    pub async fn observation(&self) -> Option<ModuleObservation> {
        self.identity
            .check_captured_local_use_lifetime(&self.authority)
            .ok()?;
        let source = self.vault.github_source().await.ok()?;
        let consent = self.vault.module_configuration("gds").await.ok()?;
        self.identity
            .check_captured_local_use_lifetime(&self.authority)
            .ok()?;
        if source.conflicted || source.selection.is_none() || consent.conflicted {
            return None;
        }
        let epoch = (source.operation_id, consent.operation_id);
        self.shared
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|observation| observation.epoch == epoch)
            .map(|observation| observation.value.clone())
    }
    async fn collect_page(&self) -> Result<bool, view::GithubCatalogError> {
        use nddev_device_sync_accounts::runtime::Instant;
        use std::time::Duration;
        self.identity
            .check_captured_local_use_lifetime(&self.authority)?;
        let source = self.vault.github_source().await?;
        let consent = self.vault.module_configuration("gds").await?;
        let epoch = (source.operation_id, consent.operation_id);
        let changed = {
            let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
            let changed = work.epoch.as_ref() != Some(&epoch);
            if changed {
                work.epoch = Some(epoch.clone());
                work.next = Some(Instant::now());
            }
            changed
        };
        if changed {
            self.background.cancel_github_catalog();
            let mut observed = self
                .shared
                .observation
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if observed.as_ref().is_some_and(|value| value.epoch != epoch) {
                *observed = None;
            }
        }
        if source.conflicted
            || source.selection.is_none()
            || consent.conflicted
            || consent
                .configuration
                .admit(true, true, &required_permissions())
                .is_err()
        {
            return Ok(false);
        }
        if self
            .work
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .next
            .is_none_or(|next| Instant::now() < next)
        {
            return Ok(false);
        }
        let result = self
            .background
            .github_repository_page(self.background.continuation())
            .await;
        let (more, delay) = match &result {
            Ok(page) if page.continuation == view::GithubCatalogContinuationView::More => {
                (true, Duration::ZERO)
            }
            Ok(_) => (false, Duration::from_secs(300)),
            Err(view::GithubCatalogError::Busy | view::GithubCatalogError::CapacityExhausted) => (
                true,
                crate::github::retry_delay().max(Duration::from_secs(1)),
            ),
            Err(
                view::GithubCatalogError::Unavailable
                | view::GithubCatalogError::InvalidInput
                | view::GithubCatalogError::InvalidResponse,
            ) => (false, Duration::from_secs(3600)),
            Err(_) => (
                false,
                crate::github::retry_delay().max(Duration::from_secs(60)),
            ),
        };
        self.work
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .next = Instant::now().checked_add(delay);
        Ok(more)
    }

    async fn collect_commits(
        &self,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, view::GithubCatalogError> {
        use view::{GithubCatalogError, GithubCommitSyncView};
        if !workspace_supported() || cancellation.is_cancelled() {
            return Ok(false);
        }
        self.identity
            .check_captured_local_use_lifetime(&self.authority)?;
        let source = self.vault.github_source().await?;
        let consent = self.vault.module_configuration("gds").await?;
        let epoch = (source.operation_id, consent.operation_id);
        if source.conflicted
            || source.selection.is_none()
            || consent.conflicted
            || consent
                .configuration
                .admit(
                    true,
                    true,
                    &nddev_device_sync_domain::repositories::workspace::write_permissions(),
                )
                .is_err()
        {
            self.shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancel();
            return Ok(false);
        }
        let original = self
            .shared
            .commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .guard
            .clone();
        if let Some(original) = original
            && original.validate().await.is_err()
        {
            self.shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancel();
        }
        let after = {
            let mut commits = self
                .shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            commits.reset(epoch.clone());
            if !commits.ready() {
                return Ok(false);
            }
            if commits.queue.is_empty() && commits.end {
                commits.finish_round();
                return Ok(false);
            }
            commits.queue.is_empty().then(|| commits.after.clone())
        };
        if let Some(after) = after {
            let page = self.vault.repository_connection_page(after).await?;
            if cancellation.is_cancelled() {
                return Err(GithubCatalogError::Cancelled);
            }
            let mut commits = self
                .shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if commits.epoch.as_ref() != Some(&epoch) {
                return Err(GithubCatalogError::Cancelled);
            }
            commits.queue = page.items.into();
            commits.after = page.next_after;
            commits.end = !page.has_more;
        }
        // Remove before I/O: every refusal advances the finite traversal.
        let current = self
            .shared
            .commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .pop_front();
        let Some(current) = current else {
            self.shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish_round();
            return Ok(false);
        };
        let location = current
            .location
            .ok_or(GithubCatalogError::InvalidResponse)?;
        let id = location.repository_id.value().to_string();
        #[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
        let result = self
            .background
            .synchronize_connection(&location, cancellation)
            .await;
        #[cfg(not(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos"))))]
        let result: Result<
            nddev_device_sync_application::repositories::commits::CommitOutcome,
            _,
        > = Err(GithubCatalogError::Unavailable);
        self.identity
            .check_captured_local_use_lifetime(&self.authority)?;
        if cancellation.is_cancelled() {
            return Err(GithubCatalogError::Cancelled);
        }
        let original = self
            .shared
            .commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .guard
            .clone();
        let Some(original) = original else {
            // Refusal before a read was admitted still advances traversal; it
            // cannot publish a running/success observation under another scope.
            return Ok(true);
        };
        if original.validate().await.is_err() {
            self.shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancel();
            return Err(GithubCatalogError::Cancelled);
        }
        use nddev_device_sync_application::repositories::commits::CommitOutcome;
        let (outcome, reason) = match result {
            Ok(CommitOutcome::UpToDate) => ("up_to_date", String::new()),
            Ok(CommitOutcome::Published) => ("published", String::new()),
            Ok(CommitOutcome::FastForwarded) => ("fast_forwarded", String::new()),
            Ok(CommitOutcome::Blocked(reason)) => (
                "blocked",
                serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "invalid_response".into()),
            ),
            Err(error) => ("refused", classify_error(&error).0),
        };
        let mut progress = self
            .shared
            .commits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cancellation.is_cancelled()
            || !progress
                .guard
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &original))
        {
            return Err(GithubCatalogError::Cancelled);
        }
        progress.record(
            &epoch,
            id,
            GithubCommitSyncView {
                outcome: outcome.into(),
                reason,
                observed_at_ms: crate::github::now_ms().ok().map(|v| v.to_string()),
            },
        );
        Ok(true)
    }
}

impl nddev_device_sync_application::repositories::catalog::collection::CatalogCollector
    for CatalogRuntime
{
    fn collect(
        self: Arc<Self>,
    ) -> nddev_device_sync_application::repositories::catalog::collection::Collection {
        // Capture at admission, before spawning; cancellation before the first
        // poll cannot create a fresh grant or start another Git operation.
        let cancellation = self
            .collection
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Default::default();
            }
            let before = self.vault.snapshot().local_revision;
            let result = observe(
                "gds",
                "gds.catalog.collection",
                async {
                    nddev_device_sync_accounts::runtime::timeout(
                        std::time::Duration::from_secs(15),
                        self.collect_page(),
                    )
                    .await
                    .map_err(|_| view::GithubCatalogError::Timeout)?
                },
                view::GithubCatalogError::Unavailable,
                classify_error,
            )
            .await;
            let commits = self.collect_commits(&cancellation).await;
            nddev_device_sync_application::repositories::catalog::collection::CollectionProgress {
                local_changes: self.vault.snapshot().local_revision != before,
                continue_traversal: matches!(result, Ok(true)) || matches!(commits, Ok(true)),
            }
        })
    }
    fn cancel(&self) {
        CatalogRuntime::cancel(self);
    }
}

async fn observe<T>(
    module: &str,
    event: &'static str,
    work: impl std::future::Future<Output = Result<T, view::GithubCatalogError>>,
    unavailable: view::GithubCatalogError,
    classify: fn(&view::GithubCatalogError) -> (String, &'static str, bool),
) -> Result<T, view::GithubCatalogError> {
    let work = Box::pin(work);
    let span =
        nddev_device_sync_telemetry::operation_span(module, "process").map_err(|_| unavailable)?;
    let completion = span.clone();
    async {
        let started=nddev_device_sync_accounts::runtime::Instant::now();
        let result=work.await;
        match &result {
            Ok(_)=>tracing::info!(event.name=event,module,duration_ms=started.elapsed().as_millis() as u64,outcome="ok"),
            Err(error)=>{
                let (error_type,outcome,failure)=classify(error);
                nddev_device_sync_telemetry::finish_operation_span(&completion,failure);
                tracing::warn!(event.name=event,module,duration_ms=started.elapsed().as_millis() as u64,error.type=%error_type,outcome);
            }
        }
        result
    }.instrument(span).await
}

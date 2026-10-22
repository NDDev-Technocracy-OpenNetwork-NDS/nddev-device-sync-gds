//! Shared Rust catalog composition. Core owns traversal/publication decisions,
//! Vault owns encrypted state and fences, and GDS owns the fixed provider I/O.
mod consent;
mod driver;
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
    admission: Arc<tokio::sync::Semaphore>,
    cache: Arc<crate::github::CatalogCache>,
    background: Arc<CatalogClient>,
    observation: Observations,
    vault: Arc<VaultClient>,
    work: Mutex<WorkSchedule>,
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
        Self {
            identity: identity.clone(),
            authority: authority.clone(),
            admission: admission.clone(),
            background: Arc::new(CatalogClient::new(
                identity.clone(),
                authority,
                identity,
                vault.clone(),
                observation.clone(),
                admission,
                cache.clone(),
            )),
            observation,
            cache,
            vault,
            work: Mutex::new(WorkSchedule {
                epoch: None,
                next: Some(nddev_device_sync_accounts::runtime::Instant::now()),
            }),
        }
    }
    pub fn foreground(&self, caller: Arc<IdentityClient>) -> Arc<CatalogClient> {
        Arc::new(CatalogClient::new(
            self.identity.clone(),
            self.authority.clone(),
            caller,
            self.vault.clone(),
            self.observation.clone(),
            self.admission.clone(),
            self.cache.clone(),
        ))
    }
    pub fn cancel(&self) {
        self.background.cancel_github_catalog();
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
        self.observation
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
            self.cancel();
            let mut observed = self
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
}

impl nddev_device_sync_application::repositories::catalog::collection::CatalogCollector
    for CatalogRuntime
{
    fn collect(
        self: Arc<Self>,
    ) -> nddev_device_sync_application::repositories::catalog::collection::Collection {
        Box::pin(async move {
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
            nddev_device_sync_application::repositories::catalog::collection::CollectionProgress {
                local_changes: self.vault.snapshot().local_revision != before,
                continue_traversal: matches!(result, Ok(true)),
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

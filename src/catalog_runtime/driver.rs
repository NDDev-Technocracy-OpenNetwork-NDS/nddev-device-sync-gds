//! CatalogClient retains one flight and a bounded page awaiting durable receipts.
//! Core owns page progress; Vault owns encrypted state. No LocalAgent dependency.
use super::{consent::ModuleUseGuard, view::*};
use catalog::observations::{PagePublication, ReadObservationBaselines, Traversal};
use nddev_device_sync_accounts::{IdentityClient, LocalUseFence, runtime};
use nddev_device_sync_application::repositories::catalog::{self, ReadGithubCatalogPage};
use nddev_device_sync_domain::repositories::catalog::{
    CatalogCursor, CatalogPageRequest, CredentialSelection, ProviderRepositoryPage,
    observations::{Candidate, ObservationKey, TargetVersion},
};
use nddev_device_sync_vault::{
    RepositoryCatalogCursor, RepositoryPublication, RepositoryRead, VaultClient,
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
#[path = "reconciliation.rs"]
mod reconciliation;
use reconciliation::{ResolutionBatch, ScopeResolution};
#[path = "connection.rs"]
mod connection;
#[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
#[path = "synchronization.rs"]
mod synchronization;

/// One caller lifetime over the canonical Core/Vault catalog pipeline.
/// Runtime composition supplies independent foreground/background sessions and
/// one provider admission; no presentation or operating-system service is owned.
pub struct CatalogClient {
    catalog: Mutex<CatalogState>,
    identity: Arc<IdentityClient>,
    authority: Arc<nddev_device_sync_accounts::LocalUseLifetime>,
    caller: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    active: std::sync::atomic::AtomicBool,
    shared: Arc<super::CatalogResources>,
}
impl CatalogClient {
    pub(super) fn continuation(&self) -> bool {
        let state = self
            .catalog
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .pending
            .as_ref()
            .map_or_else(|| state.cursor.is_some(), |page| page.requested_next)
    }
    pub(super) fn new(
        identity: Arc<IdentityClient>,
        authority: Arc<nddev_device_sync_accounts::LocalUseLifetime>,
        caller: Arc<IdentityClient>,
        vault: Arc<VaultClient>,
        shared: Arc<super::CatalogResources>,
    ) -> Self {
        Self {
            catalog: Mutex::default(),
            identity,
            authority,
            caller,
            vault,
            active: std::sync::atomic::AtomicBool::new(true),
            shared,
        }
    }
    fn shared_identity(&self) -> &Arc<IdentityClient> {
        &self.identity
    }
    fn catalog_active(&self) -> bool {
        self.active.load(std::sync::atomic::Ordering::Acquire)
    }
    pub fn set_active(&self, active: bool) {
        self.active
            .store(active, std::sync::atomic::Ordering::Release);
        if !active {
            self.invalidate_catalog();
        }
    }
    async fn capture_use(
        &self,
        flight: &Flight<'_>,
        source: &str,
        required: std::collections::BTreeSet<nddev_device_sync_domain::Permission>,
        caller_fence: Arc<LocalUseFence>,
        prior: Option<Arc<LocalUseFence>>,
    ) -> Result<CatalogUseGuard, GithubCatalogError> {
        let module = Arc::new(
            ModuleUseGuard::capture(
                self.identity.clone(),
                self.vault.clone(),
                self.shared.observation.clone(),
                source.to_owned(),
                required.clone(),
            )
            .await?,
        );
        let fence = match prior {
            Some(fence) => fence,
            None => Arc::new(self.identity.local_use_fence_from(&self.authority).await?),
        };
        flight.check()?;
        let cancellation = Arc::new(flight.lifetime.child_token());
        let mut captured = self
            .vault
            .capture_configured_repository_observations(
                &required,
                source,
                fence.clone(),
                cancellation.as_ref().clone(),
            )
            .await
            .map_err(|error| {
                if matches!(
                    error,
                    nddev_device_sync_vault::Error::SelectionChanged
                        | nddev_device_sync_vault::Error::Locked
                ) {
                    module.invalidate_observation();
                }
                GithubCatalogError::from(error)
            })?;
        self.vault
            .bind_repository_caller(&mut captured, self.caller.clone(), caller_fence)
            .await?;
        flight.check()?;
        let read = Arc::new(captured);
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        if let Some(fixture) = self.shared.fixture.lock().unwrap().as_ref() {
            fixture.record_captured_read();
        }
        Ok(CatalogUseGuard {
            module,
            identity: self.identity.clone(),
            vault: self.vault.clone(),
            selection: read.selection().clone(),
            read,
            fence,
            cancellation,
            lifetime: flight.lifetime.clone(),
            #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
            fixture: self
                .shared
                .fixture
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        })
    }
}

pub(crate) struct CatalogState {
    selection: Option<CredentialSelection>,
    source_operation: Option<String>,
    cursor: Option<CatalogCursor>,
    fence: Option<Arc<LocalUseFence>>,
    scan_id: Option<String>,
    traversal: Option<Traversal>,
    pending: Option<Arc<PendingPage>>,
    scope_resolution: Option<Arc<ScopeResolution>>,
    restart: bool,
    saved_cursor: Option<RepositoryCatalogCursor>,
    saved_fence: Option<Arc<LocalUseFence>>,
    lifetime: Arc<CancellationToken>,
    flight: Option<Arc<CancellationToken>>,
}
impl Default for CatalogState {
    fn default() -> Self {
        Self {
            selection: None,
            source_operation: None,
            cursor: None,
            fence: None,
            scan_id: None,
            traversal: None,
            pending: None,
            scope_resolution: None,
            restart: false,
            saved_cursor: None,
            saved_fence: None,
            lifetime: Arc::new(CancellationToken::new()),
            flight: None,
        }
    }
}
impl CatalogState {
    pub(crate) fn invalidate(&mut self, clear_selection: bool) {
        self.lifetime.cancel();
        if let Some(flight) = &self.flight {
            flight.cancel();
        }
        self.lifetime = Arc::new(CancellationToken::new());
        self.clear_pages();
        if clear_selection {
            self.selection = None;
            self.source_operation = None;
        }
        // The admitted future retains the flight until it has actually ended.
    }
    fn clear_pages(&mut self) {
        self.cursor = None;
        self.fence = None;
        self.scan_id = None;
        self.traversal = None;
        self.pending = None;
        self.scope_resolution = None;
        self.restart = false;
        self.saved_cursor = None;
        self.saved_fence = None;
    }
    fn request_next(&self, next: bool) -> bool {
        next && !self.restart
    }
    fn rewind(&mut self) {
        self.cursor = None;
        self.fence = None;
        self.scan_id = None;
        self.traversal = None;
        self.scope_resolution = None;
        self.restart = true;
    }
}
struct Flight<'a> {
    state: &'a Mutex<CatalogState>,
    cancellation: Arc<CancellationToken>,
    lifetime: Arc<CancellationToken>,
}
impl Flight<'_> {
    fn check(&self) -> Result<(), GithubCatalogError> {
        if self.cancellation.is_cancelled() || self.lifetime.is_cancelled() {
            Err(GithubCatalogError::Cancelled)
        } else {
            Ok(())
        }
    }
}
impl<'a> Flight<'a> {
    fn begin(owner: &'a Mutex<CatalogState>) -> Result<Self, GithubCatalogError> {
        let mut state = owner.lock().unwrap_or_else(|e| e.into_inner());
        if state.flight.is_some() {
            return Err(GithubCatalogError::Busy);
        }
        let cancellation = Arc::new(CancellationToken::new());
        state.flight = Some(cancellation.clone());
        Ok(Self {
            state: owner,
            cancellation,
            lifetime: state.lifetime.clone(),
        })
    }
}
impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .flight
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &self.cancellation))
        {
            state.flight = None;
        }
    }
}

/// This exact guard is the callback required inside the GitHub module before EACH provider GET
/// and final return, within its overall deadline; an outer recheck is insufficient.
#[derive(Clone)]
pub(super) struct CatalogUseGuard {
    module: Arc<ModuleUseGuard>,
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    selection: CredentialSelection,
    read: Arc<RepositoryRead>,
    fence: Arc<LocalUseFence>,
    cancellation: Arc<CancellationToken>,
    lifetime: Arc<CancellationToken>,
    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
    fixture: Option<Arc<super::fleet_acceptance::FixtureIo>>,
}
impl CatalogUseGuard {
    fn check(&self) -> Result<(), GithubCatalogError> {
        if self.module.lifetime.is_cancelled()
            || self.cancellation.is_cancelled()
            || self.lifetime.is_cancelled()
        {
            Err(GithubCatalogError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub(super) async fn validate(&self) -> Result<(), GithubCatalogError> {
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        let _timing = self
            .fixture
            .as_ref()
            .map(|io| io.timing(super::fleet_acceptance::TimingStage::Validation));
        let result = self.validate_once().await;
        if result != Err(GithubCatalogError::Busy) {
            return result;
        }
        // A protected metadata publication can race this read (for example,
        // Accounts' same-authority slot renewal). Reread the SAME proof once;
        // never reacquire a fence, repeat Git or manufacture a positive result.
        // The owning provider/Git deadline and cancellation still enclose it.
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        if let Some(fixture) = &self.fixture {
            fixture.pause_proof_retry().await?;
        }
        self.check()?;
        self.validate_once().await
    }

    async fn validate_once(&self) -> Result<(), GithubCatalogError> {
        self.module.validate().await?;
        self.check()?;
        self.identity.validate_local_use(&self.fence).await?;
        self.check()?;
        self.vault.validate_repository_read(&self.read).await?;
        self.check()?;
        self.identity.validate_local_use(&self.fence).await?;
        self.check()
    }

    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
    pub(super) async fn validate_original_identity_for_fixture(
        &self,
    ) -> Result<(), GithubCatalogError> {
        self.check()?;
        self.identity.validate_local_use(&self.fence).await?;
        self.check()
    }
}

#[cfg(not(target_arch = "wasm32"))]
type GuardFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(), nddev_device_sync_domain::modules::ModuleError>>
            + Send,
    >,
>;
#[cfg(target_arch = "wasm32")]
type GuardFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(), nddev_device_sync_domain::modules::ModuleError>>,
    >,
>;

// Explicit native Send/browser-local future fits FRB's existing executors.
// This box owns no grant or secret; typed refusal is transient to this read.
fn revalidation(
    guard: CatalogUseGuard,
    errors: Arc<Mutex<Option<GithubCatalogError>>>,
) -> GuardFuture {
    Box::pin(async move {
        match guard.validate().await {
            Ok(()) => Ok(()),
            Err(error) => {
                *errors.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                Err(module_error(error))
            }
        }
    })
}
fn module_error(error: GithubCatalogError) -> nddev_device_sync_domain::modules::ModuleError {
    use nddev_device_sync_domain::modules::ModuleError as E;
    match error {
        GithubCatalogError::Cancelled => E::Cancelled,
        GithubCatalogError::Busy => E::Busy,
        GithubCatalogError::Timeout => E::Timeout,
        GithubCatalogError::InvalidInput => E::InvalidInput,
        GithubCatalogError::InvalidResponse => E::InvalidResponse,
        GithubCatalogError::ProviderFailed => E::ProviderFailed,
        _ => E::Unavailable,
    }
}
struct CatalogPort {
    cache: Arc<crate::github::CatalogCache>,
    shared: Arc<super::CatalogResources>,
    source_operation: String,
    caller: Arc<IdentityClient>,
    caller_fence: Arc<LocalUseFence>,
    guard: CatalogUseGuard,
    read: Arc<RepositoryRead>,
}
struct PendingPage {
    port: CatalogPort,
    work: PageWork,
    requested_next: bool,
}
enum PageWork {
    Publish(Box<Mutex<PagePublication>>),
    Resolve(Box<ResolutionBatch>),
}
impl PendingPage {
    fn publication(&self) -> Result<&Mutex<PagePublication>, GithubCatalogError> {
        match &self.work {
            PageWork::Publish(value) => Ok(value),
            PageWork::Resolve(_) => Err(GithubCatalogError::InvalidInput),
        }
    }
}
impl Drop for PendingPage {
    fn drop(&mut self) {
        self.port.guard.cancellation.cancel();
    }
}
impl ReadObservationBaselines for CatalogPort {
    async fn observation_baselines(
        &self,
    ) -> Result<Vec<(ObservationKey, TargetVersion)>, GithubCatalogError> {
        self.guard.check()?;
        self.read
            .baselines()
            .into_iter()
            .map(|(id, version)| {
                Ok((
                    ObservationKey::parse(&id).map_err(|_| GithubCatalogError::InvalidResponse)?,
                    version,
                ))
            })
            .collect()
    }
}
impl ReadGithubCatalogPage for CatalogPort {
    type Error = GithubCatalogError;
    type Cancellation = CancellationToken;
    async fn read_page(
        &self,
        request: &CatalogPageRequest,
        cancellation: &CancellationToken,
    ) -> Result<ProviderRepositoryPage, GithubCatalogError> {
        self.guard.check()?;
        // Read the owner's ignore list before any provider I/O: a list that
        // cannot be validated refuses the page instead of including anything.
        let ignored = self.shared.ignore_snapshot()?;
        self.guard
            .identity
            .validate_local_use(&self.guard.fence)
            .await?;
        self.guard.check()?;
        // Conditional Vault resolution owns exact selection validation.
        let credential = self
            .guard
            .vault
            .native_secret_at(request.selection())
            .await?;
        self.guard.check()?;
        self.guard
            .identity
            .validate_local_use(&self.guard.fence)
            .await?;
        self.guard.check()?;
        let guard_error = Arc::new(Mutex::new(None));
        let callback_error = guard_error.clone();
        let callback_guard = self.guard.clone();
        let result = crate::github::read_cached(
            &self.cache,
            &self.source_operation,
            credential.as_str(),
            request,
            cancellation,
            move || revalidation(callback_guard.clone(), callback_error.clone()),
        )
        .await
        .map(|mut page| {
            // Ignored owners are dropped before Core prepares any observation.
            // Page numbers and continuation stay the provider's.
            ignored.retain_included(&mut page.repositories);
            page
        });
        if guard_error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_none()
        {
            self.guard.module.observe(&result);
        }
        result.map_err(|error| {
            guard_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .to_owned()
                .unwrap_or_else(|| error.into())
        })
    }
}

pub fn caller_error(error: &GithubCatalogError) -> (String, &'static str, bool) {
    use GithubCatalogError as E;
    let (class, outcome, failure) = match error {
        E::ModuleDisabled => ("module_disabled", "rejected", false),
        E::ModulePermissionRequired => ("module_permission_required", "rejected", false),
        E::ModuleConfigurationConflict => ("module_configuration_conflict", "rejected", false),
        E::InvalidInput => ("invalid_input", "rejected", false),
        E::AuthorizationRequired => ("credential_revalidation_required", "rejected", false),
        E::CredentialChanged => ("vault_selection_changed", "rejected", false),
        E::Busy => ("operation_busy", "rejected", false),
        E::Cancelled => ("operation_cancelled", "stopped", false),
        E::Timeout => ("operation_timeout", "error", true),
        E::Unavailable => ("dependency_unavailable", "error", true),
        E::ProviderFailed => ("provider_failed", "error", true),
        E::InvalidResponse => ("invalid_response", "error", true),
        E::CapacityExhausted => ("sync_capacity_exhausted", "rejected", false),
        E::AccountIgnored => ("account_ignored", "rejected", false),
        E::IgnoreListInvalid => ("ignore_list_invalid", "error", true),
    };
    (class.into(), outcome, failure)
}

impl CatalogClient {
    fn catalog_flight(&self) -> Result<Flight<'_>, GithubCatalogError> {
        self.identity
            .check_captured_local_use_lifetime(&self.authority)?;
        if !self.catalog_active() {
            return Err(GithubCatalogError::Cancelled);
        }
        Flight::begin(&self.catalog)
    }
    pub(crate) fn invalidate_catalog(&self) {
        self.catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate(true);
    }
    pub fn cancel_github_catalog(&self) {
        self.catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate(false);
    }
    async fn restore_catalog_source(&self, flight: &Flight<'_>) -> Result<(), GithubCatalogError> {
        let source = self.vault.clone().github_source().await?;
        flight.check()?;
        if source.conflicted {
            return Err(GithubCatalogError::Busy);
        }
        let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        flight.check()?;
        if source.selection != state.selection || source.operation_id != state.source_operation {
            // This caller already owns admission and its original cancellation
            // lifetime. Updating metadata must not create a new caller grant.
            state.clear_pages();
            state.selection = source.selection;
            state.source_operation = source.operation_id;
        }
        Ok(())
    }
    async fn pending_catalog_page(
        &self,
        flight: &Flight<'_>,
        next_page: bool,
        caller_fence: Arc<LocalUseFence>,
    ) -> Result<Arc<PendingPage>, GithubCatalogError> {
        let mut next_page = next_page;
        let (selection, after, prior_fence, scan_id, source_operation) = {
            let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.port.guard.module.lifetime.is_cancelled())
            {
                state.pending = None;
                state.rewind();
                next_page = false;
            }
            if let Some(pending) = &state.pending {
                if pending.requested_next == next_page {
                    return Ok(pending.clone());
                }
                // An explicit first-page refresh must not resume an unfinished
                // later page. Already durable observations remain in Vault.
                pending.port.guard.cancellation.cancel();
                state.pending = None;
            }
            let selection = state
                .selection
                .clone()
                .ok_or(GithubCatalogError::InvalidInput)?;
            if next_page {
                (
                    selection,
                    Some(
                        state
                            .cursor
                            .clone()
                            .ok_or(GithubCatalogError::InvalidInput)?,
                    ),
                    Some(
                        state
                            .fence
                            .clone()
                            .ok_or(GithubCatalogError::InvalidInput)?,
                    ),
                    state
                        .scan_id
                        .clone()
                        .ok_or(GithubCatalogError::InvalidInput)?,
                    state
                        .source_operation
                        .clone()
                        .ok_or(GithubCatalogError::CredentialChanged)?,
                )
            } else {
                state.cursor = None;
                state.fence = None;
                state.scan_id = None;
                state.traversal = None;
                state.scope_resolution = None;
                (
                    selection,
                    None,
                    None,
                    uuid::Uuid::new_v4().to_string(),
                    state
                        .source_operation
                        .clone()
                        .ok_or(GithubCatalogError::CredentialChanged)?,
                )
            }
        };
        let guard = self
            .capture_use(
                flight,
                &source_operation,
                super::required_permissions(),
                caller_fence.clone(),
                prior_fence,
            )
            .await?;
        let read = guard.read.clone();
        let fence = guard.fence.clone();
        if read.selection() != &selection {
            return Err(GithubCatalogError::CredentialChanged);
        }
        if read.writer_mode() != Some(nddev_device_sync_application::replica::WriterMode::V3) {
            guard.module.invalidate_observation();
            return Err(GithubCatalogError::Unavailable);
        }
        let port = CatalogPort {
            cache: self.shared.cache.clone(),
            shared: self.shared.clone(),
            source_operation,
            caller: self.caller.clone(),
            caller_fence,
            guard,
            read,
        };
        let mut page = catalog::observations::observation_page(
            &port,
            selection.clone(),
            after.as_ref(),
            scan_id.clone(),
            flight.cancellation.as_ref(),
        )
        .await?;
        port.guard.validate().await?;
        let scope_resolution = if !next_page {
            reconciliation::capture_scope(&port, &page).await?
        } else {
            None
        };
        if scope_resolution.is_some() {
            // Principal discovery precedes scope capture. Discard that probe:
            // every counted page must be read after the scope proof was captured.
            page = catalog::observations::observation_page(
                &port,
                selection.clone(),
                after.as_ref(),
                scan_id.clone(),
                flight.cancellation.as_ref(),
            )
            .await?;
            port.guard.validate().await?;
        }
        let work = match reconciliation::prepare_batch(
            &port,
            &page,
            after.as_ref(),
            flight.cancellation.as_ref(),
        )
        .await?
        {
            Some(batch) => PageWork::Resolve(Box::new(batch)),
            None => PageWork::Publish(Box::new(Mutex::new(PagePublication::new(page)))),
        };
        let pending = Arc::new(PendingPage {
            port,
            work,
            requested_next: next_page,
        });
        let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        flight.check()?;
        if state.selection.as_ref() != Some(&selection)
            || !Arc::ptr_eq(&state.lifetime, &flight.lifetime)
        {
            return Err(GithubCatalogError::CredentialChanged);
        }
        state.fence = Some(fence);
        state.scan_id = Some(scan_id);
        if let Some(scope) = scope_resolution {
            state.scope_resolution = Some(scope);
        }
        state.pending = Some(pending.clone());
        Ok(pending)
    }

    fn discard_catalog_page(&self, pending: &Arc<PendingPage>, restart: bool) {
        let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .pending
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, pending))
        {
            pending.port.guard.cancellation.cancel();
            state.pending = None;
            if restart {
                state.rewind();
                tracing::info!(
                    event.name = "github.catalog.traversal.restart",
                    module = "github",
                    outcome = "stopped"
                );
            }
        }
    }

    async fn queue_catalog_observation(
        &self,
        pending: &Arc<PendingPage>,
        flight: &Flight<'_>,
        candidate: Candidate,
    ) -> Result<(), GithubCatalogError> {
        flight.check()?;
        let (context, at) = {
            let publication = pending
                .publication()?
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (publication.context().clone(), publication.observed_at_ms())
        };
        let key = candidate.observation().key();
        let observation = candidate.observation().clone();
        let mut result = pending
            .port
            .guard
            .vault
            .publish_repository_observation(&pending.port.read, candidate, Some(&context), at)
            .await?;
        flight.check()?;
        let scope = matches!(&key, ObservationKey::Scope { .. });
        if scope && matches!(result, RepositoryPublication::ReRead) {
            let captured = self
                .catalog
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .scope_resolution
                .clone();
            if let Some(captured) = captured.filter(|s| s.snapshot.current().key == key) {
                result = match reconciliation::submit(
                    &pending.port.guard.vault,
                    &captured.snapshot,
                    &observation,
                    &context,
                    at,
                )
                .await
                {
                    Ok(result) => result,
                    Err(GithubCatalogError::Busy) => {
                        self.discard_catalog_page(pending, true);
                        return Err(GithubCatalogError::Busy);
                    }
                    Err(error) => return Err(error),
                };
            }
        }
        match result {
            RepositoryPublication::Unchanged | RepositoryPublication::Queued { .. } => Ok(()),
            RepositoryPublication::ReRead => {
                // Keep prior traversal/cursor unchanged; the next call captures
                // new baselines and actually reads the same provider page again.
                self.discard_catalog_page(pending, scope);
                Err(GithubCatalogError::Busy)
            }
            RepositoryPublication::SourceChanged => {
                self.discard_catalog_page(pending, false);
                Err(GithubCatalogError::CredentialChanged)
            }
        }
    }

    async fn publish_catalog_page(
        &self,
        pending: &Arc<PendingPage>,
        flight: &Flight<'_>,
    ) -> Result<GithubRepositoryPageView, GithubCatalogError> {
        if let PageWork::Resolve(batch) = &pending.work {
            let queue = &batch.choices;
            loop {
                flight.check()?;
                let choice = queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .front()
                    .cloned();
                let Some(choice) = choice else {
                    break;
                };
                let result = reconciliation::submit_choice(
                    &pending.port.guard.vault,
                    &choice,
                    &batch.context,
                )
                .await;
                flight.check()?;
                match result {
                    Ok(RepositoryPublication::Unchanged | RepositoryPublication::Queued { .. }) => {
                        queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                    }
                    Ok(RepositoryPublication::ReRead) | Err(GithubCatalogError::Busy) => {
                        self.discard_catalog_page(pending, false);
                        return Err(GithubCatalogError::Busy);
                    }
                    Ok(RepositoryPublication::SourceChanged) => {
                        self.discard_catalog_page(pending, false);
                        return Err(GithubCatalogError::CredentialChanged);
                    }
                    Err(error) => return Err(error),
                }
            }
            self.discard_catalog_page(pending, false);
            // Resolution has its own fresh read. It never advances the normal
            // traversal: queued choices must reach their real sync outcomes.
            return Err(GithubCatalogError::Busy);
        }
        loop {
            flight.check()?;
            let next = {
                let publication = pending
                    .publication()?
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                publication.next_candidate().cloned()
            };
            let Some(candidate) = next else {
                break;
            };
            let key = candidate.observation().key();
            self.queue_catalog_observation(pending, flight, candidate)
                .await?;
            pending
                .publication()?
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .acknowledge(&key)?;
        }
        let (context, scope_key) = {
            let publication = pending
                .publication()?
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (
                publication.context().clone(),
                publication.terminal_scope_key(),
            )
        };
        let traversal = self
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .traversal
            .clone()
            .map_or_else(|| Traversal::new(context), Ok)?;
        let previous = match scope_key {
            Some(key) => {
                pending
                    .port
                    .guard
                    .vault
                    .repository_observation(&pending.port.read, &key)
                    .await?
                    .value
            }
            None => None,
        };
        let (traversal, summary) = pending
            .publication()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(traversal, previous.as_ref())?;
        if let Some(candidate) = summary {
            self.queue_catalog_observation(pending, flight, candidate)
                .await?;
        }
        pending.port.guard.validate().await?;
        let page = pending
            .publication()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed_page()
            .ok_or(GithubCatalogError::Busy)?
            .provider_page()
            .clone();
        let (view, cursor) = project_page(page);
        let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
        flight.check()?;
        if state.selection.as_ref() != Some(&pending.port.guard.selection)
            || !Arc::ptr_eq(&state.lifetime, &flight.lifetime)
            || !state
                .pending
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, pending))
        {
            return Err(GithubCatalogError::CredentialChanged);
        }
        state.cursor = cursor;
        state.traversal = Some(traversal);
        state.pending = None;
        state.restart = false;
        if state.cursor.is_none() {
            state.scope_resolution = None;
        }
        Ok(view)
    }

    /// Saved facts use NDS local authorization; no GitHub credential is resolved.
    pub async fn github_catalog_credentials(
        &self,
        after: String,
    ) -> Result<GithubCredentialReferencePage, GithubCatalogError> {
        super::observe("github", "github.catalog.credentials.read", async {
            let flight = self.catalog_flight()?;
            let result = runtime::timeout(Duration::from_secs(10), async {
                tokio::select! { biased;
                    _ = flight.cancellation.cancelled() => Err(GithubCatalogError::Cancelled),
                    _ = flight.lifetime.cancelled() => Err(GithubCatalogError::Cancelled),
                    result = async {
                        let fence = self.caller.local_use_fence().await?;
                        flight.check()?;
                        self.identity.check_captured_local_use_lifetime(&self.authority)?;
                        let page = self.vault.records(after).await?;
                        self.caller.validate_local_use(&fence).await?;
                        self.identity.check_captured_local_use_lifetime(&self.authority)?;
                        flight.check()?;
                        Ok(GithubCredentialReferencePage {
                            items: page.items.into_iter().filter(|r| !r.deleted && !r.conflicted).map(|r| GithubCredentialReference {
                                record_ref: r.id, title: r.title, username: r.username,
                                revision: r.revision.to_string(), pending_id: r.pending_id,
                            }).collect(), next_after: page.next_after, has_more: page.has_more,
                        })
                    } => result,
                }
            }).await.map_err(|_| GithubCatalogError::Timeout)?;
            flight.check()?;
            result
        }, GithubCatalogError::Unavailable, caller_error).await
    }

    pub async fn saved_github_repository_page(
        &self,
        next_page: bool,
    ) -> Result<GithubSavedRepositoryPageView, GithubCatalogError> {
        super::observe("github", "github.catalog.saved.read", async {
            let flight = self.catalog_flight()?;
            let caller_fence = self.caller.local_use_fence().await?;
            flight.check()?;
            let (after, prior_fence) = {
                let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
                if next_page {
                    (Some(state.saved_cursor.clone().ok_or(GithubCatalogError::InvalidInput)?),
                     Some(state.saved_fence.clone().ok_or(GithubCatalogError::InvalidInput)?))
                } else {
                    state.saved_cursor = None;
                    state.saved_fence = None;
                    (None, None)
                }
            };
            let result = runtime::timeout(Duration::from_secs(10), async {
                tokio::select! { biased;
                    _ = flight.cancellation.cancelled() => Err(GithubCatalogError::Cancelled),
                    _ = flight.lifetime.cancelled() => Err(GithubCatalogError::Cancelled),
                    result = async {
                        let fence = match prior_fence {
                            Some(fence) => fence,
                            None => Arc::new(self.shared_identity().local_use_fence_from(&self.authority).await?),
                        };
                        self.shared_identity().validate_local_use(&fence).await?;
                        flight.check()?;
                        let page = self.vault.clone().repository_catalog_page(
                            after.as_ref(), flight.cancellation.as_ref().clone(),
                        ).await?;
                        let groups = nddev_device_sync_domain::repositories::catalog::observations::projection::saved_page(page.observations)
                            .map_err(|_| GithubCatalogError::InvalidResponse)?;
                        self.shared_identity().validate_local_use(&fence).await?;
                        flight.check()?;
                        self.caller.validate_local_use(&caller_fence).await?;
                        let ignored = self.shared.ignore_snapshot()?;
                        let mut view = project_saved_page(groups, page.observed_seq, page.local_revision, page.next.is_some(), |id, login| ignored.ignores(id, login));
                        let source = self.vault.github_source().await?;
                        let consent = self.vault.module_configuration("github").await?;
                        // Validate the ORIGINAL read, not a fresh replacement.
                        // Coalescing an undispatched Vault edit can preserve IDs
                        // while changing the encrypted source/credential/consent.
                        let original = self.shared.commits.lock().unwrap_or_else(|e| e.into_inner()).guard.clone();
                        let current = match &original {
                            Some(guard) => guard.validate().await.is_ok(),
                            None => false,
                        };
                        self.caller.validate_local_use(&caller_fence).await?;
                        let epoch = (source.operation_id, consent.operation_id);
                        let commits = self.shared.commits.lock().unwrap_or_else(|e| e.into_inner());
                        let same = match (&original, &commits.guard) {
                            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                            _ => false,
                        };
                        if current && same && !source.conflicted && !consent.conflicted {
                            for group in &mut view.groups {
                                for item in &mut group.repositories {
                                    item.synchronization = commits.observation(&epoch, &item.repository.id);
                                }
                            }
                        }
                        drop(commits);
                        let mut state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
                        flight.check()?;
                        if !Arc::ptr_eq(&state.lifetime, &flight.lifetime) {
                            return Err(GithubCatalogError::Cancelled);
                        }
                        state.saved_cursor = page.next;
                        state.saved_fence = Some(fence);
                        Ok(view)
                    } => result,
                }
            }).await.map_err(|_| GithubCatalogError::Timeout)?;
            flight.check()?;
            result
        }, GithubCatalogError::Unavailable, caller_error).await
    }

    pub async fn github_repository_page(
        &self,
        next_page: bool,
    ) -> Result<GithubRepositoryPageView, GithubCatalogError> {
        super::observe(
            "github",
            "github.catalog.page.read",
            async {
                let _provider = self.shared
                    .admission
                    .try_acquire()
                    .map_err(|_| GithubCatalogError::Busy)?;
                let flight = self.catalog_flight()?;
                let caller_fence = Arc::new(self.caller.local_use_fence().await?);
                flight.check()?;
                let _lease = self.vault.catalog_lease().await?;
                flight.check()?;
                if !next_page {
                    self.restore_catalog_source(&flight).await?;
                }
                // Refuse an obsolete continuation before timers or protected I/O.
                // Keep an unfinished first page if a caller asks for next early.
                let next_page = {
                    let state = self.catalog.lock().unwrap_or_else(|e| e.into_inner());
                    let next_page = state.request_next(next_page);
                    let resume = state
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.requested_next == next_page);
                    if state.selection.is_none()
                        || next_page
                            && !resume
                            && (state.cursor.is_none()
                                || state.fence.is_none()
                                || state.scan_id.is_none()
                                || state.traversal.is_none())
                    {
                        return Err(GithubCatalogError::InvalidInput);
                    }
                    next_page
                };
                let result = runtime::timeout(Duration::from_secs(10), async {
                    tokio::select! { biased;
                        _ = flight.cancellation.cancelled() => Err(GithubCatalogError::Cancelled),
                        _ = flight.lifetime.cancelled() => Err(GithubCatalogError::Cancelled),
                        result = async {
                            flight.check()?;
                            let pending = self.pending_catalog_page(&flight, next_page, caller_fence).await?;
                            self.publish_catalog_page(&pending, &flight).await
                        } => result,
                    }
                })
                .await
                .map_err(|_| GithubCatalogError::Timeout)?;
                flight.check()?;
                if matches!(result, Err(GithubCatalogError::CredentialChanged | GithubCatalogError::AuthorizationRequired)) {
                    let mut state = self.catalog.lock().unwrap_or_else(|error| error.into_inner());
                    if matches!(result, Err(GithubCatalogError::CredentialChanged))
                        && let Some(page) = &state.pending {
                        page.port.guard.module.invalidate_observation();
                    }
                    state.invalidate(false);
                }
                result
            },
            GithubCatalogError::Unavailable,
            caller_error,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_restart_preserves_selection_and_admission_but_cancel_revokes_it() {
        let selected = CredentialSelection::new("record".into(), 7, None).unwrap();
        let state = Mutex::new(CatalogState {
            selection: Some(selected.clone()),
            ..Default::default()
        });
        let flight = Flight::begin(&state).unwrap();
        {
            let mut current = state.lock().unwrap();
            current.rewind();
            assert_eq!(current.selection.as_ref(), Some(&selected));
            assert!(!current.request_next(true));
            assert!(!current.request_next(false));
        }
        assert_eq!(flight.check(), Ok(()));
        assert!(matches!(
            Flight::begin(&state),
            Err(GithubCatalogError::Busy)
        ));
        state.lock().unwrap().invalidate(false);
        assert_eq!(flight.check(), Err(GithubCatalogError::Cancelled));
        assert!(state.lock().unwrap().request_next(true));
        assert!(matches!(
            Flight::begin(&state),
            Err(GithubCatalogError::Busy)
        ));
        drop(flight);
        assert_eq!(Flight::begin(&state).unwrap().check(), Ok(()));
    }
    #[test]
    fn cancellation_keeps_admission_until_the_old_future_ends() {
        let state = Mutex::new(CatalogState::default());
        let first = Flight::begin(&state).unwrap();
        assert!(matches!(
            Flight::begin(&state),
            Err(GithubCatalogError::Busy)
        ));
        state.lock().unwrap().invalidate(true);
        assert_eq!(first.check(), Err(GithubCatalogError::Cancelled));
        assert!(matches!(
            Flight::begin(&state),
            Err(GithubCatalogError::Busy)
        ));
        drop(first);
        let second = Flight::begin(&state).unwrap();
        assert_eq!(second.check(), Ok(()));
    }
    #[test]
    fn replacing_selection_revokes_the_previous_lifetime() {
        let state = Mutex::new(CatalogState::default());
        let first = Flight::begin(&state).unwrap();
        {
            let mut current = state.lock().unwrap();
            current.invalidate(true);
            current.selection = Some(
                CredentialSelection::new("record".into(), 7, Some("new-operation".into())).unwrap(),
            );
        }
        assert_eq!(first.check(), Err(GithubCatalogError::Cancelled));
        drop(first);
        assert_eq!(Flight::begin(&state).unwrap().check(), Ok(()));
    }
}

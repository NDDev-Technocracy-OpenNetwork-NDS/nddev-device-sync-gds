//! Named GUI/CLI connection use case. No second owner task, mutation queue,
//! plaintext credential export or activation of periodic Git writes.
use super::*;
use nddev_device_sync_domain::repositories::workspace::CheckoutLocation;

impl CatalogClient {
    pub async fn connect_repository(
        &self,
        location: CheckoutLocation,
    ) -> Result<GithubConnectionView, GithubCatalogError> {
        super::super::observe(
            "github",
            "github.workspace.connect",
            async {
                if !self.catalog_active() {
                    return Err(GithubCatalogError::Cancelled);
                }
                location.validate().map_err(GithubCatalogError::from)?;
                #[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
                {
                    self.connect_native(location).await
                }
                #[cfg(not(all(
                    feature = "commit-sync",
                    any(target_os = "linux", target_os = "macos")
                )))]
                {
                    Ok(GithubConnectionView {
                        outcome: "unsupported".into(),
                        reason: "unsupported_platform".into(),
                        relative_path: String::new(),
                        intent_queued: false,
                    })
                }
            },
            GithubCatalogError::Unavailable,
            caller_error,
        )
        .await
    }

    #[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
    async fn connect_native(
        &self,
        location: CheckoutLocation,
    ) -> Result<GithubConnectionView, GithubCatalogError> {
        use crate::commits::ConnectionOutcome;
        use nddev_device_sync_domain::repositories::workspace::{
            ConnectionBlock, write_permissions,
        };
        let _admission = self
            .shared
            .admission
            .try_acquire()
            .map_err(|_| GithubCatalogError::Busy)?;
        let flight = self.catalog_flight()?;
        let caller = Arc::new(self.caller.local_use_fence().await?);
        self.restore_catalog_source(&flight).await?;
        let source = self
            .catalog
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .source_operation
            .clone()
            .ok_or(GithubCatalogError::InvalidInput)?;
        let guard = self
            .capture_use(&flight, &source, write_permissions(), caller, None)
            .await?;
        guard.validate().await?;
        let current = self
            .vault
            .repository_connection(location.repository_id)
            .await?;
        if current.conflicted {
            return Err(GithubCatalogError::Busy);
        }
        if current
            .location
            .as_ref()
            .is_some_and(|saved| saved != &location)
        {
            return Ok(blocked(ConnectionBlock::RelocationRequired, &location));
        }
        let credential = Arc::new(self.vault.native_secret_at(&guard.selection).await?);
        fresh_repository(&guard, &location, credential.as_str(), &flight.cancellation).await?;
        // A confirmation is required at publication too. If the first read used
        // the last provider budget, avoid cloning a tree we cannot yet confirm.
        if !crate::github::retry_delay().is_zero() {
            return Err(GithubCatalogError::Busy);
        }
        let errors = Arc::new(Mutex::new(None));
        let workspace = native_workspace(&guard, &location, errors.clone())
            .await
            .map_err(|error| operation_error(error, &errors))?;
        let outcome = workspace
            .connect(
                &location,
                credential.as_str(),
                &flight.cancellation,
                {
                    let guard = guard.clone();
                    let errors = errors.clone();
                    move || revalidation(guard.clone(), errors.clone())
                },
                {
                    let guard = guard.clone();
                    let location = location.clone();
                    let credential = credential.clone();
                    let cancellation = flight.cancellation.clone();
                    let errors = errors.clone();
                    move || {
                        confirmation(
                            guard.clone(),
                            location.clone(),
                            credential.clone(),
                            cancellation.clone(),
                            errors.clone(),
                        )
                    }
                },
            )
            .await
            .map_err(|error| operation_error(error, &errors))?;
        let (binding, created) = match outcome {
            ConnectionOutcome::Connected { binding, created } => (binding, created),
            ConnectionOutcome::Blocked(reason) => return Ok(blocked(reason, &location)),
        };
        guard.validate().await?;
        let mutation = self
            .vault
            .save_repository_connection(
                &guard.read,
                binding.location,
                current.revision,
                current.pending_id.clone(),
            )
            .await;
        let mut result = GithubConnectionView {
            outcome: "connected".into(),
            reason: if created {
                "cloned"
            } else {
                "existing_checkout"
            }
            .into(),
            relative_path: location.relative_path(),
            intent_queued: false,
        };
        match mutation {
            Ok(Some(_)) => result.intent_queued = true,
            Ok(None) => {
                let current = self
                    .vault
                    .repository_connection(location.repository_id)
                    .await?;
                result.intent_queued = current.pending_id.is_some();
            }
            Err(_) => {
                // Files already published belong to the user. Preserve them and
                // distinguish registration retry from another clone attempt.
                guard.validate().await?;
                result.outcome = "registration_pending".into();
                result.reason = "retry_registration".into();
            }
        }
        Ok(result)
    }
}

#[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
fn blocked(
    reason: nddev_device_sync_domain::repositories::workspace::ConnectionBlock,
    location: &CheckoutLocation,
) -> GithubConnectionView {
    let reason = serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid_response".into());
    GithubConnectionView {
        outcome: "blocked".into(),
        reason,
        relative_path: location.relative_path(),
        intent_queued: false,
    }
}
#[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
pub(super) fn operation_error(
    error: nddev_device_sync_domain::modules::ModuleError,
    errors: &Mutex<Option<GithubCatalogError>>,
) -> GithubCatalogError {
    errors
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .unwrap_or_else(|| error.into())
}
#[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
pub(super) async fn fresh_repository(
    guard: &CatalogUseGuard,
    location: &CheckoutLocation,
    credential: &str,
    cancellation: &CancellationToken,
) -> Result<nddev_device_sync_domain::repositories::catalog::RepositorySummary, GithubCatalogError>
{
    let errors = Arc::new(Mutex::new(None));
    let revalidate = {
        let errors = errors.clone();
        let guard = guard.clone();
        move || revalidation(guard.clone(), errors.clone())
    };
    let result = async {
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        if let Some(fixture) = &guard.fixture {
            return fixture
                .provider
                .read(credential, location, cancellation, revalidate)
                .await;
        }
        crate::github::read_repository(credential, location, cancellation, revalidate).await
    }
    .await;
    if let Some(error) = errors
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        return Err(error);
    }
    guard.module.observe(&result);
    result.map_err(Into::into)
}

#[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
pub(super) async fn native_workspace(
    guard: &CatalogUseGuard,
    _location: &CheckoutLocation,
    errors: Arc<Mutex<Option<GithubCatalogError>>>,
) -> Result<crate::commits::GitWorkspace, nddev_device_sync_domain::modules::ModuleError> {
    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
    if let Some(fixture) = &guard.fixture {
        return fixture.workspace(guard.vault.workspace_principal(), _location);
    }
    crate::commits::GitWorkspace::default_workspace(guard.vault.workspace_principal(), {
        let guard = guard.clone();
        move || revalidation(guard.clone(), errors.clone())
    })
    .await
}

#[cfg(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos")))]
fn confirmation(
    guard: CatalogUseGuard,
    location: CheckoutLocation,
    credential: Arc<zeroize::Zeroizing<String>>,
    cancellation: Arc<CancellationToken>,
    errors: Arc<Mutex<Option<GithubCatalogError>>>,
) -> GuardFuture {
    Box::pin(async move {
        fresh_repository(&guard, &location, credential.as_str(), &cancellation)
            .await
            .map(|_| ())
            .map_err(|error| {
                *errors.lock().unwrap_or_else(|error| error.into_inner()) = Some(error);
                module_error(error)
            })
    })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod executor_contract {
    use super::*;
    #[test]
    fn public_connection_future_fits_the_existing_native_executor() {
        fn require_send<T: std::future::Future + Send>(_: T) {}
        fn check(client: &CatalogClient, location: CheckoutLocation) {
            require_send(client.connect_repository(location));
        }
        let _compile_contract = check;
    }
}

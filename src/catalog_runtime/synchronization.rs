//! Existing commits for an already connected checkout. No clone or registration.
use super::connection::{fresh_repository, native_workspace, operation_error};
use super::*;
use nddev_device_sync_application::repositories::commits::CommitOutcome;
use nddev_device_sync_domain::repositories::workspace::{CheckoutLocation, write_permissions};

impl CatalogClient {
    pub(in crate::catalog_runtime) async fn synchronize_connection(
        &self,
        location: &CheckoutLocation,
        original: Option<Arc<CatalogUseGuard>>,
        cancellation: &CancellationToken,
    ) -> Result<CommitOutcome, GithubCatalogError> {
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        let fixture = self.shared.fixture.lock().unwrap().clone();
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        let _timing = fixture.as_ref().map(|io| {
            io.timing(crate::catalog_runtime::fleet_acceptance::TimingStage::Synchronization)
        });
        let _admission = self
            .shared
            .admission
            .try_acquire()
            .map_err(|_| GithubCatalogError::Busy)?;
        let flight = self.catalog_flight()?;
        if cancellation.is_cancelled() {
            return Err(GithubCatalogError::Cancelled);
        }
        // An existing connection of an ignored owner keeps its checkout and
        // registration; it is reported refused instead of synchronized.
        self.shared.refuse_ignored(location)?;
        self.restore_catalog_source(&flight).await?;
        let guard = match original {
            Some(original) => {
                // Effects and observations use the same opaque read. A guard
                // from another facade or cancelled catalog lifetime cannot be
                // rebound to this flight, even when its source ID is unchanged.
                if !Arc::ptr_eq(&original.lifetime, &flight.lifetime)
                    || !Arc::ptr_eq(&original.identity, &self.identity)
                    || !Arc::ptr_eq(&original.vault, &self.vault)
                {
                    return Err(GithubCatalogError::Cancelled);
                }
                original
            }
            None => {
                let caller = Arc::new(self.caller.local_use_fence().await?);
                let source = self
                    .catalog
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .source_operation
                    .clone()
                    .ok_or(GithubCatalogError::InvalidInput)?;
                Arc::new(
                    self.capture_use(&flight, &source, write_permissions(), caller, None)
                        .await?,
                )
            }
        };
        guard.validate().await?;
        {
            let mut progress = self
                .shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            flight.check()?;
            if cancellation.is_cancelled()
                || progress
                    .guard
                    .as_ref()
                    .is_some_and(|current| !Arc::ptr_eq(current, &guard))
            {
                return Err(GithubCatalogError::Cancelled);
            }
            progress.guard.get_or_insert_with(|| guard.clone());
            if let Some(epoch) = progress.epoch.clone() {
                progress.record(
                    &epoch,
                    location.repository_id.value().to_string(),
                    GithubCommitSyncView {
                        outcome: "running".into(),
                        reason: String::new(),
                        observed_at_ms: None,
                    },
                );
            }
        }
        let current = self
            .vault
            .repository_connection(location.repository_id)
            .await?;
        if current.conflicted || current.location.as_ref() != Some(location) {
            return Err(GithubCatalogError::Busy);
        }
        let credential = self.vault.native_secret_at(&guard.selection).await?;
        let repository =
            fresh_repository(&guard, location, credential.as_str(), cancellation).await?;
        let errors = Arc::new(Mutex::new(None));
        let workspace = native_workspace(&guard, location, errors.clone())
            .await
            .map_err(|error| operation_error(error, &errors))?;
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        let git_timing = fixture
            .as_ref()
            .map(|io| io.timing(crate::catalog_runtime::fleet_acceptance::TimingStage::Git));
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        let validation_calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        if let Some(fixture) = &fixture {
            fixture
                .git_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let result = workspace
            .synchronize(
                location,
                repository.archived,
                credential.as_str(),
                cancellation,
                {
                    let guard = guard.clone();
                    let errors = errors.clone();
                    let location = location.clone();
                    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
                    let fixture = fixture.clone();
                    move || {
                        let guard = guard.clone();
                        let errors = errors.clone();
                        let location = location.clone();
                        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
                        let fixture = fixture.clone();
                        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
                        let validation_calls = validation_calls.clone();
                        async move {
                            #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
                            if validation_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                == 1
                                && let Some(fixture) = fixture
                            {
                                fixture.pause_git_validation().await.map_err(module_error)?;
                            }
                            revalidation(guard.as_ref().clone(), errors).await?;
                            let current = guard
                                .vault
                                .repository_connection(location.repository_id)
                                .await
                                .map_err(|_| {
                                    nddev_device_sync_domain::modules::ModuleError::Unavailable
                                })?;
                            if current.conflicted || current.location.as_ref() != Some(&location) {
                                return Err(nddev_device_sync_domain::modules::ModuleError::Busy);
                            }
                            Ok(())
                        }
                    }
                },
            )
            .await
            .map_err(|error| operation_error(error, &errors));
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        drop(git_timing);
        guard.validate().await?;
        if cancellation.is_cancelled() {
            return Err(GithubCatalogError::Cancelled);
        }
        result
    }
}

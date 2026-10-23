//! Existing commits for an already connected checkout. No clone or registration.
use super::connection::{fresh_repository, operation_error};
use super::*;
use nddev_device_sync_application::repositories::commits::CommitOutcome;
use nddev_device_sync_domain::repositories::workspace::{CheckoutLocation, write_permissions};

impl CatalogClient {
    pub(in crate::catalog_runtime) async fn synchronize_connection(
        &self,
        location: &CheckoutLocation,
        cancellation: &CancellationToken,
    ) -> Result<CommitOutcome, GithubCatalogError> {
        let _admission = self
            .shared
            .admission
            .try_acquire()
            .map_err(|_| GithubCatalogError::Busy)?;
        let flight = self.catalog_flight()?;
        if cancellation.is_cancelled() {
            return Err(GithubCatalogError::Cancelled);
        }
        let caller = Arc::new(self.caller.local_use_fence().await?);
        self.restore_catalog_source(&flight).await?;
        let source = self
            .catalog
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .source_operation
            .clone()
            .ok_or(GithubCatalogError::InvalidInput)?;
        let guard = self
            .capture_use(&flight, &source, write_permissions(), caller, None)
            .await?;
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
        let workspace =
            crate::commits::GitWorkspace::default_workspace(self.vault.workspace_principal(), {
                let guard = guard.clone();
                let errors = errors.clone();
                move || revalidation(guard.clone(), errors.clone())
            })
            .await
            .map_err(|error| operation_error(error, &errors))?;
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
                    move || {
                        let guard = guard.clone();
                        let errors = errors.clone();
                        let location = location.clone();
                        async move {
                            revalidation(guard.clone(), errors).await?;
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
        guard.validate().await?;
        if cancellation.is_cancelled() {
            return Err(GithubCatalogError::Cancelled);
        }
        result
    }
}

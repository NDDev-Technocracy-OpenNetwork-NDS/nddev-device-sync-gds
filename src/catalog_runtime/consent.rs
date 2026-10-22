use super::view::GithubCatalogError;
use nddev_device_sync_accounts::{IdentityClient, LocalUseFence};
use nddev_device_sync_domain::modules::{ModuleError, ModuleObservation, control::Readiness};
use nddev_device_sync_vault::VaultClient;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) struct ModuleUseGuard {
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    operation: Option<String>,
    fence: LocalUseFence,
    pub lifetime: Arc<CancellationToken>,
    observation: super::Observations,
    source: String,
    required: std::collections::BTreeSet<nddev_device_sync_domain::Permission>,
}
impl ModuleUseGuard {
    pub async fn capture(
        identity: Arc<IdentityClient>,
        vault: Arc<VaultClient>,
        observation: super::Observations,
        source: String,
        required: std::collections::BTreeSet<nddev_device_sync_domain::Permission>,
    ) -> Result<Self, GithubCatalogError> {
        let fence = identity.local_use_fence().await?;
        let setting = vault.module_configuration("gds").await?;
        admit(&setting, &required)?;
        let guard = Self {
            identity,
            vault,
            operation: setting.operation_id,
            fence,
            lifetime: Arc::new(CancellationToken::new()),
            observation,
            source,
            required,
        };
        guard.validate().await?;
        Ok(guard)
    }
    pub async fn validate(&self) -> Result<(), GithubCatalogError> {
        if self.lifetime.is_cancelled() {
            return Err(GithubCatalogError::Cancelled);
        }
        self.identity.validate_local_use(&self.fence).await?;
        let setting = self.vault.module_configuration("gds").await?;
        admit(&setting, &self.required)?;
        if setting.operation_id != self.operation {
            self.lifetime.cancel();
            return Err(GithubCatalogError::Cancelled);
        }
        self.identity.validate_local_use(&self.fence).await?;
        Ok(())
    }
    pub fn invalidate_observation(&self) {
        let mut slot = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot
            .as_ref()
            .is_some_and(|value| value.epoch == (Some(self.source.clone()), self.operation.clone()))
        {
            *slot = None;
        }
    }
    pub fn observe<T>(&self, result: &Result<T, ModuleError>) {
        if self.lifetime.is_cancelled() {
            return;
        }
        let mut slot = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let epoch = (Some(self.source.clone()), self.operation.clone());
        if slot.as_ref().is_some_and(|value| value.epoch != epoch) {
            *slot = None;
        }
        let observation = slot.get_or_insert_with(|| super::Observed {
            epoch,
            value: ModuleObservation {
                id: "gds".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                configured: true,
                reachability: nddev_device_sync_domain::state::Reachability::NotChecked,
                observed_at_ms: None,
                error: None,
            },
        });
        observation
            .value
            .observe(result, crate::github::now_ms().ok());
    }
}
fn admit(
    setting: &nddev_device_sync_vault::ModuleSetting,
    required: &std::collections::BTreeSet<nddev_device_sync_domain::Permission>,
) -> Result<(), GithubCatalogError> {
    if setting.conflicted {
        return Err(GithubCatalogError::ModuleConfigurationConflict);
    }
    setting
        .configuration
        .admit(true, true, required)
        .map_err(|reason| match reason {
            Readiness::Disabled => GithubCatalogError::ModuleDisabled,
            Readiness::PermissionRequired => GithubCatalogError::ModulePermissionRequired,
            _ => GithubCatalogError::Unavailable,
        })
}

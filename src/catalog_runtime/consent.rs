use super::{required_permissions, view::GithubCatalogError};
use nddev_device_sync_accounts::{IdentityClient, LocalUseFence};
use nddev_device_sync_domain::modules::{ModuleError, ModuleObservation, control::Readiness};
use nddev_device_sync_vault::VaultClient;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

pub(super) struct ModuleUseGuard {
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    operation: Option<String>,
    fence: LocalUseFence,
    pub lifetime: Arc<CancellationToken>,
    observation: Arc<Mutex<Option<ModuleObservation>>>,
}
impl ModuleUseGuard {
    pub async fn capture(
        identity: Arc<IdentityClient>,
        vault: Arc<VaultClient>,
        observation: Arc<Mutex<Option<ModuleObservation>>>,
    ) -> Result<Self, GithubCatalogError> {
        let fence = identity.local_use_fence().await?;
        let setting = vault.module_configuration("gds").await?;
        admit(&setting)?;
        let guard = Self {
            identity,
            vault,
            operation: setting.operation_id,
            fence,
            lifetime: Arc::new(CancellationToken::new()),
            observation,
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
        admit(&setting)?;
        if setting.operation_id != self.operation {
            self.lifetime.cancel();
            return Err(GithubCatalogError::Cancelled);
        }
        self.identity.validate_local_use(&self.fence).await?;
        Ok(())
    }
    pub fn observe<T>(&self, result: &Result<T, ModuleError>) {
        if self.lifetime.is_cancelled() {
            return;
        }
        let mut slot = self
            .observation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let observation = slot.get_or_insert_with(|| ModuleObservation {
            id: "gds".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            configured: true,
            reachability: nddev_device_sync_domain::state::Reachability::NotChecked,
            observed_at_ms: None,
            error: None,
        });
        observation.observe(result, crate::github::now_ms().ok());
    }
}
fn admit(setting: &nddev_device_sync_vault::ModuleSetting) -> Result<(), GithubCatalogError> {
    if setting.conflicted {
        return Err(GithubCatalogError::ModuleConfigurationConflict);
    }
    setting
        .configuration
        .admit(true, true, &required_permissions())
        .map_err(|reason| match reason {
            Readiness::Disabled => GithubCatalogError::ModuleDisabled,
            Readiness::PermissionRequired => GithubCatalogError::ModulePermissionRequired,
            _ => GithubCatalogError::Unavailable,
        })
}

//! Catalog presentation only. Provider IDs and revisions remain decimal strings.
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubConnectionView {
    /// Connection is distinct from successful synchronization of commits.
    pub outcome: String,
    pub reason: String,
    pub relative_path: String,
    pub intent_queued: bool,
}
#[derive(Clone)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubCredentialReference {
    pub record_ref: String,
    pub title: String,
    pub username: String,
    pub revision: String,
    pub pending_id: Option<String>,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubCredentialReferencePage {
    pub items: Vec<GithubCredentialReference>,
    pub next_after: String,
    pub has_more: bool,
}
/// Saved owner intent, not provider access or this device's consent.
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubSourceView {
    pub credential: Option<GithubCredentialReference>,
    pub revision: String,
    pub pending_id: Option<String>,
    pub local_revision: String,
    pub conflicted: bool,
}
#[derive(Clone, Copy)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub enum GithubOwnerKindView {
    User,
    Organization,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubOwnerView {
    pub id: String,
    pub login: String,
    pub kind: GithubOwnerKindView,
}
#[derive(Clone, Copy)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub enum GithubRepositoryVisibilityView {
    Public,
    Private,
    Internal,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubRepositorySummaryView {
    pub id: String,
    pub name: String,
    pub visibility: GithubRepositoryVisibilityView,
    pub archived: bool,
    pub default_branch: Option<String>,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubRepositoryGroupView {
    pub owner: GithubOwnerView,
    pub repositories: Vec<GithubRepositorySummaryView>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub enum GithubCatalogContinuationView {
    More,
    EndOfAccessScope,
    LimitReached,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubRepositoryPageView {
    pub principal: GithubOwnerView,
    pub groups: Vec<GithubRepositoryGroupView>,
    pub observed_at_ms: String,
    pub continuation: GithubCatalogContinuationView,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubObservationView {
    pub revision: String,
    pub pending: bool,
    pub conflicted: bool,
    pub observed_at_ms: String,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubSavedOwnerView {
    pub owner: GithubOwnerView,
    pub observation: GithubObservationView,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubSavedRepositoryView {
    pub repository: GithubRepositorySummaryView,
    pub observation: GithubObservationView,
    pub synchronization: Option<GithubCommitSyncView>,
}
#[derive(Clone)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubCommitSyncView {
    pub outcome: String,
    pub reason: String,
    /// Actual local completion time; absent while an attempt is running.
    pub observed_at_ms: Option<String>,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubSavedRepositoryGroupView {
    pub owner_id: String,
    pub owner: Option<GithubSavedOwnerView>,
    pub repositories: Vec<GithubSavedRepositoryView>,
    /// The owner is on the host's ignored-account list: its saved data stays, but it is not
    /// discovered, connected or synchronized and is left out of counts and actions.
    pub ignored: bool,
}
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    derive(tsify::Tsify, serde::Serialize)
)]
#[cfg_attr(
    all(target_arch = "wasm32", feature = "browser-bindings"),
    tsify(large_number_types_as_bigints, missing_as_null)
)]
pub struct GithubSavedRepositoryPageView {
    pub local_workspace_supported: bool,
    pub groups: Vec<GithubSavedRepositoryGroupView>,
    pub observed_seq: String,
    pub local_revision: String,
    pub has_more: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubCatalogError {
    InvalidInput,
    AuthorizationRequired,
    CredentialChanged,
    Busy,
    Cancelled,
    Timeout,
    Unavailable,
    ProviderFailed,
    InvalidResponse,
    CapacityExhausted,
    ModuleDisabled,
    ModulePermissionRequired,
    ModuleConfigurationConflict,
    /// The repository owner is on the host's ignored-account list.
    AccountIgnored,
    /// The host's ignored-account list exists but cannot be read or validated.
    IgnoreListInvalid,
}

impl std::fmt::Display for GithubCatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for GithubCatalogError {}

impl From<nddev_device_sync_domain::modules::ModuleError> for GithubCatalogError {
    fn from(error: nddev_device_sync_domain::modules::ModuleError) -> Self {
        use nddev_device_sync_domain::modules::ModuleError as E;
        match error {
            E::InvalidInput => Self::InvalidInput,
            E::Busy => Self::Busy,
            E::Cancelled => Self::Cancelled,
            E::Timeout => Self::Timeout,
            E::InvalidResponse | E::InvalidContract => Self::InvalidResponse,
            E::ProviderFailed => Self::ProviderFailed,
            E::UnsupportedPlatform | E::Unavailable => Self::Unavailable,
        }
    }
}
impl From<nddev_device_sync_accounts::view::ClientError> for GithubCatalogError {
    fn from(error: nddev_device_sync_accounts::view::ClientError) -> Self {
        use nddev_device_sync_accounts::view::ClientError as E;
        match error {
            E::Server(ref code) if code == "module_disabled" => Self::ModuleDisabled,
            E::Server(ref code) if code == "module_permission_required" => {
                Self::ModulePermissionRequired
            }
            E::Server(ref code) if code == "module_configuration_conflict" => {
                Self::ModuleConfigurationConflict
            }
            E::Busy => Self::Busy,
            E::Cancelled => Self::Cancelled,
            E::Timeout => Self::Timeout,
            E::CredentialStore | E::Transport | E::TelemetryUnavailable => Self::Unavailable,
            E::InvalidResponse => Self::InvalidResponse,
            _ => Self::AuthorizationRequired,
        }
    }
}
impl From<nddev_device_sync_vault::view::VaultError> for GithubCatalogError {
    fn from(error: nddev_device_sync_vault::view::VaultError) -> Self {
        use nddev_device_sync_vault::view::VaultError as E;
        match error {
            E::Busy => Self::Busy,
            E::Invalid => Self::InvalidResponse,
            E::SelectionChanged => Self::CredentialChanged,
            E::Cancelled => Self::Cancelled,
            E::Locked | E::Authentication | E::RegistrationRequired => Self::AuthorizationRequired,
            E::CapacityExhausted => Self::CapacityExhausted,
            E::Unavailable | E::StorageFull => Self::Unavailable,
        }
    }
}

fn owner(value: nddev_device_sync_domain::repositories::catalog::GithubOwner) -> GithubOwnerView {
    GithubOwnerView {
        id: value.id.value().to_string(),
        login: value.login,
        kind: match value.kind {
            nddev_device_sync_domain::repositories::catalog::GithubOwnerKind::User => {
                GithubOwnerKindView::User
            }
            nddev_device_sync_domain::repositories::catalog::GithubOwnerKind::Organization => {
                GithubOwnerKindView::Organization
            }
        },
    }
}

pub(super) fn project_page(
    page: nddev_device_sync_domain::repositories::catalog::RepositoryPage,
) -> (
    GithubRepositoryPageView,
    Option<nddev_device_sync_domain::repositories::catalog::CatalogCursor>,
) {
    use nddev_device_sync_domain::repositories::catalog as c;
    let (continuation, cursor) = match page.continuation {
        c::CatalogContinuation::More(cursor) => (GithubCatalogContinuationView::More, Some(cursor)),
        c::CatalogContinuation::EndOfAccessScope => {
            (GithubCatalogContinuationView::EndOfAccessScope, None)
        }
        c::CatalogContinuation::LimitReached => (GithubCatalogContinuationView::LimitReached, None),
    };
    let groups = page
        .groups
        .into_iter()
        .map(|group| GithubRepositoryGroupView {
            owner: owner(group.owner),
            repositories: group
                .repositories
                .into_iter()
                .map(|repository| GithubRepositorySummaryView {
                    id: repository.id.value().to_string(),
                    name: repository.name,
                    visibility: match repository.visibility {
                        c::RepositoryVisibility::Public => GithubRepositoryVisibilityView::Public,
                        c::RepositoryVisibility::Private => GithubRepositoryVisibilityView::Private,
                        c::RepositoryVisibility::Internal => {
                            GithubRepositoryVisibilityView::Internal
                        }
                    },
                    archived: repository.archived,
                    default_branch: repository.default_branch,
                })
                .collect(),
        })
        .collect();
    (
        GithubRepositoryPageView {
            principal: owner(page.principal),
            groups,
            observed_at_ms: page.observed_at_ms.to_string(),
            continuation,
        },
        cursor,
    )
}

pub(super) fn project_saved_page(
    groups: Vec<nddev_device_sync_domain::repositories::catalog::observations::projection::SavedRepositoryGroup>,
    observed_seq: u64,
    local_revision: u64,
    has_more: bool,
    ignored: impl Fn(u64, Option<&str>) -> bool,
) -> GithubSavedRepositoryPageView {
    use nddev_device_sync_domain::repositories::catalog::{
        RepositoryVisibility, observations::TargetVersion,
    };
    fn observed(version: TargetVersion, at: u64) -> GithubObservationView {
        GithubObservationView {
            revision: version.revision.to_string(),
            pending: version.pending_id.is_some(),
            conflicted: version.conflicted,
            observed_at_ms: at.to_string(),
        }
    }
    GithubSavedRepositoryPageView {
        local_workspace_supported: super::workspace_supported(),
        observed_seq: observed_seq.to_string(),
        local_revision: local_revision.to_string(),
        has_more,
        groups: groups
            .into_iter()
            .map(|group| GithubSavedRepositoryGroupView {
                ignored: ignored(
                    group.owner_id.value(),
                    group.owner.as_ref().map(|value| value.owner.login.as_str()),
                ),
                owner_id: group.owner_id.value().to_string(),
                owner: group.owner.map(|value| GithubSavedOwnerView {
                    owner: owner(value.owner),
                    observation: observed(value.version, value.observed_at_ms),
                }),
                repositories: group
                    .repositories
                    .into_iter()
                    .map(|value| GithubSavedRepositoryView {
                        synchronization: None,
                        repository: GithubRepositorySummaryView {
                            id: value.repository.id.value().to_string(),
                            name: value.repository.name,
                            visibility: match value.repository.visibility {
                                RepositoryVisibility::Public => {
                                    GithubRepositoryVisibilityView::Public
                                }
                                RepositoryVisibility::Private => {
                                    GithubRepositoryVisibilityView::Private
                                }
                                RepositoryVisibility::Internal => {
                                    GithubRepositoryVisibilityView::Internal
                                }
                            },
                            archived: value.repository.archived,
                            default_branch: value.repository.default_branch,
                        },
                        observation: observed(value.version, value.observed_at_ms),
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nddev_device_sync_domain::repositories::catalog as c;
    #[test]
    fn provider_ids_above_javascript_integer_precision_stay_decimal_strings() {
        let principal = c::GithubOwner {
            id: c::GithubAccountId::new(9_007_199_254_740_993).unwrap(),
            kind: c::GithubOwnerKind::User,
            login: "fixture".into(),
        };
        let selection = c::CredentialSelection::new("record".into(), 7, None).unwrap();
        let request = c::CatalogPageRequest::new(selection, None).unwrap();
        let page = c::ProviderRepositoryPage {
            principal: principal.clone(),
            repositories: vec![c::RepositorySummary {
                id: c::RepositoryId::new(u64::MAX).unwrap(),
                owner: principal,
                name: "fixture".into(),
                visibility: c::RepositoryVisibility::Private,
                archived: false,
                default_branch: None,
            }],
            observed_at_ms: 100,
            has_more: false,
        }
        .into_page(&request)
        .unwrap();
        let (view, cursor) = project_page(page);
        assert_eq!(view.principal.id, "9007199254740993");
        assert_eq!(view.groups[0].owner.id, "9007199254740993");
        assert_eq!(view.groups[0].repositories[0].id, "18446744073709551615");
        assert!(cursor.is_none());
        assert!(view.continuation == GithubCatalogContinuationView::EndOfAccessScope);
    }
}

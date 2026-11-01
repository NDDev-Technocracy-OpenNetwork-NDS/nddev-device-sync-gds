//! Shared Rust catalog composition. Core owns traversal/publication decisions,
//! Vault owns encrypted state and fences, and the GitHub module owns the fixed provider I/O.
mod consent;
mod driver;
#[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
mod fleet_acceptance;
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

fn retryable_validation(error: view::GithubCatalogError) -> bool {
    use view::GithubCatalogError::*;
    matches!(error, Busy | Timeout | Unavailable)
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
    #[cfg(not(target_arch = "wasm32"))]
    ignored: crate::ignore::Source,
    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
    fixture: Mutex<Option<Arc<fleet_acceptance::FixtureIo>>>,
}

/// The owner's ignored GitHub accounts for one operation (github#137). Browsers have no
/// host list yet and include every account.
pub(super) struct IgnoreSnapshot(
    #[cfg(not(target_arch = "wasm32"))] crate::ignore::IgnoredAccounts,
);
impl IgnoreSnapshot {
    /// Whether the account is ignored. A saved owner may lack its login; a login-only entry then
    /// cannot match and only a pinned id decides.
    /// Keeps the repositories whose owner is not ignored.
    pub(super) fn retain_included(
        &self,
        repositories: &mut Vec<nddev_device_sync_domain::repositories::catalog::RepositorySummary>,
    ) {
        repositories.retain(|repository| {
            !self.ignores(repository.owner.id.value(), Some(&repository.owner.login))
                && !self.ignores_repository(
                    repository.id.value(),
                    Some(&repository.owner.login),
                    &repository.name,
                )
        });
    }

    /// Whether the repository itself is on the list; [`Self::ignores`] covers its owner.
    pub(super) fn ignores_repository(&self, id: u64, owner: Option<&str>, name: &str) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use crate::ignore::Decision;
            let full_name = owner.map(|owner| format!("{owner}/{name}"));
            let decision = self.0.decide_repository(id, full_name.as_deref());
            if decision == Decision::IgnoredRenamed {
                tracing::warn!(
                    event.name = "github.catalog.repository.renamed",
                    module = "github",
                    error.type = "ignored_repository_renamed",
                    outcome = "rejected"
                );
            }
            decision.ignored()
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (id, owner, name);
            false
        }
    }

    pub(super) fn ignores(&self, id: u64, login: Option<&str>) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use crate::ignore::Decision;
            let decision = match login {
                Some(login) => self.0.decide(id, login),
                None => self.0.decide(id, ""),
            };
            if decision == Decision::IgnoredRenamed {
                tracing::warn!(
                    event.name = "github.catalog.account.renamed",
                    module = "github",
                    error.type = "ignored_owner_renamed",
                    outcome = "rejected"
                );
            }
            decision.ignored()
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (id, login);
            false
        }
    }
}
impl CatalogResources {
    pub(super) fn ignore_snapshot(&self) -> Result<IgnoreSnapshot, view::GithubCatalogError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.ignored
                .current()
                .map(IgnoreSnapshot)
                .map_err(|_| view::GithubCatalogError::IgnoreListInvalid)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Ok(IgnoreSnapshot())
        }
    }

    /// Refuses an operation on a repository whose owner the host ignores.
    pub(super) fn refuse_ignored(
        &self,
        location: &nddev_device_sync_domain::repositories::workspace::CheckoutLocation,
    ) -> Result<(), view::GithubCatalogError> {
        let snapshot = self.ignore_snapshot()?;
        if snapshot.ignores(location.owner_id.value(), Some(location.owner())) {
            return Err(view::GithubCatalogError::AccountIgnored);
        }
        if snapshot.ignores_repository(
            location.repository_id.value(),
            Some(location.owner()),
            location.repository(),
        ) {
            return Err(view::GithubCatalogError::RepositoryIgnored);
        }
        Ok(())
    }
}
#[cfg(all(test, not(target_arch = "wasm32")))]
mod ignore_tests {
    use super::*;
    use nddev_device_sync_domain::repositories::{catalog::*, workspace::CheckoutLocation};

    const LIST: &str = "schema_version: 1\nexcluded_accounts:\n  - login: \"mysummit-school\"\n    provider: \"github\"\n    kind: \"organization\"\n    reason: \"owner decision\"\n";

    fn resources(path: &std::path::Path) -> CatalogResources {
        CatalogResources {
            observation: Arc::default(),
            admission: Arc::new(tokio::sync::Semaphore::new(1)),
            cache: Arc::default(),
            commits: Mutex::default(),
            ignored: crate::ignore::Source::at(Some(path.to_owned())),
            #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
            fixture: Mutex::default(),
        }
    }

    fn repository(id: u64, owner_id: u64, login: &str) -> RepositorySummary {
        RepositorySummary {
            id: RepositoryId::new(id).unwrap(),
            owner: GithubOwner {
                id: GithubAccountId::new(owner_id).unwrap(),
                kind: GithubOwnerKind::Organization,
                login: login.into(),
            },
            name: format!("repo{id}"),
            visibility: RepositoryVisibility::Private,
            archived: false,
            default_branch: Some("main".into()),
        }
    }

    fn location(owner_id: u64, login: &str) -> CheckoutLocation {
        CheckoutLocation::new(
            RepositoryId::new(9).unwrap(),
            GithubAccountId::new(owner_id).unwrap(),
            login.into(),
            "repo".into(),
        )
        .unwrap()
    }

    #[test]
    fn an_ignored_organization_is_never_discovered_connected_or_synchronized() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(crate::ignore::FILE_NAME);
        let shared = resources(&path);
        let mut page = vec![
            repository(1, 10, "nddev"),
            repository(2, 20, "MySummit-School"),
        ];
        std::fs::write(&path, LIST).unwrap();
        shared.ignore_snapshot().unwrap().retain_included(&mut page);
        assert_eq!(page.iter().map(|r| r.id.value()).collect::<Vec<_>>(), [1]);
        assert_eq!(
            shared.refuse_ignored(&location(20, "mysummit-school")),
            Err(view::GithubCatalogError::AccountIgnored)
        );
        assert_eq!(shared.refuse_ignored(&location(10, "nddev")), Ok(()));
        // Removing the login restores the normal path on the next operation.
        std::fs::write(&path, "schema_version: 1\nexcluded_accounts: []\n").unwrap();
        let mut page = vec![
            repository(1, 10, "nddev"),
            repository(2, 20, "mysummit-school"),
        ];
        shared.ignore_snapshot().unwrap().retain_included(&mut page);
        assert_eq!(page.len(), 2);
        assert_eq!(
            shared.refuse_ignored(&location(20, "mysummit-school")),
            Ok(())
        );
    }

    #[test]
    fn an_ignored_repository_is_dropped_refused_and_marked_while_its_owner_stays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(crate::ignore::FILE_NAME);
        std::fs::write(
            &path,
            "schema_version: 1\nexcluded_repositories:\n  - repository: \"nddev/old-site\"\n    provider: \"github\"\n    reason: \"retired\"\n",
        )
        .unwrap();
        let shared = resources(&path);
        let mut page = vec![repository(1, 10, "nddev"), repository(2, 10, "nddev")];
        page[1].name = "Old-Site".into();
        shared.ignore_snapshot().unwrap().retain_included(&mut page);
        assert_eq!(page.iter().map(|r| r.id.value()).collect::<Vec<_>>(), [1]);
        let site = CheckoutLocation::new(
            RepositoryId::new(2).unwrap(),
            GithubAccountId::new(10).unwrap(),
            "nddev".into(),
            "old-site".into(),
        )
        .unwrap();
        assert_eq!(
            shared.refuse_ignored(&site),
            Err(view::GithubCatalogError::RepositoryIgnored)
        );
        assert_eq!(shared.refuse_ignored(&location(10, "nddev")), Ok(()));
        let snapshot = shared.ignore_snapshot().unwrap();
        assert!(snapshot.ignores_repository(2, Some("nddev"), "old-site"));
        assert!(!snapshot.ignores(10, Some("nddev")));
        assert_eq!(
            driver::caller_error(&view::GithubCatalogError::RepositoryIgnored).0,
            "repository_ignored"
        );
    }

    #[test]
    fn an_unreadable_list_refuses_instead_of_including() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(crate::ignore::FILE_NAME);
        let shared = resources(&path);
        assert!(
            shared.ignore_snapshot().is_ok(),
            "an absent list ignores nothing"
        );
        std::fs::write(
            &path,
            "schema_version: 1\nexcluded_accounts:\n  - login: x\n",
        )
        .unwrap();
        assert!(matches!(
            shared.ignore_snapshot(),
            Err(view::GithubCatalogError::IgnoreListInvalid)
        ));
        assert_eq!(
            shared.refuse_ignored(&location(10, "nddev")),
            Err(view::GithubCatalogError::IgnoreListInvalid)
        );
        assert_eq!(
            driver::caller_error(&view::GithubCatalogError::IgnoreListInvalid).0,
            "ignore_list_invalid"
        );
        assert_eq!(
            driver::caller_error(&view::GithubCatalogError::AccountIgnored).0,
            "account_ignored"
        );
    }

    #[test]
    fn saved_data_of_an_ignored_owner_stays_and_is_marked() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(crate::ignore::FILE_NAME);
        std::fs::write(&path, LIST).unwrap();
        let snapshot = resources(&path).ignore_snapshot().unwrap();
        use nddev_device_sync_domain::repositories::catalog::observations::{
            RepositoryFact, TargetVersion,
            projection::{SavedOwner, SavedRepository, SavedRepositoryGroup},
        };
        let version = TargetVersion {
            revision: 3,
            next_base: 3,
            pending_id: None,
            conflicted: false,
        };
        let group = |owner_id: u64, login: Option<&str>| SavedRepositoryGroup {
            owner_id: GithubAccountId::new(owner_id).unwrap(),
            owner: login.map(|login| SavedOwner {
                owner: repository(1, owner_id, login).owner,
                version: version.clone(),
                observed_at_ms: 5,
            }),
            repositories: vec![SavedRepository {
                repository: RepositoryFact::from_summary(&repository(owner_id + 1, owner_id, "x"))
                    .unwrap(),
                version: version.clone(),
                observed_at_ms: 5,
            }],
        };
        let view = view::project_saved_page(
            vec![
                group(10, Some("nddev")),
                group(20, Some("mysummit-school")),
                group(30, None),
            ],
            1,
            1,
            false,
            |id, login| snapshot.ignores(id, login),
            |id, owner, name| snapshot.ignores_repository(id, owner, name),
        );
        let marked = view
            .groups
            .iter()
            .map(|g| (g.owner_id.as_str(), g.ignored, g.repositories.len()))
            .collect::<Vec<_>>();
        // Saved data stays; only the ignored owner is marked. Without a saved
        // owner login only a pinned id can decide.
        assert_eq!(
            marked,
            [("10", false, 1), ("20", true, 1), ("30", false, 1)]
        );
        assert!(
            view.groups
                .iter()
                .all(|g| g.repositories.iter().all(|r| !r.ignored))
        );
    }
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
            #[cfg(not(target_arch = "wasm32"))]
            ignored: crate::ignore::Source::host_default(),
            #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
            fixture: Mutex::default(),
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
        let consent = self.vault.module_configuration("github").await.ok()?;
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
        let consent = self.vault.module_configuration("github").await?;
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
        let consent = self.vault.module_configuration("github").await?;
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
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        self.pause_fixture_validation(false).await?;
        if let Some(original) = original
            && let Err(error) = original.validate().await
        {
            // An unavailable proof is not evidence of a changed scope. Keep
            // its exact witness and cursor, but admit no new Git operation.
            if retryable_validation(error) {
                return Err(error);
            }
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
        let (current, original) = {
            let mut commits = self
                .shared
                .commits
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (commits.queue.pop_front(), commits.guard.clone())
        };
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
            .synchronize_connection(&location, original, cancellation)
            .await;
        #[cfg(not(all(feature = "commit-sync", any(target_os = "linux", target_os = "macos"))))]
        let result: Result<
            nddev_device_sync_application::repositories::commits::CommitOutcome,
            _,
        > = {
            drop(original);
            Err(GithubCatalogError::Unavailable)
        };
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
        #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
        self.pause_fixture_validation(true).await?;
        let validation_error = match original.validate().await {
            Ok(()) => None,
            Err(error) if retryable_validation(error) => Some(error),
            Err(_) => {
                self.shared
                    .commits
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cancel();
                return Err(GithubCatalogError::Cancelled);
            }
        };
        // A completed operation cannot report success while its original proof
        // is unavailable. Record the refusal without discarding prior progress;
        // saved presentation still revalidates this exact witness before use.
        let result = validation_error.map_or(result, Err);
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
        validation_error.map_or(Ok(true), Err)
    }

    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
    async fn pause_fixture_validation(&self, after: bool) -> Result<(), view::GithubCatalogError> {
        let fixture = self.shared.fixture.lock().unwrap().clone();
        if let Some(fixture) = fixture {
            fixture.pause_validation(after).await?;
        }
        Ok(())
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
                "github",
                "github.catalog.collection",
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

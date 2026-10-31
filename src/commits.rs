//! Fixed native Git operations for Core's existing-commit cycle. The caller
//! owns module consent, current catalog/credential selection and authorization.
//! No ambient credential, automatic commit, force, reset, rebase or PR operation.
use base64::{Engine, engine::general_purpose::STANDARD};
use nddev_device_sync_adapter_io::{CancellationToken, ProcessIo, ProcessOutput, ProcessRequest};
use nddev_device_sync_application::repositories::commits::{
    self, CommitOutcome, Inspection, SynchronizeCommits,
};
use nddev_device_sync_domain::{
    modules::ModuleError,
    repositories::{
        workspace::{CheckoutBinding, CheckoutLocation, SyncBlock, WorkspacePrincipal},
        *,
    },
};
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::Instrument;
use zeroize::Zeroizing;

#[cfg(target_os = "windows")]
use std::fs::OpenOptions;

const OUTPUT_BYTES: usize = 1_048_576;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "native_checkout.rs"]
mod native_checkout;

#[path = "submodules.rs"]
mod submodules;

/// Native-only capability, intentionally neither Debug nor Serialize. The Git
/// executable is resolved by the installation, never by repository metadata.
pub struct GitWorkspace {
    io: ProcessIo,
    git: PathBuf,
    root: PathBuf,
    principal: WorkspacePrincipal,
    #[cfg(test)]
    fixture_remote: Option<String>,
}
impl GitWorkspace {
    #[cfg(all(test, target_os = "linux", feature = "catalog-runtime"))]
    pub(crate) fn with_fixture_remote(mut self, remote: String) -> Self {
        self.fixture_remote = Some(remote);
        self
    }
    pub fn new(
        io: ProcessIo,
        git: PathBuf,
        root: PathBuf,
        principal: WorkspacePrincipal,
    ) -> Result<Self, ModuleError> {
        principal.validate()?;
        if !git.is_absolute() || !root.is_absolute() {
            return Err(ModuleError::InvalidInput);
        }
        let root = root.canonicalize().map_err(|_| ModuleError::Unavailable)?;
        if !root.is_dir() || root.join(".git").exists() {
            return Err(ModuleError::InvalidInput);
        }
        Ok(Self {
            io,
            git,
            root,
            principal,
            #[cfg(test)]
            fixture_remote: None,
        })
    }
    fn endpoint(&self, location: &CheckoutLocation) -> String {
        #[cfg(test)]
        if let Some(endpoint) = &self.fixture_remote {
            return endpoint.clone();
        }
        location.https_url()
    }

    /// Only an existing checkout in the authorized owner/repository layout.
    /// `revalidate` checks actual NDS authority, module write consent and the
    /// exact protected credential selection before EACH command and completion.
    ///
    /// The caller retains and polls this future through cancellation until it
    /// returns. Dropping a waiter requests child termination but is not a drain
    /// acknowledgement or proof that a submitted push had no remote effect.
    pub async fn synchronize<F>(
        &self,
        location: &CheckoutLocation,
        archived: bool,
        credential: &str,
        cancellation: &CancellationToken,
        mut revalidate: impl FnMut() -> F + Send,
    ) -> Result<CommitOutcome, ModuleError>
    where
        F: std::future::Future<Output = Result<(), ModuleError>> + Send,
    {
        location.validate()?;
        let checkout = self.root.join(location.owner()).join(location.repository());
        if checkout
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .as_deref()
            != Some(self.root.join(location.owner()).as_path())
            || checkout.canonicalize().ok().as_deref() != Some(checkout.as_path())
        {
            return Err(ModuleError::InvalidInput);
        }
        let git_dir = checkout.join(".git");
        if !std::fs::symlink_metadata(&git_dir)
            .map_err(|_| ModuleError::Unavailable)?
            .file_type()
            .is_dir()
        {
            return Err(ModuleError::Unavailable);
        }
        let binding = read_binding(&git_dir)?.ok_or(ModuleError::Unavailable)?;
        if binding.location != *location || binding.principal != self.principal {
            return Ok(CommitOutcome::Blocked(SyncBlock::WrongRemote));
        }
        let endpoint = self.endpoint(location);
        self.run(
            checkout,
            endpoint,
            archived,
            credential,
            cancellation,
            move || {
                let future = revalidate();
                let git_dir = git_dir.clone();
                let binding = binding.clone();
                async move {
                    future.await?;
                    if read_binding(&git_dir)?.as_ref() != Some(&binding) {
                        return Err(ModuleError::Busy);
                    }
                    Ok(())
                }
            },
        )
        .await
    }

    async fn run<F>(
        &self,
        checkout: PathBuf,
        endpoint: String,
        archived: bool,
        credential: &str,
        cancellation: &CancellationToken,
        mut revalidate: impl FnMut() -> F + Send,
    ) -> Result<CommitOutcome, ModuleError>
    where
        F: std::future::Future<Output = Result<(), ModuleError>> + Send,
    {
        if credential.is_empty()
            || credential.len() > 8_192
            || !credential.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(ModuleError::InvalidInput);
        }
        if cancellation.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        // Capture read-only native owners before awaiting authority. Matching
        // marker bytes cannot let a replacement directory inherit this cycle.
        let git_dir = checkout.join(".git");
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let directories = native_checkout::CheckoutDirectories::capture(&checkout, &git_dir)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        tokio::select! { biased;
            () = cancellation.cancelled() => return Err(ModuleError::Cancelled),
            result = tokio::time::timeout_at(deadline, revalidate()) => result.map_err(|_| ModuleError::Timeout)??,
        }
        if cancellation.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let input = Zeroizing::new(format!("x-access-token:{credential}"));
        let mut authorization = Zeroizing::new(String::from("Authorization: Basic "));
        STANDARD.encode_string(input.as_bytes(), &mut authorization);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        directories.validate_paths(&checkout, &git_dir)?;
        if !git_dir.is_dir()
            || std::fs::symlink_metadata(&git_dir)
                .map_err(|_| ModuleError::Unavailable)?
                .file_type()
                .is_symlink()
        {
            return Err(ModuleError::Unavailable);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let lease = directories.lease()?;
        #[cfg(target_os = "windows")]
        let lease = {
            if std::fs::symlink_metadata(git_dir.join("nds-sync.lock"))
                .is_ok_and(|metadata| !metadata.file_type().is_file())
            {
                return Err(ModuleError::Unavailable);
            }
            let lease = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(git_dir.join("nds-sync.lock"))
                .map_err(|_| ModuleError::Unavailable)?;
            lease.try_lock().map_err(|_| ModuleError::Busy)?;
            lease
        };
        let cancellation = cancellation.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let location = read_binding(&git_dir)?.map(|binding| binding.location);
        let mut session = Session {
            workspace: self,
            checkout,
            endpoint,
            archived,
            authorization,
            cancellation: cancellation.clone(),
            revalidate,
            git_dir,
            location,
            _lease: lease,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            directories: Some(directories),
            fetched: None,
            deadline,
        };
        let span = nddev_device_sync_telemetry::operation_span("github", "io")
            .map_err(|_| ModuleError::InvalidContract)?;
        async {
            // Each command/guard checks the same deadline. Let ProcessIo's
            // cancellation path kill AND reap its child before dropping the
            // checkout lease; dropping an outer select would skip that drain.
            let result = async {
                    let result = async {
                        session.safe_configuration().await?;
                        let result = commits::synchronize(&mut session).await?;
                        session.validate().await?;
                        Ok(result)
                    }.await;
                    // All owned Git commands have ended. Explicit release
                    // avoids a transient forked child retaining this flock.
                    // Cancellation/outer timeout still drops the owned scope.
                    session._lease.unlock().map_err(|_| ModuleError::Unavailable)?;
                    result
            }.await;
            nddev_device_sync_telemetry::finish_operation_span(&tracing::Span::current(), nddev_device_sync_domain::modules::query_outcome(&result).terminal_failure());
            match &result {
                Ok(CommitOutcome::Blocked(_)) => tracing::info!(event.name="github.commits.completed", module="github", scope="io", outcome="rejected", error.type="git_sync_blocked"),
                Ok(_) => tracing::info!(event.name="github.commits.completed", module="github", scope="io", outcome="ok"),
                Err(error) => tracing::warn!(event.name="github.commits.failed", module="github", scope="io", error.type=%error, outcome="error"),
            }
            result
        }.instrument(span).await
    }
}

struct Session<'a, G> {
    workspace: &'a GitWorkspace,
    checkout: PathBuf,
    endpoint: String,
    archived: bool,
    authorization: Zeroizing<String>,
    cancellation: CancellationToken,
    revalidate: G,
    git_dir: PathBuf,
    _lease: File,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    directories: Option<native_checkout::CheckoutDirectories>,
    fetched: Option<String>,
    location: Option<CheckoutLocation>,
    deadline: tokio::time::Instant,
}
#[derive(PartialEq, Eq)]
struct Witness {
    branch: String,
    head: String,
    upstream: String,
    origin: String,
    status: Vec<u8>,
    active_writer: bool,
    submodules: Vec<submodules::SubmoduleWitness>,
}

impl<G, F> Session<'_, G>
where
    G: FnMut() -> F + Send,
    F: std::future::Future<Output = Result<(), ModuleError>> + Send,
{
    fn validate_checkout(&self) -> Result<(), ModuleError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(directories) = &self.directories {
            directories.validate(&self.checkout, &self.git_dir, &self._lease)?;
        }
        Ok(())
    }
    async fn validate(&mut self) -> Result<(), ModuleError> {
        self.validate_checkout()?;
        if tokio::time::Instant::now() >= self.deadline {
            return Err(ModuleError::Timeout);
        }
        if self.cancellation.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        tokio::select! { biased;
            () = self.cancellation.cancelled() => return Err(ModuleError::Cancelled),
            result = tokio::time::timeout_at(self.deadline, (self.revalidate)()) => result.map_err(|_| ModuleError::Timeout)??,
        }
        if self.cancellation.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        self.validate_checkout()
    }
    async fn git(&mut self, arguments: &[&str]) -> Result<ProcessOutput, ModuleError> {
        self.git_in(&self.checkout.clone(), &self.git_dir.clone(), arguments)
            .await
    }
    async fn git_in(
        &mut self,
        checkout: &Path,
        git_dir: &Path,
        arguments: &[&str],
    ) -> Result<ProcessOutput, ModuleError> {
        self.validate().await?;
        let network = matches!(arguments.first(), Some(&"fetch" | &"push" | &"clone"));
        let mut argv: Vec<OsString> = [
            "--no-replace-objects",
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "maintenance.auto=false",
            "-c",
            "credential.helper=",
            "-c",
            "http.followRedirects=false",
            "-c",
            "protocol.allow=never",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        // Never let a submodule's core.worktree or Git discovery select a
        // different checkout. Paths are qualified before any child command.
        if arguments.first() != Some(&"clone") {
            let mut directory_arg = OsString::from("--git-dir=");
            directory_arg.push(git_dir);
            argv.push(directory_arg);
            let mut worktree_arg = OsString::from("--work-tree=");
            worktree_arg.push(checkout);
            argv.push(worktree_arg);
        }
        if network {
            argv.extend([
                OsString::from("-c"),
                OsString::from("protocol.https.allow=always"),
                OsString::from(
                    "--config-env=http.https://github.com/.extraHeader=NDS_GITHUB_AUTHORIZATION",
                ),
            ]);
        }
        // A local directory transport is available only to this module's real
        // Git acceptance. It is absent from the production constructor/API.
        #[cfg(test)]
        if network && PathBuf::from(&self.endpoint).is_absolute() {
            argv.extend([
                OsString::from("-c"),
                OsString::from("protocol.file.allow=always"),
            ]);
        }
        argv.extend(arguments.iter().map(OsString::from));
        self.workspace
            .io
            .git_process(
                ProcessRequest {
                    executable: self.workspace.git.clone(),
                    arguments: argv,
                    directory: Some(checkout.to_path_buf()),
                    timeout: self
                        .deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .min(Duration::from_secs(30)),
                    max_output_bytes: OUTPUT_BYTES,
                },
                network.then(|| self.authorization.clone()),
                self.cancellation.clone(),
            )
            .await
            .map_err(map_io)
    }
    async fn required(&mut self, arguments: &[&str]) -> Result<Vec<u8>, ModuleError> {
        let result = self.git(arguments).await?;
        if !result.status.success() {
            #[cfg(test)]
            eprintln!(
                "isolated Git {} refusal: {}",
                arguments[0],
                String::from_utf8_lossy(&result.stderr)
            );
            tracing::warn!(event.name="github.git.failed",module="github",scope="io",error.type="git_exit",outcome="error");
            return Err(ModuleError::ProviderFailed);
        }
        Ok(result.stdout)
    }
    async fn optional(&mut self, arguments: &[&str]) -> Result<String, ModuleError> {
        let result = self.git(arguments).await?;
        if result.status.code() == Some(1) {
            return Ok(String::new());
        }
        if !result.status.success() {
            return Err(ModuleError::ProviderFailed);
        }
        text(result.stdout)
    }
    async fn safe_configuration(&mut self) -> Result<(), ModuleError> {
        self.safe_configuration_in(&self.checkout.clone(), &self.git_dir.clone(), false)
            .await
    }
    async fn safe_configuration_in(
        &mut self,
        checkout: &Path,
        git_dir: &Path,
        child: bool,
    ) -> Result<(), ModuleError> {
        if std::fs::symlink_metadata(git_dir.join("commondir")).is_ok() {
            return Err(ModuleError::Unavailable);
        }
        for name in [
            "objects/info/alternates",
            "objects/info/http-alternates",
            "info/grafts",
        ] {
            if std::fs::symlink_metadata(git_dir.join(name)).is_ok() {
                return Err(ModuleError::Unavailable);
            }
        }
        for name in [
            "config", "objects", "refs", "HEAD", "index", "logs", "hooks",
        ] {
            self.no_symlink(&git_dir.join(name))?;
        }
        let settings = self
            .git_in(checkout, git_dir, &[
                "config",
                "--no-includes",
                "--local",
                "--name-only",
                "--get-regexp",
                r"^(include|includeif|url|filter|http|push\.pushoption|core\.(worktree|bare|hookspath|alternaterefscommand)|extensions)",
            ])
            .await?;
        if !settings.status.success() && settings.status.code() != Some(1) {
            return Err(ModuleError::ProviderFailed);
        }
        let settings = text(settings.stdout)?;
        // core.bare=false is ordinary clone metadata. Every other named
        // setting here changes path/transport or executes an external driver.
        if settings
            .lines()
            .any(|key| key != "core.bare" && !(child && key == "core.worktree"))
        {
            return Err(ModuleError::Unavailable);
        }
        if child && settings.lines().any(|key| key == "core.worktree") {
            let value = self
                .git_in(
                    checkout,
                    git_dir,
                    &[
                        "config",
                        "--no-includes",
                        "--local",
                        "--get-all",
                        "core.worktree",
                    ],
                )
                .await?;
            if !value.status.success() {
                return Err(ModuleError::Unavailable);
            }
            let value = text(value.stdout)?;
            if value.lines().count() != 1
                || git_dir.join(&value).canonicalize().ok().as_deref() != Some(checkout)
            {
                return Err(ModuleError::Unavailable);
            }
        }
        for name in ["pre-push", "post-merge", "post-checkout"] {
            if git_dir.join("hooks").join(name).exists() {
                return Err(ModuleError::Unavailable);
            }
        }
        Ok(())
    }
    fn no_symlink(&self, path: &std::path::Path) -> Result<(), ModuleError> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(ModuleError::Unavailable),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(ModuleError::Unavailable),
        }
    }
    async fn ordinary_ref(&mut self, name: &str) -> Result<(), ModuleError> {
        let mut path = self.git_dir.clone();
        for component in name.split('/') {
            if matches!(component, "" | "." | "..") {
                return Err(ModuleError::InvalidResponse);
            }
            path.push(component);
            self.no_symlink(&path)?;
        }
        if !self
            .optional(&["symbolic-ref", "--quiet", name])
            .await?
            .is_empty()
        {
            return Err(ModuleError::Unavailable);
        }
        Ok(())
    }
    fn active_writer(&self) -> bool {
        submodules::active_writer(&self.git_dir)
    }
    async fn witness(&mut self) -> Result<Witness, ModuleError> {
        self.safe_configuration().await?;
        let branch = self.optional(&["symbolic-ref", "--quiet", "HEAD"]).await?;
        let head = self
            .optional(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await?;
        if !head.is_empty() {
            oid(&head)?;
        }
        let submodules = self.submodule_witnesses(&head).await?;
        let upstream = if head.is_empty() || branch.is_empty() {
            String::new()
        } else {
            let result = self
                .git(&["rev-parse", "--symbolic-full-name", "@{upstream}"])
                .await?;
            if result.status.success() {
                text(result.stdout)?
            } else {
                String::new()
            }
        };
        let origin = self
            .optional(&[
                "config",
                "--no-includes",
                "--local",
                "--get-all",
                "remote.origin.url",
            ])
            .await?;
        let status = self
            .required(&[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=normal",
                "--ignore-submodules=all",
            ])
            .await?;
        let active_writer = self.active_writer()
            || submodules.iter().any(|child| child.active_writer)
            || (!branch.is_empty() && self.git_dir.join(format!("{branch}.lock")).exists());
        Ok(Witness {
            branch,
            head,
            upstream,
            origin,
            status,
            active_writer,
            submodules,
        })
    }
    async fn unchanged(&mut self, expected: &Witness) -> Result<(), ModuleError> {
        self.safe_configuration().await?;
        self.ordinary_ref(&expected.branch).await?;
        self.ordinary_ref(&expected.upstream).await?;
        if self.witness().await? != *expected {
            return Err(ModuleError::Busy);
        }
        self.validate().await
    }
}
impl<G, F> SynchronizeCommits for Session<'_, G>
where
    G: FnMut() -> F + Send,
    F: std::future::Future<Output = Result<(), ModuleError>> + Send,
{
    type Witness = Witness;
    async fn inspect(&mut self) -> Result<Inspection<Witness>, ModuleError> {
        let witness = self.witness().await?;
        let mut changes = Changes {
            submodule_changes: witness
                .submodules
                .iter()
                .filter(|child| child.dirty)
                .count() as u32,
            ..Changes::default()
        };
        let mut rows = witness.status.split(|b| *b == 0);
        while let Some(row) = rows.next() {
            if row.is_empty() {
                continue;
            }
            match row[0] {
                b'?' => changes.untracked += 1,
                b'u' => changes.conflicted += 1,
                b'1' | b'2' if row.len() > 10 => {
                    changes.staged += u32::from(row[2] != b'.');
                    changes.unstaged += u32::from(row[3] != b'.');
                    changes.submodule_changes += u32::from(&row[5..9] != b"N...");
                    if row[0] == b'2' {
                        rows.next().ok_or(ModuleError::InvalidResponse)?;
                    }
                }
                _ => return Err(ModuleError::InvalidResponse),
            }
        }
        let comparison = if let Some(remote) = self.fetched.clone() {
            let range = format!("{}...{remote}", witness.head);
            let counts = text(
                self.required(&["rev-list", "--left-right", "--count", &range, "--"])
                    .await?,
            )?;
            let parts = counts
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| ModuleError::InvalidResponse)?;
            if parts.len() != 2 {
                return Err(ModuleError::InvalidResponse);
            }
            BranchComparison::Available {
                ahead: parts[0],
                behind: parts[1],
            }
        } else {
            BranchComparison::Unavailable
        };
        let branch = witness
            .branch
            .strip_prefix("refs/heads/")
            .map(str::to_owned);
        let upstream_matches = branch
            .as_ref()
            .is_some_and(|b| witness.upstream == format!("refs/remotes/origin/{b}"))
            && (witness.origin == self.endpoint
                || self
                    .location
                    .as_ref()
                    .is_some_and(|location| location.matches_origin(&witness.origin)));
        let status = LocalGitStatus {
            head_kind: if witness.branch.is_empty() {
                HeadKind::Detached
            } else if witness.head.is_empty() {
                HeadKind::Unborn
            } else {
                HeadKind::Branch
            },
            branch,
            upstream: (!witness.upstream.is_empty()).then(|| witness.upstream.clone()),
            comparison,
            changes,
            partial_reasons: vec![],
        };
        Ok(Inspection {
            status,
            archived: self.archived,
            active_writer: witness.active_writer,
            upstream_matches,
            witness,
        })
    }
    async fn fetch(&mut self, expected: &Witness) -> Result<(), ModuleError> {
        self.unchanged(expected).await?;
        let branch = expected
            .branch
            .strip_prefix("refs/heads/")
            .ok_or(ModuleError::InvalidResponse)?;
        let remote = format!("refs/remotes/origin/{branch}");
        let refspec = format!("{}:{remote}", expected.branch);
        let endpoint = self.endpoint.clone();
        self.required(&[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-auto-maintenance",
            "--no-write-fetch-head",
            &endpoint,
            &refspec,
        ])
        .await?;
        let fetched = text(
            self.required(&["rev-parse", "--verify", &format!("{remote}^{{commit}}")])
                .await?,
        )?;
        oid(&fetched)?;
        let tree = self
            .required(&["ls-tree", "-r", "--full-tree", "-z", &fetched])
            .await?;
        let current_tree = self
            .required(&["ls-tree", "-r", "--full-tree", "-z", &expected.head])
            .await?;
        if submodules::graph(&tree)? != submodules::graph(&current_tree)? {
            tracing::warn!(event.name="github.commits.refused",module="github",scope="io",error.type="changed_submodule_graph",outcome="rejected");
            return Err(ModuleError::Unavailable);
        }
        // A branch switch during fetch must not authorize a different branch.
        if self.witness().await? != *expected {
            return Err(ModuleError::Busy);
        }
        self.fetched = Some(fetched);
        Ok(())
    }
    async fn push(&mut self, expected: &Witness) -> Result<(), ModuleError> {
        self.unchanged(expected).await?;
        let refspec = format!("{}:{}", expected.head, expected.branch);
        let endpoint = self.endpoint.clone();
        self.required(&[
            "push",
            "--porcelain",
            "--no-follow-tags",
            "--recurse-submodules=no",
            &endpoint,
            &refspec,
        ])
        .await?;
        let remote = format!(
            "refs/remotes/origin/{}",
            expected
                .branch
                .strip_prefix("refs/heads/")
                .ok_or(ModuleError::InvalidResponse)?
        );
        let old = self.fetched.clone().ok_or(ModuleError::InvalidContract)?;
        self.ordinary_ref(&remote).await?;
        let current = text(
            self.required(&["rev-parse", "--verify", &format!("{remote}^{{commit}}")])
                .await?,
        )?;
        // Git may update a matching remote-tracking ref during a URL push.
        // An already accepted commit is idempotent; another writer's value
        // cannot authorize overwriting that ref.
        if current != expected.head {
            if current != old {
                return Err(ModuleError::Busy);
            }
            self.required(&["update-ref", &remote, &expected.head, &old])
                .await?;
        }
        self.fetched = Some(expected.head.clone());
        Ok(())
    }
    async fn fast_forward(&mut self, expected: &Witness) -> Result<(), ModuleError> {
        self.unchanged(expected).await?;
        let remote = self.fetched.clone().ok_or(ModuleError::InvalidContract)?;
        self.required(&[
            "merge",
            "--ff-only",
            "--no-autostash",
            "--no-edit",
            "--no-stat",
            &remote,
        ])
        .await?;
        Ok(())
    }
}

fn read_binding(git_dir: &std::path::Path) -> Result<Option<CheckoutBinding>, ModuleError> {
    use std::io::Read;
    let path = git_dir.join("nds-binding.json");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(ModuleError::Unavailable),
    };
    if !metadata.is_file() || metadata.len() > 8192 {
        return Err(ModuleError::InvalidResponse);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| ModuleError::Unavailable)?
        .take(8193)
        .read_to_end(&mut bytes)
        .map_err(|_| ModuleError::Unavailable)?;
    if bytes.len() > 8192 {
        return Err(ModuleError::InvalidResponse);
    }
    let binding: CheckoutBinding =
        serde_json::from_slice(&bytes).map_err(|_| ModuleError::InvalidResponse)?;
    binding
        .validate()
        .map_err(|_| ModuleError::InvalidResponse)?;
    Ok(Some(binding))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "workspace.rs"]
mod workspace;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use workspace::ConnectionOutcome;
fn text(bytes: Vec<u8>) -> Result<String, ModuleError> {
    String::from_utf8(bytes)
        .map(|v| v.trim_end_matches(['\r', '\n']).to_owned())
        .map_err(|_| ModuleError::InvalidResponse)
}
fn oid(value: &str) -> Result<(), ModuleError> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        Err(ModuleError::InvalidResponse)
    } else {
        Ok(())
    }
}
fn map_io(error: nddev_device_sync_adapter_io::IoError) -> ModuleError {
    use nddev_device_sync_adapter_io::IoError as E;
    match error {
        E::Busy => ModuleError::Busy,
        E::Timeout => ModuleError::Timeout,
        E::Cancelled => ModuleError::Cancelled,
        E::InvalidConfiguration => ModuleError::InvalidInput,
        E::OutputLimit | E::InvalidFrame | E::InvalidResponse => ModuleError::InvalidResponse,
        E::Unavailable => ModuleError::Unavailable,
        E::ProviderExit | E::Transport => ModuleError::ProviderFailed,
    }
}

#[cfg(test)]
#[path = "commits_tests.rs"]
mod tests;

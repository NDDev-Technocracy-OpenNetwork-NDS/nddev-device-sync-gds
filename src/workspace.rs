//! Explicit connection only. The existing-commit scheduler is not activated here.
//! Existing work/configuration is read, never rewritten; only an additional
//! local marker is created when a caller has proved its original write grant.
use super::*;
use nddev_device_sync_domain::repositories::workspace::{
    CheckoutPresence, ConnectionBlock, ConnectionDecision, connection_decision,
};
use rustix::fs::{Mode, OFlags, RenameFlags, openat, renameat_with};
use std::{io::Write, os::unix::fs::MetadataExt, path::Path};

pub enum ConnectionOutcome {
    Connected {
        binding: CheckoutBinding,
        created: bool,
    },
    Blocked(ConnectionBlock),
}

struct UnlockLease(File);
impl Drop for UnlockLease {
    fn drop(&mut self) {
        // A concurrently forked unrelated child may retain the descriptor until
        // exec. Release the shared OS lock explicitly after our work has ended.
        let _ = self.0.unlock();
    }
}

struct EmptyOwner<'a> {
    root: &'a File,
    name: &'a str,
    created: bool,
    identity: (u64, u64),
}
impl Drop for EmptyOwner<'_> {
    fn drop(&mut self) {
        if self.created
            && openat(self.root, self.name, directory_flags(), Mode::empty())
                .ok()
                .map(File::from)
                .and_then(|current| directory_identity(&current).ok())
                == Some(self.identity)
        {
            // Only the same empty directory created by this operation is eligible.
            // Existing directories and published/user content are preserved.
            let _ = rustix::fs::unlinkat(self.root, self.name, rustix::fs::AtFlags::REMOVEDIR);
        }
    }
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}
fn directory_identity(directory: &File) -> Result<(u64, u64), ModuleError> {
    let metadata = directory.metadata().map_err(|_| ModuleError::Unavailable)?;
    Ok((metadata.dev(), metadata.ino()))
}
fn same_directory(path: &Path, directory: &File) -> bool {
    open_directory(path)
        .and_then(|current| directory_identity(&current))
        .ok()
        .is_some_and(|current| directory_identity(directory).ok() == Some(current))
}
fn open_directory(path: &Path) -> Result<File, ModuleError> {
    rustix::fs::open(path, directory_flags(), Mode::empty())
        .map(File::from)
        .map_err(|_| ModuleError::Unavailable)
}
fn lease(directory: &File, name: &str) -> Result<File, ModuleError> {
    let file = File::from(
        openat(
            directory,
            name,
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| ModuleError::Unavailable)?,
    );
    if !file
        .metadata()
        .map_err(|_| ModuleError::Unavailable)?
        .is_file()
    {
        return Err(ModuleError::InvalidInput);
    }
    file.try_lock().map_err(|_| ModuleError::Busy)?;
    Ok(file)
}
fn collision(directory: &Path, name: &str) -> Result<bool, ModuleError> {
    for (count, entry) in std::fs::read_dir(directory)
        .map_err(|_| ModuleError::Unavailable)?
        .enumerate()
    {
        if count >= 4096 {
            return Err(ModuleError::Busy);
        }
        let entry = entry.map_err(|_| ModuleError::Unavailable)?;
        if let Some(value) = entry.file_name().to_str()
            && value.eq_ignore_ascii_case(name)
            && value != name
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn authorize<F: std::future::Future<Output = Result<(), ModuleError>> + Send>(
    cancellation: &CancellationToken,
    revalidate: &mut (impl FnMut() -> F + Send),
) -> Result<(), ModuleError> {
    tokio::select! { biased;
        () = cancellation.cancelled() => Err(ModuleError::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(10), revalidate()) => result.unwrap_or(Err(ModuleError::Timeout)),
    }
}

impl GitWorkspace {
    /// The UI supplies no filesystem path. A caller may initialize only the
    /// canonical home-relative workspace, after checking its write grant.
    pub async fn default_workspace<
        F: std::future::Future<Output = Result<(), ModuleError>> + Send,
    >(
        principal: WorkspacePrincipal,
        mut revalidate: impl FnMut() -> F + Send,
    ) -> Result<Self, ModuleError> {
        let mut admitted = || tokio::time::timeout(Duration::from_secs(10), revalidate());
        admitted().await.unwrap_or(Err(ModuleError::Timeout))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or(ModuleError::Unavailable)?;
        let mut root = home.canonicalize().map_err(|_| ModuleError::Unavailable)?;
        for component in ["Developer", "NDS"] {
            admitted().await.unwrap_or(Err(ModuleError::Timeout))?;
            let parent = open_directory(&root)?;
            match rustix::fs::mkdirat(&parent, component, Mode::RWXU) {
                Ok(()) => parent.sync_all().map_err(|_| ModuleError::Unavailable)?,
                Err(error) if error == rustix::io::Errno::EXIST => {}
                Err(_) => return Err(ModuleError::Unavailable),
            }
            root.push(component);
            open_directory(&root)?;
        }
        admitted().await.unwrap_or(Err(ModuleError::Timeout))?;
        let paths = std::env::var_os("PATH").ok_or(ModuleError::Unavailable)?;
        let git = std::env::split_paths(&paths)
            .take(64)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("git"))
            .find(|path| path.is_file())
            .ok_or(ModuleError::Unavailable)?
            .canonicalize()
            .map_err(|_| ModuleError::Unavailable)?;
        Self::new(
            nddev_device_sync_adapter_io::NativeIo::new(1)
                .map_err(map_io)?
                .process_handle(),
            git,
            root,
            principal,
        )
    }

    pub async fn connect<F, C>(
        &self,
        location: &CheckoutLocation,
        credential: &str,
        cancellation: &CancellationToken,
        mut revalidate: impl FnMut() -> F + Send,
        mut confirm_repository: impl FnMut() -> C + Send,
    ) -> Result<ConnectionOutcome, ModuleError>
    where
        F: std::future::Future<Output = Result<(), ModuleError>> + Send,
        C: std::future::Future<Output = Result<(), ModuleError>> + Send,
    {
        location.validate()?;
        if credential.is_empty()
            || credential.len() > 8192
            || !credential.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(ModuleError::InvalidInput);
        }
        authorize(cancellation, &mut revalidate).await?;
        if cancellation.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        // Clone must not inherit an enclosing repository's local Git config.
        if self
            .root
            .ancestors()
            .take(64)
            .any(|path| path.join(".git").exists())
        {
            return Err(ModuleError::InvalidInput);
        }
        let root = open_directory(&self.root)?;
        let _root_lease = UnlockLease(lease(&root, ".nds-workspace.lock")?);
        if collision(&self.root, location.owner())? {
            return Ok(ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied));
        }
        let owner_path = self.root.join(location.owner());
        authorize(cancellation, &mut revalidate).await?;
        if cancellation.is_cancelled() {
            return Err(ModuleError::Cancelled);
        }
        let created = match rustix::fs::mkdirat(&root, location.owner(), Mode::RWXU) {
            Ok(()) => {
                root.sync_all().map_err(|_| ModuleError::Unavailable)?;
                true
            }
            Err(error) if error == rustix::io::Errno::EXIST => false,
            Err(_) => return Err(ModuleError::Unavailable),
        };
        let owner = File::from(
            openat(&root, location.owner(), directory_flags(), Mode::empty())
                .map_err(|_| ModuleError::Unavailable)?,
        );
        let _empty_owner = EmptyOwner {
            root: &root,
            name: location.owner(),
            created,
            identity: directory_identity(&owner)?,
        };
        if collision(&owner_path, location.repository())? {
            return Ok(ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied));
        }
        let checkout = owner_path.join(location.repository());
        let present = match std::fs::symlink_metadata(&checkout) {
            Ok(metadata) if !metadata.file_type().is_dir() => {
                return Ok(ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied));
            }
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(ModuleError::Unavailable),
        };
        if !present {
            if self.legacy_checkout_present(location)? {
                return Ok(ConnectionOutcome::Blocked(
                    ConnectionBlock::RelocationRequired,
                ));
            }
            return self
                .clone_at(
                    location,
                    credential,
                    cancellation,
                    &mut revalidate,
                    &mut confirm_repository,
                    &owner,
                )
                .await;
        }
        if checkout.canonicalize().ok().as_deref() != Some(checkout.as_path()) {
            return Err(ModuleError::InvalidInput);
        }
        let git_dir = checkout.join(".git");
        let directory = match open_directory(&git_dir) {
            Ok(value) => value,
            Err(_) => return Ok(ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied)),
        };
        let checkout_lease = UnlockLease(lease(&directory, "nds-sync.lock")?);
        let binding = read_binding(&git_dir)?;
        let mut session = self.connection_session(
            checkout,
            location,
            credential,
            cancellation,
            revalidate,
            checkout_lease
                .0
                .try_clone()
                .map_err(|_| ModuleError::Unavailable)?,
        )?;
        session.safe_configuration().await?;
        let snapshot = session.witness().await?;
        let presence = CheckoutPresence::Repository {
            origin: snapshot.origin.clone(),
            active_writer: snapshot.active_writer,
            binding: binding.clone().map(Box::new),
        };
        match connection_decision(&self.principal, location, &presence)? {
            ConnectionDecision::AlreadyConnected => {
                authorize(cancellation, &mut confirm_repository).await?;
                session.validate().await?;
                if !same_directory(&session.git_dir, &directory) {
                    return Err(ModuleError::Busy);
                }
                Ok(ConnectionOutcome::Connected {
                    binding: binding.ok_or(ModuleError::InvalidContract)?,
                    created: false,
                })
            }
            ConnectionDecision::Adopt => {
                authorize(cancellation, &mut confirm_repository).await?;
                session.validate().await?;
                if session.witness().await? != snapshot
                    || !same_directory(&session.git_dir, &directory)
                {
                    return Err(ModuleError::Busy);
                }
                let binding = self.new_binding(location);
                write_binding(&directory, &binding)?;
                session.validate().await?;
                if !same_directory(&session.git_dir, &directory) {
                    return Err(ModuleError::Busy);
                }
                Ok(ConnectionOutcome::Connected {
                    binding,
                    created: false,
                })
            }
            ConnectionDecision::Blocked(reason) => Ok(ConnectionOutcome::Blocked(reason)),
            ConnectionDecision::Clone => Err(ModuleError::InvalidContract),
        }
    }

    fn new_binding(&self, location: &CheckoutLocation) -> CheckoutBinding {
        CheckoutBinding {
            id: uuid::Uuid::new_v4().to_string(),
            principal: self.principal.clone(),
            location: location.clone(),
        }
    }
    fn legacy_checkout_present(&self, location: &CheckoutLocation) -> Result<bool, ModuleError> {
        // The previous default is never moved or adopted implicitly. A saved
        // connection must not silently create a second checkout in the new root.
        if self.root.file_name().is_none_or(|name| name != "NDS")
            || self
                .root
                .parent()
                .and_then(Path::file_name)
                .is_none_or(|name| name != "Developer")
        {
            return Ok(false);
        }
        let previous = self
            .root
            .parent()
            .ok_or(ModuleError::InvalidInput)?
            .join("nddev")
            .join(location.relative_path());
        match std::fs::symlink_metadata(previous) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(ModuleError::Unavailable),
        }
    }
    fn connection_session<G, F>(
        &self,
        checkout: PathBuf,
        location: &CheckoutLocation,
        credential: &str,
        cancellation: &CancellationToken,
        revalidate: G,
        lease: File,
    ) -> Result<Session<'_, G>, ModuleError>
    where
        G: FnMut() -> F + Send,
        F: std::future::Future<Output = Result<(), ModuleError>> + Send,
    {
        let input = Zeroizing::new(format!("x-access-token:{credential}"));
        let mut authorization = Zeroizing::new(String::from("Authorization: Basic "));
        STANDARD.encode_string(input.as_bytes(), &mut authorization);
        Ok(Session {
            workspace: self,
            git_dir: checkout.join(".git"),
            checkout,
            endpoint: self.endpoint(location),
            archived: false,
            authorization,
            cancellation: cancellation.child_token(),
            revalidate,
            _lease: lease,
            fetched: None,
            location: Some(location.clone()),
            deadline: tokio::time::Instant::now() + Duration::from_secs(120),
        })
    }
    async fn clone_at<F, C>(
        &self,
        location: &CheckoutLocation,
        credential: &str,
        cancellation: &CancellationToken,
        revalidate: &mut (impl FnMut() -> F + Send),
        confirm_repository: &mut (impl FnMut() -> C + Send),
        owner: &File,
    ) -> Result<ConnectionOutcome, ModuleError>
    where
        F: std::future::Future<Output = Result<(), ModuleError>> + Send,
        C: std::future::Future<Output = Result<(), ModuleError>> + Send,
    {
        let stage = tempfile::Builder::new()
            .prefix(".nds-stage-")
            .tempdir_in(&self.root)
            .map_err(|_| ModuleError::Unavailable)?;
        let stage_directory = open_directory(stage.path())?;
        let mut session = self.connection_session(
            stage.path().to_path_buf(),
            location,
            credential,
            cancellation,
            revalidate,
            lease(&stage_directory, "nds-clone.lock")?,
        )?;
        let _cancel_on_drop = session.cancellation.clone().drop_guard();
        let endpoint = session.endpoint.clone();
        session
            .required(&[
                "clone",
                "--no-checkout",
                "--no-local",
                "--no-hardlinks",
                "--no-tags",
                "--no-recurse-submodules",
                "--template=",
                "--",
                &endpoint,
                "checkout",
            ])
            .await?;
        session.checkout = stage.path().join("checkout");
        session.git_dir = session.checkout.join(".git");
        session.safe_configuration().await?;
        session
            .required(&[
                "config",
                "--local",
                "remote.origin.url",
                &location.https_url(),
            ])
            .await?;
        let head = session
            .optional(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await?;
        if !head.is_empty() {
            oid(&head)?;
            let tree = session
                .required(&["ls-tree", "-r", "--full-tree", "-z", &head])
                .await?;
            if tree
                .split(|b| *b == 0)
                .any(|row| row.starts_with(b"160000 "))
            {
                return Err(ModuleError::Unavailable);
            }
            // Only an NDS-owned empty staging tree is populated. Existing user
            // checkouts never pass through this command.
            session.required(&["read-tree", "-m", "-u", "HEAD"]).await?;
        }
        let binding = self.new_binding(location);
        let git_directory = open_directory(&session.git_dir)?;
        write_binding(&git_directory, &binding)?;
        authorize(cancellation, confirm_repository).await?;
        session.validate().await?;
        if self.root.canonicalize().ok().as_deref() != Some(self.root.as_path())
            || !same_directory(&self.root.join(location.owner()), owner)
        {
            return Err(ModuleError::Busy);
        }
        if self.legacy_checkout_present(location)? {
            return Ok(ConnectionOutcome::Blocked(
                ConnectionBlock::RelocationRequired,
            ));
        }
        // NOREPLACE is an OS transaction; checking exists() then rename() would
        // overwrite an empty directory created by another writer in between.
        match renameat_with(
            &stage_directory,
            "checkout",
            owner,
            location.repository(),
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(rustix::io::Errno::EXIST | rustix::io::Errno::NOTEMPTY) => {
                return Ok(ConnectionOutcome::Blocked(ConnectionBlock::PathOccupied));
            }
            Err(_) => return Err(ModuleError::Unavailable),
        }
        owner.sync_all().map_err(|_| ModuleError::Unavailable)?;
        if !same_directory(&self.root.join(location.owner()), owner) {
            return Err(ModuleError::Busy);
        }
        // Published work is preserved even if the caller is cancelled here.
        session.validate().await?;
        Ok(ConnectionOutcome::Connected {
            binding,
            created: true,
        })
    }
}

fn write_binding(directory: &File, binding: &CheckoutBinding) -> Result<(), ModuleError> {
    binding.validate()?;
    let bytes = serde_json::to_vec(binding).map_err(|_| ModuleError::InvalidInput)?;
    if bytes.len() > 8192 {
        return Err(ModuleError::InvalidInput);
    }
    let temporary = format!(".nds-binding-{}.tmp", uuid::Uuid::new_v4());
    let mut file = File::from(
        openat(
            directory,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|_| ModuleError::Busy)?,
    );
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|_| ModuleError::Unavailable)?;
        file.sync_all().map_err(|_| ModuleError::Unavailable)?;
        renameat_with(
            directory,
            temporary.as_str(),
            directory,
            "nds-binding.json",
            RenameFlags::NOREPLACE,
        )
        .map_err(|_| ModuleError::Busy)?;
        directory.sync_all().map_err(|_| ModuleError::Unavailable)
    })();
    // Only our unpredictable temporary name is cleaned; the final marker and
    // an already published checkout are never removed on uncertain outcomes.
    let _ = rustix::fs::unlinkat(directory, temporary.as_str(), rustix::fs::AtFlags::empty());
    result
}

//! Native directory and checkout-lock witnesses shared by connection and sync.
//! These are local filesystem observations, never volume leases or authority.
use nddev_device_sync_domain::modules::ModuleError;
use rustix::fs::{Mode, OFlags, openat};
use std::{fs::File, os::unix::fs::MetadataExt, path::Path};

pub(super) fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}
pub(super) fn directory_identity(directory: &File) -> Result<(u64, u64), ModuleError> {
    let metadata = directory.metadata().map_err(|_| ModuleError::Unavailable)?;
    Ok((metadata.dev(), metadata.ino()))
}
pub(super) fn same_directory(path: &Path, directory: &File) -> bool {
    open_directory(path)
        .and_then(|current| directory_identity(&current))
        .ok()
        .is_some_and(|current| directory_identity(directory).ok() == Some(current))
}
pub(super) fn open_directory(path: &Path) -> Result<File, ModuleError> {
    rustix::fs::open(path, directory_flags(), Mode::empty())
        .map(File::from)
        .map_err(|_| ModuleError::Unavailable)
}
pub(super) fn lease(directory: &File, name: &str) -> Result<File, ModuleError> {
    let file = File::from(
        openat(
            directory,
            name,
            OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
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

/// Keep physical owners alive through authorization awaits. A matching binding
/// or path spelling cannot substitute a newly created checkout, Git directory
/// or lock inode for the one retained by this operation.
pub(super) struct CheckoutDirectories {
    checkout: File,
    git: File,
}
impl CheckoutDirectories {
    pub(super) fn capture(checkout: &Path, git: &Path) -> Result<Self, ModuleError> {
        let value = Self {
            checkout: open_directory(checkout)?,
            git: open_directory(git)?,
        };
        value.validate_paths(checkout, git)?;
        Ok(value)
    }
    pub(super) fn validate_paths(&self, checkout: &Path, git: &Path) -> Result<(), ModuleError> {
        if checkout.canonicalize().ok().as_deref() != Some(checkout)
            || !same_directory(checkout, &self.checkout)
            || !same_directory(git, &self.git)
        {
            return Err(ModuleError::Busy);
        }
        Ok(())
    }
    pub(super) fn lease(&self) -> Result<File, ModuleError> {
        lease(&self.git, "nds-sync.lock")
    }
    pub(super) fn validate(
        &self,
        checkout: &Path,
        git: &Path,
        lease: &File,
    ) -> Result<(), ModuleError> {
        self.validate_paths(checkout, git)?;
        let current = openat(
            &self.git,
            "nds-sync.lock",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|_| ModuleError::Busy)?;
        if directory_identity(&current)? != directory_identity(lease)? {
            return Err(ModuleError::Busy);
        }
        Ok(())
    }
}

//! Read-only qualification of initialized gitlinks. No child transport, config
//! writes, checkout, initialization or inferred repository authorization.
use super::*;
use std::{collections::BTreeSet, io::Read, path::Component};

const MAX_SUBMODULES: usize = 1024;
const MAX_DEPTH: usize = 16;
const MAX_WITNESS_BYTES: usize = 8 * OUTPUT_BYTES;

#[derive(PartialEq, Eq)]
pub(super) struct SubmoduleWitness {
    checkout: PathBuf,
    git_dir: PathBuf,
    head: String,
    branch: String,
    status: Vec<u8>,
    links: Vec<(String, String)>,
    pub(super) dirty: bool,
    pub(super) active_writer: bool,
}

/// Only graph changes matter here: ordinary parent files may advance, but
/// pins, paths and committed .gitmodules must remain identical in both trees.
pub(super) fn graph(tree: &[u8]) -> Result<Vec<Vec<u8>>, ModuleError> {
    tree.split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (_, path) = split_entry(entry)?;
            Ok((entry.starts_with(b"160000 ") || path == b".gitmodules").then(|| entry.to_vec()))
        })
        .filter_map(|entry| entry.transpose())
        .collect()
}

fn split_entry(entry: &[u8]) -> Result<(&[u8], &[u8]), ModuleError> {
    let separator = entry
        .iter()
        .position(|b| *b == b'\t')
        .ok_or(ModuleError::InvalidResponse)?;
    Ok((&entry[..separator], &entry[separator + 1..]))
}

fn links(bytes: &[u8], index: bool) -> Result<Vec<(String, String)>, ModuleError> {
    let mut result = Vec::new();
    for entry in bytes
        .split(|b| *b == 0)
        .filter(|entry| entry.starts_with(b"160000 "))
    {
        let (metadata, path) = split_entry(entry)?;
        let fields = std::str::from_utf8(metadata)
            .map_err(|_| ModuleError::InvalidResponse)?
            .split(' ')
            .collect::<Vec<_>>();
        if fields.len() != 3 || (index && fields[2] != "0") || (!index && fields[1] != "commit") {
            return Err(ModuleError::Unavailable);
        }
        let pin = fields[if index { 1 } else { 2 }];
        oid(pin)?;
        let path = std::str::from_utf8(path).map_err(|_| ModuleError::Unavailable)?;
        if path.is_empty()
            || path.len() > 1024
            || path.contains(['\\', ':'])
            || path
                .split('/')
                .any(|part| matches!(part, "" | "." | "..") || part.eq_ignore_ascii_case(".git"))
            || Path::new(path)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(ModuleError::Unavailable);
        }
        result.push((path.to_owned(), pin.to_owned()));
        if result.len() > MAX_SUBMODULES {
            return Err(ModuleError::Unavailable);
        }
    }
    Ok(result)
}

fn ordinary_directory(root: &Path, relative: &Path) -> Result<PathBuf, ModuleError> {
    let mut path = root.to_path_buf();
    for part in relative.components() {
        if !matches!(part, Component::Normal(_)) {
            return Err(ModuleError::Unavailable);
        }
        path.push(part);
        if !std::fs::symlink_metadata(&path)
            .map_err(|_| ModuleError::Unavailable)?
            .file_type()
            .is_dir()
        {
            return Err(ModuleError::Unavailable);
        }
    }
    Ok(path)
}

fn child_git_dir(checkout: &Path, parent_git_dir: &Path) -> Result<PathBuf, ModuleError> {
    let marker = checkout.join(".git");
    let metadata = std::fs::symlink_metadata(&marker).map_err(|_| ModuleError::Unavailable)?;
    if metadata.file_type().is_dir() {
        return Ok(marker);
    }
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return Err(ModuleError::Unavailable);
    }
    let mut bytes = Vec::new();
    File::open(marker)
        .map_err(|_| ModuleError::Unavailable)?
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| ModuleError::Unavailable)?;
    if bytes.len() > 4096 {
        return Err(ModuleError::Unavailable);
    }
    let value = text(bytes)?;
    let target = value
        .strip_prefix("gitdir: ")
        .filter(|value| !value.is_empty() && !value.contains(['\r', '\n']))
        .ok_or(ModuleError::Unavailable)?;
    let resolved = checkout
        .join(target)
        .canonicalize()
        .map_err(|_| ModuleError::Unavailable)?;
    let modules = ordinary_directory(parent_git_dir, Path::new("modules"))?;
    let relative = resolved
        .strip_prefix(&modules)
        .map_err(|_| ModuleError::Unavailable)?;
    if relative.as_os_str().is_empty() {
        return Err(ModuleError::Unavailable);
    }
    ordinary_directory(&modules, relative)
}

pub(super) fn active_writer(git_dir: &Path) -> bool {
    [
        "index.lock",
        "HEAD.lock",
        "packed-refs.lock",
        "shallow.lock",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_LOG",
    ]
    .iter()
    .any(|name| std::fs::symlink_metadata(git_dir.join(name)).is_ok())
}

impl<G, F> Session<'_, G>
where
    G: FnMut() -> F + Send,
    F: std::future::Future<Output = Result<(), ModuleError>> + Send,
{
    async fn required_in(
        &mut self,
        checkout: &Path,
        git_dir: &Path,
        args: &[&str],
    ) -> Result<Vec<u8>, ModuleError> {
        let result = self.git_in(checkout, git_dir, args).await?;
        if !result.status.success() {
            return Err(ModuleError::ProviderFailed);
        }
        Ok(result.stdout)
    }

    pub(super) async fn submodule_witnesses(
        &mut self,
        head: &str,
    ) -> Result<Vec<SubmoduleWitness>, ModuleError> {
        if head.is_empty() {
            return Ok(Vec::new());
        }
        let index = self.required(&["ls-files", "--stage", "-z"]).await?;
        let indexed = links(&index, true)?;
        let tree = self
            .required(&["ls-tree", "-r", "--full-tree", "-z", head])
            .await?;
        let committed = links(&tree, false)?;
        // --ignore-submodules=all deliberately avoids Git's own unsafe child
        // traversal. Explicitly account for staged pins it would also hide.
        if indexed != committed {
            return Ok(vec![SubmoduleWitness {
                checkout: self.checkout.clone(),
                git_dir: self.git_dir.clone(),
                head: head.to_owned(),
                branch: String::new(),
                status: Vec::new(),
                links: indexed,
                dirty: true,
                active_writer: false,
            }]);
        }
        let mut pending = indexed
            .into_iter()
            .map(|(path, pin)| (self.checkout.clone(), self.git_dir.clone(), path, pin, 1))
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        let mut witness_bytes = 0;
        while let Some((parent, parent_git_dir, path, pin, depth)) = pending.pop() {
            self.validate().await?;
            if depth > MAX_DEPTH || seen.len() >= MAX_SUBMODULES {
                return Err(ModuleError::Unavailable);
            }
            let checkout = ordinary_directory(&parent, Path::new(&path))?;
            let git_dir = child_git_dir(&checkout, &parent_git_dir)?;
            if !seen.insert(git_dir.clone()) {
                return Err(ModuleError::Unavailable);
            }
            self.safe_configuration_in(&checkout, &git_dir, true)
                .await?;
            let head = text(
                self.required_in(
                    &checkout,
                    &git_dir,
                    &["rev-parse", "--verify", "HEAD^{commit}"],
                )
                .await?,
            )?;
            oid(&head)?;
            let branch = self
                .git_in(&checkout, &git_dir, &["symbolic-ref", "--quiet", "HEAD"])
                .await?;
            let branch = if branch.status.success() {
                text(branch.stdout)?
            } else if branch.status.code() == Some(1) {
                String::new()
            } else {
                return Err(ModuleError::ProviderFailed);
            };
            if !branch.is_empty()
                && (!branch.starts_with("refs/heads/")
                    || branch
                        .split('/')
                        .any(|part| matches!(part, "" | "." | "..")))
            {
                return Err(ModuleError::Unavailable);
            }
            let index = self
                .required_in(&checkout, &git_dir, &["ls-files", "--stage", "-z"])
                .await?;
            let indexed = links(&index, true)?;
            let tree = self
                .required_in(
                    &checkout,
                    &git_dir,
                    &["ls-tree", "-r", "--full-tree", "-z", &head],
                )
                .await?;
            let committed = links(&tree, false)?;
            let status = self
                .required_in(
                    &checkout,
                    &git_dir,
                    &[
                        "status",
                        "--porcelain=v2",
                        "-z",
                        "--untracked-files=normal",
                        "--ignore-submodules=all",
                    ],
                )
                .await?;
            witness_bytes += index.len() + tree.len() + status.len();
            if witness_bytes > MAX_WITNESS_BYTES {
                return Err(ModuleError::Unavailable);
            }
            let dirty = head != pin || indexed != committed || !status.is_empty();
            let writer = active_writer(&git_dir)
                || (!branch.is_empty()
                    && std::fs::symlink_metadata(git_dir.join(format!("{branch}.lock"))).is_ok());
            if !dirty && !writer {
                if pending.len() + seen.len() + indexed.len() > MAX_SUBMODULES {
                    return Err(ModuleError::Unavailable);
                }
                pending.extend(indexed.iter().map(|(path, pin)| {
                    (
                        checkout.clone(),
                        git_dir.clone(),
                        path.clone(),
                        pin.clone(),
                        depth + 1,
                    )
                }));
            }
            result.push(SubmoduleWitness {
                checkout,
                git_dir,
                head,
                branch,
                status,
                links: indexed,
                dirty,
                active_writer: writer,
            });
        }
        Ok(result)
    }
}

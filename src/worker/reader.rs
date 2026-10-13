use gix::{
    bstr::ByteSlice,
    status::{Item, index_worktree},
};
use nddev_device_sync_domain::modules::ModuleError;
use nddev_device_sync_domain::repositories::*;
use std::{
    collections::BTreeSet,
    path::{Component, Path},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

// Internal recursion bookkeeping. No paths or Git identity are serialized.
struct ReadResult {
    status: LocalGitStatus,
    head_oid: Option<String>,
}

pub(super) struct Limits {
    pub entries: usize,
    pub bytes: u64,
    pub depth: usize,
    pub deadline: Instant,
    pub commits: usize,
    pub object_bytes: usize,
}

#[derive(Default)]
struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn charge(&mut self, entries: usize, bytes: u64, limits: &Limits) -> Result<(), ModuleError> {
        self.entries = self
            .entries
            .checked_add(entries)
            .ok_or(ModuleError::InvalidResponse)?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(ModuleError::InvalidResponse)?;
        if self.entries > limits.entries || self.bytes > limits.bytes {
            return Err(ModuleError::InvalidResponse);
        }
        Ok(())
    }
}

fn guard(limits: &Limits, cancelled: &AtomicBool) -> Result<(), ModuleError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(ModuleError::Cancelled);
    }
    if Instant::now() >= limits.deadline {
        return Err(ModuleError::Timeout);
    }
    Ok(())
}

/// Qualification envelope: preflight bounds the fixture/read scope before gix
/// allocates status results. This is not yet a hard ODB decompression/time bound.
fn preflight(
    root: &Path,
    repo: &gix::Repository,
    index: &gix::index::State,
    limits: &Limits,
    cancelled: &AtomicBool,
    budget: &mut Budget,
) -> Result<(), ModuleError> {
    let mut pending = vec![(root.to_path_buf(), 0)];
    let mut excludes = repo
        .excludes(
            index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        )
        .map_err(|_| ModuleError::InvalidResponse)?;
    while let Some((dir, depth)) = pending.pop() {
        guard(limits, cancelled)?;
        if depth > limits.depth {
            return Err(ModuleError::InvalidResponse);
        }
        for entry in std::fs::read_dir(dir).map_err(|_| ModuleError::Unavailable)? {
            guard(limits, cancelled)?;
            let entry = entry.map_err(|_| ModuleError::Unavailable)?;
            if entry.file_name() == ".git" {
                continue;
            }
            let meta =
                std::fs::symlink_metadata(entry.path()).map_err(|_| ModuleError::Unavailable)?;
            let full = entry.path();
            let relative = full
                .strip_prefix(root)
                .map_err(|_| ModuleError::InvalidResponse)?
                .to_str()
                .ok_or(ModuleError::InvalidResponse)?;
            #[cfg(windows)]
            let relative = relative.replace('\\', "/");
            let relative = relative.as_bytes().as_bstr();
            let tracked = index.prefixed_entries_range(relative).is_some_and(|range| {
                index.entries()[range]
                    .iter()
                    .any(|e| meta.is_dir() || e.path(index) == relative)
            });
            let ignored = excludes
                .at_entry(
                    relative,
                    meta.is_dir().then_some(gix::index::entry::Mode::DIR),
                )
                .map_err(|_| ModuleError::InvalidResponse)?
                .is_excluded();
            // A proven ignored, untracked cache is not Git state. Tracked data
            // still contributes to the bound even when an ignore rule matches.
            if ignored && !tracked {
                continue;
            }
            budget.charge(1, meta.len(), limits)?;
            if meta.is_dir() && !meta.file_type().is_symlink() {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    Ok(())
}

pub(super) fn inspect(
    path: &Path,
    limits: &Limits,
    cancelled: Arc<AtomicBool>,
) -> Result<LocalGitStatus, ModuleError> {
    if !path.is_absolute()
        || limits.entries == 0
        || limits.entries > 100_000
        || limits.bytes == 0
        || limits.bytes > 256 * 1024 * 1024
        || limits.depth == 0
        || limits.depth > 32
        || limits.commits == 0
        || limits.commits > 50_000
        || limits.object_bytes == 0
        || limits.object_bytes > 32 * 1024 * 1024
    {
        return Err(ModuleError::InvalidInput);
    }
    guard(limits, &cancelled)?;
    let options = gix::open::Options::isolated()
        .strict_config(true)
        .config_overrides(vec![
            "core.fsmonitor=false".into(),
            "core.untrackedCache=false".into(),
            "index.threads=1".into(),
            "gitoxide.objects.cacheLimit=0".into(),
            "core.deltaBaseCacheLimit=8388608".into(),
            "gitoxide.core.deltaBaseCacheLimit=8388608".into(),
            "diff.renameLimit=200".into(),
            "status.renameLimit=200".into(),
            format!("gitoxide.objects.allocLimit={}", limits.object_bytes),
        ]);
    let repo = gix::discover_opts(path, Default::default(), options)
        .map_err(|_| ModuleError::Unavailable)?;
    inspect_repo(repo, limits, cancelled, 0, &mut Budget::default()).map(|read| read.status)
}

fn inspect_repo(
    repo: gix::Repository,
    limits: &Limits,
    cancelled: Arc<AtomicBool>,
    nesting: usize,
    budget: &mut Budget,
) -> Result<ReadResult, ModuleError> {
    guard(limits, &cancelled)?;
    if nesting > limits.depth {
        return Err(ModuleError::InvalidResponse);
    }
    let root = repo
        .workdir()
        .ok_or(ModuleError::UnsupportedPlatform)?
        .to_path_buf();
    let config = repo.config_snapshot();
    let index_before = std::fs::metadata(repo.index_path())
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    // Reading status can execute clean filters. This qualification refuses
    // configured drivers rather than executing any third-party command.
    if config.sections_by_name("filter").is_some()
        || config.string("gitoxide.core.indexFile").is_some()
    {
        return Err(ModuleError::InvalidContract);
    }
    if config.string("diff.external").is_some()
        || config
            .sections_by_name("diff")
            .is_some_and(|mut sections| sections.any(|s| s.header().subsection_name().is_some()))
    {
        return Err(ModuleError::InvalidContract);
    }
    if std::fs::metadata(repo.index_path()).is_ok_and(|m| m.len() > limits.bytes) {
        return Err(ModuleError::InvalidResponse);
    }
    let index = repo
        .index_or_empty()
        .map_err(|_| ModuleError::InvalidResponse)?;
    budget.charge(index.entries().len(), 0, limits)?;
    preflight(&root, &repo, &index, limits, &cancelled, budget)?;
    let head = repo.head().map_err(|_| ModuleError::InvalidResponse)?;
    let head_kind = if head.is_unborn() {
        HeadKind::Unborn
    } else if head.is_detached() {
        HeadKind::Detached
    } else {
        HeadKind::Branch
    };
    let head_oid = head.id().map(|v| v.to_string());
    let branch = head
        .referent_name()
        .map(|v| {
            v.as_bstr()
                .to_str()
                .map(|name| name.trim_start_matches("refs/heads/").to_string())
                .map_err(|_| ModuleError::InvalidResponse)
        })
        .transpose()?;
    let mut staged = BTreeSet::new();
    let mut unstaged = BTreeSet::new();
    let mut conflicts = BTreeSet::new();
    let mut untracked = 0;
    let mut status = repo
        .status(gix::progress::Discard)
        .map_err(|_| ModuleError::InvalidResponse)?
        .untracked_files(gix::status::UntrackedFiles::Collapsed)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::AsConfigured)
        .index_worktree_submodules(None)
        .index_worktree_options_mut(|o| o.thread_limit = Some(1))
        .should_interrupt_owned(cancelled.clone())
        .into_iter(Vec::<gix::bstr::BString>::new())
        .map_err(|_| ModuleError::InvalidResponse)?;
    for change in status.by_ref() {
        guard(limits, &cancelled)?;
        let change = change.map_err(|_| ModuleError::InvalidResponse)?;
        match change {
            Item::TreeIndex(c) => {
                staged.insert(c.location().to_vec());
            }
            Item::IndexWorktree(index_worktree::Item::Modification {
                rela_path, status, ..
            }) => match status {
                gix::status::plumbing::index_as_worktree::EntryStatus::Conflict { .. } => {
                    conflicts.insert(rela_path.to_vec());
                }
                gix::status::plumbing::index_as_worktree::EntryStatus::Change(_) => {
                    unstaged.insert(rela_path.to_vec());
                }
                gix::status::plumbing::index_as_worktree::EntryStatus::IntentToAdd => {
                    unstaged.insert(rela_path.to_vec());
                }
                _ => {}
            },
            Item::IndexWorktree(index_worktree::Item::DirectoryContents { entry, .. }) => {
                if entry.status == gix::dir::entry::Status::Untracked {
                    untracked += 1;
                }
            }
            Item::IndexWorktree(index_worktree::Item::Rewrite { .. }) => {
                return Err(ModuleError::InvalidResponse);
            }
        }
    }
    let mut partial_reasons = Vec::new();
    if status
        .into_outcome()
        .and_then(|o| o.tree_index)
        .and_then(|o| o.rewrite)
        .is_some_and(|o| {
            o.num_similarity_checks_skipped_for_rename_tracking_due_to_limit > 0
                || o.num_similarity_checks_skipped_for_copy_tracking_due_to_limit > 0
        })
    {
        partial_reasons.push(PartialReason::RenameBudget);
    }
    for conflict in &conflicts {
        staged.remove(conflict);
        unstaged.remove(conflict);
    }
    let mut submodule_changes = 0;
    if let Some(modules) = repo
        .submodules()
        .map_err(|_| ModuleError::InvalidResponse)?
    {
        for (module_index, sm) in modules.enumerate() {
            guard(limits, &cancelled)?;
            if module_index >= limits.entries {
                return Err(ModuleError::InvalidResponse);
            }
            let path = sm
                .path()
                .map_err(|_| ModuleError::InvalidResponse)?
                .to_str()
                .map_err(|_| ModuleError::InvalidResponse)?
                .to_owned();
            let relative = Path::new(&path);
            if relative
                .components()
                .any(|v| !matches!(v, Component::Normal(_)))
            {
                return Err(ModuleError::InvalidResponse);
            }
            let mut cursor = root.clone();
            for part in relative.components() {
                cursor.push(part);
                if std::fs::symlink_metadata(&cursor).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err(ModuleError::InvalidResponse);
                }
            }
            let index_oid = sm
                .index_id()
                .map_err(|_| ModuleError::InvalidResponse)?
                .map(|v| v.to_string());
            let child = sm.open().map_err(|_| ModuleError::InvalidResponse)?;
            let initialized = child.is_some();
            let child = child
                .map(|child| inspect_repo(child, limits, cancelled.clone(), nesting + 1, budget))
                .transpose()?;
            let child_head = child.as_ref().and_then(|v| v.head_oid.clone());
            let dirty = child
                .as_ref()
                .is_some_and(|v| v.status.changes != Changes::default());
            if initialized && (child_head != index_oid || dirty) {
                submodule_changes += 1;
                unstaged.insert(path.as_bytes().to_vec());
            }
            if let Some(child) = child {
                for reason in child.status.partial_reasons {
                    if !partial_reasons.contains(&reason) {
                        partial_reasons.push(reason);
                    }
                }
            }
        }
    }
    guard(limits, &cancelled)?;
    if repo.head_id().ok().map(|v| v.to_string()) != head_oid {
        return Err(ModuleError::InvalidResponse);
    }
    let (upstream, comparison) = comparison(&repo, head_kind, limits, &cancelled)?;
    if comparison == BranchComparison::Partial {
        partial_reasons.push(PartialReason::HistoryBudget);
    }
    let index_after = std::fs::metadata(repo.index_path())
        .ok()
        .map(|m| (m.len(), m.modified().ok()));
    if index_before != index_after || repo.head_id().ok().map(|v| v.to_string()) != head_oid {
        partial_reasons.push(PartialReason::SnapshotChanged);
    }
    Ok(ReadResult {
        head_oid,
        status: LocalGitStatus {
            head_kind,
            branch,
            upstream,
            comparison,
            changes: Changes {
                staged: staged.len() as u32,
                unstaged: unstaged.len() as u32,
                untracked,
                conflicted: conflicts.len() as u32,
                submodule_changes,
            },
            partial_reasons,
        },
    })
}

fn comparison(
    repo: &gix::Repository,
    kind: HeadKind,
    limits: &Limits,
    cancelled: &AtomicBool,
) -> Result<(Option<String>, BranchComparison), ModuleError> {
    if kind != HeadKind::Branch {
        return Ok((None, BranchComparison::NotApplicable));
    }
    let Some(reference) = repo.head_ref().map_err(|_| ModuleError::InvalidResponse)? else {
        return Ok((None, BranchComparison::Unavailable));
    };
    let Some(upstream) = reference.remote_tracking_ref_name(gix::remote::Direction::Fetch) else {
        return Ok((None, BranchComparison::Unavailable));
    };
    let upstream = upstream.map_err(|_| ModuleError::InvalidResponse)?;
    let name = upstream
        .as_bstr()
        .to_str()
        .map_err(|_| ModuleError::InvalidResponse)?
        .trim_start_matches("refs/remotes/")
        .to_owned();
    let Some(mut upstream_ref) = repo
        .try_find_reference(upstream.as_ref())
        .map_err(|_| ModuleError::InvalidResponse)?
    else {
        return Ok((Some(name), BranchComparison::Unavailable));
    };
    let upstream_id = upstream_ref
        .peel_to_id()
        .map_err(|_| ModuleError::InvalidResponse)?
        .detach();
    let head = repo
        .head_id()
        .map_err(|_| ModuleError::InvalidResponse)?
        .detach();
    let mut sets = [BTreeSet::new(), BTreeSet::new()];
    let mut seen = 0;
    for (tip, set) in [head, upstream_id].into_iter().zip(&mut sets) {
        for commit in repo
            .rev_walk([tip])
            .all()
            .map_err(|_| ModuleError::InvalidResponse)?
        {
            guard(limits, cancelled)?;
            if seen >= limits.commits {
                return Ok((Some(name), BranchComparison::Partial));
            }
            seen += 1;
            set.insert(commit.map_err(|_| ModuleError::InvalidResponse)?.id);
        }
    }
    Ok((
        Some(name),
        BranchComparison::Available {
            ahead: sets[0].difference(&sets[1]).count() as u32,
            behind: sets[1].difference(&sets[0]).count() as u32,
        },
    ))
}

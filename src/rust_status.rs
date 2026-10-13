//! One replacement for the existing repository_status provider.
use crate::view::{Branch, Changes, Finding, GitStatus, Status};
use nddev_device_sync_adapter_io::{CancellationToken, IoError, ProcessIo, ProcessRequest};
use nddev_device_sync_application::repositories::{
    LocalGitClassification, ReadGitStatus, classify_local_status,
};
use nddev_device_sync_domain::{modules::ModuleError, repositories::*};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub const OUTPUT_BYTES: usize = 1_048_576;
pub const MEMORY_BYTES: u64 = 512 * 1024 * 1024;
pub const PROFILE: &str = "nds-git-readonly-linux-v1";

/// Adapter-private IPC framing, not core proof or lifecycle state.
#[derive(Serialize, Deserialize)]
pub struct WorkerEnvelope {
    pub schema_version: u8,
    pub command: String,
    pub limit_profile: String,
    pub status: Option<LocalGitStatus>,
    pub error_class: Option<String>,
}

#[derive(Clone)]
pub struct IsolatedGds {
    io: ProcessIo,
    worker: PathBuf,
    timeout: Duration,
}
impl IsolatedGds {
    /// Native composition supplies the installed worker from the verified NDS
    /// artifact. Presentation input supplies only a local checkout.
    pub fn new(io: ProcessIo, worker: PathBuf, timeout: Duration) -> Result<Self, ModuleError> {
        if !worker.is_absolute()
            || worker.file_name().is_none_or(|n| {
                n != if cfg!(windows) {
                    "nds-gds-readonly-worker.exe"
                } else {
                    "nds-gds-readonly-worker"
                }
            })
            || timeout.is_zero()
            || timeout > Duration::from_secs(12)
        {
            return Err(ModuleError::InvalidInput);
        }
        Ok(Self {
            io,
            worker,
            timeout,
        })
    }

    /// Preserve the existing Agent/desktop DTO while replacing its provider.
    pub async fn status(
        &self,
        checkout: &Path,
        cancellation: CancellationToken,
    ) -> Result<Status, ModuleError> {
        let local = self.read_git_status(checkout, cancellation).await?;
        Ok(project(local))
    }
}

impl ReadGitStatus for IsolatedGds {
    type Checkout = Path;
    type Cancellation = CancellationToken;
    async fn read_git_status(
        &self,
        checkout: &Path,
        cancellation: CancellationToken,
    ) -> Result<LocalGitStatus, ModuleError> {
        if !cfg!(target_os = "linux") {
            return Err(ModuleError::UnsupportedPlatform);
        }
        if !checkout.is_absolute() {
            return Err(ModuleError::InvalidInput);
        }
        let request = ProcessRequest {
            executable: self.worker.clone(),
            arguments: vec!["status".into(), checkout.as_os_str().into()],
            directory: None,
            timeout: self.timeout,
            max_output_bytes: OUTPUT_BYTES,
        };
        let (reply, exit): (WorkerEnvelope, _) = self
            .io
            .json(request, &[0], cancellation)
            .await
            .map_err(map_io)?;
        if exit != 0
            || reply.schema_version != 1
            || reply.command != "nds git status"
            || reply.limit_profile != PROFILE
        {
            return Err(ModuleError::InvalidResponse);
        }
        match (reply.status, reply.error_class) {
            (Some(status), None) if status.partial_reasons.len() <= 3 => {
                if status.comparison == BranchComparison::Partial
                    && !status
                        .partial_reasons
                        .contains(&PartialReason::HistoryBudget)
                {
                    return Err(ModuleError::InvalidResponse);
                }
                Ok(status)
            }
            (None, Some(code)) => Err(decode_error(&code)),
            _ => Err(ModuleError::InvalidResponse),
        }
    }
}

fn project(local: LocalGitStatus) -> Status {
    let Some(classification) = classify_local_status(&local) else {
        return Status {
            result: "partial".into(),
            exit_class: "partial".into(),
            status: None,
            findings: vec![Finding {
                code: "NDS_GIT_LOCAL_READ_INCOMPLETE".into(),
                severity: "medium".into(),
                message: "Local Git status could not be read completely.".into(),
            }],
        };
    };
    let (comparison_state, ahead, behind) = match local.comparison {
        BranchComparison::Available { ahead, behind } => ("available", ahead, behind),
        BranchComparison::NotApplicable => ("not-applicable", 0, 0),
        _ => ("unavailable", 0, 0),
    };
    let upstream_state = match local.head_kind {
        HeadKind::Detached | HeadKind::Unborn => "not-applicable",
        HeadKind::Branch if local.upstream.is_some() => "present",
        HeadKind::Branch => "missing",
    };
    use LocalGitClassification::*;
    let classification = match classification {
        Conflicted => "conflicted",
        Unborn => "unborn",
        Detached => "detached",
        Dirty => "dirty",
        DivergedCached => "diverged-cached",
        AheadCached => "ahead-cached",
        BehindCached => "behind-cached",
        NoUpstream => "no-upstream",
        CleanCached => "clean-cached",
    };
    Status {
        result: "succeeded".into(),
        exit_class: "success".into(),
        findings: vec![],
        status: Some(GitStatus {
            branch: Branch {
                name: local.branch,
                upstream: local.upstream,
                upstream_state: upstream_state.into(),
                comparison_state: comparison_state.into(),
                ahead,
                behind,
                diverged: ahead > 0 && behind > 0,
            },
            changes: Changes {
                staged: local.changes.staged,
                unstaged: local.changes.unstaged,
                untracked: local.changes.untracked,
                conflicted: local.changes.conflicted,
                submodule_changes: local.changes.submodule_changes,
            },
            classification: classification.into(),
            remote_freshness: "unknown".into(),
        }),
    }
}

fn decode_error(code: &str) -> ModuleError {
    use ModuleError::*;
    [
        UnsupportedPlatform,
        Unavailable,
        Busy,
        InvalidInput,
        Timeout,
        Cancelled,
        ProviderFailed,
        InvalidResponse,
        InvalidContract,
    ]
    .into_iter()
    .find(|error| error.to_string() == code)
    .unwrap_or(InvalidResponse)
}
fn map_io(error: IoError) -> ModuleError {
    match error {
        IoError::InvalidConfiguration => ModuleError::InvalidInput,
        IoError::Busy => ModuleError::Busy,
        IoError::Unavailable => ModuleError::Unavailable,
        IoError::Timeout => ModuleError::Timeout,
        IoError::Cancelled => ModuleError::Cancelled,
        IoError::OutputLimit | IoError::InvalidFrame | IoError::InvalidResponse => {
            ModuleError::InvalidResponse
        }
        IoError::ProviderExit | IoError::Transport => ModuleError::ProviderFailed,
    }
}

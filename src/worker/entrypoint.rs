use super::reader::{Limits, inspect};
use nddev_device_sync_domain::modules::ModuleError;
use nddev_device_sync_github::rust_status::{MEMORY_BYTES, OUTPUT_BYTES, PROFILE, WorkerEnvelope};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

fn enforce_worker_limits() -> Result<(), ModuleError> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    for (resource, ceiling) in [
        (Resource::As, MEMORY_BYTES),
        (Resource::Cpu, 3),
        (Resource::Nofile, 64),
        (Resource::Core, 0),
    ] {
        let inherited = getrlimit(resource);
        let ceiling = inherited.maximum.map_or(ceiling, |v| v.min(ceiling));
        let ceiling = inherited.current.map_or(ceiling, |v| v.min(ceiling));
        setrlimit(
            resource,
            Rlimit {
                current: Some(ceiling),
                maximum: Some(ceiling),
            },
        )
        .map_err(|_| ModuleError::InvalidContract)?;
        let actual = getrlimit(resource);
        if actual.current.is_none_or(|v| v > ceiling) || actual.maximum.is_none_or(|v| v > ceiling)
        {
            return Err(ModuleError::InvalidContract);
        }
    }
    Ok(())
}

struct BoundedJson(Vec<u8>);
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > OUTPUT_BYTES {
            return Err(io::ErrorKind::FileTooLarge.into());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn run() {
    if enforce_worker_limits().is_err() {
        std::process::exit(13);
    }
    let mut args = std::env::args_os().skip(1);
    if args.next().is_none_or(|v| v != "status") {
        std::process::exit(4);
    }
    let Some(path) = args.next().map(PathBuf::from) else {
        std::process::exit(4)
    };
    if args.next().is_some() || !path.is_absolute() {
        std::process::exit(4);
    }
    let limits = Limits {
        entries: 100_000,
        bytes: 128 * 1024 * 1024,
        depth: 24,
        commits: 50_000,
        object_bytes: 32 * 1024 * 1024,
        deadline: Instant::now() + Duration::from_secs(10),
    };
    let result = inspect(&path, &limits, Arc::new(AtomicBool::new(false)));
    let (status, error_class) = match result {
        Ok(status) => (Some(status), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let envelope = WorkerEnvelope {
        schema_version: 1,
        command: "nds git status".into(),
        limit_profile: PROFILE.into(),
        status,
        error_class,
    };
    let output = encode_reply(envelope).unwrap_or_else(|_| std::process::exit(14));
    if io::stdout().lock().write_all(&output).is_err() {
        std::process::exit(14);
    }
}

fn encode_reply(mut envelope: WorkerEnvelope) -> Result<Vec<u8>, ModuleError> {
    let mut output = BoundedJson(Vec::with_capacity(4096));
    if serde_json::to_writer(&mut output, &envelope).is_err() {
        envelope.status = None;
        envelope.error_class = Some(ModuleError::InvalidResponse.to_string());
        output.0.clear();
        if serde_json::to_writer(&mut output, &envelope).is_err() {
            return Err(ModuleError::InvalidResponse);
        }
    }
    Ok(output.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nddev_device_sync_domain::repositories::*;
    #[test]
    fn bounded_output_failure_discards_status_and_uses_the_shared_error_class() {
        let status = LocalGitStatus {
            head_kind: HeadKind::Branch,
            branch: Some("x".repeat(OUTPUT_BYTES + 1)),
            upstream: None,
            comparison: BranchComparison::Unavailable,
            changes: Changes::default(),
            partial_reasons: vec![],
        };
        let bytes = encode_reply(WorkerEnvelope {
            schema_version: 1,
            command: "nds git status".into(),
            limit_profile: PROFILE.into(),
            status: Some(status),
            error_class: None,
        })
        .unwrap();
        assert!(bytes.len() <= OUTPUT_BYTES);
        let reply: WorkerEnvelope = serde_json::from_slice(&bytes).unwrap();
        assert!(reply.status.is_none());
        assert_eq!(reply.error_class.as_deref(), Some("invalid_response"));
    }
}

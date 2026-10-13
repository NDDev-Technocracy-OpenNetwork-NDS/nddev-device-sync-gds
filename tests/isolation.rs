#![cfg(target_os = "linux")]
use nddev_device_sync_adapter_io::{CancellationToken, NativeIo};
use nddev_device_sync_application::repositories::ReadGitStatus;
use nddev_device_sync_domain::{modules::ModuleError, repositories::*};
use nddev_device_sync_gds::rust_status::{IsolatedGds, MEMORY_BYTES};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
            "-C",
        ])
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q", "--template="]);
    std::fs::write(root.path().join("content"), "base\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "fixture"]);
    root
}
fn reader(timeout: Duration) -> IsolatedGds {
    IsolatedGds::new(
        NativeIo::new(1).unwrap().process_handle(),
        PathBuf::from(env!("CARGO_BIN_EXE_nds-gds-readonly-worker")),
        timeout,
    )
    .unwrap()
}
fn worker_pid(root: &Path) -> Option<u32> {
    let path = root.as_os_str().as_encoded_bytes();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(args) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args = args.split(|b| *b == 0).collect::<Vec<_>>();
        if args
            .first()
            .is_some_and(|v| *v == env!("CARGO_BIN_EXE_nds-gds-readonly-worker").as_bytes())
            && args.get(2) == Some(&path)
        {
            return Some(pid);
        }
    }
    None
}

#[tokio::test]
async fn worker_returns_unknown_evidence_and_preserves_index() {
    let root = fixture();
    let before = std::fs::read(root.path().join(".git/index")).unwrap();
    let report = reader(Duration::from_secs(12))
        .read_git_status(root.path(), CancellationToken::new())
        .await
        .unwrap();
    assert!(report.partial_reasons.is_empty());
    assert_eq!(report.changes, Changes::default());
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before
    );
    std::fs::write(root.path().join("content"), "changed\n").unwrap();
    std::fs::write(root.path().join("new"), "untracked\n").unwrap();
    let existing_consumer = reader(Duration::from_secs(12))
        .status(root.path(), CancellationToken::new())
        .await
        .unwrap();
    let status = existing_consumer.status.unwrap();
    assert_eq!(status.classification, "dirty");
    assert_eq!(status.remote_freshness, "unknown");
    assert_eq!((status.changes.unstaged, status.changes.untracked), (1, 1));
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before
    );
}

#[tokio::test]
async fn submodule_partial_read_cannot_produce_complete_parent_status() {
    let source = fixture();
    for i in 0..201 {
        std::fs::write(
            source.path().join(format!("old-{i}")),
            format!("common fixture content\noriginal file {i}\n"),
        )
        .unwrap();
    }
    git(source.path(), &["add", "-A"]);
    git(source.path(), &["commit", "-qm", "rename source"]);
    let parent = fixture();
    git(
        parent.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--name",
            "logical-name",
            source.path().to_str().unwrap(),
            "vendor/dependency",
        ],
    );
    git(parent.path(), &["commit", "-qam", "submodule"]);
    let child = parent.path().join("vendor/dependency");
    for i in 0..201 {
        std::fs::remove_file(child.join(format!("old-{i}"))).unwrap();
        std::fs::write(
            child.join(format!("new-{i}")),
            format!("common fixture content\nchanged file {i}\n"),
        )
        .unwrap();
    }
    git(&child, &["add", "-A"]);
    let parent_index = std::fs::read(parent.path().join(".git/index")).unwrap();
    let raw = reader(Duration::from_secs(12))
        .read_git_status(parent.path(), CancellationToken::new())
        .await
        .unwrap();
    assert!(raw.partial_reasons.contains(&PartialReason::RenameBudget));
    let view = reader(Duration::from_secs(12))
        .status(parent.path(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(view.result, "partial");
    assert!(view.status.is_none());
    assert_eq!(
        std::fs::read(parent.path().join(".git/index")).unwrap(),
        parent_index
    );
}

#[tokio::test]
async fn ignored_build_cache_does_not_turn_small_git_state_into_unproven_data() {
    let root = fixture();
    std::fs::write(root.path().join(".gitignore"), "target/\n").unwrap();
    git(root.path(), &["add", ".gitignore"]);
    git(
        root.path(),
        &["commit", "-qm", "ignore synthetic build cache"],
    );
    std::fs::create_dir(root.path().join("target")).unwrap();
    std::fs::File::create(root.path().join("target/cache"))
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let report = reader(Duration::from_secs(12))
        .read_git_status(root.path(), CancellationToken::new())
        .await
        .unwrap();
    assert!(report.partial_reasons.is_empty());
    assert_eq!(report.changes, Changes::default());
}

#[tokio::test]
async fn native_path_is_local_input_and_never_serialized_to_portable_status() {
    use std::os::unix::ffi::OsStrExt;
    let root = tempfile::tempdir().unwrap();
    let path = root
        .path()
        .join(std::ffi::OsStr::from_bytes(b"fixture-\xff"));
    std::fs::create_dir(&path).unwrap();
    git(&path, &["init", "-q", "--template="]);
    let report = reader(Duration::from_secs(12))
        .read_git_status(&path, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(report.head_kind, HeadKind::Unborn);
    let serialized = serde_json::to_string(&report).unwrap();
    assert!(
        !serialized.contains("fixture-")
            && !serialized.contains("git_dir")
            && !serialized.contains("worktree")
    );
}

#[tokio::test]
async fn unrepresentable_branch_is_an_error_without_a_fabricated_head() {
    use std::os::unix::ffi::OsStrExt;
    let root = fixture();
    let oid = git(root.path(), &["rev-parse", "HEAD"]);
    let reference = root
        .path()
        .join(".git/refs/heads")
        .join(std::ffi::OsStr::from_bytes(b"branch-\xff"));
    std::fs::write(reference, format!("{oid}\n")).unwrap();
    std::fs::write(
        root.path().join(".git/HEAD"),
        b"ref: refs/heads/branch-\xff\n",
    )
    .unwrap();
    let before = std::fs::read(root.path().join(".git/index")).unwrap();
    let result = reader(Duration::from_secs(12))
        .status(root.path(), CancellationToken::new())
        .await;
    assert_eq!(result.err(), Some(ModuleError::InvalidResponse));
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before
    );
}

#[tokio::test]
async fn unsupported_filter_uses_the_existing_contract_error() {
    let root = fixture();
    git(root.path(), &["config", "filter.fixture.clean", "false"]);
    let result = reader(Duration::from_secs(12))
        .status(root.path(), CancellationToken::new())
        .await;
    assert_eq!(result.err(), Some(ModuleError::InvalidContract));
}

#[tokio::test]
async fn cached_clean_keeps_remote_freshness_unknown() {
    let root = fixture();
    git(
        root.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/fixture/repository.git",
        ],
    );
    git(root.path(), &["config", "branch.main.remote", "origin"]);
    git(
        root.path(),
        &["config", "branch.main.merge", "refs/heads/main"],
    );
    git(
        root.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    let result = reader(Duration::from_secs(12))
        .status(root.path(), CancellationToken::new())
        .await
        .unwrap();
    let status = result.status.unwrap();
    assert_eq!(status.classification, "clean-cached");
    assert_eq!(status.remote_freshness, "unknown");
}

#[tokio::test]
async fn oversized_loose_tree_cannot_become_complete_evidence() {
    let root = fixture();
    let tree = git(root.path(), &["rev-parse", "HEAD^{tree}"]);
    let object = root
        .path()
        .join(".git/objects")
        .join(&tree[..2])
        .join(&tree[2..]);
    let result=Command::new("python3").args(["-c","import pathlib,sys,zlib; p=pathlib.Path(sys.argv[1]); p.unlink(); p.write_bytes(zlib.compress(b'tree 1073741824\\0' + b'x'*64))"]).arg(object).status().unwrap();
    assert!(result.success());
    let report = reader(Duration::from_secs(12))
        .read_git_status(root.path(), CancellationToken::new())
        .await;
    assert_eq!(report.err(), Some(ModuleError::InvalidResponse));
}

#[tokio::test]
async fn malicious_index_header_is_unproven_and_never_decoded_as_clean() {
    let root = fixture();
    let mut header = b"DIRC".to_vec();
    header.extend_from_slice(&2u32.to_be_bytes());
    header.extend_from_slice(&1_000_000_000u32.to_be_bytes());
    header.extend_from_slice(&[0; 20]);
    std::fs::write(root.path().join(".git/index"), &header).unwrap();
    let report = reader(Duration::from_secs(12))
        .read_git_status(root.path(), CancellationToken::new())
        .await;
    assert_eq!(report.err(), Some(ModuleError::InvalidResponse));
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        header
    );
}

#[tokio::test]
async fn cancellation_during_real_read_kills_worker_with_kernel_limits() {
    let root = fixture();
    let bytes = vec![b'x'; 64 * 1024 * 1024];
    std::fs::write(root.path().join("large"), &bytes).unwrap();
    git(root.path(), &["add", "large"]);
    git(root.path(), &["commit", "-qm", "large fixture"]);
    std::fs::write(root.path().join("large"), vec![b'y'; bytes.len()]).unwrap();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let path = root.path().to_path_buf();
    let operation = tokio::spawn(async move {
        reader(Duration::from_secs(12))
            .read_git_status(&path, token)
            .await
    });
    let started = Instant::now();
    let pid = loop {
        if let Some(pid) = worker_pid(root.path()) {
            break pid;
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    let limit_path = format!("/proc/{pid}/limits");
    let limits = loop {
        let limits = std::fs::read_to_string(&limit_path).unwrap();
        if limits
            .lines()
            .any(|s| s.starts_with("Max address space") && s.contains(&MEMORY_BYTES.to_string()))
        {
            break limits;
        }
        assert!(started.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(2)).await;
    };
    assert!(
        limits
            .lines()
            .any(|s| s.starts_with("Max cpu time") && s.contains("3"))
    );
    assert!(
        limits
            .lines()
            .any(|s| s.starts_with("Max core file size") && s.contains("0"))
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!operation.is_finished());
    let cancelled_at = Instant::now();
    cancellation.cancel();
    assert!(matches!(
        operation.await.unwrap(),
        Err(ModuleError::Cancelled)
    ));
    assert!(cancelled_at.elapsed() < Duration::from_secs(2));
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert_eq!(
        std::fs::read(root.path().join("large")).unwrap(),
        vec![b'y'; bytes.len()]
    );
}

#[tokio::test]
async fn parent_deadline_refuses_a_real_worker_instead_of_unbounded_blocking() {
    let root = fixture();
    for i in 0..10_000 {
        std::fs::write(root.path().join(format!("untracked-{i}")), b"x").unwrap();
    }
    let before = std::fs::read(root.path().join(".git/index")).unwrap();
    let result = reader(Duration::from_millis(1))
        .read_git_status(root.path(), CancellationToken::new())
        .await;
    assert!(matches!(result, Err(ModuleError::Timeout)));
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before
    );
}

//! Positive fixed GitHub metadata and native Git acceptance. The explicit main
//! workflow lends an ephemeral repository-scoped Actions token, never a user PAT.
//! This does not qualify user catalog discovery or Accounts/Vault orchestration.
#![cfg(all(
    target_os = "linux",
    feature = "github-catalog",
    feature = "commit-sync"
))]
use nddev_device_sync_adapter_io::{CancellationToken, NativeIo};
use nddev_device_sync_application::repositories::commits::CommitOutcome;
use nddev_device_sync_domain::repositories::{
    catalog::{GithubAccountId, RepositoryId},
    workspace::{CheckoutLocation, SyncBlock, WorkspacePrincipal},
};
use nddev_device_sync_gds::commits::{ConnectionOutcome, GitWorkspace};
use std::{io::Write, path::Path, process::Command};
use zeroize::Zeroizing;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn git(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(path)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=NDS generated acceptance",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err("Local fixture Git command failed".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn existing_commit(path: &Path, name: &str, receipt: &Path) -> Result<String> {
    std::fs::write(path.join(name), "NDS generated GitHub acceptance\n")?;
    git(path, &["add", "--", name])?;
    git(
        path,
        &[
            "commit",
            "--quiet",
            "-m",
            "test(gds): generated existing fixture commit",
        ],
    )?;
    let head = git(path, &["rev-parse", "HEAD"])?;
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(receipt)?)?;
    let heads = value["allowed_heads"]
        .as_array_mut()
        .ok_or("Missing owned commit witnesses")?;
    if heads.len() >= 3 {
        return Err("Fixture commit budget exceeded".into());
    }
    heads.push(head.clone().into());
    let mut pending =
        tempfile::NamedTempFile::new_in(receipt.parent().ok_or("Missing fixture root")?)?;
    pending.write_all(&serde_json::to_vec(&value)?)?;
    pending.as_file().sync_all()?;
    pending.persist(receipt)?;
    Ok(head)
}

#[tokio::test]
#[ignore = "explicit trusted-main GitHub workflow; ephemeral Actions credential required"]
async fn github_existing_commits() -> Result {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
        || std::env::var("GITHUB_REF").as_deref() != Ok("refs/heads/main")
        || std::env::var("GITHUB_REPOSITORY").as_deref()
            != Ok("NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-gds")
    {
        return Err("Requires the owning trusted main workflow".into());
    }
    let token = Zeroizing::new(std::env::var("NDS_GDS_ACCEPTANCE_TOKEN")?);
    let receipt = std::path::PathBuf::from(std::env::var("NDS_GDS_ACCEPTANCE_RECEIPT")?);
    let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(&receipt)?)?;
    let branch = fixture["branch"].as_str().ok_or("Missing owned branch")?;
    if !branch.starts_with("nds-acceptance/") || fixture["created"] != true {
        return Err("Missing confirmed owned branch".into());
    }
    let location = CheckoutLocation::new(
        RepositoryId::new(std::env::var("NDS_GDS_REPOSITORY_ID")?.parse()?)?,
        GithubAccountId::new(std::env::var("NDS_GDS_OWNER_ID")?.parse()?)?,
        "NDDev-Technocracy-OpenNetwork-NDS".into(),
        "nddev-device-sync-gds".into(),
    )?;
    let cancellation = CancellationToken::new();
    let metadata = nddev_device_sync_gds::github::read_repository(
        &token,
        &location,
        &cancellation,
        async || Ok(()),
    )
    .await?;
    assert!(!metadata.archived);
    let directory = tempfile::tempdir()?;
    let mut workspaces = Vec::new();
    let mut paths = Vec::new();
    for device in ["first", "second"] {
        let root = directory.path().join(device).join("Developer/NDS");
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let workspace = GitWorkspace::new(
            NativeIo::new(1)?.process_handle(),
            "/usr/bin/git".into(),
            root.clone(),
            WorkspacePrincipal {
                origin: "https://nds.example.invalid".into(),
                tenant: "fixture".into(),
                user: "fixture".into(),
                device: device.into(),
            },
        )?;
        let outcome = workspace
            .connect(
                &location,
                &token,
                &cancellation,
                || async { Ok(()) },
                || async {
                    nddev_device_sync_gds::github::read_repository(
                        &token,
                        &location,
                        &cancellation,
                        async || Ok(()),
                    )
                    .await
                    .map(|_| ())
                },
            )
            .await?;
        assert!(matches!(
            outcome,
            ConnectionOutcome::Connected { created: true, .. }
        ));
        let path = root.join(location.relative_path());
        git(
            &path,
            &[
                "switch",
                "--track",
                "-c",
                branch,
                &format!("origin/{branch}"),
            ],
        )?;
        paths.push(path);
        workspaces.push(workspace);
    }
    let first = existing_commit(&paths[0], "nds-github-acceptance-first.txt", &receipt)?;
    assert_eq!(
        workspaces[0]
            .synchronize(&location, false, &token, &cancellation, || async { Ok(()) })
            .await?,
        CommitOutcome::Published
    );
    assert_eq!(
        workspaces[1]
            .synchronize(&location, false, &token, &cancellation, || async { Ok(()) })
            .await?,
        CommitOutcome::FastForwarded
    );
    assert_eq!(git(&paths[1], &["rev-parse", "HEAD"])?, first);
    let second = existing_commit(&paths[1], "nds-github-acceptance-second.txt", &receipt)?;
    // Dirty local work must prevent a real remote fast-forward as well.
    std::fs::write(
        paths[0].join("nds-github-acceptance-first.txt"),
        "retained dirty work",
    )?;
    let index = std::fs::read(paths[0].join(".git/index"))?;
    assert_eq!(
        workspaces[1]
            .synchronize(&location, false, &token, &cancellation, || async { Ok(()) })
            .await?,
        CommitOutcome::Published
    );
    assert_eq!(
        workspaces[0]
            .synchronize(&location, false, &token, &cancellation, || async { Ok(()) })
            .await?,
        CommitOutcome::Blocked(SyncBlock::Dirty)
    );
    assert_eq!(std::fs::read(paths[0].join(".git/index"))?, index);
    assert_eq!(git(&paths[0], &["rev-parse", "HEAD"])?, first);
    assert_eq!(
        std::fs::read_to_string(paths[0].join("nds-github-acceptance-first.txt"))?,
        "retained dirty work"
    );
    // Only the fixture restores the exact generated bytes it changed above.
    std::fs::write(
        paths[0].join("nds-github-acceptance-first.txt"),
        "NDS generated GitHub acceptance\n",
    )?;
    assert_eq!(
        workspaces[0]
            .synchronize(&location, false, &token, &cancellation, || async { Ok(()) })
            .await?,
        CommitOutcome::FastForwarded
    );
    assert_eq!(git(&paths[0], &["rev-parse", "HEAD"])?, second);
    for workspace in workspaces {
        assert_eq!(
            workspace
                .synchronize(&location, false, &token, &cancellation, || async { Ok(()) })
                .await?,
            CommitOutcome::UpToDate
        );
    }
    Ok(())
}

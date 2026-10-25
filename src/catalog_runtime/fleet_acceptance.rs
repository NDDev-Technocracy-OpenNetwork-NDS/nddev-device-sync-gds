//! Isolated native periodic composition. NDS HTTP, credentials, SQLite and Git
//! are real. The supervisor supplies generated GitHub metadata over loopback;
//! this is not external-provider discovery/rate-limit acceptance.
use super::*;
use nddev_device_sync_accounts::{SessionStore, SyncRequest};
use nddev_device_sync_adapter_io::NativeIo;
use nddev_device_sync_domain::{
    modules::{ModuleError, control::Configuration},
    repositories::{
        catalog::{
            CredentialSelection, GithubAccountId, GithubOwner, GithubOwnerKind, RepositoryId,
            RepositorySummary, RepositoryVisibility,
        },
        workspace::{CheckoutLocation, WorkspacePrincipal, write_permissions},
    },
};
use nddev_device_sync_vault::replication::native::NativeReplica;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[path = "guard_acceptance.rs"]
mod guard_acceptance;

pub(super) struct ValidationPause {
    after: bool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

pub(super) struct FixtureIo {
    pub provider: crate::github::RepositoryFixture,
    root: PathBuf,
    remotes: PathBuf,
    validation_pause: Mutex<Option<Arc<ValidationPause>>>,
}
impl FixtureIo {
    pub(super) async fn pause_validation(
        &self,
        after: bool,
    ) -> std::result::Result<(), view::GithubCatalogError> {
        let pause = {
            let mut slot = self.validation_pause.lock().unwrap();
            if slot.as_ref().is_some_and(|p| p.after == after) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            pause.entered.notify_one();
            tokio::time::timeout(Duration::from_secs(20), pause.release.notified())
                .await
                .map_err(|_| view::GithubCatalogError::Timeout)?;
        }
        Ok(())
    }
    pub fn workspace(
        &self,
        principal: WorkspacePrincipal,
        location: &CheckoutLocation,
    ) -> std::result::Result<crate::commits::GitWorkspace, ModuleError> {
        let git = std::env::split_paths(&std::env::var_os("PATH").ok_or(ModuleError::Unavailable)?)
            .map(|path| path.join("git"))
            .find(|path| path.is_file())
            .ok_or(ModuleError::Unavailable)?;
        Ok(crate::commits::GitWorkspace::new(
            NativeIo::new(1)
                .map_err(|_| ModuleError::Unavailable)?
                .process_handle(),
            git,
            self.root.clone(),
            principal,
        )?
        .with_fixture_remote(
            self.remotes
                .join(format!("{}.git", location.repository()))
                .to_str()
                .ok_or(ModuleError::InvalidInput)?
                .to_owned(),
        ))
    }
}

fn location(id: usize) -> CheckoutLocation {
    CheckoutLocation::from_repository(&RepositorySummary {
        id: RepositoryId::new(id as u64).unwrap(),
        owner: GithubOwner {
            id: GithubAccountId::new(1).unwrap(),
            login: "generated-owner".into(),
            kind: GithubOwnerKind::Organization,
        },
        name: format!("repository-{id:04}"),
        visibility: RepositoryVisibility::Private,
        archived: false,
        default_branch: Some("main".into()),
    })
    .unwrap()
}
fn git(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=NDS generated fleet",
            "-c",
            "user.email=fleet@example.invalid",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err("generated fleet Git command failed".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
async fn settle(vault: &VaultClient) -> Result<()> {
    use nddev_device_sync_application::lifecycle::{BatchOutcome, ReplicaSchedule};
    let mut schedule = ReplicaSchedule::default();
    loop {
        match vault.synchronize_attempt(false).await?.result {
            Ok(progress) if progress.caught_up && progress.view.pending == 0 => return Ok(()),
            Ok(_) => {}
            Err(nddev_device_sync_vault::Error::Busy) => {
                tokio::time::sleep(schedule.after(BatchOutcome::Retryable).unwrap()).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
async fn login(receipt: &Value) -> Result<Arc<IdentityClient>> {
    let endpoint = receipt["base_url"].as_str().ok_or("missing NDS endpoint")?;
    let mailbox = receipt["mailpit_url"]
        .as_str()
        .ok_or("missing SMTP endpoint")?;
    let owner = receipt["owner_email"]
        .as_str()
        .ok_or("missing generated owner")?;
    for value in [endpoint, mailbox] {
        let url = reqwest::Url::parse(value)?;
        assert!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1") && url.port().is_some()
        );
    }
    assert!(owner.ends_with(".invalid"));
    let identity = Arc::new(IdentityClient::new(endpoint)?);
    identity.refresh().await?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let previous: Value = http
        .get(format!("{mailbox}/api/v1/messages"))
        .send()
        .await?
        .json()
        .await?;
    identity.request_email(owner.into(), "en").await?;
    let code = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let messages: Value = http
                .get(format!("{mailbox}/api/v1/messages"))
                .send()
                .await?
                .json()
                .await?;
            for message in messages["messages"].as_array().ok_or("missing messages")? {
                if previous["messages"]
                    .as_array()
                    .ok_or("missing previous messages")?
                    .iter()
                    .any(|prior| prior["ID"] == message["ID"])
                {
                    continue;
                }
                let id = message["ID"].as_str().ok_or("missing message identity")?;
                let content: Value = http
                    .get(format!("{mailbox}/api/v1/message/{id}"))
                    .send()
                    .await?
                    .json()
                    .await?;
                if let Some((_, suffix)) = content["Text"]
                    .as_str()
                    .and_then(|text| text.split_once("code: "))
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Zeroizing::new(
                        suffix.chars().take(8).collect::<String>(),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    identity.verify_email(code.to_string()).await?;
    identity
        .enroll_device("Generated Git fleet 0".into())
        .await?;
    Ok(identity)
}
async fn broker(endpoint: String, path: PathBuf, peers: usize) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    for _ in 0..peers {
        let (mut peer, _) = listener.accept().await?;
        let token = SessionStore::for_endpoint(&endpoint)?
            .read()
            .await?
            .ok_or("missing genuine fixture session")?;
        peer.write_all(&token).await?;
        peer.shutdown().await?;
    }
    drop(listener);
    std::fs::remove_file(path)?;
    Ok(())
}
fn runtime(
    identity: Arc<IdentityClient>,
    vault: Arc<VaultClient>,
    io: Arc<FixtureIo>,
    epoch: sync_state::Epoch,
) -> CatalogRuntime {
    let runtime = CatalogRuntime::new(identity, vault);
    *runtime.shared.fixture.lock().unwrap() = Some(io);
    // This case qualifies commit scheduling, not generated user-catalog scans.
    // The real GitHub catalog fixture remains a separate provider gate.
    {
        let mut work = runtime.work.lock().unwrap();
        work.epoch = Some(epoch);
        work.next = None;
    }
    runtime
}
async fn seed(identity: &IdentityClient, vault: &VaultClient, count: usize) -> Result<()> {
    identity.sync_request(SyncRequest::CapabilitiesV3).await?;
    loop {
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(std::env::var("NDS_FIXTURE_RECEIPT")?)?)?;
        if receipt["new_operation_version"] == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    settle(vault).await?;
    vault
        .save_record(
            Some("fleet-credential".into()),
            0,
            None,
            "Generated invalid Git credential".into(),
            String::new(),
            Zeroizing::new("intentionally-invalid-github-fleet-value".into()),
        )
        .await?;
    settle(vault).await?;
    let record = vault
        .records(String::new())
        .await?
        .items
        .into_iter()
        .find(|record| record.id == "fleet-credential")
        .ok_or("missing generated credential")?;
    vault
        .set_github_source(
            Some(CredentialSelection::new(
                record.id,
                record.revision,
                record.pending_id,
            )?),
            0,
            None,
        )
        .await?;
    vault
        .set_module_configuration(
            "gds".into(),
            0,
            None,
            Configuration {
                enabled: true,
                permissions: write_permissions(),
            },
        )
        .await?;
    settle(vault).await?;
    for batch in (1..=count).collect::<Vec<_>>().chunks(32) {
        let source = vault.github_source().await?;
        let read = vault
            .capture_configured_repository_observations(
                &write_permissions(),
                source
                    .operation_id
                    .as_deref()
                    .ok_or("missing saved source")?,
                Arc::new(identity.local_use_fence().await?),
                CancellationToken::new(),
            )
            .await?;
        for id in batch {
            assert!(
                vault
                    .save_repository_connection(&read, location(*id), 0, None)
                    .await?
                    .is_some()
            );
        }
        settle(vault).await?;
    }
    Ok(())
}
async fn frame(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Result<Value> {
    let mut line = String::new();
    let read = reader.take(65_537).read_line(&mut line).await?;
    if read == 0 || read > 65_536 || !line.ends_with('\n') {
        return Err("invalid fleet frame".into());
    }
    Ok(serde_json::from_str(&line)?)
}
async fn reply(stream: &mut tokio::net::unix::OwnedWriteHalf, value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "owned sync3 PG/SMTP, isolated Secret Service, real Git and generated provider metadata required"]
async fn native_periodic_fleet_member() -> Result<()> {
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let outcome = tokio::select! {
        result = Box::pin(run_member()) => result,
        _ = stop.recv() => Err("owned native fleet interrupted".into()),
    };
    let drained = NativeReplica::shutdown().await;
    outcome?;
    drained?;
    Ok(())
}

async fn run_member() -> Result<()> {
    let root = PathBuf::from(std::env::var("NDS_GIT_FLEET_ROOT")?).canonicalize()?;
    let index: usize = std::env::var("NDS_FLEET_INDEX")?.parse()?;
    let count: usize = std::env::var("NDS_FLEET_COUNT")?.parse()?;
    let repositories: usize = std::env::var("NDS_GIT_FLEET_REPOSITORIES")?.parse()?;
    let epoch: usize = std::env::var("NDS_FLEET_EPOCH")?.parse()?;
    assert!(
        (2..=32).contains(&count)
            && index < count
            && (2..=1024).contains(&repositories)
            && epoch <= 1
    );
    assert_eq!(
        PathBuf::from(std::env::var("XDG_DATA_HOME")?),
        root.join("data")
    );
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(std::env::var("NDS_FIXTURE_RECEIPT")?)?)?;
    let endpoint = receipt["base_url"].as_str().ok_or("missing NDS fixture")?;
    let mut logger = nddev_device_sync_telemetry::install(nddev_device_sync_telemetry::Config {
        service: "nds-git-fleet-acceptance".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        repository: "NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-gds".into(),
        commit: std::env::var("NDS_FLEET_SOURCE")?,
        channel: "alpha".into(),
        standards: "unreleased".into(),
        environment: "development".into(),
        target_prefixes: vec!["nds_accounts".into(), "nddev_device_sync_gds".into()],
        default_module: "gds".into(),
        debug: None,
    })?;
    let stream = UnixStream::connect(std::env::var("NDS_FLEET_SOCKET")?).await?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    reply(&mut writer, json!({"index":index,"epoch":epoch})).await?;
    assert_eq!(frame(&mut reader).await?["action"], "initialize");
    let identity = if index == 0 && epoch == 0 {
        login(&receipt).await?
    } else {
        if epoch == 0 {
            let peer = UnixStream::connect(std::env::var("NDS_FLEET_TOKEN_SOCKET")?).await?;
            let mut token = Zeroizing::new(Vec::new());
            peer.take(129).read_to_end(&mut token).await?;
            assert_eq!(token.len(), 43);
            let store = SessionStore::for_endpoint(endpoint)?;
            assert!(store.read().await?.is_none());
            store.write(&token).await?;
        }
        let identity = Arc::new(IdentityClient::new(endpoint)?);
        let view = identity.refresh().await?;
        assert!(view.authenticated);
        if epoch == 0 {
            loop {
                match identity
                    .prepare_device(format!("Generated Git fleet {index}"))
                    .await
                {
                    Ok(_) => break,
                    Err(error) if error.code() == "rate_limited" => {
                        tokio::time::sleep(Duration::from_secs(30)).await
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            assert!(view.offline_authorized);
        }
        identity
    };
    let broker = if index == 0 && epoch == 0 {
        let path = PathBuf::from(std::env::var("NDS_FLEET_TOKEN_SOCKET")?);
        let task = tokio::spawn(broker(endpoint.into(), path.clone(), count - 1));
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Some(task)
    } else {
        None
    };
    let vault = Arc::new(VaultClient::new(identity.clone(), root.join("replica")).await?);
    if epoch == 0 {
        vault.refresh().await?;
    } else {
        assert_eq!(vault.refresh_local().await?.pending, 1);
    }
    let workspace = root.join("Developer/NDS");
    std::fs::create_dir_all(&workspace)?;
    let io = Arc::new(FixtureIo {
        provider: crate::github::RepositoryFixture::new(std::env::var("NDS_GIT_FLEET_PROVIDER")?),
        root: workspace,
        remotes: PathBuf::from(std::env::var("NDS_GIT_FLEET_REMOTES")?).canonicalize()?,
        validation_pause: Mutex::new(None),
    });
    let view = identity.view();
    reply(
        &mut writer,
        json!({"device":view.current_device_id,"user":view.user_id,"tenant":view.tenant_id}),
    )
    .await?;
    let mut owner: Option<Arc<NativeReplica>> = None;
    let mut catalog: Option<Arc<CatalogRuntime>> = None;
    loop {
        let command = frame(&mut reader).await?;
        let action = command["action"].as_str().ok_or("missing action")?;
        let result = tokio::time::timeout(
            Duration::from_secs(5400),
            Box::pin(async {
                match action {
                    "seed" => {
                        assert_eq!(index, 0);
                        seed(&identity, &vault, repositories).await?;
                        Ok::<Value, Box<dyn std::error::Error + Send + Sync>>(
                            json!({"seeded":repositories}),
                        )
                    }
                    "connect" => {
                        settle(&vault).await?;
                        let setting = vault.module_configuration("gds").await?;
                        if setting.configuration
                            != (Configuration {
                                enabled: true,
                                permissions: write_permissions(),
                            })
                        {
                            vault
                                .set_module_configuration(
                                    "gds".into(),
                                    setting.revision,
                                    setting.pending_id,
                                    Configuration {
                                        enabled: true,
                                        permissions: write_permissions(),
                                    },
                                )
                                .await?;
                            settle(&vault).await?;
                        }
                        let source = vault.github_source().await?;
                        let consent = vault.module_configuration("gds").await?;
                        let client = runtime(
                            identity.clone(),
                            vault.clone(),
                            io.clone(),
                            (source.operation_id, consent.operation_id),
                        )
                        .foreground(identity.clone());
                        for id in 1..=repositories {
                            let outcome = client.connect_repository(location(id)).await?;
                            assert_eq!(outcome.outcome, "connected");
                            assert!(!outcome.intent_queued);
                        }
                        Ok(json!({"connected":repositories}))
                    }
                    "edit" => {
                        let path = io.root.join(location(1).relative_path());
                        std::fs::write(
                            path.join("initial.txt"),
                            format!("generated dirty work {index}\n"),
                        )?;
                        Ok(json!({"dirty":true}))
                    }
                    "guard-contention" => {
                        assert!(owner.is_none());
                        let database = PathBuf::from(command["database"].as_str().ok_or("missing owned database")?).canonicalize()?;
                        assert!(database.starts_with(root.join("replica").canonicalize()?));
                        assert!(database.is_file() && database.extension().is_some_and(|e| e=="db"));
                        guard_acceptance::probe(identity.clone(), vault.clone(), io.clone(), database).await
                    }
                    "commit" => {
                        let path = io.root.join(location(2).relative_path());
                        std::fs::write(
                            path.join("existing.txt"),
                            format!("generated existing commit from {index} epoch {epoch}\n"),
                        )?;
                        git(&path, &["add", "--", "existing.txt"])?;
                        git(
                            &path,
                            &[
                                "commit",
                                "-qm",
                                "test(gds): generated existing fleet commit",
                            ],
                        )?;
                        Ok(json!({"head":git(&path,&["rev-parse","HEAD"])?}))
                    }
                    "start" => {
                        assert!(owner.is_none());
                        let retained =
                            NativeReplica::attach(&identity, root.join("replica"), None).await?;
                        let source = vault.github_source().await?;
                        let consent = vault.module_configuration("gds").await?;
                        let selected = retained.catalog(|identity, vault| {
                            runtime(
                                identity,
                                vault,
                                io.clone(),
                                (source.operation_id, consent.operation_id),
                            )
                        })?;
                        retained.wake();
                        catalog = Some(selected);
                        owner = Some(retained);
                        Ok(json!({"started":true}))
                    }
                    "published" => {
                        let expected = command["head"].as_str().ok_or("missing generated head")?;
                        let remote = io.remotes.join("repository-0002.git");
                        while git(&remote, &["rev-parse", "refs/heads/main"])? != expected {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Ok(json!({"published": expected}))
                    }
                    "canary" => {
                        let started = std::time::Instant::now();
                        let id = format!("online-canary-{index}");
                        vault.save_record(Some(id.clone()), 0, None, "Generated concurrent Vault canary".into(),
                            String::new(), Zeroizing::new("SYNTHETIC_ONLINE_FLEET_VALUE".into())).await?;
                        tokio::time::timeout(Duration::from_secs(45), async {
                            loop {
                                if vault.records(String::new()).await?.items.iter().any(|record|
                                    record.id == id && record.pending_id.is_none() && record.revision > 0) { break; }
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                            Ok::<(), nddev_device_sync_vault::Error>(())
                        }).await??;
                        Ok(json!({"acknowledged":true,"milliseconds":started.elapsed().as_millis()}))
                    }
                    "wait" => {
                        let runtime = catalog.as_ref().ok_or("missing runtime")?;
                        let minimum = command["after_ms"]
                            .as_u64()
                            .ok_or("missing observation boundary")?;
                        let expected = command["head"]
                            .as_str()
                            .ok_or("missing expected existing commit")?;
                        let mut reported = std::time::Instant::now() - Duration::from_secs(10);
                        let mut successful = std::collections::BTreeSet::new();
                        let mut dirty_refusal = false;
                        let mut witness = None;
                        let terminal_outcomes = loop {
                            let (completed, outcomes) = {
                                let state = runtime.shared.commits.lock().unwrap();
                                if let Some(original) = &witness {
                                    if !state.guard.as_ref().is_some_and(|current| Arc::ptr_eq(original, current)) {
                                        return Err("original fleet scope changed during observation".into());
                                    }
                                } else {
                                    witness = state.guard.clone();
                                }
                                let mut outcomes = std::collections::BTreeMap::<String, usize>::new();
                                let mut observed = 0;
                                for (id, view) in state.recorded() {
                                    let id: usize = id.parse()?;
                                    assert!((1..=repositories).contains(&id));
                                    if !view.observed_at_ms.as_deref().and_then(|s| s.parse::<u64>().ok()).is_some_and(|at| at >= minimum) {
                                        continue;
                                    }
                                    let expected_result = if id == 1 {
                                        view.outcome == "blocked" && view.reason == "dirty"
                                    } else {
                                        matches!(view.outcome.as_str(), "up_to_date" | "published" | "fast_forwarded")
                                    };
                                    let temporary = view.outcome == "refused" && matches!(view.reason.as_str(), "operation_busy" | "operation_timeout" | "dependency_unavailable");
                                    if !expected_result && !temporary {
                                        return Err("unexpected fleet terminal outcome".into());
                                    }
                                    if expected_result {
                                        if id == 1 { dirty_refusal = true; } else { successful.insert(id); }
                                    }
                                    *outcomes.entry(format!("{}:{}", view.outcome, view.reason)).or_default() += 1;
                                    observed += 1;
                                }
                                (observed == repositories, outcomes)
                            };
                            if reported.elapsed() >= Duration::from_secs(10) {
                                eprintln!("{}", json!({"fleet_progress":outcomes,"repositories":repositories,"successful_observed":successful.len()}));
                                reported = std::time::Instant::now();
                            }
                            if completed && dirty_refusal && successful.contains(&2)
                                && git(&io.root.join(location(2).relative_path()), &["rev-parse", "HEAD"])? == expected
                            {
                                assert_eq!(std::fs::read_to_string(io.root.join(location(1).relative_path()).join("initial.txt"))?, format!("generated dirty work {index}\n"));
                                break outcomes;
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        };
                        Ok(json!({"observed":repositories,"head":expected,"dirty_preserved":true,
                            "dirty_refusal_observed":dirty_refusal,"successful_repository_ids":successful,
                            "terminal_outcomes":terminal_outcomes}))
                    }
                    "offline" => {
                        vault
                            .save_record(
                                Some(format!("unsent-{index}")),
                                0,
                                None,
                                "Generated offline canary".into(),
                                String::new(),
                                Zeroizing::new("SYNTHETIC_UNSENT_FLEET_VALUE".into()),
                            )
                            .await?;
                        assert_eq!(vault.refresh_local().await?.pending, 1);
                        owner.take().ok_or("missing owner")?.stop().await?;
                        catalog = None;
                        Ok(json!({"pending":1}))
                    }
                    "drain" => {
                        settle(&vault).await?;
                        assert_eq!(vault.refresh_local().await?.pending, 0);
                        Ok(json!({"pending":0}))
                    }
                    "stop" => {
                        if let Some(owner) = owner.take() {
                            owner.stop().await?;
                        }
                        Ok(json!({"stopped":true}))
                    }
                    _ => Err("unknown Git fleet command".into()),
                }
            }),
        )
        .await??;
        reply(&mut writer, result).await?;
        if action == "stop" {
            break;
        }
    }
    if let Some(broker) = broker {
        broker.await??;
    }
    logger.shutdown();
    assert!(!logger.control.output_health().shutdown_incomplete);
    assert_eq!(logger.control.rejected_fields_or_events(), 0);
    Ok(())
}

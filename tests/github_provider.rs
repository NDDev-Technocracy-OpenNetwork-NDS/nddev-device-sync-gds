#![cfg(all(not(target_arch = "wasm32"), feature = "github-catalog"))]
use nddev_device_sync_domain::{
    modules::ModuleError,
    repositories::catalog::{CatalogPageRequest, CredentialSelection},
};
#[tokio::test]
#[ignore = "actual fixed GitHub negative; generated invalid credential only, no positive fixture"]
async fn fixed_github_rejects_generated_invalid_credential_without_empty_success() {
    let request = CatalogPageRequest::new(
        CredentialSelection::new("generated-invalid-fixture".into(), 0, None).unwrap(),
        None,
    )
    .unwrap();
    let token = format!(
        "nds_generated_invalid_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let mut logger = nddev_device_sync_telemetry::install(nddev_device_sync_telemetry::Config {
        service: "nddev-device-sync-github".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        repository: "NDDev-Technocracy-OpenNetwork-NDS/nddev-device-sync-github".into(),
        commit: "unknown".into(),
        channel: "alpha".into(),
        standards: "v0.0.1-alpha.9".into(),
        environment: "development".into(),
        target_prefixes: vec!["nddev_device_sync_github".into()],
        default_module: "github".into(),
        debug: None,
    })
    .unwrap();
    let feed = logger.control.subscribe().unwrap();
    let result = nddev_device_sync_github::github::read_user_and_page(
        &token,
        &request,
        &Default::default(),
        async || Ok(()),
    )
    .await;
    assert!(matches!(result, Err(ModuleError::Unavailable)));
    assert_eq!(
        logger.control.rejected_fields_or_events(),
        0,
        "real provider outcomes must satisfy the canonical SDK envelope"
    );
    let mut text = String::new();
    while let Some(batch) = feed.take_batch() {
        text.push_str(std::str::from_utf8(batch.bytes()).unwrap());
        batch.acknowledge();
    }
    let events: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["event.name"] == "github.http.response" && event["status"] == 401)
    );
    assert!(text.contains("github.catalog.read.failed"));
    assert!(
        !text.contains(&token)
            && !text.contains("Bearer ")
            && !text.contains("https://api.github.com")
    );
    logger.shutdown();
}

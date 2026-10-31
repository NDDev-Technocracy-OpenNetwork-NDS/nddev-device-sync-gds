#[test]
fn descriptor_only_advertises_qualified_worker_platforms() {
    let manifest: serde_json::Value =
        serde_json::from_str(nddev_device_sync_github::MANIFEST).unwrap();
    assert_eq!(manifest["platforms"], serde_json::json!(["linux"]));
    assert_eq!(
        manifest["permissions"],
        serde_json::json!(["read_only", "process_execution"])
    );
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[tokio::test]
async fn unqualified_worker_platform_refuses_execution_before_io() {
    use nddev_device_sync_adapter_io::{CancellationToken, NativeIo};
    use nddev_device_sync_domain::modules::ModuleError;
    use nddev_device_sync_github::Github;
    let directory = tempfile::tempdir().unwrap();
    let provider = Github::new(
        NativeIo::new(1).unwrap().process_handle(),
        std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(if cfg!(windows) {
                "nds-github-readonly-worker.exe"
            } else {
                "nds-github-readonly-worker"
            }),
        std::time::Duration::from_secs(12),
    )
    .unwrap();
    assert!(matches!(
        provider
            .status(directory.path(), CancellationToken::new())
            .await,
        Err(ModuleError::UnsupportedPlatform)
    ));
}

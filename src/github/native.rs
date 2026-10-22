use super::{API_VERSION, DEADLINE, MAX_BODY, ModuleError, ORIGIN, Reply, Route, status};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use zeroize::Zeroizing;

pub(super) struct Client {
    pub(super) admission: tokio::sync::Semaphore,
    pub(super) limits: std::sync::Mutex<super::limits::Limits>,
    http: reqwest::Client,
    #[cfg(test)]
    origin: Option<String>,
}
impl Client {
    #[cfg(test)]
    fn fixture(origin: String) -> Self {
        let mut client = Self::new().unwrap();
        client.origin = Some(origin);
        client
    }
    pub(super) fn new() -> Result<Self, ModuleError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(DEADLINE)
            .timeout(DEADLINE)
            .pool_max_idle_per_host(1)
            .referer(false)
            .build()
            .map_err(|_| ModuleError::Unavailable)?;
        Ok(Self {
            admission: tokio::sync::Semaphore::new(1),
            limits: std::sync::Mutex::default(),
            http,
            #[cfg(test)]
            origin: None,
        })
    }
    pub(super) async fn get(
        &self,
        credential: &str,
        route: Route,
        etag: Option<&str>,
    ) -> Result<Reply, ModuleError> {
        self.limits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .admit(super::now_ms()?)?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {credential}"))
            .map_err(|_| ModuleError::InvalidInput)?;
        authorization.set_sensitive(true);
        #[cfg(test)]
        let origin = self.origin.as_deref().unwrap_or(ORIGIN);
        #[cfg(not(test))]
        let origin = ORIGIN;
        let request = self
            .http
            .get(format!("{origin}{}", route.path()))
            .header(AUTHORIZATION, authorization)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "nddev-device-sync-gds");
        let request = if let Some(etag) = etag {
            request.header("If-None-Match", etag)
        } else {
            request
        };
        let mut response = request.send().await.map_err(map_error)?;
        let code = response.status().as_u16();
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        self.limits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(
                super::now_ms()?,
                code,
                header("x-ratelimit-remaining"),
                header("x-ratelimit-reset"),
                header("retry-after"),
            );
        status(
            code,
            response
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok()),
            response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
        )?;
        let unchanged = code == 304;
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 512 && value.is_ascii())
            .map(str::to_owned);
        let mut link = None;
        for value in response.headers().get_all("link") {
            let value = value.to_str().map_err(|_| ModuleError::InvalidResponse)?;
            let joined = link.get_or_insert_with(String::new);
            if !joined.is_empty() {
                joined.push(',');
            }
            if joined.len().saturating_add(value.len()) > 8192 {
                return Err(ModuleError::InvalidResponse);
            }
            joined.push_str(value);
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY as u64)
        {
            return Err(ModuleError::InvalidResponse);
        }
        let mut body = Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(map_error)? {
            if body.len().saturating_add(chunk.len()) > MAX_BODY {
                return Err(ModuleError::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Reply {
            unchanged,
            etag,
            body,
            link,
        })
    }
}
fn map_error(error: reqwest::Error) -> ModuleError {
    if error.is_timeout() {
        ModuleError::Timeout
    } else {
        ModuleError::ProviderFailed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{CancellationToken, bounded_client};
    use nddev_device_sync_domain::repositories::catalog::{
        CatalogPageRequest, CredentialSelection,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    struct Fixture {
        origin: String,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn fixture(response: Vec<u8>, delay: std::time::Duration) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if stream.read_exact(&mut byte).await.is_err() {
                    return;
                }
                header.push(byte[0]);
                assert!(header.len() < 32768);
            }
            assert!(
                header
                    .windows(b"x-github-api-version: 2026-03-10".len())
                    .any(|v| v == b"x-github-api-version: 2026-03-10")
            );
            tokio::time::sleep(delay).await;
            let _ = stream.write_all(&response).await;
        });
        Fixture { origin, task }
    }
    fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut result = format!(
            "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        result.extend_from_slice(body);
        result
    }
    fn request() -> CatalogPageRequest {
        CatalogPageRequest::new(
            CredentialSelection::new("fixture".into(), 0, None).unwrap(),
            None,
        )
        .unwrap()
    }
    #[tokio::test]
    async fn unchanged_full_last_page_still_probes_for_new_following_repositories() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let rows = serde_json::to_vec(
            &(1..=25)
                .map(|id| {
                    serde_json::json!({
                        "id":id,"owner":{"id":1,"login":"fixture","type":"User"},
                        "name":format!("repo-{id}"),"full_name":format!("fixture/repo-{id}"),
                        "visibility":"private","archived":false,"default_branch":"main"
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let task = tokio::spawn(async move {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 32768);
                }
                let code = if index < 2 { 200 } else { 304 };
                let body: &[u8] = match index {
                    0 => br#"{"id":1,"login":"fixture","type":"User"}"#,
                    1 => &rows,
                    _ => b"",
                };
                stream
                    .write_all(&response(code, "ETag: \"fixture\"\r\n", body))
                    .await
                    .unwrap();
            }
        });
        let server = Fixture { origin, task };
        let client = Client::fixture(server.origin.clone());
        let cache = crate::github::CatalogCache::default();
        let initial = crate::github::bounded_cached_client(
            &client,
            "generated_fixture_invalid",
            &request(),
            &CancellationToken::new(),
            &mut async || Ok(()),
            Some((&cache, "source")),
        )
        .await
        .unwrap();
        assert!(!initial.has_more && initial.repositories.len() == 25);
        let repeated = crate::github::bounded_cached_client(
            &client,
            "generated_fixture_invalid",
            &request(),
            &CancellationToken::new(),
            &mut async || Ok(()),
            Some((&cache, "source")),
        )
        .await
        .unwrap();
        assert!(repeated.has_more && repeated.repositories.len() == 25);
        assert!(matches!(
            repeated.into_page(&request()).unwrap().continuation,
            nddev_device_sync_domain::repositories::catalog::CatalogContinuation::More(_)
        ));
    }
    #[tokio::test]
    async fn conditional_reads_preserve_authorization_and_do_not_cross_source_edits() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for index in 0..7 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    header.push(byte[0]);
                    assert!(header.len() < 32768);
                }
                let conditional = header
                    .windows(b"if-none-match:".len())
                    .any(|v| v == b"if-none-match:");
                assert_eq!(conditional, matches!(index, 2 | 3 | 6));
                let (code, body): (u16, &[u8]) = match index {
                    0 | 4 => (200, br#"{"id":1,"login":"fixture","type":"User"}"#),
                    1 | 5 => (200, b"[]"),
                    2 | 3 => (304, b""),
                    _ => (401, b"private refusal"),
                };
                stream
                    .write_all(&response(code, "ETag: \"fixture-etag\"\r\n", body))
                    .await
                    .unwrap();
            }
        });
        let server = Fixture { origin, task };
        let client = Client::fixture(server.origin.clone());
        let cache = crate::github::CatalogCache::default();
        for scope in ["source-a", "source-a", "source-b"] {
            let mut validations = 0;
            let page = crate::github::bounded_cached_client(
                &client,
                "generated_fixture_invalid",
                &request(),
                &CancellationToken::new(),
                &mut async || {
                    validations += 1;
                    Ok(())
                },
                Some((&cache, scope)),
            )
            .await
            .unwrap();
            assert!(page.repositories.is_empty());
            assert_eq!(validations, 3);
        }
        let denied = crate::github::bounded_cached_client(
            &client,
            "generated_fixture_invalid",
            &request(),
            &CancellationToken::new(),
            &mut async || Ok(()),
            Some((&cache, "source-b")),
        )
        .await;
        assert_eq!(denied.err(), Some(ModuleError::Unavailable));
        // The cached user/page must not turn a fresh authentication refusal into success.
    }
    #[tokio::test]
    async fn rate_refusal_prevents_a_second_network_attempt() {
        let server = fixture(
            response(429, "Retry-After: 120\r\n", b"private failure"),
            std::time::Duration::ZERO,
        )
        .await;
        let client = Client::fixture(server.origin.clone());
        for _ in 0..2 {
            assert_eq!(
                bounded_client(
                    &client,
                    "generated_fixture_invalid",
                    &request(),
                    &CancellationToken::new(),
                    &mut async || Ok(())
                )
                .await
                .err(),
                Some(ModuleError::Busy)
            );
        }
        // The fixture only serves one request. A second GET would fail transport,
        // not produce Busy. This exercises the production transport admission.
        assert!(
            client
                .limits
                .lock()
                .unwrap()
                .remaining(crate::github::now_ms().unwrap())
                >= std::time::Duration::from_secs(119)
        );
    }
    #[tokio::test]
    async fn owned_receiver_guard_denies_between_gets_before_second_request() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    header.push(byte[0]);
                    assert!(header.len() < 32768);
                }
                let current = seen.fetch_add(1, Ordering::SeqCst);
                let body: &[u8] = if current == 0 {
                    br#"{"id":1,"login":"fixture","type":"User"}"#
                } else {
                    b"[]"
                };
                let _ = stream.write_all(&response(200, "", body)).await;
            }
        });
        let server = Fixture { origin, task };
        let mut checks = 0;
        let mut guard = async || {
            checks += 1;
            if checks == 2 {
                Err(ModuleError::Unavailable)
            } else {
                Ok(())
            }
        };
        let result = bounded_client(
            &Client::fixture(server.origin.clone()),
            "generated_fixture_invalid",
            &request(),
            &CancellationToken::new(),
            &mut guard,
        )
        .await;
        assert_eq!(result.err(), Some(ModuleError::Unavailable));
        assert_eq!(checks, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Controlled guard admission, not actual Accounts/Vault revocation proof.
    }
    #[tokio::test]
    async fn owned_native_http_negatives_are_not_github_positive_proof() {
        for (code, headers, body, expected) in [
            (
                401,
                "",
                b"private failure body".to_vec(),
                ModuleError::Unavailable,
            ),
            (429, "Retry-After: 10\r\n", vec![], ModuleError::Busy),
            (
                302,
                "Location: https://other.invalid/\r\n",
                vec![],
                ModuleError::InvalidResponse,
            ),
            (200, "", b"not json".to_vec(), ModuleError::InvalidResponse),
            (
                200,
                "",
                vec![b' '; MAX_BODY + 1],
                ModuleError::InvalidResponse,
            ),
        ] {
            let server = fixture(response(code, headers, &body), std::time::Duration::ZERO).await;
            let result = bounded_client(
                &Client::fixture(server.origin.clone()),
                "generated_fixture_invalid",
                &request(),
                &CancellationToken::new(),
                &mut async || Ok(()),
            )
            .await;
            assert_eq!(result.err(), Some(expected));
        }
    }
    #[tokio::test]
    async fn repeated_link_fields_preserve_the_next_relation() {
        let next = "https://api.github.com/user/repos?affiliation=owner,organization_member&visibility=all&sort=full_name&per_page=25&page=2";
        let headers = format!(
            "Link: <https://api.github.com/user/repos?page=9>; rel=last\r\nLink: <{next}>; rel=next\r\n"
        );
        let server = fixture(response(200, &headers, b"[]"), std::time::Duration::ZERO).await;
        let reply = Client::fixture(server.origin.clone())
            .get("generated_fixture_invalid", Route::Repositories(1), None)
            .await
            .unwrap();
        assert_eq!(crate::github::next_page(reply.link.as_deref(), 1), Ok(true));
    }
    #[tokio::test]
    async fn decoded_gzip_limit_and_owned_cancel_deadline_are_real_io_negatives() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&vec![b' '; MAX_BODY + 1]).unwrap();
        let body = gzip.finish().unwrap();
        let server = fixture(
            response(200, "Content-Encoding: gzip\r\n", &body),
            std::time::Duration::ZERO,
        )
        .await;
        assert_eq!(
            bounded_client(
                &Client::fixture(server.origin.clone()),
                "generated_fixture_invalid",
                &request(),
                &CancellationToken::new(),
                &mut async || Ok(()),
            )
            .await
            .err(),
            Some(ModuleError::InvalidResponse)
        );
        let server = fixture(vec![], std::time::Duration::from_secs(20)).await;
        let cancel = CancellationToken::new();
        let delayed = cancel.clone();
        let owner = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            delayed.cancel();
        });
        assert_eq!(
            bounded_client(
                &Client::fixture(server.origin.clone()),
                "generated_fixture_invalid",
                &request(),
                &cancel,
                &mut async || Ok(()),
            )
            .await
            .err(),
            Some(ModuleError::Cancelled)
        );
        owner.await.unwrap();
        let deadline_server = fixture(vec![], std::time::Duration::from_secs(20)).await;
        assert_eq!(
            bounded_client(
                &Client::fixture(deadline_server.origin.clone()),
                "generated_fixture_invalid",
                &request(),
                &CancellationToken::new(),
                &mut async || Ok(()),
            )
            .await
            .err(),
            Some(ModuleError::Timeout)
        );
    }
}

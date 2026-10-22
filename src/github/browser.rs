use super::{API_VERSION, MAX_BODY, ModuleError, ORIGIN, Reply, Route, status};
use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, Request, RequestInit, Response};
use zeroize::Zeroizing;

pub(super) struct Client {
    pub(super) admission: tokio::sync::Semaphore,
    pub(super) limits: std::sync::Mutex<super::limits::Limits>,
    #[cfg(test)]
    origin: Option<String>,
}
struct Abort(AbortController);
impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Client {
    pub(super) fn new() -> Result<Self, ModuleError> {
        Ok(Self {
            admission: tokio::sync::Semaphore::new(1),
            limits: std::sync::Mutex::default(),
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
        let window = web_sys::window().ok_or(ModuleError::Unavailable)?;
        let abort = Abort(AbortController::new().map_err(|_| ModuleError::Unavailable)?);
        let options = RequestInit::new();
        options.set_method("GET");
        options.set_mode(web_sys::RequestMode::Cors);
        options.set_credentials(web_sys::RequestCredentials::Omit);
        options.set_redirect(web_sys::RequestRedirect::Error);
        options.set_cache(web_sys::RequestCache::NoStore);
        options.set_referrer_policy(web_sys::ReferrerPolicy::NoReferrer);
        options.set_signal(Some(&abort.0.signal()));
        #[cfg(test)]
        let origin = self.origin.as_deref().unwrap_or(ORIGIN);
        #[cfg(not(test))]
        let origin = ORIGIN;
        let request =
            Request::new_with_str_and_init(&format!("{origin}{}", route.path()), &options)
                .map_err(|_| ModuleError::InvalidInput)?;
        request
            .headers()
            .set("Authorization", &format!("Bearer {credential}"))
            .map_err(|_| ModuleError::InvalidInput)?;
        request
            .headers()
            .set("Accept", "application/vnd.github+json")
            .map_err(|_| ModuleError::InvalidInput)?;
        request
            .headers()
            .set("X-GitHub-Api-Version", API_VERSION)
            .map_err(|_| ModuleError::InvalidInput)?;
        if let Some(etag) = etag {
            request
                .headers()
                .set("If-None-Match", etag)
                .map_err(|_| ModuleError::InvalidResponse)?;
        }
        let response = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|_| ModuleError::ProviderFailed)?
            .dyn_into::<Response>()
            .map_err(|_| ModuleError::InvalidResponse)?;
        let header = |name| response.headers().get(name).ok().flatten();
        self.limits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(
                super::now_ms()?,
                response.status(),
                header("x-ratelimit-remaining").as_deref(),
                header("x-ratelimit-reset").as_deref(),
                header("retry-after").as_deref(),
            );
        status(
            response.status(),
            response
                .headers()
                .get("x-ratelimit-remaining")
                .map_err(|_| ModuleError::InvalidResponse)?
                .as_deref(),
            response
                .headers()
                .get("retry-after")
                .map_err(|_| ModuleError::InvalidResponse)?
                .as_deref(),
        )?;
        let link = response
            .headers()
            .get("link")
            .map_err(|_| ModuleError::InvalidResponse)?;
        let unchanged = response.status() == 304;
        let etag = response
            .headers()
            .get("etag")
            .ok()
            .flatten()
            .filter(|value| value.len() <= 512 && value.is_ascii());
        if unchanged {
            return Ok(Reply {
                unchanged,
                etag,
                body: Zeroizing::new(Vec::new()),
                link,
            });
        }
        let stream = response.body().ok_or(ModuleError::InvalidResponse)?;
        let reader = stream
            .get_reader()
            .dyn_into::<web_sys::ReadableStreamDefaultReader>()
            .map_err(|_| ModuleError::InvalidResponse)?;
        let mut body = Zeroizing::new(Vec::new());
        loop {
            let value = JsFuture::from(reader.read())
                .await
                .map_err(|_| ModuleError::ProviderFailed)?;
            let done = Reflect::get(&value, &JsValue::from_str("done"))
                .map_err(|_| ModuleError::InvalidResponse)?
                .as_bool()
                .ok_or(ModuleError::InvalidResponse)?;
            if done {
                break;
            }
            let chunk = Reflect::get(&value, &JsValue::from_str("value"))
                .map_err(|_| ModuleError::InvalidResponse)?
                .dyn_into::<Uint8Array>()
                .map_err(|_| ModuleError::InvalidResponse)?;
            if body.len().saturating_add(chunk.length() as usize) > MAX_BODY {
                let _ = reader.cancel();
                return Err(ModuleError::InvalidResponse);
            }
            let start = body.len();
            body.resize(start + chunk.length() as usize, 0);
            chunk.copy_to(&mut body[start..]);
            chunk.fill(0, 0, chunk.length());
        }
        reader.release_lock();
        // No JS/Dart DTO contains headers/token/body; only parsed Rust metadata
        // escapes this adapter. JS engine copies are not a memory-erasure claim.
        Ok(Reply {
            unchanged,
            etag,
            body,
            link,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{CancellationToken, bounded_client};
    use nddev_device_sync_domain::repositories::catalog::{
        CatalogPageRequest, CredentialSelection,
    };
    use wasm_bindgen_test::*;
    wasm_bindgen_test_configure!(run_in_browser);
    fn fixture(case: &str) -> Client {
        Client {
            admission: tokio::sync::Semaphore::new(1),
            limits: std::sync::Mutex::default(),
            origin: Some(format!("http://127.0.0.1:19483/{case}")),
        }
    }
    fn request() -> CatalogPageRequest {
        CatalogPageRequest::new(
            CredentialSelection::new("fixture".into(), 0, None).unwrap(),
            None,
        )
        .unwrap()
    }
    #[wasm_bindgen_test]
    async fn owned_browser_http_negatives_are_not_github_positive_proof() {
        for (case, expected) in [
            ("malformed", ModuleError::InvalidResponse),
            ("oversized", ModuleError::InvalidResponse),
            ("rate", ModuleError::Busy),
            ("redirect", ModuleError::ProviderFailed),
            ("denied", ModuleError::Unavailable),
        ] {
            assert_eq!(
                bounded_client(
                    &fixture(case),
                    "generated_fixture_invalid",
                    &request(),
                    &CancellationToken::new(),
                    &mut async || Ok(()),
                )
                .await
                .err(),
                Some(expected)
            );
        }
    }
    #[wasm_bindgen_test]
    async fn owned_browser_cancel_and_deadline_abort_real_fetch() {
        let cancellation = CancellationToken::new();
        let cancel = cancellation.clone();
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(25).await;
            cancel.cancel();
        });
        assert_eq!(
            bounded_client(
                &fixture("slow"),
                "generated_fixture_invalid",
                &request(),
                &cancellation,
                &mut async || Ok(()),
            )
            .await
            .err(),
            Some(ModuleError::Cancelled)
        );
        assert_eq!(
            bounded_client(
                &fixture("slow"),
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

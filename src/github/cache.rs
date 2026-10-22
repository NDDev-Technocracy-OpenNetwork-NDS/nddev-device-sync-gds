//! Bounded parsed metadata only, scoped to an immutable encrypted source edit.
//! Authorization is rechecked around every conditional GET, including 304.
use super::*;
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone)]
struct Cached<T> {
    etag: String,
    bytes: usize,
    value: T,
}
#[derive(Clone, Default)]
pub(super) struct Snapshot {
    user: Option<Cached<GithubOwner>>,
    pages: BTreeMap<u8, Cached<(Vec<RepositorySummary>, bool)>>,
}
#[derive(Default)]
pub(crate) struct CatalogCache(Mutex<(String, Snapshot)>);
impl CatalogCache {
    pub(super) fn snapshot(&self, scope: &str, page: u8) -> Snapshot {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.0 != scope {
            *state = (scope.to_owned(), Snapshot::default());
        }
        Snapshot {
            user: state.1.user.clone(),
            pages: state
                .1
                .pages
                .get(&page)
                .map(|value| BTreeMap::from([(page, value.clone())]))
                .unwrap_or_default(),
        }
    }
    fn update(&self, scope: &str, apply: impl FnOnce(&mut Snapshot)) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.0 == scope {
            apply(&mut state.1);
            while state.1.user.as_ref().map_or(0, |value| value.bytes)
                + state
                    .1
                    .pages
                    .values()
                    .map(|value| value.bytes)
                    .sum::<usize>()
                > 4 * MAX_BODY
            {
                if state.1.pages.pop_first().is_none() {
                    state.1.user = None;
                    break;
                }
            }
        }
    }
}
pub(super) async fn user(
    client: &platform::Client,
    credential: &str,
    snapshot: Option<&Snapshot>,
    cache: Option<(&CatalogCache, &str)>,
) -> Result<GithubOwner, ModuleError> {
    let cached = snapshot.and_then(|value| value.user.as_ref());
    let reply = client
        .get(
            credential,
            Route::User,
            cached.map(|value| value.etag.as_str()),
        )
        .await?;
    if reply.unchanged {
        return cached
            .map(|value| value.value.clone())
            .ok_or(ModuleError::InvalidResponse);
    }
    let value = serde_json::from_slice::<OwnerWire>(&reply.body)
        .map_err(|_| ModuleError::InvalidResponse)?
        .decode()?;
    if let Some((cache, scope)) = cache {
        cache.update(scope, |state| {
            state.user = reply.etag.map(|etag| Cached {
                etag,
                bytes: reply.body.len() + 4096,
                value: value.clone(),
            })
        });
    }
    Ok(value)
}
pub(super) async fn repositories(
    client: &platform::Client,
    credential: &str,
    page: u8,
    snapshot: Option<&Snapshot>,
    cache: Option<(&CatalogCache, &str)>,
) -> Result<(Vec<RepositorySummary>, bool), ModuleError> {
    let cached = snapshot.and_then(|value| value.pages.get(&page));
    let reply = client
        .get(
            credential,
            Route::Repositories(page),
            cached.map(|value| value.etag.as_str()),
        )
        .await?;
    if reply.unchanged {
        let (rows, previous_more) = cached
            .map(|value| value.value.clone())
            .ok_or(ModuleError::InvalidResponse)?;
        let more = match reply.link.as_deref() {
            Some(link) => next_page(Some(link), page)?,
            None => previous_more || rows.len() == REPOSITORIES_PER_PAGE,
        };
        return Ok((rows, more));
    }
    let more = next_page(reply.link.as_deref(), page)?;
    let rows: Vec<RepositoryWire> =
        serde_json::from_slice(&reply.body).map_err(|_| ModuleError::InvalidResponse)?;
    if rows.len() > REPOSITORIES_PER_PAGE {
        return Err(ModuleError::InvalidResponse);
    }
    let rows = rows
        .into_iter()
        .map(RepositoryWire::decode)
        .collect::<Result<Vec<_>, _>>()?;
    let result = (rows, more);
    if let Some((cache, scope)) = cache {
        cache.update(scope, |state| {
            if !more {
                state.pages.retain(|key, _| *key <= page);
            }
            state.pages.remove(&page);
            // CatalogPageRequest bounds page numbers to 64, at most 1600 rows.
            if let Some(etag) = reply.etag {
                state.pages.insert(
                    page,
                    Cached {
                        etag,
                        bytes: reply.body.len() + 4096,
                        value: result.clone(),
                    },
                );
            }
        });
    }
    Ok(result)
}

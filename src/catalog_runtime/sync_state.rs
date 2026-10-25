//! Bounded local observations, never replicated authority or a second scheduler.
use super::view::GithubCommitSyncView;
use nddev_device_sync_accounts::runtime::Instant;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

pub(super) type Epoch = (Option<String>, Option<String>);
pub(super) struct CommitState {
    // One bounded read witness for this scope, not one full baseline per repo.
    pub guard: Option<std::sync::Arc<super::driver::CatalogUseGuard>>,
    pub epoch: Option<Epoch>,
    pub queue: VecDeque<nddev_device_sync_vault::ConnectionSetting>,
    pub after: String,
    pub next: Option<Instant>,
    pub end: bool,
    observations: BTreeMap<String, GithubCommitSyncView>,
}
impl Default for CommitState {
    fn default() -> Self {
        Self {
            guard: None,
            epoch: None,
            queue: Default::default(),
            after: String::new(),
            next: Some(Instant::now()),
            end: false,
            observations: Default::default(),
        }
    }
}
impl CommitState {
    #[cfg(all(test, target_os = "linux", feature = "commit-sync"))]
    pub(super) fn recorded(&self) -> &BTreeMap<String, GithubCommitSyncView> {
        &self.observations
    }

    pub fn reset(&mut self, epoch: Epoch) {
        if self.epoch.as_ref() != Some(&epoch) {
            *self = Self {
                epoch: Some(epoch),
                ..Default::default()
            };
        }
    }
    pub fn cancel(&mut self) {
        self.guard = None;
        self.queue.clear();
        self.observations.clear();
        self.after.clear();
        self.end = false;
        self.next = Some(Instant::now());
    }
    pub fn ready(&self) -> bool {
        self.next.is_some_and(|next| next <= Instant::now())
    }
    pub fn finish_round(&mut self) {
        self.after.clear();
        self.end = false;
        self.next = Instant::now().checked_add(Duration::from_secs(300));
    }
    pub fn record(&mut self, epoch: &Epoch, id: String, value: GithubCommitSyncView) {
        if self.epoch.as_ref() == Some(epoch)
            && (self.observations.contains_key(&id) || self.observations.len() < 1024)
        {
            self.observations.insert(id, value);
        }
    }
    pub fn observation(&self, epoch: &Epoch, id: &str) -> Option<GithubCommitSyncView> {
        (self.epoch.as_ref() == Some(epoch))
            .then(|| self.observations.get(id).cloned())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_are_bounded_and_cannot_survive_source_consent_or_owner_cancellation() {
        let epoch = (
            Some("source-original".into()),
            Some("consent-original".into()),
        );
        for _device in 0..32 {
            let mut state = CommitState::default();
            state.reset(epoch.clone());
            for id in 1..=1025 {
                state.record(
                    &epoch,
                    id.to_string(),
                    GithubCommitSyncView {
                        outcome: "published".into(),
                        reason: String::new(),
                        observed_at_ms: Some("100".into()),
                    },
                );
            }
            assert_eq!(state.observations.len(), 1024);
            assert!(state.observation(&epoch, "1025").is_none());
            let replacement = (Some("new-source".into()), epoch.1.clone());
            assert!(state.observation(&replacement, "1").is_none());
            state.reset(replacement.clone());
            assert!(state.observation(&epoch, "1").is_none());
            state.record(
                &epoch,
                "1".into(),
                GithubCommitSyncView {
                    outcome: "published".into(),
                    reason: String::new(),
                    observed_at_ms: Some("101".into()),
                },
            );
            assert!(state.observation(&replacement, "1").is_none());
            state.record(
                &replacement,
                "1".into(),
                GithubCommitSyncView {
                    outcome: "running".into(),
                    reason: String::new(),
                    observed_at_ms: None,
                },
            );
            state.finish_round();
            assert!(!state.ready());
            state.cancel();
            assert!(state.ready());
            assert!(state.observation(&replacement, "1").is_none());
        }
    }
}

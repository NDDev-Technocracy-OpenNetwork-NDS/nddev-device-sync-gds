//! Fixed-origin process admission. Never retains credentials or response bodies.
//! GitHub's reset is an epoch timestamp; deadlines survive source/cursor edits.
use super::ModuleError;

#[derive(Default)]
pub(super) struct Limits {
    until_ms: u64,
    refusals: u8,
}
impl Limits {
    #[cfg(any(feature = "catalog-runtime", test))]
    pub(super) fn remaining(&self, now: u64) -> std::time::Duration {
        std::time::Duration::from_millis(self.until_ms.saturating_sub(now))
    }
    pub(super) fn admit(&mut self, now: u64) -> Result<(), ModuleError> {
        if now < self.until_ms {
            return Err(if self.refusals >= 6 {
                ModuleError::Unavailable
            } else {
                ModuleError::Busy
            });
        }
        if self.refusals >= 6 {
            self.refusals = 0;
        }
        Ok(())
    }
    pub(super) fn observe(
        &mut self,
        now: u64,
        status: u16,
        remaining: Option<&str>,
        reset: Option<&str>,
        retry_after: Option<&str>,
    ) {
        if status == 429 || status == 403 || remaining == Some("0") {
            self.refusals = self.refusals.saturating_add(1).min(6);
            // Forbidden responses without exposed rate headers also pause:
            // secondary-limit responses may omit the primary-budget headers.
            let fallback = 60_u64.saturating_mul(1 << (self.refusals - 1));
            let retry = retry_after
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| now.saturating_add(seconds.saturating_mul(1000)));
            let reset = (remaining == Some("0"))
                .then_some(reset)
                .flatten()
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| seconds.saturating_mul(1000).saturating_add(1000));
            self.until_ms = retry
                .into_iter()
                .chain(reset)
                .fold(now.saturating_add(fallback * 1000), u64::max);
            if self.refusals >= 6 {
                // End the bounded retry episode; allow a later scheduled probe.
                self.until_ms = self.until_ms.max(now.saturating_add(3_600_000));
            }
            tracing::warn!(
                event.name = "github.http.deferred",
                module = "github",
                scope = "io",
                outcome = "rejected",
                status,
                retry_ms = self.until_ms.saturating_sub(now),
                attempt = self.refusals
            );
        } else if status == 200 {
            self.refusals = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_and_reset_are_both_lower_bounds_even_on_success() {
        let mut limits = Limits::default();
        limits.observe(10_000, 200, Some("0"), Some("150"), Some("200"));
        assert_eq!(limits.admit(209_999), Err(ModuleError::Busy));
        assert!(limits.admit(210_000).is_ok());
        limits.observe(210_000, 200, Some("20"), None, None);
        assert_eq!(limits.refusals, 0);
    }
    #[test]
    fn missing_or_malformed_headers_back_off_and_end_the_retry_episode() {
        let mut limits = Limits::default();
        let mut now = 0;
        for attempt in 1..=6 {
            limits.observe(now, 403, Some("invalid"), Some("invalid"), Some("invalid"));
            let delay = if attempt == 6 {
                3_600_000
            } else {
                60_000 << (attempt - 1)
            };
            assert_eq!(limits.until_ms, now + delay);
            assert!(limits.admit(now + delay - 1).is_err());
            now += delay;
            assert!(limits.admit(now).is_ok());
        }
        assert_eq!(limits.refusals, 0);
    }
    #[test]
    fn untrusted_large_deadlines_never_wrap_to_an_early_retry() {
        let mut limits = Limits::default();
        limits.observe(1000, 429, None, None, Some("18446744073709551615"));
        assert_eq!(limits.until_ms, u64::MAX);
        assert_eq!(limits.admit(1_000_000), Err(ModuleError::Busy));
    }
}

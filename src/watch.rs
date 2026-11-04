//! Polling change detection: a pure state machine the CLI loop drives.
//! No sleeping, no clock — callers act on the returned [`Tick`].

use std::time::Duration;

use crate::api::{ApiError, FigmaApi};

pub(crate) const BACKOFF_START: Duration = Duration::from_secs(5);
pub(crate) const BACKOFF_CAP: Duration = Duration::from_secs(300);

/// Outcome of one poll.
#[derive(Debug)]
pub enum Tick {
    /// File unchanged since the last successfully synchronized `last_touched_at`.
    Unchanged,
    /// File changed (or a prior change is still pending) — caller should
    /// pull, then call [`Watcher::note_pull_success`] only on success.
    Changed,
    /// Transient failure or rate limit — caller should sleep `after`
    /// (instead of its normal interval), then tick again.
    Wait { after: Duration },
}

/// Tracks synchronized and pending metadata watermarks and poll backoff.
pub struct Watcher {
    synchronized: Option<String>,
    pending: Option<String>,
    backoff: Duration,
}

impl Watcher {
    /// `synchronized` must be a metadata `last_touched_at` observed before
    /// a successful pull, never the full-file `lastModified`: the two
    /// endpoints can report different timestamps for unchanged content.
    /// Use `None` at startup, since no metadata watermark is persisted.
    pub fn new(synchronized: Option<String>) -> Self {
        Watcher {
            synchronized,
            pending: None,
            backoff: BACKOFF_START,
        }
    }

    /// Acknowledge only the metadata observed *before* this pull. Without
    /// a pending observation (e.g. an explicit sync before the first
    /// poll), keep the previous watermark, possibly unknown. Never sample
    /// post-fetch metadata here: a newer edit may not be in the fetched file.
    /// Metadata failure backoff resets only after a successful metadata poll.
    pub fn note_pull_success(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.synchronized = Some(pending);
        }
    }

    /// Poll `file_meta` once and classify the result. Never fetches the
    /// file itself — that's the caller's job on [`Tick::Changed`].
    pub fn tick(&mut self, api: &dyn FigmaApi, key: &str) -> Tick {
        match api.file_meta(key) {
            Ok(meta) => {
                self.backoff = BACKOFF_START;
                if self.pending.is_none()
                    && self.synchronized.as_deref() == Some(meta.last_touched_at.as_str())
                {
                    Tick::Unchanged
                } else {
                    self.pending = Some(meta.last_touched_at);
                    Tick::Changed
                }
            }
            Err(ApiError::RateLimited { retry_after }) => Tick::Wait { after: retry_after },
            Err(_) => {
                let after = self.backoff;
                self.backoff = (self.backoff * 2).min(BACKOFF_CAP);
                Tick::Wait { after }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiError, FigmaApi, FileMetaResp};
    use std::cell::RefCell;
    use std::time::Duration;

    /// Scripted API: pops one response per call; panics if `file` is called.
    struct Script(RefCell<Vec<Result<FileMetaResp, ApiError>>>);
    impl Script {
        fn new(mut responses: Vec<Result<FileMetaResp, ApiError>>) -> Self {
            responses.reverse();
            Script(RefCell::new(responses))
        }
    }
    impl FigmaApi for Script {
        fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
            self.0
                .borrow_mut()
                .pop()
                .expect("unexpected extra file_meta call")
        }
        fn file(&self, _key: &str, _geometry: bool) -> Result<serde_json::Value, ApiError> {
            panic!("watcher must never fetch the file itself");
        }
    }

    fn meta(t: &str) -> Result<FileMetaResp, ApiError> {
        Ok(FileMetaResp {
            name: "F".into(),
            last_touched_at: t.into(),
        })
    }

    #[test]
    fn unchanged_then_changed() {
        let api = Script::new(vec![meta("t1"), meta("t1"), meta("t2"), meta("t2")]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        w.note_pull_success();
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
    }

    #[test]
    fn first_tick_with_no_history_is_changed() {
        let api = Script::new(vec![meta("t1")]);
        let mut w = Watcher::new(None);
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
    }

    #[test]
    fn change_stays_pending_until_a_pull_succeeds_even_if_metadata_reverts() {
        let api = Script::new(vec![meta("t2"), meta("t2"), meta("t1"), meta("t1")]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        // Neither observing the same change again nor reverting to the
        // synchronized timestamp acknowledges the outstanding pull.
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        w.note_pull_success();
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
    }

    #[test]
    fn distinct_changes_within_one_second_each_need_a_successful_pull() {
        let api = Script::new(vec![
            meta("2026-01-01T12:00:00.123Z"),
            meta("2026-01-01T12:00:00.456Z"),
            meta("2026-01-01T12:00:00.456Z"),
        ]);
        let mut w = Watcher::new(None);
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        w.note_pull_success();
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
        w.note_pull_success();
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
    }

    #[test]
    fn successful_pull_without_a_metadata_observation_keeps_watermark_unknown() {
        let api = Script::new(vec![meta("t1")]);
        let mut w = Watcher::new(None);
        w.note_pull_success();
        assert!(matches!(w.tick(&api, "k"), Tick::Changed));
    }

    #[test]
    fn rate_limit_uses_retry_after() {
        let api = Script::new(vec![
            Err(ApiError::RateLimited {
                retry_after: Duration::from_secs(30),
            }),
            meta("t1"),
        ]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(30))
        );
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
    }

    #[test]
    fn failures_back_off_exponentially_and_reset_on_success() {
        let api = Script::new(vec![
            Err(ApiError::Network("down".into())),
            Err(ApiError::Network("down".into())),
            Err(ApiError::Network("down".into())),
            meta("t1"),
            Err(ApiError::Network("down".into())),
        ]);
        let mut w = Watcher::new(Some("t1".into()));
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(5))
        );
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(10))
        );
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(20))
        );
        assert!(matches!(w.tick(&api, "k"), Tick::Unchanged));
        assert!(
            matches!(w.tick(&api, "k"), Tick::Wait { after } if after == Duration::from_secs(5))
        );
    }

    #[test]
    fn backoff_caps_at_five_minutes() {
        let mut responses: Vec<Result<FileMetaResp, ApiError>> = (0..10)
            .map(|_| Err(ApiError::Network("down".into())))
            .collect();
        responses.push(meta("t1"));
        let api = Script::new(responses);
        let mut w = Watcher::new(Some("t1".into()));
        let mut last = Duration::ZERO;
        for _ in 0..10 {
            match w.tick(&api, "k") {
                Tick::Wait { after } => last = after,
                other => panic!("expected Wait, got {other:?}"),
            }
        }
        assert_eq!(last, Duration::from_secs(300));
    }
}

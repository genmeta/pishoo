use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use dhttp::name::DhttpName;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use super::source::{TlsIdentityWatchFilter, TypedServerSource};

const DEBOUNCE: Duration = Duration::from_millis(250);
const PERIODIC_RETRY: Duration = Duration::from_secs(30);

enum WatchMessage {
    Event(Event),
    Error(notify::Error),
}

pub(super) struct IdentityWatcher {
    watcher: Option<RecommendedWatcher>,
    tx: tokio::sync::mpsc::UnboundedSender<WatchMessage>,
    rx: tokio::sync::mpsc::UnboundedReceiver<WatchMessage>,
    names: Vec<DhttpName<'static>>,
    watch_sources: Vec<(DhttpName<'static>, TlsIdentityWatchFilter)>,
    pending_names: HashSet<DhttpName<'static>>,
    retry: tokio::time::Interval,
    debounce_deadline: Option<tokio::time::Instant>,
}

impl IdentityWatcher {
    pub(super) fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut retry = tokio::time::interval(PERIODIC_RETRY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        retry.reset_after(PERIODIC_RETRY);
        Self {
            watcher: None,
            tx,
            rx,
            names: Vec::new(),
            watch_sources: Vec::new(),
            pending_names: HashSet::new(),
            retry,
            debounce_deadline: None,
        }
    }

    pub(super) fn reconfigure(&mut self, sources: &HashMap<DhttpName<'static>, TypedServerSource>) {
        self.watcher = None;
        self.discard_messages();
        self.names = sources.keys().cloned().collect();
        self.names
            .sort_by(|left, right| left.as_full().cmp(right.as_full()));
        self.watch_sources = sources
            .iter()
            .map(|(name, source)| (name.clone(), source.identity_source().watch_filter()))
            .collect();
        self.watch_sources
            .sort_by(|(left, _), (right, _)| left.as_full().cmp(right.as_full()));
        self.pending_names.clear();
        self.debounce_deadline = None;
        self.retry.reset_after(PERIODIC_RETRY);

        let tx = self.tx.clone();
        let mut watcher = match notify::recommended_watcher(move |result| {
            let message = match result {
                Ok(event) => WatchMessage::Event(event),
                Err(error) => WatchMessage::Error(error),
            };
            let _ = tx.send(message);
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(error = %error, "failed to create TLS identity watcher; periodic retry remains active");
                self.watcher = None;
                return;
            }
        };

        let mut targets = Vec::<(PathBuf, bool)>::new();
        for source in sources.values() {
            for (path, recursive) in source.identity_source().watch_targets() {
                if let Some((_, existing_recursive)) =
                    targets.iter_mut().find(|target| target.0 == path)
                {
                    *existing_recursive |= recursive;
                } else {
                    targets.push((path, recursive));
                }
            }
        }
        for (path, recursive) in targets {
            let mode = if recursive {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if let Err(error) = watcher.watch(path.as_path(), mode) {
                tracing::warn!(path = %path.display(), error = %error, "failed to watch TLS identity path; periodic retry remains active");
            }
        }
        self.watcher = Some(watcher);
    }

    pub(super) async fn next_changes(&mut self) -> Vec<DhttpName<'static>> {
        if self.names.is_empty() {
            return std::future::pending().await;
        }

        loop {
            if let Some(deadline) = self.debounce_deadline {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        self.debounce_deadline = None;
                        self.drain_messages();
                        let changes = self.take_pending_names();
                        if !changes.is_empty() {
                            return changes;
                        }
                    }
                    message = self.rx.recv() => self.handle_message(message),
                    _ = self.retry.tick() => {
                        return self.periodic_changes();
                    },
                }
            } else {
                tokio::select! {
                    message = self.rx.recv() => self.handle_message(message),
                    _ = self.retry.tick() => return self.periodic_changes(),
                }
            }
        }
    }

    fn handle_message(&mut self, message: Option<WatchMessage>) {
        if self.record_message(message) {
            self.debounce_deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
        }
    }

    fn record_message(&mut self, message: Option<WatchMessage>) -> bool {
        match message {
            Some(WatchMessage::Event(event)) if !matches!(event.kind, EventKind::Access(_)) => {
                self.record_event(&event)
            }
            Some(WatchMessage::Event(_)) | None => false,
            Some(WatchMessage::Error(error)) => {
                tracing::warn!(error = %error, "TLS identity watcher reported an error");
                false
            }
        }
    }

    fn record_event(&mut self, event: &Event) -> bool {
        if event.need_rescan() {
            self.pending_names.extend(self.names.iter().cloned());
            return !self.names.is_empty();
        }

        let mut matched = false;
        for (name, filter) in &self.watch_sources {
            if event.paths.iter().any(|path| filter.matches(path)) {
                self.pending_names.insert(name.clone());
                matched = true;
            }
        }
        matched
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.record_message(Some(message));
        }
    }

    fn discard_messages(&mut self) {
        while self.rx.try_recv().is_ok() {}
    }

    fn take_pending_names(&mut self) -> Vec<DhttpName<'static>> {
        let mut names = self.pending_names.drain().collect::<Vec<_>>();
        names.sort_by(|left, right| left.as_full().cmp(right.as_full()));
        names
    }

    fn periodic_changes(&mut self) -> Vec<DhttpName<'static>> {
        self.debounce_deadline = None;
        self.drain_messages();
        self.pending_names.clear();
        self.names.clone()
    }
}

impl Default for IdentityWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{Flag, ModifyKind};

    use super::*;
    use crate::service::source::TlsIdentitySource;

    fn configure_profile(watcher: &mut IdentityWatcher, profile_path: &str) {
        let profile_path = PathBuf::from(profile_path);
        let profile = dhttp::home::identity::IdentityProfile::try_from(profile_path).unwrap();
        let name = profile.name().clone();
        watcher.names = vec![name.clone()];
        watcher.watch_sources = vec![(name, TlsIdentitySource::Profile(profile).watch_filter())];
    }

    fn modified(path: &str) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.into())
    }

    #[tokio::test]
    async fn file_events_are_debounced_into_one_identity_batch() {
        let mut watcher = IdentityWatcher::new();
        configure_profile(&mut watcher, "/tmp/watch.example.dhttp.net");
        watcher
            .tx
            .send(WatchMessage::Event(modified(
                "/tmp/watch.example.dhttp.net/ssl/fullchain.crt",
            )))
            .unwrap();
        watcher
            .tx
            .send(WatchMessage::Event(modified(
                "/tmp/watch.example.dhttp.net/ssl/privkey.pem",
            )))
            .unwrap();

        let changed = watcher.next_changes().await;

        assert_eq!(changed, watcher.names);
        assert!(watcher.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn debounce_survives_cancelled_wait() {
        let mut watcher = IdentityWatcher::new();
        configure_profile(&mut watcher, "/tmp/watch.example.dhttp.net");
        watcher
            .tx
            .send(WatchMessage::Event(modified(
                "/tmp/watch.example.dhttp.net/ssl/fullchain.crt",
            )))
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(20), watcher.next_changes())
                .await
                .is_err()
        );

        let changed = tokio::time::timeout(Duration::from_secs(1), watcher.next_changes())
            .await
            .expect("debounced event should remain pending after cancellation");
        assert_eq!(changed, watcher.names);
    }

    #[tokio::test]
    async fn access_log_writes_do_not_schedule_identity_reload() {
        let mut watcher = IdentityWatcher::new();
        configure_profile(&mut watcher, "/tmp/watch.example.dhttp.net");
        watcher
            .tx
            .send(WatchMessage::Event(modified(
                "/tmp/watch.example.dhttp.net/logs/access.log",
            )))
            .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_millis(50), watcher.next_changes())
                .await
                .is_err()
        );
        assert!(watcher.pending_names.is_empty());
        assert!(watcher.debounce_deadline.is_none());
    }

    #[tokio::test]
    async fn event_reload_contains_only_the_affected_identity() {
        let mut watcher = IdentityWatcher::new();
        let first = dhttp::home::identity::IdentityProfile::try_from(PathBuf::from(
            "/tmp/first.example.dhttp.net",
        ))
        .unwrap();
        let second = dhttp::home::identity::IdentityProfile::try_from(PathBuf::from(
            "/tmp/second.example.dhttp.net",
        ))
        .unwrap();
        watcher.names = vec![first.name().clone(), second.name().clone()];
        watcher.watch_sources = vec![
            (
                first.name().clone(),
                TlsIdentitySource::Profile(first.clone()).watch_filter(),
            ),
            (
                second.name().clone(),
                TlsIdentitySource::Profile(second.clone()).watch_filter(),
            ),
        ];
        watcher
            .tx
            .send(WatchMessage::Event(modified(
                "/tmp/second.example.dhttp.net/ssl/fullchain.crt",
            )))
            .unwrap();

        let changed = watcher.next_changes().await;

        assert_eq!(changed, vec![second.name().clone()]);
    }

    #[tokio::test]
    async fn rescan_event_conservatively_reloads_every_identity() {
        let mut watcher = IdentityWatcher::new();
        configure_profile(&mut watcher, "/tmp/watch.example.dhttp.net");
        watcher
            .tx
            .send(WatchMessage::Event(
                Event::new(EventKind::Other).set_flag(Flag::Rescan),
            ))
            .unwrap();

        let changed = watcher.next_changes().await;

        assert_eq!(changed, watcher.names);
    }
}

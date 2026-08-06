use std::{collections::HashMap, path::PathBuf, time::Duration};

use dhttp::name::DhttpName;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use super::source::TypedServerSource;

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
            retry,
            debounce_deadline: None,
        }
    }

    pub(super) fn reconfigure(&mut self, sources: &HashMap<DhttpName<'static>, TypedServerSource>) {
        self.names = sources.keys().cloned().collect();
        self.names
            .sort_by(|left, right| left.as_full().cmp(right.as_full()));
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

        let mut targets = Vec::<(PathBuf, RecursiveMode)>::new();
        for source in sources.values() {
            for (path, recursive) in source.identity_source().watch_targets() {
                let mode = if recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                if !targets.iter().any(|target| target.0 == path) {
                    targets.push((path, mode));
                }
            }
        }
        for (path, mode) in targets {
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
                        return self.names.clone();
                    }
                    message = self.rx.recv() => self.handle_message(message),
                    _ = self.retry.tick() => {
                        self.debounce_deadline = None;
                        return self.names.clone();
                    },
                }
            } else {
                tokio::select! {
                    message = self.rx.recv() => self.handle_message(message),
                    _ = self.retry.tick() => return self.names.clone(),
                }
            }
        }
    }

    fn handle_message(&mut self, message: Option<WatchMessage>) {
        match message {
            Some(WatchMessage::Event(event)) if !matches!(event.kind, EventKind::Access(_)) => {
                self.debounce_deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
            }
            Some(WatchMessage::Event(_)) | None => {}
            Some(WatchMessage::Error(error)) => {
                tracing::warn!(error = %error, "TLS identity watcher reported an error");
            }
        }
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            if let WatchMessage::Error(error) = message {
                tracing::warn!(error = %error, "TLS identity watcher reported an error");
            }
        }
    }
}

impl Default for IdentityWatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use notify::event::ModifyKind;

    use super::*;

    #[tokio::test]
    async fn file_events_are_debounced_into_one_identity_batch() {
        let mut watcher = IdentityWatcher::new();
        watcher.names = vec![DhttpName::try_from("watch.example.dhttp.net".to_owned()).unwrap()];
        watcher
            .tx
            .send(WatchMessage::Event(Event::new(EventKind::Modify(
                ModifyKind::Any,
            ))))
            .unwrap();
        watcher
            .tx
            .send(WatchMessage::Event(Event::new(EventKind::Modify(
                ModifyKind::Any,
            ))))
            .unwrap();

        let changed = watcher.next_changes().await;

        assert_eq!(changed, watcher.names);
        assert!(watcher.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn debounce_survives_cancelled_wait() {
        let mut watcher = IdentityWatcher::new();
        watcher.names = vec![DhttpName::try_from("watch.example.dhttp.net".to_owned()).unwrap()];
        watcher
            .tx
            .send(WatchMessage::Event(Event::new(EventKind::Modify(
                ModifyKind::Any,
            ))))
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
}

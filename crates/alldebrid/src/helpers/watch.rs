//! Live-mode watcher over `magnet/status`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_core::Stream;

use crate::error::Result;
use crate::types::{LiveSession, LiveUpdate, Magnet, MagnetId};
use crate::Client;

/// Options for [`Client::watch_magnets`].
#[derive(Debug, Clone, PartialEq)]
pub struct WatchOptions {
    /// Time between polls (default 3 s).
    pub interval: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3),
        }
    }
}

/// A change observed by [`Client::watch_magnets`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MagnetEvent {
    /// A magnet appeared (including every magnet present at the first sync).
    Added(Magnet),
    /// A magnet's fields changed (progress, speed, status...).
    Updated(Magnet),
    /// A magnet became ready. Emitted once per magnet.
    Ready(Magnet),
    /// A magnet reached an error status. Emitted once per magnet.
    Failed(Magnet),
    /// A magnet was deleted (from a `deleted` delta, or missing from a full sync).
    Removed(MagnetId),
}

/// Keeps the reconstructed state between live-mode polls.
#[derive(Debug, Default)]
pub(crate) struct WatchState {
    magnets: HashMap<MagnetId, Magnet>,
    notified: HashSet<MagnetId>,
}

impl WatchState {
    pub(crate) fn apply(&mut self, mut update: LiveUpdate) -> Vec<MagnetEvent> {
        let mut events = Vec::new();

        if update.fullsync {
            update.magnets.retain(|d| !d.deleted);
            let seen: HashSet<MagnetId> = update.magnets.iter().map(|d| d.id).collect();
            let gone: Vec<MagnetId> = self
                .magnets
                .keys()
                .filter(|id| !seen.contains(id))
                .copied()
                .collect();
            for id in gone {
                self.magnets.remove(&id);
                self.notified.remove(&id);
                events.push(MagnetEvent::Removed(id));
            }
            for d in update.magnets {
                let new = d.to_magnet();
                match self.magnets.get(&d.id) {
                    None => events.push(MagnetEvent::Added(new.clone())),
                    Some(old) if *old != new => events.push(MagnetEvent::Updated(new.clone())),
                    Some(_) => {}
                }
                self.magnets.insert(d.id, new);
                self.transition(d.id, &mut events);
            }
        } else {
            for d in update.magnets {
                if d.deleted {
                    self.notified.remove(&d.id);
                    if self.magnets.remove(&d.id).is_some() {
                        events.push(MagnetEvent::Removed(d.id));
                    }
                    continue;
                }
                match self.magnets.get_mut(&d.id) {
                    Some(m) => {
                        let before = m.clone();
                        d.apply_to(m);
                        if *m != before {
                            events.push(MagnetEvent::Updated(m.clone()));
                        }
                    }
                    None => {
                        let m = d.to_magnet();
                        events.push(MagnetEvent::Added(m.clone()));
                        self.magnets.insert(d.id, m);
                    }
                }
                self.transition(d.id, &mut events);
            }
        }
        events
    }

    fn transition(&mut self, id: MagnetId, events: &mut Vec<MagnetEvent>) {
        let Some(m) = self.magnets.get(&id) else {
            return;
        };
        if !m.status_code.is_terminal() {
            // A restarted magnet may become ready/failed again later.
            self.notified.remove(&id);
            return;
        }
        if self.notified.insert(id) {
            events.push(if m.status_code.is_ready() {
                MagnetEvent::Ready(m.clone())
            } else {
                MagnetEvent::Failed(m.clone())
            });
        }
    }
}

impl Client {
    /// Watch every magnet of the account using `magnet/status` live mode.
    ///
    /// Only changes travel over the wire after the first call. Errors are yielded
    /// but do not end the stream: after a failed poll, the next one starts a fresh
    /// full sync. Drop the stream to stop watching.
    ///
    /// ```no_run
    /// # async fn run(client: alldebrid::Client) {
    /// use futures_util::StreamExt;
    /// use alldebrid::{MagnetEvent, WatchOptions};
    ///
    /// let mut events = std::pin::pin!(client.watch_magnets(WatchOptions::default()));
    /// while let Some(ev) = events.next().await {
    ///     match ev {
    ///         Ok(MagnetEvent::Ready(m)) => println!("{} is ready", m.filename),
    ///         Ok(_) => {}
    ///         Err(e) => eprintln!("poll failed: {e}"),
    ///     }
    /// }
    /// # }
    /// ```
    pub fn watch_magnets(
        &self,
        opts: WatchOptions,
    ) -> impl Stream<Item = Result<MagnetEvent>> + Send + 'static {
        let client = self.clone();
        async_stream::stream! {
            let mut session = LiveSession::new();
            let mut state = WatchState::default();
            loop {
                match client.live_status(&mut session).await {
                    Ok(update) => {
                        for ev in state.apply(update) {
                            yield Ok(ev);
                        }
                    }
                    Err(e) => {
                        session = LiveSession::new();
                        yield Err(e);
                    }
                }
                tokio::time::sleep(opts.interval).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MagnetDelta;

    fn delta(json: &str) -> MagnetDelta {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn full_sync_then_deltas() {
        let mut st = WatchState::default();
        let ev = st.apply(LiveUpdate {
            fullsync: true,
            magnets: vec![
                delta(r#"{"id":1,"filename":"a","statusCode":1,"downloaded":0}"#),
                delta(r#"{"id":2,"filename":"b","statusCode":4}"#),
            ],
        });
        assert!(matches!(ev[0], MagnetEvent::Added(ref m) if m.id == MagnetId(1)));
        assert!(matches!(ev[1], MagnetEvent::Added(ref m) if m.id == MagnetId(2)));
        assert!(matches!(ev[2], MagnetEvent::Ready(ref m) if m.id == MagnetId(2)));
        assert_eq!(ev.len(), 3);

        let ev = st.apply(LiveUpdate {
            fullsync: false,
            magnets: vec![delta(r#"{"id":1,"downloaded":50}"#)],
        });
        assert!(
            matches!(ev.as_slice(), [MagnetEvent::Updated(m)] if m.downloaded == 50 && m.filename == "a")
        );

        let ev = st.apply(LiveUpdate {
            fullsync: false,
            magnets: vec![delta(r#"{"id":1,"statusCode":4,"status":"Ready"}"#)],
        });
        assert!(matches!(
            ev.as_slice(),
            [MagnetEvent::Updated(_), MagnetEvent::Ready(_)]
        ));

        // Ready is not emitted twice.
        let ev = st.apply(LiveUpdate {
            fullsync: false,
            magnets: vec![delta(r#"{"id":1,"seeders":3}"#)],
        });
        assert!(matches!(ev.as_slice(), [MagnetEvent::Updated(_)]));
    }

    #[test]
    fn resync_detects_removals_and_failures() {
        let mut st = WatchState::default();
        st.apply(LiveUpdate {
            fullsync: true,
            magnets: vec![
                delta(r#"{"id":1,"statusCode":1}"#),
                delta(r#"{"id":2,"statusCode":1}"#),
            ],
        });
        let ev = st.apply(LiveUpdate {
            fullsync: true,
            magnets: vec![delta(r#"{"id":2,"statusCode":15,"status":"No peer"}"#)],
        });
        assert!(ev.contains(&MagnetEvent::Removed(MagnetId(1))));
        assert!(ev
            .iter()
            .any(|e| matches!(e, MagnetEvent::Failed(m) if m.id == MagnetId(2))));
    }

    #[test]
    fn deleted_delta_removes_once() {
        let mut st = WatchState::default();
        st.apply(LiveUpdate {
            fullsync: true,
            magnets: vec![delta(r#"{"id":1,"statusCode":4}"#)],
        });
        // The API repeats the deletion marker on later polls.
        let ev = st.apply(LiveUpdate {
            fullsync: false,
            magnets: vec![delta(r#"{"deleted":true,"id":1}"#)],
        });
        assert_eq!(ev, vec![MagnetEvent::Removed(MagnetId(1))]);
        let ev = st.apply(LiveUpdate {
            fullsync: false,
            magnets: vec![delta(r#"{"deleted":true,"id":1}"#)],
        });
        assert!(ev.is_empty());
        // A deletion of a magnet never seen is not an addition.
        let ev = st.apply(LiveUpdate {
            fullsync: false,
            magnets: vec![delta(r#"{"deleted":true,"id":9}"#)],
        });
        assert!(ev.is_empty());
    }

    #[test]
    fn empty_delta_produces_nothing() {
        let mut st = WatchState::default();
        st.apply(LiveUpdate {
            fullsync: true,
            magnets: vec![],
        });
        assert!(st
            .apply(LiveUpdate {
                fullsync: false,
                magnets: vec![]
            })
            .is_empty());
    }
}

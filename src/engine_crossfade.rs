//! One speculative read per next occurrence. A failed read/decoder never
//! changes playback; ordinary EOF remains the fallback. Dropping a lookahead
//! also drops its reply channel, so stale bytes cannot reach a different run.
use std::{future::Future, sync::Arc};
use tokio::{sync::oneshot, task::JoinHandle};

pub(super) const ONLINE_LOOKAHEAD: f64 = 12.0;

pub(super) struct Lookahead<K, A = ()> {
    pub key: K,
    phase: Phase<A>,
}

enum Phase<A> {
    Loading(Load),
    Attempted,
    Playing(A),
}

struct Load {
    task: JoinHandle<()>,
    bytes: oneshot::Receiver<Option<Arc<[u8]>>>,
}
impl Drop for Load {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<K, A> Lookahead<K, A> {
    pub fn new(key: K, load: impl Future<Output = Option<Arc<[u8]>>> + Send + 'static) -> Self {
        let (tx, bytes) = oneshot::channel();
        Self {
            key,
            phase: Phase::Loading(Load {
                task: tokio::spawn(async move {
                    let _ = tx.send(load.await);
                }),
                bytes,
            }),
        }
    }

    pub fn take_bytes(&mut self) -> Option<Arc<[u8]>> {
        let Phase::Loading(load) = &mut self.phase else {
            return None;
        };
        let bytes = match load.bytes.try_recv() {
            Ok(bytes) => bytes,
            Err(oneshot::error::TryRecvError::Closed) => None,
            Err(oneshot::error::TryRecvError::Empty) => return None,
        };
        self.phase = Phase::Attempted;
        bytes
    }

    pub fn started(&mut self, playback: A) {
        self.phase = Phase::Playing(playback);
    }

    pub fn into_playing(self) -> Option<A> {
        match self.phase {
            Phase::Playing(playback) => Some(playback),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[tokio::test]
    async fn completed_loads_are_single_use_including_failures() {
        let mut failed = Lookahead::<_, ()>::new(1, async { None });
        let mut ready = Lookahead::<_, ()>::new(2, async { Some(Arc::from([4])) });
        tokio::task::yield_now().await;
        assert!(failed.take_bytes().is_none());
        assert!(matches!(failed.phase, Phase::Attempted));
        assert_eq!(ready.take_bytes().as_deref(), Some(&[4][..]));
        assert!(ready.take_bytes().is_none());
        assert!(ready.into_playing().is_none());
    }

    #[tokio::test]
    async fn replacing_pending_load_cancels_its_task() {
        let (held, dropped) = oneshot::channel::<()>();
        let mut next = Lookahead::<_, ()>::new(1, async move {
            let _held = held;
            pending().await
        });
        assert!(next.take_bytes().is_none());
        drop(next);
        assert!(dropped.await.is_err());
    }
}

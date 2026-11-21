//! One speculative read per next occurrence. A failed read/decoder never
//! changes playback; ordinary EOF remains the fallback. Dropping a lookahead
//! also drops its reply channel, so stale media cannot reach a different run.
use crate::audio_input::AudioInput;
use std::future::Future;
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

pub(super) struct Load {
    task: JoinHandle<()>,
    source: oneshot::Receiver<Option<AudioInput>>,
}
impl Load {
    pub async fn ready(mut self) -> Option<AudioInput> {
        (&mut self.source).await.ok().flatten()
    }
}

pub(super) enum Landing<A> {
    Loading(Load),
    Ready(AudioInput),
    Playing(A),
    Unavailable,
}
impl Drop for Load {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<K, A> Lookahead<K, A> {
    pub fn new(key: K, load: impl Future<Output = Option<AudioInput>> + Send + 'static) -> Self {
        let (tx, source) = oneshot::channel();
        Self {
            key,
            phase: Phase::Loading(Load {
                task: tokio::spawn(async move {
                    let _ = tx.send(load.await);
                }),
                source,
            }),
        }
    }

    pub fn take_source(&mut self) -> Option<AudioInput> {
        let Phase::Loading(load) = &mut self.phase else {
            return None;
        };
        let source = match load.source.try_recv() {
            Ok(source) => source,
            Err(oneshot::error::TryRecvError::Closed) => None,
            Err(oneshot::error::TryRecvError::Empty) => return None,
        };
        self.phase = Phase::Attempted;
        source
    }

    pub fn started(&mut self, playback: A) {
        self.phase = Phase::Playing(playback);
    }

    pub fn landing(mut self) -> Landing<A> {
        if let Some(source) = self.take_source() {
            return Landing::Ready(source);
        }
        match self.phase {
            Phase::Loading(load) => Landing::Loading(load),
            Phase::Playing(playback) => Landing::Playing(playback),
            Phase::Attempted => Landing::Unavailable,
        }
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
    use std::{future::pending, io::Read, sync::Arc};

    #[tokio::test]
    async fn completed_loads_are_single_use_including_failures() {
        let mut failed = Lookahead::<_, ()>::new(1, async { None });
        let mut ready =
            Lookahead::<_, ()>::new(2, async { Some(AudioInput::from(Arc::from([4]))) });
        tokio::task::yield_now().await;
        assert!(failed.take_source().is_none());
        assert!(matches!(failed.phase, Phase::Attempted));
        let mut byte = [0];
        ready
            .take_source()
            .unwrap()
            .reader()
            .read_exact(&mut byte)
            .unwrap();
        assert_eq!(byte, [4]);
        assert!(ready.take_source().is_none());
        assert!(ready.into_playing().is_none());
    }

    #[tokio::test]
    async fn canonical_landing_reuses_in_flight_preview() {
        let (send, receive) = oneshot::channel();
        let load = Lookahead::<_, ()>::new(1, async { receive.await.ok() });
        let Landing::Loading(in_flight) = load.landing() else {
            panic!("load still pending")
        };
        assert!(send.send(AudioInput::from(Arc::from([9]))).is_ok());
        let mut byte = [0];
        in_flight
            .ready()
            .await
            .unwrap()
            .reader()
            .read_exact(&mut byte)
            .unwrap();
        assert_eq!(byte, [9]);
    }

    #[tokio::test]
    async fn replacing_pending_load_cancels_its_task() {
        let (held, dropped) = oneshot::channel::<()>();
        let mut next = Lookahead::<_, ()>::new(1, async move {
            let _held = held;
            pending().await
        });
        assert!(next.take_source().is_none());
        drop(next);
        assert!(dropped.await.is_err());
    }
}

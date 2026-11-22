//! Latest explicit mode choice. Independent of the bounded playback command
//! mailbox: a superseded Go online cannot reopen the socket after Work offline.
use crate::{desk::Access, playlist_pin_store::PUBLICATION_GATE};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::watch;

/// Permission for anonymous update reads, not an account or session state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateNetwork {
    Pending,
    Offline,
    Online,
    Closed,
}

#[derive(Default)]
struct Choice {
    epoch: u64,
    offline: bool,
    serial: u64,
    unread: bool,
}
#[derive(Clone)]
pub struct ModeControl {
    choice: Arc<Mutex<Choice>>,
    changed: watch::Sender<u64>,
    updates: watch::Sender<UpdateNetwork>,
}
impl Default for ModeControl {
    fn default() -> Self {
        Self {
            choice: Arc::default(),
            changed: watch::channel(0).0,
            updates: watch::channel(UpdateNetwork::Pending).0,
        }
    }
}
impl ModeControl {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    pub(crate) fn update_network(&self) -> watch::Receiver<UpdateNetwork> {
        self.updates.subscribe()
    }

    pub(crate) fn resolve_update_startup(&self, epoch: u64, current: &AtomicU64) {
        self.publish_update_network(UpdateNetwork::Online, |state| {
            state == UpdateNetwork::Pending && current.load(Ordering::SeqCst) == epoch
        });
    }

    pub(crate) fn allow_updates(&self, epoch: u64, current: &AtomicU64) {
        self.publish_update_network(UpdateNetwork::Online, |_| {
            current.load(Ordering::SeqCst) == epoch
        });
    }

    pub(crate) fn block_updates(&self) {
        self.publish_update_network(UpdateNetwork::Offline, |_| true);
    }

    pub(crate) fn close_updates(&self) {
        self.publish_update_network(UpdateNetwork::Closed, |_| true);
    }

    fn publish_update_network(
        &self,
        next: UpdateNetwork,
        permitted: impl FnOnce(UpdateNetwork) -> bool,
    ) {
        // Serialize authorization with account-epoch changes and the updater's
        // final rename, but never hold this across network reads or fsync.
        let _publication = PUBLICATION_GATE.lock().unwrap();
        self.updates.send_if_modified(|state| {
            if *state == next || *state == UpdateNetwork::Closed || !permitted(*state) {
                return false;
            }
            *state = next;
            true
        });
    }

    pub fn request_current(
        &self,
        access: &tokio::sync::watch::Receiver<Option<Access>>,
        epoch: u64,
        offline: bool,
    ) -> bool {
        self.request_inner(offline, || {
            access
                .borrow()
                .as_ref()
                .filter(|a| a.epoch == epoch)
                .cloned()
        })
    }

    // Read the current access *under* the mode lock. A reconnect may have
    // replaced an offline-only session just before this request acquired it.
    fn request_inner(&self, offline: bool, access: impl FnOnce() -> Option<Access>) -> bool {
        let mut s = self.choice.lock().unwrap_or_else(|e| e.into_inner());
        let Some(access) = access() else { return false };
        if offline {
            self.block_updates();
            access.session.set_offline(true);
        }
        *s = Choice {
            epoch: access.epoch,
            offline,
            serial: s.serial.wrapping_add(1),
            unread: true,
        };
        self.changed.send_replace(s.serial);
        true
    }

    #[cfg(test)]
    pub(crate) fn request(&self, access: &Access, offline: bool) {
        self.request_inner(offline, || Some(access.clone()));
    }
    /// Serialize opening network access with a possible concurrent close. The
    /// callback only performs local state transitions/tasks, never network I/O.
    pub fn apply(&self, epoch: u64, callback: impl FnOnce(bool, u64)) {
        let mut s = self.choice.lock().unwrap_or_else(|e| e.into_inner());
        if s.unread && s.epoch <= epoch {
            s.unread = false;
            if s.epoch == epoch {
                callback(s.offline, s.serial);
            }
        }
    }
    pub async fn cancelled(&self, serial: u64) {
        let mut rx = self.changed.subscribe();
        let _ = rx.wait_for(|current| *current != serial).await;
    }
    pub(crate) fn can_probe(&self, epoch: u64, serial: u64) -> bool {
        self.if_current_online(epoch, serial, || {})
    }

    pub fn if_current_online(&self, epoch: u64, serial: u64, callback: impl FnOnce()) -> bool {
        let s = self.choice.lock().unwrap_or_else(|e| e.into_inner());
        if !s.offline && s.epoch == epoch && s.serial == serial {
            callback();
            true
        } else {
            false
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin};

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn superseded_auth_cannot_open_anonymous_networking() {
        let epoch = AtomicU64::new(2);
        let mode = ModeControl::default();
        let network = mode.update_network();
        mode.resolve_update_startup(1, &epoch);
        assert!(matches!(*network.borrow(), UpdateNetwork::Pending));
        mode.block_updates();
        mode.allow_updates(1, &epoch);
        assert!(matches!(*network.borrow(), UpdateNetwork::Offline));
        mode.allow_updates(2, &epoch);
        assert!(matches!(*network.borrow(), UpdateNetwork::Online));
        let next = crate::advance_account_epoch(&epoch);
        assert!(matches!(*network.borrow(), UpdateNetwork::Online));
        mode.close_updates();
        mode.allow_updates(next, &epoch);
        assert!(matches!(*network.borrow(), UpdateNetwork::Closed));
    }

    #[test]
    fn a_late_work_offline_click_fences_the_replacement_session() {
        let api = Arc::new(Api::new(Origin::parse("https://music.invalid").unwrap()).unwrap());
        let local = api.test_session();
        local.set_offline(true);
        let old = Access {
            api: api.clone(),
            session: local,
            epoch: 4,
            offline: true,
            personal: Default::default(),
        };
        let (tx, rx) = tokio::sync::watch::channel(Some(old.clone()));
        let mode = ModeControl::default();
        mode.request(&old, false);
        let online = api.test_session();
        assert!(mode.if_current_online(4, 1, || {
            tx.send_replace(Some(Access {
                session: online.clone(),
                offline: false,
                ..old
            }));
        }));
        assert!(mode.request_current(&rx, 4, true));
        assert!(online.is_offline());
    }

    #[test]
    fn queued_go_online_cannot_overrule_later_offline() {
        let api = Arc::new(Api::new(Origin::parse("https://music.invalid").unwrap()).unwrap());
        let session = api.test_session();
        let a = Access {
            api,
            session,
            epoch: 4,
            offline: true,
            personal: Default::default(),
        };
        let m = ModeControl::default();
        m.request(&a, false);
        m.request(&a, true);
        let mut choices = vec![];
        m.apply(3, |s, _| choices.push(s));
        m.apply(4, |s, _| choices.push(s));
        m.apply(4, |s, _| choices.push(s));
        assert!(!m.if_current_online(4, 1, || panic!("old probe reopened the socket")));
        assert_eq!(choices, [true]);
        assert!(a.session.is_offline());
        m.request(&a, false);
        m.apply(5, |_, _| panic!("stale mode reached new account"));
    }
}

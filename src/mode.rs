//! Latest explicit mode choice. Independent of the bounded playback command
//! mailbox: a superseded Go online cannot reopen the socket after Work offline.
use crate::desk::Access;
use std::sync::{Arc, Mutex};

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
    changed: tokio::sync::watch::Sender<u64>,
}
impl Default for ModeControl {
    fn default() -> Self {
        Self {
            choice: Arc::default(),
            changed: tokio::sync::watch::channel(0).0,
        }
    }
}
impl ModeControl {
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

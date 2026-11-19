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
    pub fn request(&self, access: &Access, offline: bool) {
        let mut s = self.choice.lock().unwrap_or_else(|e| e.into_inner());
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

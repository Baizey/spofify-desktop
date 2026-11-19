//! Session-owned read models, independent of the wire protocol. Today collection
//! changes invalidate snapshots backed by personal REST reads; the playback room
//! uses a separate live replica. No cached objects survive a change of identity.
use crate::pages::{Page, Route};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

const MAX_PAGES: usize = 24;
const MAX_ROWS: usize = 1400;
const MAX_HISTORY: usize = 40;
struct Entry {
    route: Route,
    value: Arc<Page>,
    received: Instant,
    dirty: bool,
}
#[derive(Default)]
pub struct Replica(VecDeque<Entry>);
impl Replica {
    pub fn get(&mut self, route: &Route) -> Option<(Arc<Page>, bool)> {
        let i = self.0.iter().position(|e| &e.route == route)?;
        let e = self.0.remove(i)?;
        // Revalidate on a later visit, never on a periodic refresh timer.
        let ttl = match route {
            Route::History => 60,
            Route::Search(_) => 90,
            Route::Playlist(_) | Route::Playlists | Route::Picker(_) => 180,
            _ => 600,
        };
        let fresh = !e.dirty && e.received.elapsed() < Duration::from_secs(ttl);
        let page = e.value.clone();
        self.0.push_back(e);
        Some((page, fresh))
    }
    pub fn put(&mut self, route: Route, page: Arc<Page>) {
        self.0.retain(|e| e.route != route);
        self.0.push_back(Entry {
            route,
            value: page,
            received: Instant::now(),
            dirty: false,
        });
        while self.0.len() > MAX_PAGES
            || self.0.iter().map(|e| e.value.rows.len()).sum::<usize>() > MAX_ROWS
        {
            self.0.pop_front();
        }
    }
    pub fn recent_changed(&mut self) {
        for e in &mut self.0 {
            if matches!(e.route, Route::History) {
                e.dirty = true;
            }
        }
    }
    pub fn collection_changed(&mut self) {
        for e in &mut self.0 {
            if matches!(
                e.route,
                Route::Playlist(_) | Route::Playlists | Route::Picker(_)
            ) {
                e.dirty = true;
            }
        }
    }
    pub fn saved_changed(&mut self) {
        for e in &mut self.0 {
            if matches!(e.route, Route::Offline | Route::SavedCollection(_)) {
                e.dirty = true;
            }
        }
    }
}
#[derive(Default)]
pub struct Trail(VecDeque<Route>);
impl Trail {
    pub fn visit(&mut self, old: &Route, new: &Route) {
        if old == new || matches!((old, new), (Route::Search(_), Route::Search(_))) {
            return;
        }
        if self.0.back() == Some(new) {
            self.0.pop_back(); // Directly returning to the previous screen.
        } else if !matches!(old, Route::Picker(_))
            && !matches!(old, Route::Search(q) if q.is_empty())
            && self.0.back() != Some(old)
        {
            if self.0.len() >= MAX_HISTORY {
                self.0.pop_front();
            }
            self.0.push_back(old.clone());
        }
    }
    /// When a mutation lands on the previous screen, don't leave it as Back.
    pub fn landed(&mut self, route: &Route) {
        if self.0.back() == Some(route) {
            self.0.pop_back();
        }
    }
    pub fn pop(&mut self) -> Option<Route> {
        self.0.pop_back()
    }
    pub fn can_back(&self) -> bool {
        !self.0.is_empty()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_read_models_stay_available() {
        let mut r = Replica::default();
        let p = Arc::new(Page {
            title: "Last good".into(),
            ..Default::default()
        });
        r.put(Route::History, p.clone());
        r.put(Route::Playlists, p);
        r.recent_changed();
        let (old, fresh) = r.get(&Route::History).unwrap();
        assert_eq!(old.title, "Last good");
        assert!(!fresh);
        assert!(r.get(&Route::Playlists).unwrap().1);
        r.collection_changed();
        assert!(!r.get(&Route::Playlists).unwrap().1);
    }
    #[test]
    fn bounded_trail_and_search_edits() {
        let mut t = Trail::default();
        let blank = Route::Search(String::new());
        let first = Route::Search("a".into());
        let query = Route::Search("ab".into());
        t.visit(&Route::History, &blank);
        t.visit(&blank, &first);
        t.visit(&first, &query);
        t.visit(&query, &Route::Track("track".into()));
        assert_eq!(t.pop(), Some(query));
        assert_eq!(t.pop(), Some(Route::History));
        for i in 0..100 {
            t.visit(&Route::Album(i.to_string()), &Route::History);
        }
        assert_eq!(t.0.len(), MAX_HISTORY);
    }
    #[test]
    fn direct_returns_and_transient_screens_do_not_duplicate_back() {
        let mut t = Trail::default();
        t.visit(&Route::History, &Route::Playlists);
        t.visit(&Route::Playlists, &Route::History);
        assert!(!t.can_back());

        t.visit(&Route::History, &Route::Search(String::new()));
        t.visit(&Route::Search(String::new()), &Route::Playlists);
        assert_eq!(t.pop(), Some(Route::History));
        assert!(!t.can_back());

        let playlist = Route::Playlist("list".into());
        let picker = Route::Picker("track".into());
        t.visit(&Route::Playlists, &playlist);
        t.visit(&playlist, &picker);
        t.landed(&playlist); // Add succeeds and returns to its destination.
        assert_eq!(t.pop(), Some(Route::Playlists));
        assert!(!t.can_back());

        t.visit(&Route::History, &Route::Offline);
        t.visit(&Route::Offline, &Route::Appearance);
        t.visit(&Route::Appearance, &Route::Offline);
        assert_eq!(t.pop(), Some(Route::History));
        assert!(!t.can_back());
    }
    #[test]
    fn cache_evicts_least_recently_used() {
        let mut r = Replica::default();
        for i in 0..MAX_PAGES {
            r.put(Route::Album(i.to_string()), Arc::default());
        }
        r.get(&Route::Album("0".into()));
        r.put(Route::History, Arc::default());
        assert!(r.get(&Route::Album("0".into())).is_some());
        assert!(r.get(&Route::Album("1".into())).is_none());
    }
}

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
            Route::Home | Route::History => 60,
            Route::Search(_) => 90,
            Route::Playlist(_) | Route::Playlists | Route::Liked | Route::Picker(_) => 180,
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
            if matches!(e.route, Route::Home | Route::History) {
                e.dirty = true;
            }
        }
    }
    pub fn collection_changed(&mut self) {
        for e in &mut self.0 {
            if matches!(
                e.route,
                Route::Home
                    | Route::Liked
                    | Route::Playlist(_)
                    | Route::Playlists
                    | Route::Picker(_)
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
        if old != new
            && !matches!((old, new), (Route::Search(_), Route::Search(_)))
            && self.0.back() != Some(old)
        {
            if self.0.len() >= MAX_HISTORY {
                self.0.pop_front();
            }
            self.0.push_back(old.clone());
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
        r.put(Route::Home, p.clone());
        r.put(Route::Liked, p);
        r.recent_changed();
        let (old, fresh) = r.get(&Route::Home).unwrap();
        assert_eq!(old.title, "Last good");
        assert!(!fresh);
        assert!(r.get(&Route::Liked).unwrap().1);
        r.collection_changed();
        assert!(!r.get(&Route::Liked).unwrap().1);
    }
    #[test]
    fn bounded_trail_and_search_edits() {
        let mut t = Trail::default();
        t.visit(&Route::Home, &Route::Search("a".into()));
        t.visit(&Route::Search("a".into()), &Route::Search("ab".into()));
        assert_eq!(t.pop(), Some(Route::Home));
        for i in 0..100 {
            t.visit(&Route::Album(i.to_string()), &Route::Home);
        }
        assert_eq!(t.0.len(), MAX_HISTORY);
    }
    #[test]
    fn cache_evicts_least_recently_used() {
        let mut r = Replica::default();
        for i in 0..MAX_PAGES {
            r.put(Route::Album(i.to_string()), Arc::default());
        }
        r.get(&Route::Album("0".into()));
        r.put(Route::Home, Arc::default());
        assert!(r.get(&Route::Album("0".into())).is_some());
        assert!(r.get(&Route::Album("1".into())).is_none());
    }
}

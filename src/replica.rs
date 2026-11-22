//! Session-owned presentation cache and navigation trail. The account's v3
//! personal playlist and Recent authority lives in `crate::personal`; caching
//! a rendered page here does not establish or repair that account state.
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
        let ttl = if matches!(route, Route::Search(_)) {
            90
        } else {
            600
        };
        let fresh = !e.dirty && e.received.elapsed() < Duration::from_secs(ttl);
        let page = e.value.clone();
        self.0.push_back(e);
        Some((page, fresh))
    }
    pub fn put(&mut self, route: Route, page: Arc<Page>) {
        self.0.retain(|e| e.route != route);
        // A large current Saved view is usable without a retained cache copy.
        // Don't evict all other views trying to make room for one that can't fit.
        if page.rows.len() > MAX_ROWS {
            return;
        }
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
    pub fn catalog_pages(&self) -> Vec<(Route, Arc<Page>)> {
        self.0
            .iter()
            .filter(|e| e.route.catalog_continuity() || matches!(e.route, Route::Playlist(_)))
            .map(|e| (e.route.clone(), e.value.clone()))
            .collect()
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
pub struct Trail(VecDeque<(u64, Route)>, u64);
impl Trail {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Follow a link from the actual current route. A rail selection clears
    /// the trail instead of calling this.
    pub fn visit(&mut self, old: &Route, new: &Route) {
        self.visit_with_source(old, new, false);
    }

    /// A title launch preserves its source for exact control focus on Back,
    /// even when the destination album is already in the trail.
    pub fn visit_from_title(&mut self, old: &Route, new: &Route) {
        self.visit_with_source(old, new, true);
    }

    fn visit_with_source(&mut self, old: &Route, new: &Route, preserve_title_source: bool) {
        if old == new || matches!((old, new), (Route::Search(_), Route::Search(_))) {
            return;
        }
        let empty_search = matches!(old, Route::Search(q) if q.is_empty());
        if self.top_route() == Some(new) && !preserve_title_source {
            self.0.pop_back(); // Directly returning to the previous screen.
        } else if !matches!(old, Route::Picker(_))
            && (!empty_search || preserve_title_source)
            && self.top_route() != Some(old)
        {
            if self.0.len() >= MAX_HISTORY {
                self.0.pop_front();
            }
            self.1 = self.1.wrapping_add(1);
            self.0.push_back((self.1, old.clone()));
        }
    }
    /// When a mutation lands on the previous screen, don't leave it as Back.
    pub fn landed(&mut self, route: &Route) {
        if self.top_route() == Some(route) {
            self.0.pop_back();
        }
    }
    pub fn pop(&mut self) -> Option<Route> {
        self.0.pop_back().map(|(_, route)| route)
    }
    fn top_route(&self) -> Option<&Route> {
        self.0.back().map(|(_, route)| route)
    }
    pub fn top_id(&self) -> Option<u64> {
        self.0.back().map(|(id, _)| *id)
    }
    pub fn contains_id(&self, id: u64) -> bool {
        self.0.iter().any(|(entry, _)| *entry == id)
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
        r.put(Route::Offline, p.clone());
        r.put(Route::Album("a".into()), p);
        r.saved_changed();
        let (old, fresh) = r.get(&Route::Offline).unwrap();
        assert_eq!(old.title, "Last good");
        assert!(!fresh);
        assert!(r.get(&Route::Album("a".into())).unwrap().1);
    }
    #[test]
    fn title_navigation_retains_source_even_if_the_album_was_the_previous_route() {
        let album = Route::Album("release".into());
        for source in [
            Route::Search(String::new()),
            Route::Search("song".into()),
            Route::Playlist("list".into()),
        ] {
            let mut trail = Trail::default();
            trail.visit(&album, &source);
            trail.visit(&source, &album);
            assert!(!trail.can_back());

            trail.visit(&album, &source);
            trail.visit_from_title(&source, &album);
            assert_eq!(trail.pop(), Some(source));
            assert_eq!(trail.pop(), Some(album.clone()));
        }
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
        t.visit(&query, &Route::Album("album".into()));
        assert_eq!(t.pop(), Some(query));
        assert_eq!(t.pop(), Some(Route::History));
        for i in 0..100 {
            t.visit(&Route::Album(i.to_string()), &Route::Artist("a".into()));
        }
        assert_eq!(t.0.len(), MAX_HISTORY);
    }
    #[test]
    fn direct_returns_and_transient_screens_do_not_duplicate_back() {
        let mut t = Trail::default();
        let album = Route::Album("a".into());
        let artist = Route::Artist("ar".into());
        t.visit(&album, &artist);
        t.visit(&artist, &album);
        assert!(!t.can_back());

        let blank = Route::Search(String::new());
        t.visit(&album, &blank);
        t.visit(&blank, &artist);
        assert_eq!(t.pop(), Some(album));
        assert!(!t.can_back());

        let playlist = Route::Playlist("list".into());
        let picker = Route::Picker("track".into());
        t.visit(&Route::Playlists, &playlist);
        t.visit(&playlist, &picker);
        t.landed(&playlist); // Add succeeds and returns to its destination.
        assert_eq!(t.pop(), Some(Route::Playlists));
        assert!(!t.can_back());
    }
    #[test]
    fn an_uncacheable_saved_view_does_not_evict_other_views() {
        let mut r = Replica::default();
        r.put(Route::Offline, Arc::default());
        r.put(Route::History, Arc::default());
        r.put(
            Route::Offline,
            Arc::new(Page {
                rows: vec![Default::default(); MAX_ROWS + 1],
                ..Default::default()
            }),
        );
        assert!(r.get(&Route::Offline).is_none());
        assert!(r.get(&Route::History).is_some());
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

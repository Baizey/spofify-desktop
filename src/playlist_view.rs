//! The small set of occurrence indices currently mounted in the catalog scroll surface.
use crate::AppWindow;
use slint::{Model, ModelNotify, ModelRc, ModelTracker};
use std::{cell::Cell, rc::Rc};

#[derive(Default)]
struct VisibleRows {
    bounds: Cell<(i32, i32)>,
    notify: ModelNotify,
}

impl VisibleRows {
    fn show(&self, start: i32, end: i32) {
        let (mut first, mut last) = self.bounds.get();
        if (first, last) == (start, end) {
            return;
        }
        if first == last || start == end || last <= start || end <= first {
            self.bounds.set((start, end));
            self.notify.reset();
            return;
        }

        // Splice only the edges: an occurrence remaining in view keeps its
        // delegate, focus, and gesture identity as the viewport moves.
        if start > first {
            self.bounds.set((start, last));
            self.notify.row_removed(0, (start - first) as usize);
            first = start;
        }
        if end < last {
            self.bounds.set((first, end));
            self.notify
                .row_removed((end - first) as usize, (last - end) as usize);
            last = end;
        }
        if start < first {
            self.bounds.set((start, last));
            self.notify.row_added(0, (first - start) as usize);
            first = start;
        }
        if end > last {
            self.bounds.set((first, end));
            self.notify
                .row_added((last - first) as usize, (end - last) as usize);
        }
    }
}

impl Model for VisibleRows {
    type Data = i32;

    fn row_count(&self) -> usize {
        let (start, end) = self.bounds.get();
        (end - start) as usize
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        (row < self.row_count()).then(|| self.bounds.get().0 + row as i32)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) fn install(ui: &AppWindow) {
    let rows = Rc::new(VisibleRows::default());
    ui.set_visible_playlist_indices(ModelRc::from(rows.clone()));
    ui.on_playlist_viewport_changed(move |start, end| rows.show(start, end));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_indices_are_absolute_through_overlapping_and_disjoint_moves() {
        let rows = VisibleRows::default();
        for (start, end) in [(0, 12), (8, 16), (2, 14), (6, 10), (1151, 1168), (0, 0)] {
            rows.show(start, end);
            assert!(rows.iter().eq(start..end));
            assert_eq!(rows.row_data(rows.row_count()), None);
        }
    }
}

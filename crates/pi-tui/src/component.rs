//! The JS `Component` bridge protocol (VED-307, ADR 0002).
//!
//! Extensions render custom components as **arrays of terminal lines for a
//! given width** (pi's `Component#render(width): string[]`). The native
//! renderer never interprets their internals; it asks for lines and composites
//! them into a [`Buffer`]. This module defines that transport-free contract and
//! the cache that keeps it cheap:
//!
//! * [`ComponentSource`] is the one mechanism: given a width, return lines.
//! * [`ComponentCache`] renders only when the source is invalidated or the width
//!   changes, so an unchanged component is never re-run per frame.
//! * [`composite`] writes cached lines into a buffer region.
//!
//! The QuickJS adapter lives in `pi-plugins` and implements
//! [`ComponentSource`], so this crate stays free of any JS dependency.

use crate::buffer::{Buffer, Style};

/// Something that can render terminal lines at a width — the generic bridge
/// between a native caller and a JS component. `invalidate` marks cached output
/// stale (pi's `Component#invalidate()` + `tui.requestRender()`).
pub trait ComponentSource {
    /// Render the component as lines, one per terminal row, each fitting within
    /// `width` visible columns.
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Drop any cached output; the next `render` must recompute.
    fn invalidate(&mut self);
}

/// Cached lines for one component at one width.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CachedLines {
    lines: Vec<String>,
    width: Option<usize>,
    valid: bool,
}

/// Wraps a [`ComponentSource`] and only calls it when the width changes or the
/// source was invalidated. This is the "incremental / invalidate-on-change"
/// requirement: an unchanged component is not re-rendered each frame.
pub struct ComponentCache<S: ComponentSource> {
    source: S,
    cache: CachedLines,
    /// Count of actual `source.render` calls, for tests and diagnostics.
    renders: usize,
}

impl<S: ComponentSource> ComponentCache<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            cache: CachedLines::default(),
            renders: 0,
        }
    }

    /// The component's lines at `width`, rendering only when needed.
    pub fn lines(&mut self, width: usize) -> &[String] {
        if !self.cache.valid || self.cache.width != Some(width) {
            self.cache.lines = self.source.render(width);
            self.cache.width = Some(width);
            self.cache.valid = true;
            self.renders += 1;
        }
        &self.cache.lines
    }

    /// Invalidate the cache; the next `lines` call re-renders.
    pub fn invalidate(&mut self) {
        self.cache.valid = false;
        self.source.invalidate();
    }

    /// How many times the underlying source has actually rendered.
    pub fn render_count(&self) -> usize {
        self.renders
    }
}

/// Write `lines` into `buffer` starting at `(x, y)`, clipped to the buffer.
/// Returns the number of rows written. Each line is placed on its own row; a
/// line wider than the remaining width is clipped by [`Buffer::put_str`].
pub fn composite(buffer: &mut Buffer, x: usize, y: usize, lines: &[String], style: Style) -> usize {
    let mut written = 0;
    for (offset, line) in lines.iter().enumerate() {
        let row = y + offset;
        if row >= buffer.height() {
            break;
        }
        buffer.put_str(x, row, line, style);
        written += 1;
    }
    written
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct CountingSource {
        renders: Rc<RefCell<usize>>,
    }

    impl ComponentSource for CountingSource {
        fn render(&mut self, width: usize) -> Vec<String> {
            *self.renders.borrow_mut() += 1;
            vec![format!("w={width}")]
        }
        fn invalidate(&mut self) {}
    }

    #[test]
    fn cache_renders_once_per_width() {
        let renders = Rc::new(RefCell::new(0));
        let mut cache = ComponentCache::new(CountingSource {
            renders: renders.clone(),
        });
        assert_eq!(cache.lines(10), &["w=10".to_string()]);
        assert_eq!(cache.lines(10), &["w=10".to_string()]);
        assert_eq!(*renders.borrow(), 1, "same width should not re-render");
        assert_eq!(cache.lines(20), &["w=20".to_string()]);
        assert_eq!(*renders.borrow(), 2);
    }

    #[test]
    fn invalidate_forces_a_render() {
        let renders = Rc::new(RefCell::new(0));
        let mut cache = ComponentCache::new(CountingSource {
            renders: renders.clone(),
        });
        cache.lines(10);
        cache.invalidate();
        cache.lines(10);
        assert_eq!(*renders.borrow(), 2);
    }
}

//! Reuse captures from unchanged children of a syntax layer.
//! Entries own their ranges and metadata; they never borrow query matches.
use crate::{Range, TREE_SITTER_MATCH_LIMIT};
use hashbrown::HashMap;
use ropey::RopeSlice;
use tree_sitter::{InactiveQueryCursor, InputEdit, MatchedNodeIdx, Node, Query, QueryMatch, Tree};

/// Capture data that can move with an unchanged subtree.
pub(crate) trait Event: Clone {
    /// Move every stored byte range and assign this child's stream id.
    /// Stream 0 is reserved for the original full query.
    fn shift(&mut self, offset: i64, stream: u32);
    /// `false` sends the caller back to the original full query.
    fn cacheable(&self) -> bool;
}
#[derive(Debug, Clone)]
struct Child<T> {
    kind: u16,
    range: Range,
    events: Vec<T>,
}
/// `None` from `collect` requests full execution; `Some([])` is a complete empty result.
#[derive(Debug, Clone)]
pub(crate) struct SubtreeQueryCache<T> {
    query: u64,
    // Results are reusable only across consecutive updates.
    generation: u64,
    ranges: Vec<tree_sitter::Range>,
    // Retain node identities until comparing them with the next tree.
    tree: Option<Tree>,
    children: HashMap<usize, Child<T>>,
}
impl<T> Default for SubtreeQueryCache<T> {
    fn default() -> Self {
        Self {
            query: 0,
            generation: 0,
            ranges: Vec::new(),
            tree: None,
            children: HashMap::new(),
        }
    }
}
const MAX_EVENTS: usize = 65_536;
const MAX_CHILDREN: u32 = 4_096;

impl<T: Event> SubtreeQueryCache<T> {
    /// `edits` use byte positions in the previous source. The caller must run
    /// the original query if eligibility checks or capture limits fail.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn collect(
        &mut self,
        query: &Query,
        tree: &Tree,
        source: RopeSlice<'_>,
        edits: &[InputEdit],
        ranges: &[tree_sitter::Range],
        generation: u64,
        mut project: impl FnMut(QueryMatch<'_, '_>, MatchedNodeIdx) -> Option<T>,
    ) -> Option<Vec<T>> {
        #[cfg(all(test, feature = "fixtures"))]
        if FORCE_FULL.with(|flag| flag.get()) {
            return None;
        }
        let root = tree.root_node();
        if root.child_count() > MAX_CHILDREN || !query.can_partition_subtrees() {
            *self = Self::default();
            return None;
        }
        let mut shallow = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT);
        shallow.set_max_start_depth(0);
        let mut shallow = shallow.execute_query(query, &root, source);
        // Captures on the layer root can surround several children's captures.
        // Keep them on the full-query path to preserve their nesting and order.
        if shallow.next_matched_node().is_some() {
            *self = Self::default();
            return None;
        }
        let exceeded = shallow.did_exceed_match_limit();
        drop(shallow);
        if exceeded {
            *self = Self::default();
            return None;
        }
        // A skipped update may have edited a cached child. Do not reuse its old
        // coordinates, even if the current edit misses that child.
        if self.query != query.revision()
            || self.ranges != ranges
            || self.generation.checked_add(1) != Some(generation)
        {
            *self = Self::default();
        }
        let mut old = std::mem::take(&mut self.children);
        let mut children = HashMap::new();
        let mut result = Vec::new();
        for (stream, node) in root.children().enumerate() {
            let range = node.byte_range();
            if range.is_empty() {
                *self = Self::default();
                return None;
            }
            // A same-length text edit can keep a node's shape and identity.
            // Check the original edit ranges, including edits at child boundaries.
            let reused = old.remove(&node.id()).filter(|child| {
                child.kind == node.kind_id()
                    && child.range.len() == range.len()
                    && !edits.iter().any(|edit| {
                        edit.start_byte <= child.range.end && edit.old_end_byte >= child.range.start
                    })
            });
            let child = if let Some(mut child) = reused {
                let offset = i64::from(range.start) - i64::from(child.range.start);
                for event in &mut child.events {
                    event.shift(offset, stream as u32 + 1);
                }
                child.range = range;
                child
            } else {
                let Some(mut events) = collect_partition(
                    query,
                    &node,
                    source,
                    MAX_EVENTS - result.len(),
                    &mut project,
                ) else {
                    *self = Self::default();
                    return None;
                };
                for event in &mut events {
                    event.shift(0, stream as u32 + 1);
                }
                Child {
                    kind: node.kind_id(),
                    range,
                    events,
                }
            };
            if result.len().saturating_add(child.events.len()) > MAX_EVENTS {
                *self = Self::default();
                return None;
            }
            result.extend(child.events.iter().cloned());
            children.insert(node.id(), child);
        }
        self.children = children;
        self.tree = Some(tree.clone());
        self.query = query.revision();
        self.ranges = ranges.to_vec();
        self.generation = generation;
        Some(result)
    }
}
fn collect_partition<T: Event>(
    query: &Query,
    node: &Node<'_>,
    source: RopeSlice<'_>,
    budget: usize,
    project: &mut impl FnMut(QueryMatch<'_, '_>, MatchedNodeIdx) -> Option<T>,
) -> Option<Vec<T>> {
    let mut cursor = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT)
        .execute_query(query, node, source);
    let mut events = Vec::new();
    while let Some((mat, idx)) = cursor.next_matched_node() {
        if let Some(event) = project(mat, idx) {
            if events.len() == budget || !event.cacheable() {
                return None;
            }
            events.push(event);
        }
    }
    (!cursor.did_exceed_match_limit()).then_some(events)
}
// Cached ranges lie inside their child, so this offset keeps them within the
// same child's new position in the source.
pub(crate) fn shift_range(range: &mut Range, offset: i64) {
    range.start = (i64::from(range.start) + offset) as u32;
    range.end = (i64::from(range.end) + offset) as u32;
}

#[cfg(all(test, feature = "fixtures"))]
thread_local! { static FORCE_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(all(test, feature = "fixtures"))]
pub(crate) fn with_full_queries<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            FORCE_FULL.with(|flag| flag.set(self.0));
        }
    }
    let _restore = Restore(FORCE_FULL.with(|flag| flag.replace(true)));
    f()
}

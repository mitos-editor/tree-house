//! Reuse captures from unchanged children of a syntax layer.
//! Entries own their ranges and metadata; they never borrow query matches.
use crate::{Range, TREE_SITTER_MATCH_LIMIT};
use ropey::RopeSlice;
use std::mem::{size_of, size_of_val};
use tree_sitter::{InactiveQueryCursor, InputEdit, MatchedNodeIdx, Node, Query, QueryMatch, Tree};

/// Capture data that can move with an unchanged subtree.
pub(crate) trait Event: Clone {
    /// Move every stored byte range and assign this child's stream id.
    /// Stream 0 is reserved for the original full query.
    fn shift(&mut self, offset: i64, stream: u32);
    /// `false` sends the caller back to the original full query.
    fn cacheable(&self) -> bool;
    /// Heap allocations owned by this event, including unused capacity.
    fn heap_bytes(&self) -> usize;
}
#[derive(Debug)]
struct Child<T> {
    id: usize,
    kind: u16,
    range: Range,
    events: Box<[T]>,
}
/// `None` from `collect` requests full execution; `Some([])` is a complete empty result.
#[derive(Debug)]
pub(crate) struct SubtreeQueryCache<T> {
    query: u64,
    // Results are reusable only across consecutive updates.
    generation: u64,
    ranges: Box<[tree_sitter::Range]>,
    // Retain node identities until comparing them with the next tree.
    tree: Option<Tree>,
    // Sorted by node id for lookup. Query results are assembled in tree order.
    children: Box<[Child<T>]>,
}
impl<T> Default for SubtreeQueryCache<T> {
    fn default() -> Self {
        Self {
            query: 0,
            generation: 0,
            ranges: Box::default(),
            tree: None,
            children: Box::default(),
        }
    }
}
impl<T> Clone for SubtreeQueryCache<T> {
    fn clone(&self) -> Self {
        // Syntax snapshots retain their semantic state, but need no query cache.
        // A snapshot edited later rebuilds this optional state on its first update.
        Self::default()
    }
}
const MAX_EVENTS: usize = 65_536;
const MAX_CHILDREN: u32 = 4_096;
// Owned Rust buffers per Syntax. Shared native trees, allocator overhead
// and temporary query results are outside this retained-data budget.
const MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(crate) struct Budget(usize);
impl Default for Budget {
    fn default() -> Self {
        Self(MAX_BYTES)
    }
}
impl Budget {
    fn charge(&mut self, bytes: usize) -> Option<()> {
        self.0 = self.0.checked_sub(bytes)?;
        Some(())
    }
}

impl<T: Event> SubtreeQueryCache<T> {
    /// `edits` use byte positions in the previous source. The caller must run
    /// the original query if eligibility checks or capture limits fail.
    /// `project` receives the remaining heap budget for one event; it must reject
    /// oversized allocations before creating them. Its error requests full execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn collect(
        &mut self,
        query: &Query,
        tree: &Tree,
        source: RopeSlice<'_>,
        edits: &[InputEdit],
        ranges: &[tree_sitter::Range],
        generation: u64,
        budget: &mut Budget,
        mut project: impl FnMut(QueryMatch<'_, '_>, MatchedNodeIdx, usize) -> Result<Option<T>, ()>,
    ) -> Option<Vec<T>> {
        // Never retain a partly rebuilt or over-budget cache after a fallback.
        let cache = std::mem::take(self);
        let (next, result, remaining) = cache.collect_inner(
            query,
            tree,
            source,
            edits,
            ranges,
            generation,
            *budget,
            &mut project,
        )?;
        *self = next;
        *budget = remaining;
        Some(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_inner(
        mut self,
        query: &Query,
        tree: &Tree,
        source: RopeSlice<'_>,
        edits: &[InputEdit],
        ranges: &[tree_sitter::Range],
        generation: u64,
        mut budget: Budget,
        project: &mut impl FnMut(QueryMatch<'_, '_>, MatchedNodeIdx, usize) -> Result<Option<T>, ()>,
    ) -> Option<(Self, Vec<T>, Budget)> {
        #[cfg(all(test, feature = "fixtures"))]
        if FORCE_FULL.with(|flag| flag.get()) {
            return None;
        }
        let root = tree.root_node();
        if root.child_count() > MAX_CHILDREN || !query.can_partition_subtrees() {
            return None;
        }
        budget.charge(root.child_count() as usize * size_of::<Child<T>>())?;
        budget.charge(size_of_val(ranges))?;
        let mut shallow = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT);
        shallow.set_max_start_depth(0);
        let mut shallow = shallow.execute_query(query, &root, source);
        // Captures on the layer root can surround several children's captures.
        // Keep them on the full-query path to preserve their nesting and order.
        if shallow.next_matched_node().is_some() || shallow.did_exceed_match_limit() {
            return None;
        }
        drop(shallow);
        // A skipped update may have edited a cached child. Do not reuse its old
        // coordinates, even if the current edit misses that child.
        if self.query != query.revision()
            || self.ranges.as_ref() != ranges
            || self.generation.checked_add(1) != Some(generation)
        {
            self = Self::default();
        }
        let mut children = Vec::with_capacity(root.child_count() as usize);
        let mut result = Vec::new();
        for (stream, node) in root.children().enumerate() {
            let range = node.byte_range();
            if range.is_empty() {
                return None;
            }
            // A same-length text edit can keep a node's shape and identity.
            // Check the original edit ranges, including edits at child boundaries.
            let reused = self
                .children
                .binary_search_by_key(&node.id(), |child| child.id)
                .ok()
                .filter(|&index| {
                    let child = &self.children[index];
                    child.kind == node.kind_id()
                        && child.range.len() == range.len()
                        && !edits.iter().any(|edit| {
                            edit.start_byte <= child.range.end
                                && edit.old_end_byte >= child.range.start
                        })
                });
            let events = if let Some(index) = reused {
                let child = &mut self.children[index];
                budget.charge(size_of_val(child.events.as_ref()))?;
                for event in child.events.iter() {
                    budget.charge(event.heap_bytes())?;
                }
                let offset = i64::from(range.start) - i64::from(child.range.start);
                let mut events = std::mem::take(&mut child.events);
                for event in events.iter_mut() {
                    event.shift(offset, stream as u32 + 1);
                }
                events
            } else {
                let mut events = collect_partition(
                    query,
                    &node,
                    source,
                    MAX_EVENTS - result.len(),
                    &mut budget,
                    project,
                )?;
                for event in events.iter_mut() {
                    event.shift(0, stream as u32 + 1);
                }
                events
            };
            if result.len().saturating_add(events.len()) > MAX_EVENTS {
                return None;
            }
            result.extend(events.iter().cloned());
            children.push(Child {
                id: node.id(),
                kind: node.kind_id(),
                range,
                events,
            });
        }
        children.sort_unstable_by_key(|child| child.id);
        self.children = children.into_boxed_slice();
        self.tree = Some(tree.clone());
        self.query = query.revision();
        self.ranges = ranges.into();
        self.generation = generation;
        Some((self, result, budget))
    }
}
fn collect_partition<T: Event>(
    query: &Query,
    node: &Node<'_>,
    source: RopeSlice<'_>,
    event_limit: usize,
    budget: &mut Budget,
    project: &mut impl FnMut(QueryMatch<'_, '_>, MatchedNodeIdx, usize) -> Result<Option<T>, ()>,
) -> Option<Box<[T]>> {
    let mut cursor = InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT)
        .execute_query(query, node, source);
    let mut events = Vec::new();
    while let Some((mat, idx)) = cursor.next_matched_node() {
        let heap_budget = budget.0.checked_sub(size_of::<T>())?;
        if let Some(event) = project(mat, idx, heap_budget).ok()? {
            if events.len() == event_limit || !event.cacheable() {
                return None;
            }
            budget.charge(size_of::<T>())?;
            budget.charge(event.heap_bytes())?;
            events.push(event);
        }
    }
    (!cursor.did_exceed_match_limit()).then(|| events.into_boxed_slice())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct CloneProbe;

    impl Clone for CloneProbe {
        fn clone(&self) -> Self {
            panic!("syntax snapshots must not copy cache entries");
        }
    }

    #[test]
    fn cloning_discards_cached_results_without_cloning_entries() {
        let cache = SubtreeQueryCache {
            children: vec![Child {
                id: 1,
                kind: 0,
                range: 0..1,
                events: vec![CloneProbe].into_boxed_slice(),
            }]
            .into_boxed_slice(),
            ..SubtreeQueryCache::default()
        };
        let snapshot = cache.clone();
        assert!(snapshot.children.is_empty());
        assert_eq!(cache.children.len(), 1);
    }

    #[cfg(feature = "fixtures")]
    mod fixtures {
        use super::*;
        use crate::tests::TestLanguageLoader;
        use crate::{LanguageLoader, Syntax};
        use std::time::Duration;

        #[derive(Clone)]
        struct Payload(Vec<u8>);
        impl Event for Payload {
            fn shift(&mut self, _: i64, _: u32) {}
            fn cacheable(&self) -> bool {
                true
            }
            fn heap_bytes(&self) -> usize {
                self.0.capacity()
            }
        }

        #[test]
        fn nested_capacity_is_budgeted_before_allocating_and_fallback_clears_results() {
            let loader = TestLanguageLoader::new();
            let lang = loader.get("rust");
            let grammar = loader.get_config(lang).unwrap().grammar;
            let query = Query::new(grammar, "(function_item) @function", |_, _| Ok(())).unwrap();
            let text = "fn first() {}\nfn second() {}";
            let syntax = Syntax::new(text.into(), lang, Duration::from_secs(1), &loader).unwrap();
            let mut cache = SubtreeQueryCache::default();
            let allocations = std::cell::Cell::new(0);
            let mut project = |_: QueryMatch<'_, '_>, _, remaining| {
                if remaining < 64 {
                    return Err(());
                }
                allocations.set(allocations.get() + 1);
                // Empty length still owns its entire allocation.
                Ok(Some(Payload(Vec::with_capacity(64))))
            };
            let limit = 2 * (size_of::<Child<Payload>>() + size_of::<Payload>() + 64) - 1;
            let mut budget = Budget(limit);
            assert!(cache
                .collect(
                    &query,
                    syntax.tree(),
                    text.into(),
                    &[],
                    &[],
                    1,
                    &mut budget,
                    &mut project
                )
                .is_none());
            assert_eq!(allocations.get(), 1);
            assert_eq!(
                budget.0, limit,
                "failed attempts must not spend the shared budget"
            );
            assert!(cache.children.is_empty());
            assert!(cache.tree.is_none());

            let mut budget = Budget::default();
            assert_eq!(
                cache
                    .collect(
                        &query,
                        syntax.tree(),
                        text.into(),
                        &[],
                        &[],
                        2,
                        &mut budget,
                        &mut project
                    )
                    .unwrap()
                    .len(),
                2
            );
            let mut other = SubtreeQueryCache::default();
            let mut unavailable = Budget(1);
            assert!(other
                .collect(
                    &query,
                    syntax.tree(),
                    text.into(),
                    &[],
                    &[],
                    2,
                    &mut unavailable,
                    &mut project
                )
                .is_none());

            // Insufficient space on a later update must release the old cache too.
            assert!(cache
                .collect(
                    &query,
                    syntax.tree(),
                    text.into(),
                    &[],
                    &[],
                    3,
                    &mut unavailable,
                    &mut project
                )
                .is_none());
            assert!(cache.children.is_empty());
            assert!(cache.tree.is_none());
        }

        #[test]
        fn queries_share_one_budget_including_child_and_range_storage() {
            let loader = TestLanguageLoader::new();
            let lang = loader.get("rust");
            let grammar = loader.get_config(lang).unwrap().grammar;
            let query = Query::new(grammar, "(function_item) @function", |_, _| Ok(())).unwrap();
            let text = "fn function() {}";
            let syntax = Syntax::new(text.into(), lang, Duration::from_secs(1), &loader).unwrap();
            let ranges = [tree_sitter::Range::new(
                tree_sitter::Point::ZERO,
                tree_sitter::Point::MAX,
                0,
                u32::MAX,
            )];
            let bytes =
                size_of::<Child<Payload>>() + size_of_val(&ranges) + size_of::<Payload>() + 64;
            let mut budget = Budget(bytes);
            let mut first = SubtreeQueryCache::default();
            let mut second = SubtreeQueryCache::default();
            let mut project = |_: QueryMatch<'_, '_>, _, remaining| {
                assert!(remaining >= 64);
                Ok(Some(Payload(Vec::with_capacity(64))))
            };
            assert!(first
                .collect(
                    &query,
                    syntax.tree(),
                    text.into(),
                    &[],
                    &ranges,
                    1,
                    &mut budget,
                    &mut project
                )
                .is_some());
            assert_eq!(budget.0, 0);
            assert!(second
                .collect(
                    &query,
                    syntax.tree(),
                    text.into(),
                    &[],
                    &ranges,
                    1,
                    &mut budget,
                    &mut project
                )
                .is_none());
        }

        #[test]
        fn interrupted_updates_release_all_query_caches() {
            let loader = TestLanguageLoader::new();
            let lang = loader.get("rust");
            let before = "fn initial() {}";
            let mut syntax =
                Syntax::new(before.into(), lang, Duration::from_secs(1), &loader).unwrap();
            assert!(syntax
                .layers
                .iter()
                .any(|(_, layer)| !layer.injection_cache.children.is_empty()));
            let after = "fn renamed() { let value = 1; }\n".repeat(1_000);
            let edit = InputEdit {
                start_byte: 0,
                old_end_byte: before.len() as u32,
                new_end_byte: after.len() as u32,
                start_point: tree_sitter::Point::ZERO,
                old_end_point: tree_sitter::Point {
                    row: 0,
                    col: before.len() as u32,
                },
                new_end_point: tree_sitter::Point { row: 1_000, col: 0 },
            };
            assert!(matches!(
                syntax.update(after.as_str().into(), Duration::ZERO, &[edit], &loader),
                Err(crate::Error::Timeout)
            ));
            for (_, layer) in &syntax.layers {
                assert!(layer.injection_cache.children.is_empty());
                assert!(layer.local_cache.children.is_empty());
                assert!(layer.injection_cache.tree.is_none());
                assert!(layer.local_cache.tree.is_none());
            }
        }
    }
}

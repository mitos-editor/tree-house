use std::cmp::Reverse;
use std::iter::{self, from_fn, Peekable};
use std::mem::take;
use std::sync::Arc;

use arc_swap::ArcSwap;
use hashbrown::{HashMap, HashSet};
use regex_cursor::engines::meta::Regex;
use ropey::{LineType, RopeSlice};
use std::sync::LazyLock;

use crate::config::{LanguageConfig, LanguageLoader};
use crate::highlighter::Highlight;
use crate::locals::Locals;
use crate::parse::LayerUpdateFlags;
use crate::{Injection, Language, Layer, LayerData, Range, Syntax, TREE_SITTER_MATCH_LIMIT};
use tree_sitter::{
    query::{self, InvalidPredicateError, UserPredicate},
    Capture, Grammar, InactiveQueryCursor, MatchedNodeIdx, Pattern, Query, QueryMatch, Tree,
};

const SHEBANG: &str = r"#!\s*(?:\S*[/\\](?:env\s+(?:\-\S+\s+)*)?)?([^\s\.\d]+)";
static SHEBANG_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(SHEBANG).unwrap());

#[derive(Clone, Default, Debug)]
pub struct InjectionProperties {
    include_children: IncludedChildren,
    language: Option<Box<str>>,
    combined: bool,
}

/// An indicator in the document or query source file which used by the loader to know which
/// language an injection should use.
///
/// For example if a query sets a property `(#set! injection.language "rust")` then the loader
/// should load the Rust language. Alternatively the loader might be asked to load a language
/// based on some text in the document, for example a markdown code fence language name.
#[derive(Debug, Clone, Copy)]
pub enum InjectionLanguageMarker<'a> {
    /// The language is specified by name in the injection query itself.
    ///
    /// For example `(#set! injection.language "rust")`. These names should match exactly and so
    /// they can be looked up by equality - very efficiently.
    Name(&'a str),
    /// The language is specified by name - or similar - within the parsed document.
    ///
    /// This is slightly different than the `ExactName` variant: within a document you might
    /// specify Markdown as "md" or "markdown" for example. The loader should look up the language
    /// name by longest matching regex.
    Match(RopeSlice<'a>),
    Filename(RopeSlice<'a>),
    Shebang(RopeSlice<'a>),
}

#[derive(Clone, Debug)]
pub struct InjectionQueryMatch {
    include_children: IncludedChildren,
    language: Language,
    scope: Option<InjectionScope>,
    range: Range,
    excluded: Vec<Range>,
    last_match: bool,
    pattern: Pattern,
}

#[derive(Clone, Debug)]
pub(crate) enum Marker {
    Match(Range),
    Filename(Range),
    Shebang(Range),
}
#[derive(Clone, Debug)]
pub(crate) struct InjectionEvent {
    pattern: Pattern,
    // Each child cursor has its own native match ids; the stream keeps them distinct.
    id: (u32, u32),
    marker: Option<Marker>,
    range: Range,
    excluded: Vec<Range>,
    multiple_content: bool,
    last_content: bool,
}
impl crate::subtree_query::Event for InjectionEvent {
    fn shift(&mut self, offset: i64, stream: u32) {
        self.id.0 = stream;
        crate::subtree_query::shift_range(&mut self.range, offset);
        for range in &mut self.excluded {
            crate::subtree_query::shift_range(range, offset);
        }
        if let Some(Marker::Match(range) | Marker::Filename(range) | Marker::Shebang(range)) =
            &mut self.marker
        {
            crate::subtree_query::shift_range(range, offset);
        }
    }
    fn cacheable(&self) -> bool {
        !self.multiple_content
            && !self.range.is_empty()
            && self.marker.as_ref().is_none_or(|m| match m {
                Marker::Match(r) | Marker::Filename(r) | Marker::Shebang(r) => !r.is_empty(),
            })
    }
    fn heap_bytes(&self) -> usize {
        self.excluded.capacity() * std::mem::size_of::<Range>()
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum InjectionScope {
    Match {
        id: (u32, u32),
    },
    Pattern {
        pattern: Pattern,
        language: Language,
    },
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum IncludedChildren {
    #[default]
    None,
    All,
    Unnamed,
}

impl IncludedChildren {
    fn excluded_count(self, node: &tree_sitter::Node<'_>) -> usize {
        match self {
            Self::None => node.child_count() as usize,
            Self::All => 0,
            Self::Unnamed => node.named_child_count() as usize,
        }
    }
}

#[derive(Debug)]
pub struct InjectionsQuery {
    injection_query: Query,
    injection_properties: HashMap<Pattern, InjectionProperties>,
    injection_content_capture: Option<Capture>,
    injection_language_capture: Option<Capture>,
    injection_filename_capture: Option<Capture>,
    injection_shebang_capture: Option<Capture>,
    pub(crate) local_query: Query,
    // TODO: Use a Vec<bool> instead?
    pub(crate) not_scope_inherits: HashSet<Pattern>,
    pub(crate) local_scope_capture: Option<Capture>,
    pub(crate) local_definition_captures: ArcSwap<HashMap<Capture, Highlight>>,
}

impl InjectionsQuery {
    pub fn new(
        grammar: Grammar,
        injection_query_text: &str,
        local_query_text: &str,
    ) -> Result<Self, query::ParseError> {
        let mut injection_properties: HashMap<Pattern, InjectionProperties> = HashMap::new();
        let mut not_scope_inherits = HashSet::new();
        let injection_query = Query::new(grammar, injection_query_text, |pattern, predicate| {
            match predicate {
                // injections
                UserPredicate::SetProperty {
                    key: "injection.include-unnamed-children",
                    val: None,
                } => {
                    injection_properties
                        .entry(pattern)
                        .or_default()
                        .include_children = IncludedChildren::Unnamed
                }
                UserPredicate::SetProperty {
                    key: "injection.include-children",
                    val: None,
                } => {
                    injection_properties
                        .entry(pattern)
                        .or_default()
                        .include_children = IncludedChildren::All
                }
                UserPredicate::SetProperty {
                    key: "injection.language",
                    val: Some(lang),
                } => injection_properties.entry(pattern).or_default().language = Some(lang.into()),
                UserPredicate::SetProperty {
                    key: "injection.combined",
                    val: None,
                } => injection_properties.entry(pattern).or_default().combined = true,
                predicate => {
                    return Err(InvalidPredicateError::unknown(predicate));
                }
            }
            Ok(())
        })?;
        let mut local_query = Query::new(grammar, local_query_text, |pattern, predicate| {
            match predicate {
                UserPredicate::SetProperty {
                    key: "local.scope-inherits",
                    val,
                } => {
                    if val.is_some_and(|val| val != "true") {
                        not_scope_inherits.insert(pattern);
                    }
                }
                predicate => {
                    return Err(InvalidPredicateError::unknown(predicate));
                }
            }
            Ok(())
        })?;

        // The injection queries do not track references - these are read by the highlight
        // query instead.
        local_query.disable_capture("local.reference");

        Ok(InjectionsQuery {
            injection_properties,
            injection_content_capture: injection_query.get_capture("injection.content"),
            injection_language_capture: injection_query.get_capture("injection.language"),
            injection_filename_capture: injection_query.get_capture("injection.filename"),
            injection_shebang_capture: injection_query.get_capture("injection.shebang"),
            injection_query,
            not_scope_inherits,
            local_scope_capture: local_query.get_capture("local.scope"),
            local_definition_captures: ArcSwap::from_pointee(HashMap::new()),
            local_query,
        })
    }

    pub(crate) fn configure(&self, f: &mut impl FnMut(&str) -> Option<Highlight>) {
        let local_definition_captures = self
            .local_query
            .captures()
            .filter_map(|(capture, name)| {
                let suffix = name.strip_prefix("local.definition.")?;
                Some((capture, f(suffix)?))
            })
            .collect();
        self.local_definition_captures
            .store(Arc::new(local_definition_captures));
    }

    fn project_match(
        &self,
        query_match: &QueryMatch<'_, '_>,
        node_idx: MatchedNodeIdx,
    ) -> Option<InjectionEvent> {
        if Some(query_match.matched_node(node_idx).capture) != self.injection_content_capture {
            return None;
        }
        let properties = self.injection_properties.get(&query_match.pattern());
        let include = properties.map(|p| p.include_children).unwrap_or_default();
        let mut marker = None;
        let mut count = 0;
        let mut last = 0;
        for (i, node) in query_match.matched_nodes().enumerate() {
            let capture = Some(node.capture);
            if capture == self.injection_language_capture {
                marker = Some(Marker::Match(node.node.byte_range()));
            } else if capture == self.injection_filename_capture {
                marker = Some(Marker::Filename(node.node.byte_range()));
            } else if capture == self.injection_shebang_capture {
                marker = Some(Marker::Shebang(node.node.byte_range()));
            } else if capture == self.injection_content_capture {
                count += 1;
                last = i as u32;
            }
        }
        let node = &query_match.matched_node(node_idx).node;
        let mut excluded = Vec::with_capacity(include.excluded_count(node));
        excluded.extend(
            node.children()
                .filter(|node| match include {
                    IncludedChildren::None => true,
                    IncludedChildren::All => false,
                    IncludedChildren::Unnamed => node.is_named(),
                })
                .map(|n| n.byte_range()),
        );
        let event = InjectionEvent {
            pattern: query_match.pattern(),
            id: (0, query_match.id()),
            marker,
            range: node.byte_range(),
            excluded,
            multiple_content: count != 1,
            last_content: last == node_idx,
        };
        Some(event)
    }

    fn project_cached_match(
        &self,
        mat: &QueryMatch<'_, '_>,
        idx: MatchedNodeIdx,
        heap_budget: usize,
    ) -> Result<Option<InjectionEvent>, ()> {
        let node = mat.matched_node(idx);
        if Some(node.capture) != self.injection_content_capture {
            return Ok(None);
        }
        let include = self
            .injection_properties
            .get(&mat.pattern())
            .map(|p| p.include_children)
            .unwrap_or_default();
        // Bound the actual exclusion buffer before allocating it.
        if include.excluded_count(&node.node) > heap_budget / std::mem::size_of::<Range>() {
            return Err(());
        }
        Ok(self.project_match(mat, idx))
    }

    // Resolve markers with the current source and loader, even for cached captures.
    fn process_event(
        &self,
        event: InjectionEvent,
        source: RopeSlice<'_>,
        loader: &impl LanguageLoader,
    ) -> Option<InjectionQueryMatch> {
        let properties = self.injection_properties.get(&event.pattern);
        let marker = match &event.marker {
            Some(Marker::Match(range)) => Some(InjectionLanguageMarker::Match(
                source.slice(range.start as usize..range.end as usize),
            )),
            Some(Marker::Filename(range)) => Some(InjectionLanguageMarker::Filename(
                source.slice(range.start as usize..range.end as usize),
            )),
            Some(Marker::Shebang(range)) => {
                let text = source.slice(range.start as usize..range.end as usize);
                let lines = if 2 <= text.len_lines(LineType::LF) {
                    text.slice(..text.line_to_byte_idx(2, LineType::LF))
                } else {
                    text
                };
                SHEBANG_REGEX
                    .captures_iter(regex_cursor::Input::new(lines))
                    .next()
                    .map(|cap| {
                        InjectionLanguageMarker::Shebang(
                            lines.slice(cap.get_group(1).unwrap().range()),
                        )
                    })
            }
            None => None,
        }
        .or(properties
            .and_then(|p| p.language.as_deref())
            .map(InjectionLanguageMarker::Name))?;
        let language = loader.language_for_marker(marker)?;
        let scope = if properties.is_some_and(|p| p.combined) {
            Some(InjectionScope::Pattern {
                pattern: event.pattern,
                language,
            })
        } else if event.multiple_content {
            Some(InjectionScope::Match { id: event.id })
        } else {
            None
        };
        Some(InjectionQueryMatch {
            language,
            scope,
            include_children: properties.map(|p| p.include_children).unwrap_or_default(),
            range: event.range,
            excluded: event.excluded,
            last_match: event.last_content,
            pattern: event.pattern,
        })
    }

    /// Executes the query on the given input and return an iterator of
    /// injection ranges together with their injection properties
    ///
    /// The ranges yielded by the iterator have an ascending start range.
    /// The ranges do not overlap exactly (matches of the exact same node are
    /// resolved with normal precedence rules). However, ranges can be nested.
    /// For example:
    ///
    /// ``` no-compile
    ///   | range 2 |
    /// |   range 1  |
    /// ```
    /// is possible and will always result in iteration order [range1, range2].
    /// This case should be handled by the calling function
    #[allow(clippy::too_many_arguments)]
    fn execute<'a>(
        &'a self,
        tree: &'a Tree,
        source: RopeSlice<'a>,
        loader: &'a impl LanguageLoader,
        cache: &mut crate::subtree_query::SubtreeQueryCache<InjectionEvent>,
        edits: &[tree_sitter::InputEdit],
        ranges: &[tree_sitter::Range],
        generation: u64,
        budget: &mut crate::subtree_query::Budget,
    ) -> impl Iterator<Item = InjectionQueryMatch> + 'a {
        let events = cache.collect(
            &self.injection_query,
            tree,
            source,
            edits,
            ranges,
            generation,
            budget,
            |mat, idx, heap_budget| {
                let event = self.project_cached_match(&mat, idx, heap_budget)?;
                if event.as_ref().is_some_and(|e| e.last_content) {
                    // Release native capture storage after the final content capture.
                    mat.remove();
                }
                Ok(event)
            },
        );
        let mut cursor = events.is_none().then(|| {
            InactiveQueryCursor::new(0..u32::MAX, TREE_SITTER_MATCH_LIMIT).execute_query(
                &self.injection_query,
                &tree.root_node(),
                source,
            )
        });
        let mut events = events.map(Vec::into_iter);
        let iter = iter::from_fn(move || loop {
            if let Some(events) = &mut events {
                let event = events.next()?;
                if let Some(mat) = self
                    .process_event(event, source, loader)
                    .filter(|m| !m.range.is_empty())
                {
                    return Some(mat);
                }
            } else {
                let (query_match, idx) = cursor.as_mut()?.next_matched_node()?;
                if Some(query_match.matched_node(idx).capture) != self.injection_content_capture {
                    continue;
                }
                let event = self.project_match(&query_match, idx).unwrap();
                let Some(mat) = self.process_event(event, source, loader) else {
                    query_match.remove();
                    continue;
                };
                if mat.last_match {
                    query_match.remove();
                }
                if !mat.range.is_empty() {
                    return Some(mat);
                }
            }
        });
        let mut buf = Vec::new();
        let mut iter = iter.peekable();
        // handle identical/overlapping matches to correctly account for precedence
        iter::from_fn(move || {
            if let Some(mat) = buf.pop() {
                return Some(mat);
            }
            let mut res = iter.next()?;
            // if children are not included then nested injections don't
            // interfere with each other unless exactly identical. Since
            // this is the default setting we have a fastpath for it
            if res.include_children == IncludedChildren::None {
                let mut fast_return = true;
                while let Some(overlap) = iter.next_if(|mat| mat.range.clone() == res.range.clone())
                {
                    if overlap.include_children != IncludedChildren::None {
                        buf.push(overlap);
                        fast_return = false;
                        break;
                    }
                    // Prefer the last capture which matches this exact node.
                    res = overlap;
                }
                if fast_return {
                    return Some(res);
                }
            }

            // we if can't use the fastpath we accumulate all overlapping matches
            // and then sort them according to precedence rules...
            while let Some(overlap) = iter.next_if(|mat| mat.range.end <= res.range.end) {
                buf.push(overlap)
            }
            if buf.is_empty() {
                return Some(res);
            }
            buf.push(res);
            buf.sort_unstable_by_key(|mat| (mat.pattern, Reverse(mat.range.start)));
            buf.pop()
        })
    }
}

impl Syntax {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_injection_query(
        &mut self,
        layer: Layer,
        edits: &[tree_sitter::InputEdit],
        source: RopeSlice<'_>,
        loader: &impl LanguageLoader,
        budget: &mut crate::subtree_query::Budget,
        mut parse_layer: impl FnMut(Layer),
    ) {
        let generation = self.query_generation;
        self.map_injections(layer, None, edits);
        let layer_data = &mut self.layer_mut(layer);
        let Some(LanguageConfig {
            injection_query: ref injections_query,
            ..
        }) = loader.get_config(layer_data.language)
        else {
            layer_data.injection_cache = Default::default();
            return;
        };
        if injections_query.injection_content_capture.is_none() {
            layer_data.injection_cache = Default::default();
            return;
        }

        // work around borrow checker
        let parent_ranges = take(&mut layer_data.ranges);
        let parse_tree = layer_data.parse_tree.take().unwrap();
        let mut injections: Vec<Injection> = Vec::with_capacity(layer_data.injections.len());
        let mut old_injections = take(&mut layer_data.injections).into_iter().peekable();

        let injection_query = injections_query.execute(
            &parse_tree,
            source,
            loader,
            &mut layer_data.injection_cache,
            edits,
            &parent_ranges,
            generation,
            budget,
        );

        let mut combined_injections: HashMap<InjectionScope, Layer> = HashMap::with_capacity(32);
        for mat in injection_query {
            let matched_node_range = mat.range.clone();
            let mut insert_position = injections.len();
            // if a parent node already has an injection ignore this injection
            // in theory the first condition would be enough to detect that
            // however in case the parent node does not include children it
            // is possible that one of these children is another separate
            // injection. In these cases we cannot skip the injection
            //
            // also the precedence sorting (and rare intersection) means that
            // overlapping injections may be sorted not by position but by
            // precedence (highest precedence first). the code here ensures
            // that injections get sorted to the correct position
            if let Some(last_injection) = injections
                .last()
                .filter(|injection| ranges_intersect(&injection.range, &matched_node_range))
            {
                // this condition is not needed but serves as fast path
                // for common cases
                if last_injection.range.start <= matched_node_range.start {
                    continue;
                } else {
                    insert_position = injections.partition_point(|injection| {
                        injection.range.end <= matched_node_range.start
                    });
                    if injections[insert_position].range.start < matched_node_range.end {
                        continue;
                    }
                }
            }

            let language = mat.language;
            let reused_injection =
                self.reuse_injection(language, matched_node_range.clone(), &mut old_injections);
            let layer = match mat.scope {
                Some(scope @ InjectionScope::Match { .. }) if mat.last_match => {
                    combined_injections.remove(&scope).unwrap_or_else(|| {
                        self.init_injection(layer, mat.language, reused_injection.clone())
                    })
                }
                Some(scope) => *combined_injections.entry(scope).or_insert_with(|| {
                    self.init_injection(layer, mat.language, reused_injection.clone())
                }),
                None => self.init_injection(layer, mat.language, reused_injection.clone()),
            };
            let mut layer_data = self.layer_mut(layer);
            if !layer_data.flags.touched {
                layer_data.flags.touched = true;
                parse_layer(layer)
            }
            if layer_data.flags.reused {
                layer_data.flags.modified |= reused_injection.as_ref().is_none_or(|injection| {
                    injection.matched_node_range != matched_node_range || injection.layer != layer
                });
            } else if let Some(reused_injection) = reused_injection {
                layer_data.flags.reused = true;
                layer_data.flags.modified = true;
                let reused_parse_tree = self.layer(reused_injection.layer).tree().cloned();
                layer_data = self.layer_mut(layer);
                layer_data.parse_tree = reused_parse_tree;
            }

            let old_len = injections.len();
            for range in intersect_ranges_impl(
                mat.range,
                mat.excluded.into_iter(),
                parent_ranges.iter().map(|r| r.start_byte..r.end_byte),
            ) {
                layer_data.ranges.push(tree_sitter::Range::new(
                    tree_sitter::Point::ZERO,
                    tree_sitter::Point::ZERO,
                    range.start,
                    range.end,
                ));
                injections.push(Injection {
                    range,
                    layer,
                    matched_node_range: matched_node_range.clone(),
                });
            }
            if old_len != insert_position {
                let inserted = injections.len() - old_len;
                injections[insert_position..].rotate_right(inserted);
                layer_data.ranges[insert_position..].rotate_right(inserted);
            }
        }

        // Any remaining injections which were not reused should have their layers marked as
        // modified. These layers might have a new set of ranges (if they were visited) and so
        // their trees need to be re-parsed.
        for old_injection in old_injections {
            self.layer_mut(old_injection.layer).flags.modified = true;
        }

        let layer_data = &mut self.layer_mut(layer);
        layer_data.ranges = parent_ranges;
        layer_data.parse_tree = Some(parse_tree);
        layer_data.injections = injections;
    }

    /// Maps the layers injection ranges through edits to enable incremental re-parsing.
    fn map_injections(
        &mut self,
        layer: Layer,
        // TODO: drop this parameter?
        offset: Option<i32>,
        mut edits: &[tree_sitter::InputEdit],
    ) {
        if edits.is_empty() && offset.unwrap_or(0) == 0 {
            return;
        }
        let layer_data = self.layer_mut(layer);
        let first_relevant_injection = layer_data
            .injections
            .partition_point(|injection| injection.range.end < edits[0].start_byte);
        if first_relevant_injection == layer_data.injections.len() {
            return;
        }
        let mut offset = if let Some(offset) = offset {
            let first_relevant_edit = edits.partition_point(|edit| {
                (edit.old_end_byte as i32) < (layer_data.ranges[0].end_byte as i32 - offset)
            });
            edits = &edits[first_relevant_edit..];
            offset
        } else {
            0
        };
        // injections and edits are non-overlapping and sorted so we can
        // apply edits in O(M+N) instead of O(NM)
        let mut edits = edits.iter().peekable();
        let mut injections = take(&mut layer_data.injections);
        for injection in &mut injections[first_relevant_injection..] {
            let injection_range = &mut injection.range;
            let matched_node_range = &mut injection.matched_node_range;
            let flags = &mut self.layer_mut(injection.layer).flags;

            debug_assert!(matched_node_range.start <= injection_range.start);
            debug_assert!(matched_node_range.end >= injection_range.end);

            while let Some(edit) =
                edits.next_if(|edit| edit.old_end_byte < matched_node_range.start)
            {
                offset += edit.offset();
            }
            let mut mapped_node_range_start = (matched_node_range.start as i32 + offset) as u32;
            if let Some(edit) = edits
                .peek()
                .filter(|edit| edit.start_byte <= matched_node_range.start)
            {
                mapped_node_range_start = (edit.new_end_byte as i32 + offset) as u32;
            }
            while let Some(edit) = edits.next_if(|edit| edit.old_end_byte < injection_range.start) {
                offset += edit.offset();
            }
            flags.moved = offset != 0;
            let mut mapped_start = (injection_range.start as i32 + offset) as u32;
            if let Some(edit) = edits.next_if(|edit| edit.old_end_byte <= injection_range.end) {
                if edit.start_byte < injection_range.start {
                    flags.moved = true;
                    mapped_start = (edit.new_end_byte as i32 + offset) as u32;
                } else {
                    flags.modified = true;
                }
                offset += edit.offset();
                while let Some(edit) =
                    edits.next_if(|edit| edit.old_end_byte <= injection_range.end)
                {
                    offset += edit.offset();
                }
            }
            let mut mapped_end = (injection_range.end as i32 + offset) as u32;
            if let Some(edit) = edits
                .peek()
                .filter(|edit| edit.start_byte <= injection_range.end)
            {
                flags.modified = true;

                if edit.start_byte < injection_range.start {
                    mapped_start = (edit.new_end_byte as i32 + offset) as u32;
                    mapped_end = mapped_start;
                }
            }
            let mut mapped_node_range_end = (matched_node_range.end as i32 + offset) as u32;
            if let Some(edit) = edits
                .peek()
                .filter(|edit| edit.start_byte <= matched_node_range.end)
            {
                if edit.start_byte < matched_node_range.start {
                    mapped_node_range_start = (edit.new_end_byte as i32 + offset) as u32;
                    mapped_node_range_end = mapped_node_range_start;
                }
            }
            *injection_range = mapped_start..mapped_end;
            *matched_node_range = mapped_node_range_start..mapped_node_range_end;
        }
        self.layer_mut(layer).injections = injections;
    }

    fn init_injection(
        &mut self,
        parent: Layer,
        language: Language,
        reuse: Option<Injection>,
    ) -> Layer {
        match reuse {
            Some(old_injection) => {
                let layer_data = self.layer_mut(old_injection.layer);
                debug_assert_eq!(layer_data.parent, Some(parent));
                layer_data.flags.reused = true;
                layer_data.ranges.clear();
                old_injection.layer
            }
            None => {
                let layer = self.layers.insert(LayerData {
                    language,
                    parse_tree: None,
                    ranges: Vec::new(),
                    injections: Vec::new(),
                    flags: LayerUpdateFlags::default(),
                    parent: Some(parent),
                    locals: Locals::default(),
                    local_cache: Default::default(),
                    injection_cache: Default::default(),
                });
                Layer(layer as u32)
            }
        }
    }

    // TODO: only reuse if same pattern is matched
    fn reuse_injection(
        &mut self,
        language: Language,
        new_range: Range,
        injections: &mut Peekable<impl Iterator<Item = Injection>>,
    ) -> Option<Injection> {
        while let Some(skipped) =
            injections.next_if(|injection| injection.range.end <= new_range.start)
        {
            // If the layer had an injection and now does not have the injection, consider the
            // skipped layer to be modified so that its tree is re-parsed. It must be re-parsed
            // since the skipped layer now has a different set of ranges than it used to. Note
            // that the layer isn't marked as `touched` so it could be discarded if the layer
            // is not ever visited.
            self.layer_mut(skipped.layer).flags.modified = true;
        }
        injections
            .next_if(|injection| {
                injection.range.start < new_range.end
                    && self.layer(injection.layer).language == language
                    && !self.layer(injection.layer).flags.reused
            })
            .clone()
    }
}

/// Creates an iterator over the subtraction of one set of ranges from another.
///
/// Given two iterators that yield a sorted series of ranges, this function
/// returns a new iterator that yields a series of ranges, equivalent to the
/// ranges from the first iterator, but excluding any ranges returned by the
/// second.
///
/// To reiterate, both iterators are assumed to return the ranges in sorted order.
fn exclude_ranges(
    mut ranges: impl Iterator<Item = Range>,
    excluded_ranges: impl Iterator<Item = Range>,
) -> impl Iterator<Item = Range> {
    let mut excluded_ranges = excluded_ranges.filter(|range| !range.is_empty());
    let mut next_range = ranges.next();
    let mut current_excluded_range = excluded_ranges.next();
    from_fn(move || {
        loop {
            let range = next_range.take()?;

            // Consume any ranges that precede the rest of the input ranges
            while let Some(excluded_range) = &current_excluded_range {
                if excluded_range.end <= range.start {
                    current_excluded_range = excluded_ranges.next()
                } else {
                    break;
                }
            }
            // Handle the next exclusion, if applicable
            if let Some(excluded_range) = &current_excluded_range {
                if ranges_intersect(&range, excluded_range) {
                    let preceding_range = range.start..excluded_range.start;
                    let remaining_range = excluded_range.end..range.end;

                    // Handle the rest of the range on the next iteration, if
                    // applicable, or take the next one.
                    if !remaining_range.is_empty() {
                        next_range = Some(remaining_range);
                        // This would also be handled by the loop above
                        current_excluded_range = excluded_ranges.next()
                    } else {
                        next_range = ranges.next()
                    }

                    // Return any part of the range that preceded the exclusion
                    if !preceding_range.is_empty() {
                        return Some(preceding_range);
                    } else {
                        continue;
                    }
                }
            }
            // Return any ranges that precede the next excluded range
            next_range = ranges.next();
            return Some(range);
        }
    })
}

/// Return an iterator over the ranges resulting from intersection calculation.
///
/// Given a range, a set of ranges to exclude, and a set of parent ranges,
/// this function calculates the intersection of range with the parent ranges,
/// minus the excluded ranges.
fn intersect_ranges_impl(
    mut range: Range,
    excluded_ranges: impl Iterator<Item = Range>,
    parent_ranges: impl Iterator<Item = Range>,
) -> impl Iterator<Item = Range> {
    // Skip ranges that end before the start of range
    let parent_ranges = parent_ranges.skip_while(move |r| r.end <= range.start);
    // filter by the excluded ranges
    let mut parent_ranges = exclude_ranges(parent_ranges, excluded_ranges).peekable();
    let mut next_parent_range = parent_ranges.next();
    // Pre-merge any adjacent parent ranges before the first iteration so that the very first
    // intersection is computed against the full merged span rather than just the first piece.
    while let Some(ref mut current) = next_parent_range {
        match parent_ranges.peek() {
            Some(next) if next.start == current.end => {
                current.end = next.end;
                parent_ranges.next();
            }
            _ => break,
        }
    }
    from_fn(move || {
        loop {
            // If either range is exhausted, so is this iterator
            let Some(parent_range) = &next_parent_range else {
                return None;
            };
            if range.is_empty() {
                return None;
            }

            // Discard non-intersecting part of range
            if parent_range.start > range.start {
                range.start = parent_range.start;
            }

            // Consume some or all of the range
            let intersection = range.start..std::cmp::min(range.end, parent_range.end);
            range.start = intersection.end;

            // Discard any parents that fully precede the new range, merging any adjacent ones
            // into the replacement so the next intersection is computed against the full span.
            while let Some(parent_range) = &next_parent_range {
                if parent_range.end <= range.start {
                    let mut next = parent_ranges.next();
                    while let Some(ref mut current) = next {
                        match parent_ranges.peek() {
                            Some(n) if n.start == current.end => {
                                current.end = n.end;
                                parent_ranges.next();
                            }
                            _ => break,
                        }
                    }
                    next_parent_range = next;
                } else {
                    break;
                }
            }

            // Return the part of the range that was consumed
            if !intersection.is_empty() {
                return Some(intersection);
            }
        }
    })
}

fn ranges_intersect(a: &Range, b: &Range) -> bool {
    // Adapted from <https://github.com/helix-editor/helix/blob/8df58b2e1779dcf0046fb51ae1893c1eebf01e7c/helix-core/src/selection.rs#L156-L163>
    a.start == b.start || (a.end > b.start && b.end > a.start)
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[cfg(feature = "fixtures")]
    #[test]
    fn cached_exclusions_check_the_budget_before_projection() {
        use crate::tests::TestLanguageLoader;
        let loader = TestLanguageLoader::new();
        let lang = loader.get("rust");
        let grammar = loader.get_config(lang).unwrap().grammar;
        let text = "fn example() { let value = 1; }";
        let syntax = Syntax::new(
            text.into(),
            lang,
            std::time::Duration::from_secs(1),
            &loader,
        )
        .unwrap();
        for (include, property) in [
            (IncludedChildren::None, ""),
            (IncludedChildren::All, "(#set! injection.include-children)"),
            (
                IncludedChildren::Unnamed,
                "(#set! injection.include-unnamed-children)",
            ),
        ] {
            let source = format!("((function_item body: (block) @injection.content) (#set! injection.language \"comment\") {property})");
            let query = InjectionsQuery::new(grammar, &source, "").unwrap();
            let mut cursor = InactiveQueryCursor::default().execute_query(
                &query.injection_query,
                &syntax.tree().root_node(),
                ropey::RopeSlice::from(text),
            );
            let (mat, idx) = cursor.next_matched_node().unwrap();
            let count = include.excluded_count(&mat.matched_node(idx).node);
            assert_eq!(
                query.project_cached_match(&mat, idx, 0).is_err(),
                count != 0
            );
            let full = query.project_match(&mat, idx).unwrap();
            assert_eq!(full.excluded.len(), count);
            let bytes = full.excluded.capacity() * std::mem::size_of::<Range>();
            assert_eq!(crate::subtree_query::Event::heap_bytes(&full), bytes);
            let cached = query
                .project_cached_match(&mat, idx, bytes)
                .unwrap()
                .unwrap();
            assert_eq!(cached.excluded, full.excluded);
        }
    }

    #[test]
    fn exclude_ranges_no_exclusions() {
        let result: Vec<Range> =
            exclude_ranges([0..10, 20..30].into_iter(), [].into_iter()).collect();
        assert_eq!(result, vec![0..10, 20..30]);
    }

    #[test]
    fn exclude_ranges_excludes_prefix() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [0..5].into_iter()).collect();
        assert_eq!(result, vec![5..10]);
    }

    #[test]
    fn exclude_ranges_excludes_suffix() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [5..10].into_iter()).collect();
        assert_eq!(result, vec![0..5]);
    }

    #[test]
    fn exclude_ranges_excludes_middle() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [3..7].into_iter()).collect();
        assert_eq!(result, vec![0..3, 7..10]);
    }

    #[test]
    fn exclude_ranges_full_exclusion() {
        let result: Vec<Range> = exclude_ranges([0..10].into_iter(), [0..10].into_iter()).collect();
        assert_eq!(result, vec![]);
    }

    #[test]
    fn exclude_ranges_multiple_exclusions() {
        let result: Vec<Range> =
            exclude_ranges([0..100].into_iter(), [10..20, 40..50, 70..80].into_iter()).collect();
        assert_eq!(result, vec![0..10, 20..40, 50..70, 80..100]);
    }

    #[test]
    fn exclude_ranges_spans_multiple_input_ranges() {
        let result: Vec<Range> =
            exclude_ranges([0..20, 30..50].into_iter(), [15..35].into_iter()).collect();
        assert_eq!(result, vec![0..15, 35..50]);
    }

    #[test]
    fn intersect_ranges_impl_basic() {
        let result: Vec<Range> =
            intersect_ranges_impl(10..50, [].into_iter(), [0..100].into_iter()).collect();
        assert_eq!(result, vec![10..50]);
    }

    #[test]
    fn intersect_ranges_impl_with_excluded_child() {
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [30..70].into_iter(), [0..100].into_iter()).collect();
        assert_eq!(result, vec![0..30, 70..100]);
    }

    #[test]
    fn intersect_ranges_impl_non_adjacent_parent_ranges() {
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..40, 60..90].into_iter()).collect();
        assert_eq!(result, vec![10..40, 60..90]);
    }

    #[test]
    fn intersect_ranges_impl_adjacent_parent_ranges() {
        // Two adjacent parent ranges must be merged into one to avoid emitting adjacent
        // injection ranges. The first range's adjacency to the second must be detected before
        // computing the first intersection, not after yielding it.
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..50, 50..80].into_iter()).collect();
        assert_eq!(result, vec![10..80]);
    }

    #[test]
    fn intersect_ranges_impl_three_adjacent_parent_ranges() {
        // All three adjacent ranges must be collapsed into one.
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..30, 30..50, 50..80].into_iter())
                .collect();
        assert_eq!(result, vec![10..80]);
    }

    #[test]
    fn intersect_ranges_impl_adjacent_then_gap() {
        // First two adjacent, third separated: merge only the adjacent pair.
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..30, 30..50, 60..80].into_iter())
                .collect();
        assert_eq!(result, vec![10..50, 60..80]);
    }

    #[test]
    fn intersect_ranges_impl_gap_then_adjacent() {
        // Gap then adjacent: the post-discard merge in the existing code already handles this,
        // but it should also be correct after the pre-merge fix.
        let result: Vec<Range> =
            intersect_ranges_impl(0..100, [].into_iter(), [10..30, 40..60, 60..80].into_iter())
                .collect();
        assert_eq!(result, vec![10..30, 40..80]);
    }
}

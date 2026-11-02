use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::{IndexMap, IndexSet};
use skidder::Repo;
use std::cell::OnceCell;
use std::sync::LazyLock;
use tree_sitter::{Grammar, InputEdit, Point};

use crate::config::{LanguageConfig, LanguageLoader};
use crate::fixtures::{check_highlighter_fixture, check_injection_fixture};
use crate::highlighter::{Highlight, HighlightEvent, Highlighter};
use crate::injections_query::InjectionLanguageMarker;
use crate::{Language, Layer, Syntax};

const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[test]
fn predicate_prefilter_preserves_injections_across_edits() {
    let plain = r#"
((line_comment) @injection.content (#set! injection.language "comment") (#set! injection.include-children))
((block_comment) @injection.content (#set! injection.language "comment") (#set! injection.include-children))
"#;
    let required = plain.replace(
        "(#set! injection.language",
        "(#match? @injection.content \"^/\") (#set! injection.language",
    );
    let mut reference_loader = TestLanguageLoader::new();
    reference_loader.overwrite_injections("rust", plain.into());
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_injections("rust", required);
    let mut before = "fn main() {}";
    let mut syntax =
        Syntax::new(before.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();
    for after in [
        "fn main() { /* todo */ }",
        "// note\nfn main() {}",
        "fn main() {}",
        "fn main() { let s = \"/\"; }",
        "fn main() { /* again */ }",
    ] {
        let edit = InputEdit {
            start_byte: 0,
            old_end_byte: before.len() as u32,
            new_end_byte: after.len() as u32,
            start_point: Point::ZERO,
            old_end_point: Point::ZERO,
            new_end_point: Point::ZERO,
        };
        syntax
            .update(after.into(), PARSE_TIMEOUT, &[edit], &loader)
            .unwrap();
        let fresh = Syntax::new(after.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();
        let reference = Syntax::new(
            after.into(),
            reference_loader.get("rust"),
            PARSE_TIMEOUT,
            &reference_loader,
        )
        .unwrap();
        for byte in 0..after.len() as u32 {
            let languages = |syntax: &Syntax| {
                syntax
                    .layers_for_byte_range(byte, byte)
                    .map(|layer| syntax.layer(layer).language)
                    .collect::<Vec<_>>()
            };
            assert_eq!(languages(&syntax), languages(&fresh));
            assert_eq!(languages(&syntax), languages(&reference));
        }
        let has_comment_layer = (0..after.len() as u32).any(|byte| {
            syntax
                .layers_for_byte_range(byte, byte)
                .any(|layer| syntax.layer(layer).language == loader.get("comment"))
        });
        assert_eq!(
            has_comment_layer,
            after.contains("/*") || after.starts_with("//")
        );
        before = after;
    }
}

#[test]
fn predicate_prefilter_preserves_unannotated_and_mixed_patterns() {
    let comment = r#"((line_comment) @injection.content
        (#set! injection.language "comment") (#set! injection.include-children) (#match? @injection.content "^/"))"#;
    for property in ["", "(#match? @injection.content \"#\")"] {
        let query = format!("{comment}\n((string_literal) @injection.content (#set! injection.language \"comment\") (#set! injection.include-children) {property})");
        let mut loader = TestLanguageLoader::new();
        loader.overwrite_injections("rust", query);
        let source = "fn main() { let text = \"#text\"; }";
        let syntax =
            Syntax::new(source.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();
        let byte = source.find("#text").unwrap() as u32;
        assert!(syntax
            .layers_for_byte_range(byte, byte)
            .any(|layer| syntax.layer(layer).language == loader.get("comment")));
    }
}

#[test]
fn custom_required_byte_hint_is_not_supported() {
    let grammar = get_grammar("rust", &Overwrites::default()).grammar;
    let query = r#"((line_comment) @injection.content
        (#set! injection.language "comment") (#set! injection.required-byte "/"))"#;
    assert!(crate::injections_query::InjectionsQuery::new(grammar, query, "").is_err());
}

#[test]
fn predicate_prefilter_scan_covers_rope_chunks_and_nonzero_slices() {
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_injections(
        "rust",
        r#"((line_comment) @injection.content
        (#set! injection.language "comment") (#set! injection.include-children) (#match? @injection.content "^/"))"#
            .into(),
    );
    let prefix = "fn ignored() {}\n".repeat(100);
    let body = format!("{}\n// last chunk comment", "fn main() {}\n".repeat(5_000));
    let text = ropey::Rope::from_str(&format!("{prefix}{body}"));
    let source = text.slice(prefix.len()..);
    assert!(source.chunks().count() > 1);
    let syntax = Syntax::new(source, loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();
    let byte = body.find("//").unwrap() as u32;
    assert!(syntax
        .layers_for_byte_range(byte, byte)
        .any(|layer| syntax.layer(layer).language == loader.get("comment")));
}

#[test]
fn predicate_prefilter_matches_full_queries_in_python_html_and_rust() {
    let python = r#"
((comment) @injection.content
 (#set! injection.language "comment") (#set! injection.include-children))
((string (string_content) @injection.content)
 (#match? @injection.content "^fn ")
 (#set! injection.language "rust") (#set! injection.include-children))
"#;
    let html = r#"
((comment) @injection.content
 (#set! injection.language "comment") (#set! injection.include-children))
((style_element (raw_text) @injection.content) @_element
 (#set! injection.language "css") (#set! injection.include-children))
"#;
    let rust = r#"
((line_comment) @injection.content
 (#set! injection.language "comment") (#set! injection.include-children))
((block_comment) @injection.content
 (#set! injection.language "comment") (#set! injection.include-children))
"#;
    let cases: [(&str, &str, String, &[&str]); 3] = [
        (
            "python",
            python,
            python.replace(
                "(#set! injection.language \"comment\")",
                "(#match? @injection.content \"^#\") (#set! injection.language \"comment\")",
            ),
            &[
                "value = 1\n",
                "# todo café\nvalue = 1\n",
                "code = \"fn inner() {}\"\n",
                "code = \"fn inner() { /* 日本語 */ }\"\n",
                "code = \"# false positive f\"\n",
                "value = 2\n",
            ],
        ),
        (
            "html",
            html,
            html.replace(
                "(#set! injection.language \"comment\")",
                "(#match? @injection.content \"^<\") (#set! injection.language \"comment\")",
            )
            .replace(
                "(#set! injection.language \"css\")",
                "(#match? @_element \"^<\") (#set! injection.language \"css\")",
            ),
            &[
                "plain é日 text",
                "<!-- todo -->plain text",
                "<style>body { color: red; }</style>",
                "<!-- / outside CSS --><style>body { color: red; }</style>",
                "<style>/* todo */ body { color: red; }</style>",
                "<style>body { color: red; }</style>",
                "plain text",
            ],
        ),
        (
            "rust",
            rust,
            rust.replace(
                "(#set! injection.language",
                "(#match? @injection.content \"^/\") (#set! injection.language",
            ),
            &[
                "fn main() {}",
                "// todo 日本語\nfn main() {}",
                "fn main() { /* café */ }",
                "fn main() { let text = \"/ false positive\"; }",
                "fn main() {}",
            ],
        ),
    ];
    for (language, plain, annotated, sources) in cases {
        let mut reference_loader = TestLanguageLoader::new();
        reference_loader.overwrite_injections(language, plain.into());
        let mut loader = TestLanguageLoader::new();
        loader.overwrite_injections(language, annotated);
        let css = r#"((comment) @injection.content (#set! injection.language "comment") (#set! injection.include-children))"#;
        reference_loader.overwrite_injections("css", css.into());
        loader.overwrite_injections(
            "css",
            css.replace(
                "(#set! injection.language",
                "(#match? @injection.content \"^/\") (#set! injection.language",
            ),
        );
        let mut previous = sources[0];
        let mut syntax = Syntax::new(
            previous.into(),
            loader.get(language),
            PARSE_TIMEOUT,
            &loader,
        )
        .unwrap();
        for &source in sources {
            let end = |text: &str| Point {
                row: text.bytes().filter(|&byte| byte == b'\n').count() as u32,
                col: text
                    .rfind('\n')
                    .map_or(text.len(), |last| text.len() - last - 1) as u32,
            };
            let edit = InputEdit {
                start_byte: 0,
                old_end_byte: previous.len() as u32,
                new_end_byte: source.len() as u32,
                start_point: Point::ZERO,
                old_end_point: end(previous),
                new_end_point: end(source),
            };
            syntax
                .update(source.into(), PARSE_TIMEOUT, &[edit], &loader)
                .unwrap();
            let full = Syntax::new(
                source.into(),
                reference_loader.get(language),
                PARSE_TIMEOUT,
                &reference_loader,
            )
            .unwrap();
            let nested = match language {
                "python" if source.contains("\"fn ") => Some("rust"),
                "html" if source.contains("<style>") => Some("css"),
                _ => None,
            };
            if let Some(nested) = nested {
                assert!(
                    full.layers
                        .iter()
                        .any(|(_, layer)| { layer.language == reference_loader.get(nested) }),
                    "the reference must exercise {language} -> {nested}"
                );
            }
            assert_eq!(
                prefilter_observation(&syntax, source, &loader),
                prefilter_observation(&full, source, &reference_loader),
                "{language}: {source:?}"
            );
            previous = source;
        }
    }
}

#[allow(clippy::type_complexity)]
fn prefilter_observation(
    syntax: &Syntax,
    source: &str,
    loader: &TestLanguageLoader,
) -> (Vec<(u32, Vec<String>)>, Vec<(u32, Vec<(u32, u32)>)>) {
    let mut highlighter = Highlighter::new(syntax, source.into(), loader, 0..source.len() as u32);
    let mut highlights = Vec::new();
    while highlighter.next_event_offset() != u32::MAX {
        let offset = highlighter.next_event_offset();
        highlighter.advance();
        highlights.push((
            offset,
            highlighter
                .active_highlights()
                .map(|highlight| loader.test_theme.borrow()[highlight.idx()].clone())
                .collect(),
        ));
        assert!(highlights.len() < source.len() * 20 + 100);
    }
    let mut layers = syntax
        .layers
        .iter()
        .map(|(_, layer)| {
            (
                layer.language.0,
                layer
                    .ranges
                    .iter()
                    .map(|range| (range.start_byte, range.end_byte))
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    layers.sort();
    (highlights, layers)
}

#[test]
fn predicate_prefilter_rejects_before_visiting_each_identifier() {
    use std::cell::Cell;
    use tree_sitter::{Input, RopeInput};
    struct Counted<'a> {
        inner: RopeInput<'a>,
        seeks: &'a Cell<usize>,
    }
    impl<'a> Input for Counted<'a> {
        type Cursor = <RopeInput<'a> as Input>::Cursor;
        fn cursor_at(&mut self, offset: u32) -> &mut Self::Cursor {
            self.seeks.set(self.seeks.get() + 1);
            self.inner.cursor_at(offset)
        }
        fn eq(&mut self, a: std::ops::Range<u32>, b: std::ops::Range<u32>) -> bool {
            self.inner.eq(a, b)
        }
    }
    let loader = TestLanguageLoader::new();
    let source = ropey::Rope::from_str(&"fn actual() {}\n".repeat(128));
    let syntax = Syntax::new(source.slice(..), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();
    let tree = syntax.layer(syntax.root).parse_tree.as_ref().unwrap();
    let grammar = loader.get_config(loader.get("rust")).unwrap().grammar;
    let query = tree_sitter::Query::new(
        grammar,
        r#"((identifier) @name (#eq? @name "needle"))"#,
        |_, predicate| {
            Err(tree_sitter::query::InvalidPredicateError::unknown(
                predicate,
            ))
        },
    )
    .unwrap();
    let seeks = Cell::new(0);
    let mut cursor =
        tree_sitter::InactiveQueryCursor::new(0..u32::MAX, crate::TREE_SITTER_MATCH_LIMIT)
            .execute_query(
                &query,
                &tree.root_node(),
                Counted {
                    inner: RopeInput::new(source.slice(..)),
                    seeks: &seeks,
                },
            );
    assert!(cursor.next_match().is_none());
    assert_eq!(
        seeks.get(),
        1,
        "one source prefilter scan should replace per-identifier predicate reads"
    );
}

type PredicateMatch = (usize, Vec<(String, std::ops::Range<u32>)>);
type PredicateCapture = (usize, String, std::ops::Range<u32>);

fn predicate_observation(
    grammar: Grammar,
    node: &tree_sitter::Node<'_>,
    source: ropey::RopeSlice<'_>,
    text: &str,
) -> (Vec<PredicateMatch>, Vec<PredicateCapture>) {
    let query = tree_sitter::Query::new(grammar, text, |_, predicate| {
        Err(tree_sitter::query::InvalidPredicateError::unknown(
            predicate,
        ))
    })
    .unwrap();
    let mut cursor =
        tree_sitter::InactiveQueryCursor::new(0..u32::MAX, crate::TREE_SITTER_MATCH_LIMIT)
            .execute_query(&query, node, source);
    let mut matches = Vec::new();
    while let Some(mat) = cursor.next_match() {
        matches.push((
            mat.pattern().idx(),
            mat.matched_nodes()
                .map(|node| (node.capture.name(&query).into(), node.node.byte_range()))
                .collect(),
        ));
    }
    let mut cursor = cursor.reuse().execute_query(&query, node, source);
    let mut captures = Vec::new();
    while let Some((mat, index)) = cursor.next_matched_node() {
        let node = mat.matched_node(index);
        captures.push((
            mat.pattern().idx(),
            node.capture.name(&query).into(),
            node.node.byte_range(),
        ));
    }
    (matches, captures)
}

#[test]
fn predicate_prefilter_agrees_with_full_queries_for_supported_and_unknown_constraints() {
    let loader = TestLanguageLoader::new();
    let grammar = loader.get_config(loader.get("rust")).unwrap().grammar;
    let queries = [
        r#"((identifier) @name (#eq? @name "needle"))"#,
        r#"((identifier) @name (#any-eq? @name "needle"))"#,
        r#"((identifier) @name (#any-of? @name "needle" "actual"))"#,
        r#"((identifier) @name (#match? @name "^needle$"))"#,
        r#"((identifier) @name (#any-match? @name "\\bneedle\\b"))"#,
        r#"((identifier) @name (#match? @name "^(?:needle|other)$"))"#,
        r#"((identifier) @name (#match? @name "(?i)^kelvin$"))"#,
        r#"((identifier) @name (#match? @name "^(?:日本語|café)$"))"#,
        r#"((identifier) @name (#match? @name "^[a-z]+$"))"#,
        r#"((identifier) @name (#match? @name "^$"))"#,
        r#"((identifier) @name (#match? @name "^(?:needle)?$"))"#,
        r#"((identifier) @name (#not-match? @name "^needle$"))"#,
        r#"((identifier) @name (#not-eq? @name "needle"))"#,
        r#"((identifier) @name (#not-any-of? @name "needle" "actual"))"#,
        r#"((identifier) @name (#any-of? @name "" "needle"))"#,
        r#"((identifier) @name (#not-match? @name "^other$") (#match? @name "^needle$"))"#,
        r#"((identifier) @name (#eq? @name "needle")) ((identifier) @name (#eq? @name "actual"))"#,
        r#"((identifier) @name (#eq? @name "needle")) (integer_literal) @number"#,
        r#"([(identifier) @name (integer_literal)] @value (#eq? @name "needle"))"#,
        r#"([(identifier) @name (integer_literal)] @value (#any-eq? @name "needle"))"#,
        r#"((function_item name: (identifier) @fn parameters: (parameters (parameter pattern: (identifier) @param)?)) (#match? @param "^needle$"))"#,
        r#"((function_item name: (identifier) @fn parameters: (parameters (parameter pattern: (identifier) @param)*)) (#eq? @param "needle"))"#,
        r#"((function_item name: (identifier) @fn parameters: (parameters (parameter pattern: (identifier) @param)+)) (#match? @param "^needle$"))"#,
        r#"((function_item name: (identifier) @fn parameters: (parameters (parameter pattern: (identifier) @param)?)) (#any-match? @param "^needle$"))"#,
        r#"((binary_expression left: (identifier) @a right: (identifier) @b) (#eq? @a @b))"#,
    ];
    for source in [
        "fn actual() {}",
        "fn needle() { let actual = 123; needle + needle; }",
        "fn actual(value: i32) {} fn needle(needle: i32, needle: i32) {}",
        "fn café() {} fn 日本語() {} fn KELVIN() {}",
    ] {
        let syntax = Syntax::new(
            ropey::RopeSlice::from(source),
            loader.get("rust"),
            PARSE_TIMEOUT,
            &loader,
        )
        .unwrap();
        let node = syntax
            .layer(syntax.root)
            .parse_tree
            .as_ref()
            .unwrap()
            .root_node();
        for query in queries {
            // This extra alternative always fails, but its negated predicate has
            // no positive requirement. It forces ordinary full-query execution.
            let full = format!("{query}\n((_) @_never (#not-match? @_never \"(?s)\"))");
            assert_eq!(
                predicate_observation(grammar, &node, ropey::RopeSlice::from(source), query),
                predicate_observation(grammar, &node, ropey::RopeSlice::from(source), &full),
                "{query}: {source}"
            );
        }
    }
}

#[test]
fn predicate_prefilter_handles_capture_disabling_cursor_reuse_and_ranges() {
    let loader = TestLanguageLoader::new();
    let grammar = loader.get_config(loader.get("rust")).unwrap().grammar;
    let source = "fn actual() {}";
    let syntax = Syntax::new(
        ropey::RopeSlice::from(source),
        loader.get("rust"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();
    let node = syntax
        .layer(syntax.root)
        .parse_tree
        .as_ref()
        .unwrap()
        .root_node();
    let make = |text| {
        tree_sitter::Query::new(grammar, text, |_, p| {
            Err(tree_sitter::query::InvalidPredicateError::unknown(p))
        })
        .unwrap()
    };
    let mut absent = make(r#"((identifier) @name (#eq? @name "needle"))"#);
    let present = make(r#"((identifier) @name (#eq? @name "actual"))"#);
    let mut cursor = tree_sitter::InactiveQueryCursor::default().execute_query(
        &absent,
        &node,
        ropey::RopeSlice::from(source),
    );
    assert!(cursor.next_match().is_none());
    let mut cursor = cursor
        .reuse()
        .execute_query(&present, &node, ropey::RopeSlice::from(source));
    assert!(cursor.next_match().is_some());
    let mut cursor = cursor
        .reuse()
        .execute_query(&absent, &node, ropey::RopeSlice::from(source));
    assert!(cursor.next_matched_node().is_none());
    drop(cursor);
    absent.disable_capture("name");
    let mut cursor = tree_sitter::InactiveQueryCursor::default().execute_query(
        &absent,
        &node,
        ropey::RopeSlice::from(source),
    );
    assert!(
        cursor.next_match().is_some(),
        "a disabled capture makes #eq? vacuously true"
    );
    drop(cursor);

    let source = "fn needle() { let actual = 1; }";
    let syntax = Syntax::new(
        ropey::RopeSlice::from(source),
        loader.get("rust"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();
    let node = syntax
        .layer(syntax.root)
        .parse_tree
        .as_ref()
        .unwrap()
        .root_node();
    let query = make(
        r#"((function_item name: (identifier) @name body: (block) @body) (#eq? @name "needle"))"#,
    );
    let body = source.find('{').unwrap() as u32;
    let mut cursor = tree_sitter::InactiveQueryCursor::new(
        body..source.len() as u32,
        crate::TREE_SITTER_MATCH_LIMIT,
    )
    .execute_query(&query, &node, ropey::RopeSlice::from(source));
    assert!(
        cursor.next_matched_node().is_some(),
        "requirements outside the capture range must remain visible"
    );
}

#[test]
fn predicate_prefilter_finds_literals_across_rope_chunks_and_nonzero_subtrees() {
    let loader = TestLanguageLoader::new();
    let grammar = loader.get_config(loader.get("rust")).unwrap().grammar;
    let source = format!("{}\nfn needle() {{}}", "fn actual() {}\n".repeat(500));
    let rope = ropey::Rope::from_str(&source);
    let query = r#"((identifier) @name (#eq? @name "needle"))"#;
    let syntax = Syntax::new(rope.slice(..), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();
    let node = syntax
        .layer(syntax.root)
        .parse_tree
        .as_ref()
        .unwrap()
        .root_node();
    assert_eq!(
        predicate_observation(grammar, &node, rope.slice(..), query)
            .0
            .len(),
        1
    );
    let first = node.children().next().unwrap();
    assert!(
        predicate_observation(grammar, &first, rope.slice(..), query)
            .0
            .is_empty()
    );
}

static GRAMMARS: LazyLock<Vec<PathBuf>> = LazyLock::new(|| {
    let skidder_config = skidder_config();
    skidder::fetch(&skidder_config, false).unwrap();
    skidder::build_all_grammars(&skidder_config, false, None).unwrap();
    let grammars = skidder::list_grammars(&skidder_config).unwrap();
    assert!(!grammars.is_empty());
    grammars
});

fn skidder_config() -> skidder::Config {
    skidder::Config {
        repos: vec![Repo::Local {
            // `./test-grammars` in the root of the repo.
            path: Path::new("../test-grammars").canonicalize().unwrap(),
        }],
        index: PathBuf::new(),
        verbose: true,
    }
}

#[derive(Debug, Clone, Default)]
struct Overwrites {
    highlights: Option<String>,
    locals: Option<String>,
    injections: Option<String>,
}

fn get_grammar(lang_name: &str, overwrites: &Overwrites) -> LanguageConfig {
    let skidder_config = skidder_config();
    let grammar_dir = skidder_config.grammar_dir(lang_name).unwrap();
    let parser_path = skidder::build_grammar(&skidder_config, lang_name, false).unwrap();
    let grammar = unsafe { Grammar::new(lang_name, &parser_path).unwrap() };
    let highlights_query_path = grammar_dir.join("highlights.scm");
    let injections_query_path = grammar_dir.join("injections.scm");
    if !injections_query_path.exists() {
        println!("\x1b[36mskipping loading of injections for {lang_name:?} since {injections_query_path:?} does not exist\x1b[0m");
    }
    let locals_query_path = grammar_dir.join("locals.scm");
    if !locals_query_path.exists() {
        println!("\x1b[36mskipping loading of locals for {lang_name:?} since {locals_query_path:?} does not exist\x1b[0m");
    }
    LanguageConfig::new(
        grammar,
        &overwrites.highlights.clone().unwrap_or_else(|| {
            fs::read_to_string(&highlights_query_path)
                .map_err(|err| {
                    format!(
                        "failed to read highlights in {}: {err}",
                        highlights_query_path.display()
                    )
                })
                .unwrap()
        }),
        &overwrites
            .injections
            .clone()
            .unwrap_or_else(|| fs::read_to_string(&injections_query_path).unwrap_or_default()),
        &overwrites
            .locals
            .clone()
            .unwrap_or_else(|| fs::read_to_string(&locals_query_path).unwrap_or_default()),
    )
    .unwrap()
}

#[derive(Debug)]
struct TestLanguageLoader {
    languages: IndexMap<String, Language>,
    lang_config: Box<[OnceCell<LanguageConfig>]>,
    overwrites: Box<[Overwrites]>,
    test_theme: RefCell<IndexSet<String>>,
    /// If set, only scopes in this set receive a highlight; others return `None`.
    highlight_filter: Option<HashSet<String>>,
}

impl TestLanguageLoader {
    fn new() -> Self {
        let grammars = &GRAMMARS;

        Self {
            lang_config: (0..grammars.len()).map(|_| OnceCell::new()).collect(),
            overwrites: vec![Overwrites::default(); grammars.len()].into_boxed_slice(),
            test_theme: RefCell::default(),
            highlight_filter: None,
            languages: grammars
                .iter()
                .enumerate()
                .map(|(i, grammar)| {
                    (
                        grammar.file_name().unwrap().to_str().unwrap().to_owned(),
                        Language::new(i as u32),
                    )
                })
                .collect(),
        }
    }

    fn get(&self, name: &str) -> Language {
        self.languages[name]
    }

    // TODO: remove on first use.
    #[allow(dead_code)]
    fn overwrite_injections(&mut self, lang: &str, content: String) {
        let lang = self.get(lang);
        self.overwrites[lang.idx()].injections = Some(content);
        self.lang_config[lang.idx()] = OnceCell::new();
    }

    fn overwrite_highlights(&mut self, lang: &str, content: String) {
        let lang = self.get(lang);
        self.overwrites[lang.idx()].highlights = Some(content);
        self.lang_config[lang.idx()] = OnceCell::new();
    }

    fn shadow_injections(&mut self, lang: &str, content: &str) {
        let lang = self.get(lang);
        let skidder_config = skidder_config();
        let grammar = self.languages.get_index(lang.idx()).unwrap().0;
        let grammar_dir = skidder_config.grammar_dir(grammar).unwrap();
        let mut injections =
            fs::read_to_string(grammar_dir.join("injections.scm")).unwrap_or_default();
        injections.push('\n');
        injections.push_str(content);
        self.overwrites[lang.idx()].injections = Some(injections);
        self.lang_config[lang.idx()] = OnceCell::new();
    }

    /// Restrict the test theme so only the given scopes receive a highlight index.
    /// All other scopes will return `None` from the configure callback.
    fn set_highlight_filter(&mut self, scopes: impl IntoIterator<Item = impl Into<String>>) {
        self.highlight_filter = Some(scopes.into_iter().map(Into::into).collect());
    }

    fn shadow_highlights(&mut self, lang: &str, content: &str) {
        let lang = self.get(lang);
        let skidder_config = skidder_config();
        let grammar = self.languages.get_index(lang.idx()).unwrap().0;
        let grammar_dir = skidder_config.grammar_dir(grammar).unwrap();
        let mut highlights = fs::read_to_string(grammar_dir.join("highlights.scm")).unwrap();
        highlights.push('\n');
        highlights.push_str(content);
        self.overwrites[lang.idx()].highlights = Some(highlights);
        self.lang_config[lang.idx()] = OnceCell::new();
    }
}

impl LanguageLoader for TestLanguageLoader {
    fn language_for_marker(&self, marker: InjectionLanguageMarker) -> Option<Language> {
        match marker {
            InjectionLanguageMarker::Name(name) => self.languages.get(name).copied(),
            InjectionLanguageMarker::Match(text) => {
                let name: Cow<str> = text.into();
                self.languages.get(name.as_ref()).copied()
            }
            _ => unimplemented!(),
        }
    }

    fn get_config(&self, lang: Language) -> Option<&LanguageConfig> {
        let config = self.lang_config[lang.idx()].get_or_init(|| {
            let config = get_grammar(
                self.languages.get_index(lang.idx()).unwrap().0,
                &self.overwrites[lang.idx()],
            );
            let mut theme = self.test_theme.borrow_mut();
            config.configure(|scope| {
                if let Some(ref allowed) = self.highlight_filter {
                    if !allowed.contains(scope) {
                        return None;
                    }
                }
                Some(Highlight::new(theme.insert_full(scope.to_owned()).0 as u32))
            });
            config
        });
        Some(config)
    }
}

fn lang_for_path(path: &Path, loader: &TestLanguageLoader) -> Language {
    match path
        .extension()
        .and_then(|it| it.to_str())
        .unwrap_or_default()
    {
        "rs" => loader.get("rust"),
        "html" => loader.get("html"),
        "css" => loader.get("css"),
        "erl" => loader.get("erlang"),
        "md" => loader.get("markdown"),
        extension => panic!("unknown file type .{extension}"),
    }
}

fn highlight_fixture(loader: &TestLanguageLoader, fixture: impl AsRef<Path>) {
    let path = Path::new("../fixtures").join(fixture);
    let lang = lang_for_path(&path, loader);
    check_highlighter_fixture(
        path,
        "// ",
        lang,
        loader,
        |highlight| loader.test_theme.borrow()[highlight.idx()].clone(),
        |_| ..,
    )
}

fn injection_fixture(loader: &TestLanguageLoader, fixture: impl AsRef<Path>) {
    let path = Path::new("../fixtures").join(fixture);
    let lang = lang_for_path(&path, loader);
    check_injection_fixture(
        path,
        "// ",
        lang,
        loader,
        |lang| loader.languages.get_index(lang.idx()).unwrap().0.clone(),
        |_| ..,
    )
}

#[test]
fn highlight() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/hello_world.rs");
}

#[test]
fn layers() {
    let loader = TestLanguageLoader::new();

    let input = "/// Says hello.
///
/// this is *markdown-inline* markdown
///
/// # Example
///
/// ```rust
/// fn add(left: usize, right: usize) -> usize {
///     left + right
/// }
/// ```
pub fn hello() {}";

    let syntax = Syntax::new(input.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();

    let assert_injection = |snippet: &str, expected: &[&str]| {
        assert!(!expected.is_empty(), "all layers have at least 1 injection");

        let layer_lang_name = |layer: Layer| {
            loader
                .languages
                .get_index(syntax.layer(layer).language.idx())
                .unwrap()
                .0
                .clone()
        };

        let snippet_start = input.find(snippet).unwrap() as u32;
        let snippet_end = snippet_start + snippet.len() as u32;

        let layers = syntax
            .layers_for_byte_range(snippet_start, snippet_end)
            .map(layer_lang_name)
            .collect::<Vec<_>>();

        assert_eq!(&layers, expected, r#"snippet: "{snippet}""#);

        let layer = syntax.layer_for_byte_range(snippet_start, snippet_end);
        assert_eq!(
            &layer_lang_name(layer),
            expected.last().unwrap(),
            "last layer is the smallest fully encompassing layer"
        );
    };

    // Rust function in a code block in the rust documentation
    assert_injection("fn add(left: usize, ri", &["rust", "markdown", "rust"]);

    // Markdown heading  `# Example`
    assert_injection("# Example", &["rust", "markdown"]);

    // Outer-most Rust function `hello`
    assert_injection("pub fn hello() {}", &["rust"]);

    // Paragraph in the rust documentation
    assert_injection("markdown-inline", &["rust", "markdown", "markdown-inline"]);
}

#[test]
fn layers_for_byte_range_injection_boundary() {
    let mut loader = TestLanguageLoader::new();
    // Inject into each block comment as a separate (non-combined) markdown injection.
    loader.overwrite_injections(
        "rust",
        r#"((block_comment) @injection.content
 (#set! injection.language "markdown")
 (#set! injection.include-children))"#
            .to_string(),
    );
    // Two adjacent block comments share a boundary at the byte where the first ends
    // and the second begins. The boundary byte belongs to the second injection.
    let input = "/* first *//* second */";
    let syntax = Syntax::new(input.into(), loader.get("rust"), PARSE_TIMEOUT, &loader).unwrap();

    let second_start = input.find("/* second */").unwrap() as u32;

    let root = syntax.layer(syntax.root());
    // The first injection covers 0..second_start; the second starts at second_start.
    let first_inj_layer = root.injection_at_byte_idx(second_start - 1).unwrap().layer;
    let second_inj_layer = root.injection_at_byte_idx(second_start).unwrap().layer;

    let layers: Vec<_> = syntax
        .layers_for_byte_range(second_start, second_start)
        .collect();
    // Querying at the boundary byte reaches the second injection's layer, not the first.
    assert!(layers.contains(&second_inj_layer));
    assert!(!layers.contains(&first_inj_layer));
}

#[test]
fn highlight_overlaps_with_injection() {
    let loader = TestLanguageLoader::new();
    // The comment node is highlighted both by the comment capture and as an injection for the
    // comment grammar.
    highlight_fixture(&loader, "highlighter/comment.html");
}

#[test]
fn rust_parameter_locals() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/rust_parameter_locals.rs");
}

#[test]
fn codefence_rust_doc_comments() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/codefence_rust_doc_comments.md");
}

#[test]
fn parameters_within_injections_within_injections() {
    let loader = TestLanguageLoader::new();
    // The root language is Rust. Then markdown is injected in a doc comment. Then within that
    // we have a code fence which is Rust again. Within that block we check that locals are
    // highlighted as expected.
    highlight_fixture(&loader, "highlighter/injectionception.rs");
    injection_fixture(&loader, "injections/injectionception.rs");
}

#[test]
fn html_in_edoc_in_erlang() {
    let loader = TestLanguageLoader::new();
    // This fixture exhibited a bug (which has been fixed) where a combined injection became
    // dormant at the same time as a new highlight started, causing a total reset of all
    // highlights (incorrectly).
    highlight_fixture(&loader, "highlighter/html_in_edoc_in_erlang.erl");
    injection_fixture(&loader, "injections/html_in_edoc_in_erlang.erl");
}

#[test]
fn non_local_pattern() {
    let mut loader = TestLanguageLoader::new();
    // Pretend that `this` is a builtin like `self`, but only when it is not a parameter.
    loader.shadow_highlights(
        "rust",
        r#"
((identifier) @variable.builtin
 (#eq? @variable.builtin "this")
 (#is-not? local))
"#,
    );
    highlight_fixture(&loader, "highlighter/non_local.rs");
}

#[test]
fn reference_highlight_starts_after_definition_ends() {
    let loader = TestLanguageLoader::new();
    // In this example the function name matches one of the parameters. The function name can be
    // a reference but since the definition occurs after the function name it, the function name
    // should not be highlighted as a parameter.
    highlight_fixture(
        &loader,
        "highlighter/reference_highlight_starts_after_definition_ends.rs",
    );
}

#[test]
fn combined_injection() {
    let mut loader = TestLanguageLoader::new();
    loader.shadow_injections(
        "rust",
        r#"
((doc_comment) @injection.content
 (#set! injection.language "markdown")
 (#set! injection.combined))"#,
    );
    highlight_fixture(&loader, "highlighter/rust_doc_comment.rs");
}

#[test]
fn injection_in_child() {
    let mut loader = TestLanguageLoader::new();
    // here doc_comment is a child of line_comment which has higher precedence
    // however since it doesn't include children the doc_comment injection is
    // still active here. This could probably use a more real world use case (maybe nix?)
    loader.shadow_injections(
        "rust",
        r#"
([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment"))

([(line_comment (doc_comment) @injection.content) (block_comment (doc_comment) @injection.content)]
 (#set! injection.language "markdown")
 (#set! injection.combined))
"#,
    );
    highlight_fixture(&loader, "highlighter/rust_doc_comment.rs");
    injection_fixture(&loader, "injections/rust_doc_comment.rs");
}

#[test]
fn injection_precedence() {
    let mut loader = TestLanguageLoader::new();
    loader.shadow_injections(
        "rust",
        r#"
([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment")
 (#set! injection.include-children))

([(line_comment (doc_comment) @injection.content) (block_comment (doc_comment) @injection.content)]
 (#set! injection.language "markdown")
 (#set! injection.combined))
 "#,
    );
    highlight_fixture(&loader, "highlighter/rust_doc_comment.rs");

    loader.shadow_injections(
        "rust",
        r#"
([(line_comment (doc_comment) @injection.content) (block_comment (doc_comment) @injection.content)]
 (#set! injection.language "markdown")
 (#set! injection.combined))

([(line_comment) (block_comment)] @injection.content
 (#set! injection.language "comment")
 (#set! injection.include-children))
 "#,
    );
    highlight_fixture(&loader, "highlighter/rust_no_doc_comment.rs");
    injection_fixture(&loader, "injections/rust_no_doc_comment.rs");

    loader.shadow_injections(
        "rust",
        r#"
((macro_invocation
   macro:
     [
       (scoped_identifier
         name: (_) @_macro_name)
       (identifier) @_macro_name
     ]
   (token_tree . (_) . (_) . (string_literal) @injection.content))
 (#any-of? @_macro_name
  ; std
  "some_macro")
  (#set! injection.language "rust")
  (#set! injection.include-children))
    "#,
    );
    injection_fixture(&loader, "injections/overlapping_injection.rs");
}

#[test]
fn rust_nested_vec() {
    let loader = TestLanguageLoader::new();
    highlight_fixture(&loader, "highlighter/nested_vec.rs");
    injection_fixture(&loader, "injections/nested_vec.rs");
}

#[test]
fn edit_remove_and_add_injection_layer() {
    let loader = TestLanguageLoader::new();
    // Add another backtick, causing the double old backtick to become a codefence and the second
    // HTML comment to become the codefence's body.
    // When we reuse the injection for the HTML comments, we need to be sure to re-parse the HTML
    // layer so that it recognizes that the second comment is no longer valid.
    let before_text = "<!---->\n``\n<!---->";
    let after_text = "<!---->\n```\n<!---->";
    let edit = InputEdit {
        start_byte: 10,
        old_end_byte: 10,
        new_end_byte: 11,
        start_point: Point::ZERO,
        old_end_point: Point::ZERO,
        new_end_point: Point::ZERO,
    };
    let mut syntax = Syntax::new(
        before_text.into(),
        loader.get("markdown"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();
    // The test here is that `Syntax::update` can apply the edit `Ok(_)` without panicking.
    syntax
        .update(after_text.into(), PARSE_TIMEOUT, &[edit], &loader)
        .unwrap();

    // Now test the inverse. Start with the after text and edit it to be the before text. In this
    // case an injection is added for the HTML comment.
    let edit = InputEdit {
        start_byte: 10,
        old_end_byte: 11,
        new_end_byte: 10,
        start_point: Point::ZERO,
        old_end_point: Point::ZERO,
        new_end_point: Point::ZERO,
    };
    let mut syntax = Syntax::new(
        after_text.into(),
        loader.get("markdown"),
        PARSE_TIMEOUT,
        &loader,
    )
    .unwrap();
    // The test here is that `Syntax::update` can apply the edit `Ok(_)` without panicking.
    syntax
        .update(before_text.into(), PARSE_TIMEOUT, &[edit], &loader)
        .unwrap();
}

#[test]
fn markdown_bold_highlight() {
    let loader = TestLanguageLoader::new();
    // This is a very simple case to check that adjacent equivalent highlights are merged
    // properly: the `punctuation.bracket` highlight on the consecutive `*`s should be combined
    // into one span.
    highlight_fixture(&loader, "highlighter/markdown_bold.md");
}

#[test]
fn css_parent_child_highlight_precedence() {
    let mut loader = TestLanguageLoader::new();
    // NOTE: the pattern being tested here `((color_value) "#") @string.special` is odd and should
    // be rewritten to `(color_value "#" @string.special)` - that was probably the original intent
    // of the pattern. We overwrite the highlights to take the parts we need for this case so that
    // if/when we fix that pattern in the future it does not break this test case.
    loader.overwrite_highlights(
        "css",
        r##"
((property_name) @variable
 (#match? @variable "^--"))

"#" @punctuation
((color_value) "#") @string.special
(color_value) @string.special

[";" ":"] @punctuation.delimiter
"##
        .to_string(),
    );

    // In this case two patterns fight over the `#` character and both should actually highlight
    // it. Because of the odd way that the pattern `((color_value) "#") @string.special` is
    // written, the QueryIter yields the captures in the opposite order it would normally:
    // first the child pattern `{Node # 9..10}` and then `{Node color_value 9..13}`.
    //
    // This case checks the invariant that "`active_highlights` ends are sorted descending" is
    // preserved.
    highlight_fixture(&loader, "highlighter/parent_child_highlight_precedence.css");
}

#[test]
fn edoc_code_combined_injection() {
    let loader = TestLanguageLoader::new();

    highlight_fixture(&loader, "highlighter/edoc_code_combined_injection.erl");
    injection_fixture(&loader, "injections/edoc_code_combined_injection.erl");
}

#[test]
fn edoc_code_combined_injection_in_markdown() {
    let loader = TestLanguageLoader::new();
    // Same as the above but within markdown to add extra layers.
    highlight_fixture(
        &loader,
        "highlighter/edoc_code_combined_injection_in_markdown.md",
    );
}

#[test]
fn combined_injection_no_invalid_ranges() {
    // Regression test for https://github.com/helix-editor/tree-house/issues/35.
    //
    // A Rust file with multiple block doc-comments and a line doc-comment in an impl block
    // used to trigger Error::InvalidRanges when computing injection ranges for the combined
    // markdown layer, because intersect_ranges_impl could produce ranges where start > end.
    let mut loader = TestLanguageLoader::new();
    loader.shadow_injections(
        "rust",
        r#"
((doc_comment) @injection.content
 (#set! injection.language "markdown")
 (#set! injection.combined))"#,
    );
    let input = "/**
 * foo
 */

impl Foo {
    /**
     * foo
     */

    /// foo
}";
    let result = Syntax::new(input.into(), loader.get("rust"), PARSE_TIMEOUT, &loader);
    assert!(
        result.is_ok(),
        "parsing should succeed without Error::InvalidRanges, got: {:?}",
        result.err()
    );
}

#[test]
fn highlighter_no_panic_overlapping_captures_missing_theme() {
    // Regression test for https://github.com/helix-editor/helix/issues/14751.
    //
    // When multiple captures match the same node and some have no highlight in the
    // current theme, `start_highlight` can remove entries that were present in
    // `active_highlights` before the current `advance()` call began. The subsequent
    // slice `active_highlights[prev_stack_size..]` then panics.
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_highlights(
        "python",
        r#"
(attribute attribute: (identifier) @variable.other.member)
((identifier) @type   (#match? @type "^[A-Z]"))
(decorator) @function
((identifier) @constant  (#match? @constant "^_*[A-Z][A-Z\d_]*$"))
"#
        .to_string(),
    );
    // `@type` and `@constant` both match `B` but resolve to None in this theme.
    // Each None-returning capture removes the previously-added highlight for `B`,
    // and the third removal walks back into highlights added by a prior advance() call.
    loader.set_highlight_filter(["function", "variable.other.member"]);

    let source = "@a.B";
    let syntax = Syntax::new(source.into(), loader.get("python"), PARSE_TIMEOUT, &loader)
        .expect("parse should succeed");
    let mut highlighter = Highlighter::new(&syntax, source.into(), &loader, 0..);
    while highlighter.next_event_offset() != u32::MAX {
        highlighter.advance();
    }
}

#[test]
fn highlighter_ancestor_highlight_not_replaced_by_child_capture() {
    // Regression test for the sibling bug in start_highlight: when the sequence of
    // captures for a child node is Some/None/Some, and the child shares an end
    // byte with an ancestor highlight from a prior advance() call, the old code
    // would find the ancestor via rposition and replace it with the last Some capture.
    // The ancestor's highlight would silently vanish from active_highlights, and the
    // Push event for the child would be empty.
    //
    // The fix scopes the rposition search to highlights added in the current advance()
    // call only. The ancestor is invisible to the search, so the last Some capture is
    // pushed as a new entry alongside the ancestor.
    let mut loader = TestLanguageLoader::new();
    loader.overwrite_highlights(
        "python",
        r#"
(decorator) @function
(attribute attribute: (identifier) @variable.other.member)
((identifier) @type     (#match? @type     "^[A-Z]"))
((identifier) @constant (#match? @constant "^[A-Z]"))
"#
        .to_string(),
    );
    // For `B` in `@a.B`:
    //   @variable.other.member: Some (push)
    //   @type:                  None (remove, not in theme)
    //   @constant:              Some (old: replaces ancestor @function; new: pushes alongside)
    loader.set_highlight_filter(["function", "variable.other.member", "constant"]);

    let source = "@a.B";
    let syntax = Syntax::new(source.into(), loader.get("python"), PARSE_TIMEOUT, &loader)
        .expect("parse should succeed");
    let mut highlighter = Highlighter::new(&syntax, source.into(), &loader, 0..);

    // Advance past events before `B` (offset 3).
    let offset_of_b = source.find('B').unwrap() as u32;
    while highlighter.next_event_offset() < offset_of_b {
        highlighter.advance();
    }
    assert_eq!(highlighter.next_event_offset(), offset_of_b);

    let (event, pushed) = highlighter.advance();
    assert_eq!(event, HighlightEvent::Push);
    // The @constant highlight should be pushed as a new entry.
    assert_eq!(
        pushed.len(),
        1,
        "expected exactly one new highlight pushed for `B`"
    );
    // The ancestor @function highlight must still be present — it was not replaced.
    assert_eq!(
        highlighter.active_highlights().len(),
        2,
        "both the decorator's @function and `B`'s @constant should be active"
    );
}

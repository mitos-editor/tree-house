# `tree-house`

This repository contains a number of crates used by the [Helix editor](https://github.com/helix-editor/helix) for integration with the [tree-sitter](https://github.com/tree-sitter/tree-sitter) C library.

Most notably the highlighter crate [`tree-house`](https://crates.io/crates/tree-house) provides Helix's syntax highlighting and all other tree-sitter features since the 25.07 release. The highlighter was rewritten from scratch for simplification and to fix a number of bugs. Read more in the [25.07 release highlights](https://helix-editor.com/news/release-25-07-highlights/#tree-house).

Documentation is a work-in-progress and these crates may see breaking changes as we expand our use of Tree-sitter in Helix.

* `bindings/` contains the `tree-house-bindings` crate which provides Rust bindings over the C library and optional integration with the [Ropey](https://github.com/cessen/ropey) rope crate.
* `highlighter/` contains the `tree-house` crate which exposes a robust highlighter and query iterator for working across [injections].
* `skidder/` contains the `skidder` crate which exposes utilities for building a package repository for tree-sitter grammars.
* `cli/` contains the `skidder-cli` crate which wraps `skidder` in a command line interface.
### Predicate-derived query prefilters

The query engine derives necessary literals from ordinary positive text
predicates: `#eq?`, `#any-of?`, `#match?` and their existential `#any-*` forms.
No custom optimization properties or language-specific Rust code are needed.
For example, a query for slash-prefixed comments can use a normal predicate:

```scheme
((comment) @injection.content
 (#match? @injection.content "^/")
 (#set! injection.language "comment"))
```

This predicate retains its ordinary meaning. The compiler can additionally prove
that a match requires `/`, and query cursors can reject a slash-free subtree
before walking its nodes. A present literal only permits normal execution;
all predicates, capture ordering and query semantics are still checked.

Each pattern contributes a necessary condition from a positive predicate.
Patterns are alternatives, so a whole query can be rejected only when all of
its alternatives are impossible. One pattern without a sound, nonempty literal
requirement disables rejection for the query. Negated predicates and
capture-to-capture comparisons supply no literal requirement. Predicates over
all captures require a nonoptional capture; existential predicates already
require at least one capture. Tree-sitter's public capture-quantifier API
provides this distinction.

Regex predicates are parsed once. Their HIR builds the ordinary matcher and
feeds `regex-syntax`'s conservative literal extractor. Anchors, boundaries,
Unicode case folding, alternatives and empty matches are handled by that
library, not by inspecting regex strings. Extraction is bounded to 64 literals
of at most 64 bytes each, with at most 4 KiB of literal payload. Unsupported,
empty, over-budget or truncated UTF-8 requirements retain full execution.

Compiled plans are immutable. Single-byte requirements use a direct chunk scan;
other literal sets use the existing cursor-based regex engine, including
matches spanning chunks. Scans cover the queried node's entire byte span,
not just the cursor's narrower capture range, and never flatten the source.
Disabling captures discards the plan because it can make an all-captures
predicate vacuously true. Removing patterns leaves its conservative proof valid.

This optimization belongs to the generic query engine and is independent of
injection discovery and incremental query-result caching. Language packages
continue to own ordinary query semantics. An opaque node name such as
`comment` does not by itself prove a particular delimiter.

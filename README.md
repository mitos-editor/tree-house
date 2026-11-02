# `tree-house`

This repository contains a number of crates used by the [Helix editor](https://github.com/helix-editor/helix) for integration with the [tree-sitter](https://github.com/tree-sitter/tree-sitter) C library.

Most notably the highlighter crate [`tree-house`](https://crates.io/crates/tree-house) provides Helix's syntax highlighting and all other tree-sitter features since the 25.07 release. The highlighter was rewritten from scratch for simplification and to fix a number of bugs. Read more in the [25.07 release highlights](https://helix-editor.com/news/release-25-07-highlights/#tree-house).

Documentation is a work-in-progress and these crates may see breaking changes as we expand our use of Tree-sitter in Helix.

* `bindings/` contains the `tree-house-bindings` crate which provides Rust bindings over the C library and optional integration with the [Ropey](https://github.com/cessen/ropey) rope crate.
* `highlighter/` contains the `tree-house` crate which exposes a robust highlighter and query iterator for working across [injections].
* `skidder/` contains the `skidder` crate which exposes utilities for building a package repository for tree-sitter grammars.
* `cli/` contains the `skidder-cli` crate which wraps `skidder` in a command line interface.
### Necessary-byte prefilter for injection queries

Injection patterns can declare `(#set! injection.required-byte "/")`. This is
an assertion by the query author: **every match must contain that ASCII byte
within its matched subtree**. It is an optimization hint, not an extra predicate.
Slash-delimited comments can require `/`, Python comments can require `#`, and
HTML comment/script/style patterns can require `<`. No language names or node
kinds are special-cased in the highlighter.

For example, these two Python injection patterns have different necessary bytes:

```scheme
((comment) @injection.content
 (#set! injection.required-byte "#")
 (#set! injection.language "comment"))

((string (string_content) @injection.content)
 (#match? @injection.content "^fn ")
 (#set! injection.required-byte "f")
 (#set! injection.language "rust"))
```

A query is an alternative between patterns. Its compiled, immutable prefilter
therefore permits execution when **any** pattern's required byte is present.
It rejects execution only if **all** alternatives are impossible. One unannotated
pattern disables rejection for the whole query. Empty queries also retain the
normal path. When a marker is present, the original query runs with all its
patterns, predicates, precedence and combined-injection behavior intact.

Compilation deduplicates markers into a fixed-size ASCII set. A single marker
uses the optimized single-byte scan; multiple markers share one pass. Scanning
uses rope chunks within the queried subtree's byte span, without flattening the
source or repeatedly scanning unrelated parts of a containing document. Gaps
between combined injection ranges are included conservatively. A rejected query
still goes through normal reconciliation to remove obsolete injection layers.

The value must be exactly one ASCII byte. Repeating the same value is allowed;
conflicting values on one pattern are rejected. If no necessary byte can be
proved for every match of a pattern, leave it unannotated. An incorrect hint can
hide valid injections, so new annotations need comparisons against the original
query through edits, false-positive markers and nested languages. The test suite
checks complete highlight events and injection ranges for Python, HTML/CSS and
Rust, as well as rope chunk boundaries, Unicode and unannotated alternatives.

The highlighter owns prefilter compilation and execution; language packages own
the assertions in their query files. The mechanism does not infer lexical rules
from node names or mutate shared query patterns. Automatic inference would need
a separate soundness model for optional captures, negated predicates and regex
anchors; it is not part of this contract.

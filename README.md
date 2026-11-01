# `tree-house`

This repository contains a number of crates used by the [Helix editor](https://github.com/helix-editor/helix) for integration with the [tree-sitter](https://github.com/tree-sitter/tree-sitter) C library.

Most notably the highlighter crate [`tree-house`](https://crates.io/crates/tree-house) provides Helix's syntax highlighting and all other tree-sitter features since the 25.07 release. The highlighter was rewritten from scratch for simplification and to fix a number of bugs. Read more in the [25.07 release highlights](https://helix-editor.com/news/release-25-07-highlights/#tree-house).

Documentation is a work-in-progress and these crates may see breaking changes as we expand our use of Tree-sitter in Helix.

* `bindings/` contains the `tree-house-bindings` crate which provides Rust bindings over the C library and optional integration with the [Ropey](https://github.com/cessen/ropey) rope crate.
* `highlighter/` contains the `tree-house` crate which exposes a robust highlighter and query iterator for working across [injections].
* `skidder/` contains the `skidder` crate which exposes utilities for building a package repository for tree-sitter grammars.
* `cli/` contains the `skidder-cli` crate which wraps `skidder` in a command line interface.
### Necessary-byte prefilter for injection queries

An injection pattern can declare a necessary ASCII byte with
`(#set! injection.required-byte "/")`. This is a query-author assertion: every
match of that pattern must require that byte somewhere in the source. It is not
an extra predicate. For example, slash-delimited comment patterns can require
`/`; a pattern matching arbitrary strings cannot.

When every pattern declares the same byte and that byte is absent from the
source, Tree-house can skip the tree query. Otherwise it executes the original
query, including all predicates. Unannotated patterns and queries with different
requirements retain the ordinary path. The source is scanned without flattening
the rope, and an empty result still removes obsolete injection layers on edits.

# `tree-house`

This repository contains a number of crates used by the [Helix editor](https://github.com/helix-editor/helix) for integration with the [tree-sitter](https://github.com/tree-sitter/tree-sitter) C library.

Most notably the highlighter crate [`tree-house`](https://crates.io/crates/tree-house) provides Helix's syntax highlighting and all other tree-sitter features since the 25.07 release. The highlighter was rewritten from scratch for simplification and to fix a number of bugs. Read more in the [25.07 release highlights](https://helix-editor.com/news/release-25-07-highlights/#tree-house).

Documentation is a work-in-progress and these crates may see breaking changes as we expand our use of Tree-sitter in Helix.

* `bindings/` contains the `tree-house-bindings` crate which provides Rust bindings over the C library and optional integration with the [Ropey](https://github.com/cessen/ropey) rope crate.
* `highlighter/` contains the `tree-house` crate which exposes a robust highlighter and query iterator for working across [injections].
* `skidder/` contains the `skidder` crate which exposes utilities for building a package repository for tree-sitter grammars.
* `cli/` contains the `skidder-cli` crate which wraps `skidder` in a command line interface.

### Incremental syntax queries

After an edit, injection and local-scope queries can reuse captures from
unchanged direct children of a syntax layer. Edited children are queried again;
captures in moved children get updated byte ranges. Scope construction and
injection reconciliation still run in full.

Queries that depend on parent or sibling context use the original full query.
So do root captures, multiple injection-content captures and cases that exceed
the cache limits: 4,096 children and 65,536 events per layer/query. Query changes,
changed included ranges and skipped updates invalidate cached results.

The cache checks compiled Tree-sitter steps through `bindings/src/extended.c`.
Review that helper when updating the vendored library.

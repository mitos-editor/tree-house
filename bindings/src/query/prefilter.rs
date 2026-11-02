//! Skip a query only when none of its patterns can match the searched node.
//! Each pattern supplies literals required by one positive text predicate.
//! Finding a literal permits the normal query to run; it does not prove a match.
use std::ops::Range;

use regex_cursor::{engines::meta::Regex, Cursor};
use regex_syntax::hir::{literal::Extractor, Hir};

use crate::Input;

// Keep optional prefilter compilation small. Exceeding these limits runs the full query.
const MAX_LITERALS: usize = 64;
const MAX_LITERAL_LEN: usize = 64;
const MAX_LITERAL_BYTES: usize = MAX_LITERALS * MAX_LITERAL_LEN;

pub(super) type Literals = Vec<Vec<u8>>;

/// Collect requirements from every alternative in a query.
/// `None` means at least one pattern has no usable requirement.
pub(super) struct Builder(Option<Literals>);
impl Builder {
    pub(super) fn new() -> Self {
        Self(Some(Vec::new()))
    }
    /// Once a pattern prevents prefiltering, later patterns cannot restore it.
    pub(super) fn enabled(&self) -> bool {
        self.0.is_some()
    }
    /// Any one pattern can match, so retain literals from all of them.
    pub(super) fn pattern(&mut self, required: Option<Literals>) {
        self.0 = self.0.take().zip(required).and_then(|(mut all, next)| {
            if all.len() + next.len() > MAX_LITERALS {
                return None;
            }
            all.extend(next);
            Some(all)
        });
    }
    pub(super) fn finish(self) -> Option<Prefilter> {
        let mut literals = self.0?;
        literals.sort_unstable();
        literals.dedup();
        match literals.as_slice() {
            [] => None,
            [literal] if literal.len() == 1 => Some(Prefilter::Byte(literal[0])),
            _ => {
                // Extraction can truncate a UTF-8 literal inside a codepoint.
                // Do not feed such byte prefixes to a UTF-8 regex matcher.
                if literals
                    .iter()
                    .any(|literal| std::str::from_utf8(literal).is_err())
                {
                    return None;
                }
                let hirs: Vec<_> = literals.into_iter().map(Hir::literal).collect();
                // An optional optimization must not fail an otherwise valid query.
                Regex::builder()
                    .build_many_from_hir(&hirs)
                    .ok()
                    .map(Prefilter::Literals)
            }
        }
    }
}

pub(super) fn literals<'a>(values: impl IntoIterator<Item = &'a [u8]>) -> Option<Literals> {
    let mut result = Vec::new();
    let mut bytes = 0;
    for value in values {
        bytes += value.len();
        if value.is_empty()
            || value.len() > MAX_LITERAL_LEN
            || result.len() == MAX_LITERALS
            || bytes > MAX_LITERAL_BYTES
        {
            return None;
        }
        result.push(value.to_vec());
    }
    (!result.is_empty()).then_some(result)
}

pub(super) fn regex_literals(hir: &Hir) -> Option<Literals> {
    // A capture can start inside the searched node. Extraction ignores anchors
    // and boundaries here; the original regex checks them on the capture later.
    let sequence = Extractor::new()
        .limit_total(MAX_LITERALS)
        .limit_literal_len(MAX_LITERAL_LEN)
        .extract(hir);
    literals(
        sequence
            .literals()?
            .iter()
            .map(|literal| literal.as_bytes()),
    )
}

#[derive(Debug)]
pub(super) enum Prefilter {
    Byte(u8),
    Literals(Regex),
}
impl Prefilter {
    /// `false` rules out a match; `true` still requires normal query execution.
    pub(crate) fn may_match(&self, input: &mut impl Input, range: Range<u32>) -> bool {
        let cursor = input.cursor_at(range.start);
        match self {
            Self::Literals(regex) => {
                let mut input = regex_cursor::Input::new(cursor);
                input.slice(range.start as usize..range.end as usize);
                regex.is_match(input)
            }
            Self::Byte(byte) => loop {
                let offset = cursor.offset();
                let chunk = cursor.chunk();
                let start = (range.start as usize)
                    .saturating_sub(offset)
                    .min(chunk.len());
                let end = (range.end as usize).saturating_sub(offset).min(chunk.len());
                if chunk[start..end].contains(byte) {
                    return true;
                }
                if offset + chunk.len() >= range.end as usize || !cursor.advance() {
                    return false;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Chunks<'a> {
        bytes: &'a [u8],
        split: usize,
        second: bool,
    }
    impl Cursor for Chunks<'_> {
        fn chunk(&self) -> &[u8] {
            if self.second {
                &self.bytes[self.split..]
            } else {
                &self.bytes[..self.split]
            }
        }
        fn advance(&mut self) -> bool {
            if self.second {
                false
            } else {
                self.second = true;
                true
            }
        }
        fn backtrack(&mut self) -> bool {
            if self.second {
                self.second = false;
                true
            } else {
                false
            }
        }
        fn total_bytes(&self) -> Option<usize> {
            Some(self.bytes.len())
        }
        fn offset(&self) -> usize {
            if self.second {
                self.split
            } else {
                0
            }
        }
    }
    struct ChunkInput<'a>(Chunks<'a>);
    impl<'a> Input for ChunkInput<'a> {
        type Cursor = Chunks<'a>;
        fn cursor_at(&mut self, offset: u32) -> &mut Self::Cursor {
            self.0.second = offset as usize >= self.0.split;
            &mut self.0
        }
        fn eq(&mut self, a: Range<u32>, b: Range<u32>) -> bool {
            self.0.bytes[a.start as usize..a.end as usize]
                == self.0.bytes[b.start as usize..b.end as usize]
        }
    }

    fn compile(pattern: &str) -> Option<Prefilter> {
        let mut builder = Builder::new();
        builder.pattern(regex_literals(
            &regex_syntax::Parser::new().parse(pattern).unwrap(),
        ));
        builder.finish()
    }

    #[test]
    fn anchors_unicode_case_folding_and_alternatives_are_conservative_across_chunks() {
        for (pattern, value) in [
            ("^needle$", "needle"),
            (r"\bneedle\b", "needle"),
            ("^(foo|bar)$", "bar"),
            ("(?i)^kelvin$", "KELVIN"),
            ("^café$", "café"),
            ("^日本語$", "日本語"),
        ] {
            let filter = compile(pattern).expect("a bounded necessary literal");
            let haystack = format!("prefix {value} suffix");
            for (split, _) in haystack.char_indices().skip(1) {
                let mut input = ChunkInput(Chunks {
                    bytes: haystack.as_bytes(),
                    split,
                    second: false,
                });
                assert!(
                    filter.may_match(&mut input, 0..haystack.len() as u32),
                    "{pattern}: chunk at {split}"
                );
                assert!(
                    !filter.may_match(&mut input, 0..6),
                    "the marker is outside the searched subtree"
                );
            }
        }
    }

    #[test]
    fn unknown_empty_and_over_budget_requirements_disable_rejection() {
        for pattern in ["^$", "a?", "a*", ".*", "[a-z]+"] {
            assert!(compile(pattern).is_none(), "{pattern}");
        }
        assert!(
            compile(&"日本語".repeat(16)).is_none(),
            "a truncated UTF-8 prefix must fall back"
        );
        let mut unknown = Builder::new();
        unknown.pattern(literals([b"needle".as_slice()]));
        unknown.pattern(None);
        assert!(!unknown.enabled());
        assert!(unknown.finish().is_none());
        assert!(Builder::new().finish().is_none());
        assert!(literals([b"".as_slice()]).is_none());
        assert!(literals([vec![b'x'; MAX_LITERAL_LEN + 1].as_slice()]).is_none());
        let mut bounded = Builder::new();
        for _ in 0..=MAX_LITERALS {
            bounded.pattern(literals([b"needle".as_slice()]));
        }
        assert!(bounded.finish().is_none());
    }
}

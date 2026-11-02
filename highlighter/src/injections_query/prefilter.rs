//! A necessary condition for a whole injection query, compiled once from its patterns.
use crate::Range;
use ropey::RopeSlice;
use tree_sitter::query::InvalidPredicateError;

/// A query-author assertion: every match contains this byte in its matched subtree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RequiredByte(u8);

impl RequiredByte {
    pub(super) fn parse(value: Option<&str>) -> Result<Self, InvalidPredicateError> {
        match value {
            Some(value) if value.len() == 1 && value.is_ascii() => Ok(Self(value.as_bytes()[0])),
            _ => Err(InvalidPredicateError::Other {
                msg: "injection.required-byte requires exactly one ASCII byte".into(),
            }),
        }
    }
}

#[derive(Debug)]
pub(super) struct Prefilter(Requirement);

#[derive(Debug)]
enum Requirement {
    Unrestricted,
    Byte(u8),
    AnyByte(u128),
}

impl Prefilter {
    pub(super) fn compile(patterns: impl IntoIterator<Item = Option<RequiredByte>>) -> Self {
        let mut bytes = 0u128;
        for pattern in patterns {
            // An unannotated alternative can match without any known byte.
            let Some(RequiredByte(byte)) = pattern else {
                return Self(Requirement::Unrestricted);
            };
            bytes |= 1u128 << byte;
        }
        Self(match bytes.count_ones() {
            0 => Requirement::Unrestricted,
            1 => Requirement::Byte(bytes.trailing_zeros() as u8),
            _ => Requirement::AnyByte(bytes),
        })
    }

    pub(super) fn may_match(&self, source: RopeSlice<'_>, range: Range) -> bool {
        let matches = |chunk: &str| match self.0 {
            Requirement::Unrestricted => unreachable!(),
            Requirement::Byte(byte) => chunk.as_bytes().contains(&byte),
            Requirement::AnyByte(bytes) => chunk
                .bytes()
                .any(|byte| byte.is_ascii() && bytes & (1u128 << byte) != 0),
        };
        // No scan or slicing when the query has an unknown alternative.
        matches!(self.0, Requirement::Unrestricted)
            || source
                .slice(range.start as usize..range.end as usize)
                .chunks()
                .any(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn byte(value: &str) -> Option<RequiredByte> {
        Some(RequiredByte::parse(Some(value)).unwrap())
    }

    #[test]
    fn alternatives_need_any_required_byte_and_unknown_alternatives_disable_rejection() {
        for requirements in [vec![byte("/")], vec![byte("/"), byte("/")]] {
            let filter = Prefilter::compile(requirements);
            assert!(!filter.may_match("text".into(), 0..4));
            assert!(filter.may_match("/text".into(), 0..5));
        }
        let boundaries = Prefilter::compile([byte("\0"), byte("\x7f")]);
        assert!(boundaries.may_match("\0".into(), 0..1));
        assert!(boundaries.may_match("\x7f".into(), 0..1));
        assert!(!boundaries.may_match("é".into(), 0..2));
        let mixed = Prefilter::compile([byte("/"), byte("#")]);
        assert!(!mixed.may_match("text".into(), 0..4));
        assert!(mixed.may_match("/text".into(), 0..5));
        assert!(mixed.may_match("#text".into(), 0..5));
        assert!(mixed.may_match("é#".into(), 0..3));
        assert!(!mixed.may_match("é日".into(), 0..5));
        for requirements in [
            vec![],
            vec![None],
            vec![byte("/"), None],
            vec![None, byte("#")],
        ] {
            let filter = Prefilter::compile(requirements);
            assert!(filter.may_match("".into(), 0..0));
            assert!(filter.may_match("text".into(), 0..4));
        }
    }

    #[test]
    fn scan_stays_inside_the_queried_subtree_and_handles_chunk_boundaries() {
        let prefix = "/outside/";
        let body = "é日 plain text\n".repeat(5_000);
        let text = Rope::from_str(&format!("{prefix}{body}#suffix"));
        let start = prefix.len() as u32;
        let end = start + body.len() as u32;
        assert!(text.slice(start as usize..end as usize).chunks().count() > 1);
        for requirements in [vec![byte("/")], vec![byte("/"), byte("#")]] {
            let filter = Prefilter::compile(requirements);
            assert!(!filter.may_match(text.slice(..), start..end));
            assert!(filter.may_match(text.slice(..), 0..end));
        }
        let mixed = Prefilter::compile([byte("/"), byte("#")]);
        assert!(mixed.may_match(text.slice(..), start..end + 1));
        assert!(!mixed.may_match(text.slice(prefix.len()..), 0..body.len() as u32));
    }
}

use super::*;

fn point(text: &str, byte: usize) -> Point {
    let prefix = &text[..byte];
    Point {
        row: prefix.bytes().filter(|&byte| byte == b'\n').count() as u32,
        col: prefix.rsplit('\n').next().unwrap().len() as u32,
    }
}

fn edit(text: &str, start: usize, end: usize, insert: &str) -> InputEdit {
    let prefix = format!("{}{insert}", &text[..start]);
    InputEdit {
        start_byte: start as u32,
        old_end_byte: end as u32,
        new_end_byte: (start + insert.len()) as u32,
        start_point: point(text, start),
        old_end_point: point(text, end),
        new_end_point: point(&prefix, prefix.len()),
    }
}

#[test]
#[ignore = "manual release-mode performance comparison"]
fn injection_query_performance() {
    use std::time::{Duration, Instant};
    let timeout = Duration::from_secs(30);
    let mode = std::env::var("TREE_HOUSE_BENCH_MODE").unwrap_or_else(|_| "current".into());
    for (query_name, query) in [
        (
            "comment",
            Some(
                r#"
            ((block_comment) @injection.content (#set! injection.language "markdown"))
            ((line_comment) @injection.content (#set! injection.language "markdown"))
        "#,
            ),
        ),
        ("default-rust", None),
    ] {
        let mut loader = TestLanguageLoader::new();
        if let Some(query) = query {
            loader.overwrite_injections("rust", query.into());
        }
        Syntax::new(
            "/* warm */ fn warm() {}".into(),
            loader.get("rust"),
            timeout,
            &loader,
        )
        .unwrap();
        for rows in [1_000, 10_000, 100_000] {
            let plain = "fn item() {}\n".repeat(rows);
            for (case, source) in [
                ("plain", plain.clone()),
                (
                    "URL",
                    plain.replacen("{}", "{ let url = \"https://example.test\"; }", 1),
                ),
                ("one-comment", format!("/* comment */{plain}")),
                (
                    "1000-comments",
                    plain.replacen("fn item", "/* comment */fn item", 1_000),
                ),
            ] {
                let before = ropey::Rope::from(source.as_str());
                let middle = source.len() / 2;
                let at = middle + source[middle..].find("{}").unwrap() + 1;
                let mut after = source.clone();
                after.insert(at, ' ');
                let forward = edit(&source, at, at, " ");
                let backward = edit(&after, at, at + 1, "");
                let after = ropey::Rope::from(after.as_str());
                let start = Instant::now();
                let mut syntax =
                    Syntax::new(before.slice(..), loader.get("rust"), timeout, &loader).unwrap();
                let opening = start.elapsed();
                let mut times = Vec::new();
                for _ in 0..11 {
                    let start = Instant::now();
                    syntax
                        .update(after.slice(..), timeout, &[forward], &loader)
                        .unwrap();
                    syntax
                        .update(before.slice(..), timeout, &[backward], &loader)
                        .unwrap();
                    times.push(start.elapsed());
                }
                times.sort_unstable();
                eprintln!(
                    "BENCH {mode} {query_name} {rows} {case} update_us={} initial_us={}",
                    times[5].as_micros(),
                    opening.as_micros()
                );
            }
        }
    }
}

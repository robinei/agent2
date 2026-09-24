//! **What routing regress over code units costs, measured.**
//!
//! `docs/30_STRINGS.md` flagged this as the one number nobody had. Both of
//! regress's u16 inputs declare `CODE_UNITS_ARE_BYTES = false`, which gates
//! the `memchr`-backed literal-prefix prefilter at
//! `classicalbacktrack.rs:1078-1080` and `scm.rs:118`. Agent programs use
//! `split(/…/)`, `match` and `replace` constantly, so "it could be 5×" was not
//! something to find out three stages later.
//!
//! Run it:
//!
//! ```text
//! cargo test -p interp --release --test regex_input_cost -- --ignored --nocapture
//! ```
//!
//! `#[ignore]` because it is a measurement, not an assertion: it prints a
//! table and fails only if the code-unit path is more than 5× the UTF-8 one,
//! which is the threshold `30_STRINGS.md` set for stopping to reconsider.

use std::time::Instant;

/// 100 KB of realistic source text — the size the JSON-boundary measurements
/// in `30_STRINGS.md` used, so the numbers sit beside each other.
fn corpus() -> String {
    let unit = "pub fn handle_request(req: &Request) -> Result<Response, Error> {\n\
                \x20   let body = req.body().to_string();\n\
                \x20   // TODO: validate the payload before parsing it\n\
                \x20   let parsed: Value = serde_json::from_str(&body)?;\n\
                \x20   Ok(Response::new(parsed))\n\
                }\n\n";
    let mut s = String::new();
    while s.len() < 100_000 {
        s.push_str(unit);
    }
    s.truncate(100_000);
    s
}

fn bench(label: &str, runs: u32, mut f: impl FnMut() -> usize) -> f64 {
    // One warm-up run, then the timed ones.
    let warm = f();
    let t = Instant::now();
    let mut sink = 0usize;
    for _ in 0..runs {
        sink += f();
    }
    let us = t.elapsed().as_secs_f64() * 1e6 / runs as f64;
    println!("  {label:<34} {us:>9.1} µs   ({warm} matches, sink {sink})");
    us
}

#[test]
#[ignore]
fn code_unit_input_versus_utf8_input() {
    let text = corpus();
    let units: Vec<u16> = text.encode_utf16().collect();
    assert_eq!(units.len(), text.len(), "the corpus is ASCII");
    let runs = 50;

    println!("\n100 KB of ASCII source, mean of {runs} runs\n");

    // A literal prefix is the case the prefilter exists for — the worst case
    // for the change, and the commonest thing a program writes.
    let re = regress::Regex::new("TODO").unwrap();
    println!("/TODO/g  — a literal prefix, the prefilter's best case");
    let utf8 = bench("find_iter(&str)", runs, || re.find_iter(&text).count());
    let ucs2 = bench("find_from_ucs2(&[u16])", runs, || {
        re.find_from_ucs2(&units, 0).count()
    });
    println!("  ratio: {:.2}×\n", ucs2 / utf8);

    // A pattern with no literal prefix: the prefilter never fired here, so
    // this isolates the rest of the input abstraction.
    let re2 = regress::Regex::new(r"\bfn\s+(\w+)").unwrap();
    println!(r"/\bfn\s+(\w+)/g  — no literal prefix to filter on");
    let utf8_2 = bench("find_iter(&str)", runs, || re2.find_iter(&text).count());
    let ucs2_2 = bench("find_from_ucs2(&[u16])", runs, || {
        re2.find_from_ucs2(&units, 0).count()
    });
    println!("  ratio: {:.2}×\n", ucs2_2 / utf8_2);

    // **The prefilter's actual best case: a needle that is almost never
    // there.** 410 matches in 100 KB gives `memchr` very little to skip, so
    // the two runs above would flatter the change if they were the whole
    // measurement. One match in the whole corpus is the other end.
    let sparse = {
        let mut t = text.clone();
        t.replace_range(99_000..99_010, "ZZQQXXJJKK");
        t
    };
    let sparse_units: Vec<u16> = sparse.encode_utf16().collect();
    let re_sparse = regress::Regex::new("ZZQQXXJJKK").unwrap();
    println!("/ZZQQXXJJKK/g — one match in 100 KB, the prefilter's best case");
    let utf8_s = bench("find_iter(&str)", runs, || {
        re_sparse.find_iter(&sparse).count()
    });
    let ucs2_s = bench("find_from_ucs2(&[u16])", runs, || {
        re_sparse.find_from_ucs2(&sparse_units, 0).count()
    });
    println!("  ratio: {:.2}×\n", ucs2_s / utf8_s);

    // Unicode mode pairs surrogates; it is the `u`-flag path, for completeness.
    let re3 = regress::Regex::with_flags("TODO", "u").unwrap();
    println!("/TODO/gu — the Utf16Input path");
    let utf16 = bench("find_from_utf16(&[u16])", runs, || {
        re3.find_from_utf16(&units, 0).count()
    });
    println!("  vs the UTF-8 literal run: {:.2}×\n", utf16 / utf8);

    let worst = (ucs2 / utf8)
        .max(ucs2_2 / utf8_2)
        .max(ucs2_s / utf8_s)
        .max(utf16 / utf8);
    println!("worst ratio: {worst:.2}×");
    assert!(
        worst < 5.0,
        "the code-unit path is {worst:.2}× the UTF-8 one — `docs/30_STRINGS.md` \
         says to stop at 3× and reconsider keeping a byte lane for all-ASCII \
         subjects behind the `is_ascii` header bit"
    );
}

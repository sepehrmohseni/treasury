//! The project page's statement table matches what the analyzer and guardrails decide.

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use treasury_tests::site;

#[test]
fn the_page_shows_what_the_code_decides() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("site")
        .join("index.html");
    let page = std::fs::read_to_string(&path).expect("site/index.html");
    let shown = site::block_in(&page).expect("the statement markers in site/index.html");
    let expected = site::rows();
    assert!(
        shown == expected,
        "The statement table in site/index.html is out of date. Regenerate it with \
         `cargo run -q -p treasury-tests --example site-statements` and put the output between \
         the markers.\n\nExpected:\n{expected}"
    );
}

//! Prints the statement table rows for `site/index.html`.

#![allow(clippy::print_stdout)]

fn main() {
    print!("{}", treasury_tests::site::rows());
}

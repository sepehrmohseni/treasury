# Treasury

Treasury is an open-source desktop and mobile client for PostgreSQL, MySQL, MariaDB, SQL Server,
MongoDB, Redis, Valkey, Cassandra, ScyllaDB, Snowflake, Databricks SQL, Oracle, and IBM Db2.

It has no account, no paid tier, and no telemetry. It is in early development and has no release
yet.

## Building and checking

Every check CI runs is a step of `cargo xtask ci`. Without a local Rust toolchain, run them in
Docker:

```sh
scripts/docker-check.sh            # every check, as CI runs them
scripts/docker-check.sh ci --list  # the steps
scripts/docker-check.sh ci clippy  # one step
scripts/docker-check.sh fmt        # format the checkout
```

The script mounts the checkout read-only, keeps build outputs and caches in Docker volumes, and
uses half of the CPU cores unless `TREASURY_CI_CPUS` says otherwise.

## Project page

The page at [sepehrmohseni.github.io/treasury](https://sepehrmohseni.github.io/treasury/) is
plain HTML and CSS in `site/`. Its statement table comes from the analyzer and guardrails, and a
test fails when the page and the code disagree. After changing either, print the new rows with
`scripts/docker-check.sh -- cargo run -q -p treasury-tests --example site-statements` and put
them between the `statements` markers in `site/index.html`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

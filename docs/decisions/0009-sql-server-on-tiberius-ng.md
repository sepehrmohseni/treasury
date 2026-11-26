# 0009: SQL Server uses tiberius-ng; read-only is enforced by the guardrails

## Context

tiberius 0.13 is the async SQL Server client for Rust, but with rustls it hard-codes the aws-lc
crypto provider, and its other TLS option uses OpenSSL on Linux. Treasury allows neither: one
rustls provider (ring) across the workspace. Microsoft's own mssql-tds crate links OpenSSL.

SQL Server also has no session-wide read-only switch like PostgreSQL's
`default_transaction_read_only` or MySQL's `SET SESSION TRANSACTION READ ONLY`.
`ApplicationIntent=ReadOnly` only routes to readable secondaries.

## Decision

Use tiberius-ng, the maintained continuation of tiberius, with its `rustls-no-provider` feature.
The driver installs ring as rustls' process-wide provider (the only one Treasury ever uses), so
tiberius builds its TLS configuration on it. Trust follows the shared rules where tiberius allows
it: the system's roots by default, a custom CA file, the certificate checked against the
configured host also through an SSH tunnel, and "accept invalid certificates" as an explicit
setting.

For T-SQL the unit is the batch: statements up to `GO` share variables, so Treasury's analyzer
reviews a batch as one unit whose most dangerous statement decides, and the driver runs exactly
what was reviewed. Text holding more than one batch is refused. Cancel uses the protocol's Attention signal. Read-only sessions are
enforced by the guardrails, which block every statement that could write and every statement they
cannot classify.

## Consequences

The driver depends on a community fork that must be watched for maintenance. Messages from
`PRINT` and informational `RAISERROR` are not shown: tiberius-ng logs them instead of returning
them. A client certificate is not offered, as SQL Server does not use one for login. With
"accept invalid certificates", handshake signatures are not checked either, which changes nothing
when the certificate itself is not trusted.

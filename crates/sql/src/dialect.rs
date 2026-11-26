//! What differs between the SQL dialects, in one place.

use sqlparser::dialect::{
    DatabricksDialect, Dialect, GenericDialect, MsSqlDialect, MySqlDialect, OracleDialect,
    PostgreSqlDialect, SnowflakeDialect,
};
use treasury_core::driver::SqlDialect;

/// How a dialect writes a row limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitStyle {
    /// `... LIMIT n`
    Limit,
    /// `SELECT TOP n ...`
    Top,
    /// `... FETCH FIRST n ROWS ONLY`
    FetchFirst,
}

/// Lexical and semantic facts about a dialect.
#[derive(Clone, Copy, Debug)]
pub struct Rules {
    /// The dialect.
    pub dialect: SqlDialect,
    /// Backslash escapes inside single-quoted strings.
    pub backslash_escapes: bool,
    /// `"..."` is a string, not an identifier (MySQL without ANSI_QUOTES).
    pub double_quote_strings: bool,
    /// `` `ident` `` identifiers.
    pub backticks: bool,
    /// `[ident]` identifiers.
    pub brackets: bool,
    /// `$tag$ ... $tag$` strings.
    pub dollar_quotes: bool,
    /// `#` starts a comment.
    pub hash_comments: bool,
    /// Block comments nest.
    pub nested_comments: bool,
    /// `/*! ... */` is executed.
    pub executable_comments: bool,
    /// Oracle `q'[ ... ]'` strings.
    pub q_quotes: bool,
    /// Batches end at a `GO` line; `;` does not split them.
    pub go_batches: bool,
    /// A `/` line ends PL/SQL units.
    pub slash_terminator: bool,
    /// `DELIMITER` directives change the statement terminator.
    pub delimiter_directive: bool,
    /// User-defined functions may change data, so an unknown function makes a SELECT unknown.
    pub functions_may_write: bool,
    /// Limit syntax.
    pub limit: LimitStyle,
}

/// The rules for `dialect`.
pub fn rules(dialect: SqlDialect) -> Rules {
    let base = Rules {
        dialect,
        backslash_escapes: false,
        double_quote_strings: false,
        backticks: false,
        brackets: false,
        dollar_quotes: false,
        hash_comments: false,
        nested_comments: false,
        executable_comments: false,
        q_quotes: false,
        go_batches: false,
        slash_terminator: false,
        delimiter_directive: false,
        functions_may_write: true,
        limit: LimitStyle::Limit,
    };
    match dialect {
        SqlDialect::Postgres => Rules {
            dollar_quotes: true,
            nested_comments: true,
            ..base
        },
        SqlDialect::MySql | SqlDialect::MariaDb => Rules {
            backslash_escapes: true,
            double_quote_strings: true,
            backticks: true,
            hash_comments: true,
            executable_comments: true,
            delimiter_directive: true,
            ..base
        },
        SqlDialect::MsSql => Rules {
            brackets: true,
            go_batches: true,
            // T-SQL functions cannot modify database state.
            functions_may_write: false,
            limit: LimitStyle::Top,
            ..base
        },
        SqlDialect::Snowflake => Rules {
            backslash_escapes: true,
            dollar_quotes: true,
            // Snowflake UDFs cannot change data; only procedures can, and CALL is unknown.
            functions_may_write: false,
            ..base
        },
        SqlDialect::Databricks => Rules {
            backslash_escapes: true,
            backticks: true,
            functions_may_write: false,
            ..base
        },
        SqlDialect::Oracle => Rules {
            q_quotes: true,
            slash_terminator: true,
            limit: LimitStyle::FetchFirst,
            ..base
        },
        SqlDialect::Db2 => Rules {
            limit: LimitStyle::FetchFirst,
            ..base
        },
    }
}

/// The sqlparser dialect used to parse `dialect`.
pub fn parser_dialect(dialect: SqlDialect) -> Box<dyn Dialect> {
    match dialect {
        SqlDialect::Postgres => Box::new(PostgreSqlDialect {}),
        SqlDialect::MySql | SqlDialect::MariaDb => Box::new(MySqlDialect {}),
        SqlDialect::MsSql => Box::new(MsSqlDialect {}),
        SqlDialect::Snowflake => Box::new(SnowflakeDialect {}),
        SqlDialect::Databricks => Box::new(DatabricksDialect {}),
        SqlDialect::Oracle => Box::new(OracleDialect {}),
        SqlDialect::Db2 => Box::new(GenericDialect {}),
    }
}

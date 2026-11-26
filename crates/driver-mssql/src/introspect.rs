//! The object tree, details and DDL from the sys catalog views of each database.

use crate::session::{MsSqlSession, cell, int};
use async_trait::async_trait;
use tiberius::ToSql;
use treasury_core::driver::{Introspect, SqlDialect};
use treasury_core::exec::ValueKind;
use treasury_core::schema::{
    ChildPage, ChildQuery, ColumnInfo, ForeignKeyInfo, IndexInfo, ObjectDetails, ObjectKind,
    ObjectNode, ObjectPath, PathPart,
};
use treasury_core::{Error, ErrorKind, Result};
use treasury_sql::render;

const D: SqlDialect = SqlDialect::MsSql;
const DEFAULT_PAGE: u32 = 200;
const MAX_PAGE: u32 = 2_000;
const SCHEMA_GROUPS: &[&str] = &[
    "Tables",
    "Views",
    "Functions",
    "Procedures",
    "Sequences",
    "Types",
];

/// Listings select `name, detail, k`; `{db}` is the quoted database, `@P4` the schema and `@P5`
/// the quoted three-part table name.
fn listing(group: &str) -> Option<(&'static str, ObjectKind)> {
    Some(match group {
        "Schemas" => (
            "SELECT s.name, NULL, s.name FROM {db}.sys.schemas s \
             WHERE s.schema_id < 16384 AND s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest')",
            ObjectKind::Schema,
        ),
        "Tables" => (
            "SELECT t.name, CAST(SUM(p.rows) AS nvarchar(30)), t.name FROM {db}.sys.tables t \
             JOIN {db}.sys.schemas s ON s.schema_id = t.schema_id \
             LEFT JOIN {db}.sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0, 1) \
             WHERE s.name = @P4 GROUP BY t.name",
            ObjectKind::Table,
        ),
        "Views" => (
            "SELECT v.name, NULL, v.name FROM {db}.sys.views v \
             JOIN {db}.sys.schemas s ON s.schema_id = v.schema_id WHERE s.name = @P4",
            ObjectKind::View,
        ),
        "Functions" => (
            "SELECT o.name, NULL, o.name FROM {db}.sys.objects o \
             JOIN {db}.sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P4 AND o.type IN ('FN', 'IF', 'TF', 'FS', 'FT')",
            ObjectKind::Function,
        ),
        "Procedures" => (
            "SELECT o.name, NULL, o.name FROM {db}.sys.objects o \
             JOIN {db}.sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P4 AND o.type IN ('P', 'PC')",
            ObjectKind::Procedure,
        ),
        "Sequences" => (
            "SELECT q.name, NULL, q.name FROM {db}.sys.sequences q \
             JOIN {db}.sys.schemas s ON s.schema_id = q.schema_id WHERE s.name = @P4",
            ObjectKind::Sequence,
        ),
        "Types" => (
            "SELECT t.name, NULL, t.name FROM {db}.sys.types t \
             JOIN {db}.sys.schemas s ON s.schema_id = t.schema_id \
             WHERE s.name = @P4 AND t.is_user_defined = 1",
            ObjectKind::Type,
        ),
        "Columns" => (
            "SELECT c.name, ty.name, RIGHT('00000' + CAST(c.column_id AS varchar(5)), 5) \
             FROM {db}.sys.columns c JOIN {db}.sys.types ty ON ty.user_type_id = c.user_type_id \
             WHERE c.object_id = OBJECT_ID(@P5)",
            ObjectKind::Column,
        ),
        "Indexes" => (
            "SELECT i.name, CASE WHEN i.is_primary_key = 1 THEN 'primary key' \
                                 WHEN i.is_unique = 1 THEN 'unique' ELSE LOWER(i.type_desc) END, i.name \
             FROM {db}.sys.indexes i WHERE i.object_id = OBJECT_ID(@P5) AND i.name IS NOT NULL",
            ObjectKind::Index,
        ),
        "Constraints" => (
            "SELECT o.name, LOWER(REPLACE(o.type_desc, '_CONSTRAINT', '')), o.name \
             FROM {db}.sys.objects o \
             WHERE o.parent_object_id = OBJECT_ID(@P5) AND o.type IN ('PK', 'UQ', 'F', 'C', 'D')",
            ObjectKind::Constraint,
        ),
        "Triggers" => (
            "SELECT t.name, NULL, t.name FROM {db}.sys.triggers t WHERE t.parent_id = OBJECT_ID(@P5)",
            ObjectKind::Trigger,
        ),
        _ => return None,
    })
}

fn like_pattern(filter: &str) -> String {
    let mut p = String::from("%");
    for c in filter.chars() {
        if matches!(c, '\\' | '%' | '_' | '[') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

fn rows_detail(n: f64) -> String {
    let n = n.max(0.0).round();
    let (value, unit) = if n < 1_000.0 {
        return if n == 1.0 {
            "1 row".into()
        } else {
            format!("{n:.0} rows")
        };
    } else if n < 1_000_000.0 {
        (n / 1_000.0, "K")
    } else if n < 1_000_000_000.0 {
        (n / 1_000_000.0, "M")
    } else {
        (n / 1_000_000_000.0, "B")
    };
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text);
    format!("{text}{unit} rows")
}

fn kind_of(type_name: &str) -> ValueKind {
    match type_name {
        "bit" => ValueKind::Bool,
        "tinyint" | "smallint" | "int" | "bigint" => ValueKind::Integer,
        "real" | "float" => ValueKind::Float,
        "decimal" | "numeric" | "money" | "smallmoney" => ValueKind::Decimal,
        "date" => ValueKind::Date,
        "time" => ValueKind::Time,
        "datetime" | "datetime2" | "smalldatetime" => ValueKind::Timestamp,
        "datetimeoffset" => ValueKind::TimestampTz,
        "uniqueidentifier" => ValueKind::Uuid,
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => ValueKind::Binary,
        "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext" | "xml" | "sysname" => {
            ValueKind::Text
        }
        _ => ValueKind::Other,
    }
}

/// A type as written in DDL, from sys.types and the column's length, precision and scale.
fn type_text(
    name: &str,
    max_length: i64,
    precision: i64,
    scale: i64,
    user_schema: Option<&str>,
) -> String {
    if let Some(schema) = user_schema {
        return render::table(D, Some(schema), name);
    }
    let len = |divisor: i64| {
        if max_length == -1 {
            "max".to_owned()
        } else {
            (max_length / divisor).to_string()
        }
    };
    match name {
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({})", len(1)),
        "nvarchar" | "nchar" => format!("{name}({})", len(2)),
        "decimal" | "numeric" => format!("{name}({precision}, {scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        _ => name.to_owned(),
    }
}

fn not_found(path: &ObjectPath) -> Error {
    let name = path.leaf().map_or("", |l| l.name.as_str());
    Error::new(ErrorKind::Server, format!("\"{name}\" no longer exists."))
}

fn groups(parent: &ObjectPath, names: &[&str]) -> ChildPage {
    ChildPage {
        nodes: names
            .iter()
            .map(|g| ObjectNode {
                path: parent.child(ObjectKind::Group, *g),
                kind: ObjectKind::Group,
                name: (*g).to_owned(),
                expandable: true,
                detail: None,
            })
            .collect(),
        next_cursor: None,
    }
}

fn props(pairs: Vec<(&str, Option<String>)>) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k.to_owned(), v)))
        .collect()
}

/// A foreign key being assembled from one row per column.
struct FkLine {
    name: String,
    target: String,
    cols: Vec<String>,
    ref_cols: Vec<String>,
    actions: String,
}

/// An index being assembled from one row per column.
struct IndexDdl {
    name: String,
    head: String,
    keys: Vec<String>,
    include: Vec<String>,
    filter: Option<String>,
}

/// The parts of an object's path.
struct Where<'a> {
    db: String,
    schema: &'a str,
    /// The quoted three-part name of the table or view the path is under (or is).
    object: String,
}

fn locate<'a>(path: &'a ObjectPath, table: Option<&'a str>) -> Where<'a> {
    let db = path.find(ObjectKind::Database).unwrap_or("master");
    let schema = path.find(ObjectKind::Schema).unwrap_or("dbo");
    Where {
        db: render::ident(D, db),
        schema,
        object: format!(
            "{}.{}",
            render::ident(D, db),
            render::table(D, Some(schema), table.unwrap_or(""))
        ),
    }
}

impl MsSqlSession {
    async fn page(
        &self,
        inner: &str,
        w: &Where<'_>,
        q: &ChildQuery,
    ) -> Result<(Vec<(String, Option<String>)>, Option<String>)> {
        let limit = if q.limit == 0 {
            DEFAULT_PAGE
        } else {
            q.limit.min(MAX_PAGE)
        };
        let inner = inner.replace("{db}", &w.db);
        let sql = format!(
            "SELECT TOP (CAST(@P1 AS int)) name, detail, k FROM ({inner}) AS items(name, detail, k) \
             WHERE (@P2 IS NULL OR LOWER(name) LIKE LOWER(@P2) ESCAPE '\\') \
               AND (@P3 IS NULL OR k COLLATE Latin1_General_BIN2 > @P3 COLLATE Latin1_General_BIN2) \
             ORDER BY k COLLATE Latin1_General_BIN2"
        );
        let top = (limit + 1).to_string();
        let pattern = q
            .filter
            .as_deref()
            .filter(|f| !f.is_empty())
            .map(like_pattern);
        let cursor = q.cursor.clone();
        let params: [&dyn ToSql; 5] = [
            &top.as_str(),
            &pattern.as_deref(),
            &cursor.as_deref(),
            &w.schema,
            &w.object.as_str(),
        ];
        let rows = self.meta(&sql, &params).await?;
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let more = rows.len() > limit;
        let mut items = Vec::new();
        let mut last = None;
        for r in rows.iter().take(limit) {
            items.push((cell(r, 0).unwrap_or_default(), cell(r, 1)));
            last = cell(r, 2);
        }
        Ok((items, if more { last } else { None }))
    }

    async fn columns(&self, w: &Where<'_>) -> Result<Vec<ColumnInfo>> {
        let sql = format!(
            "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                    dc.definition, CAST(ep.value AS nvarchar(4000)), \
                    CASE WHEN ic.column_id IS NULL THEN 0 ELSE 1 END, \
                    CASE WHEN ty.is_user_defined = 1 THEN SCHEMA_NAME(ty.schema_id) END \
             FROM {db}.sys.columns c \
             JOIN {db}.sys.types ty ON ty.user_type_id = c.user_type_id \
             LEFT JOIN {db}.sys.default_constraints dc ON dc.object_id = c.default_object_id \
             LEFT JOIN {db}.sys.extended_properties ep ON ep.major_id = c.object_id \
                  AND ep.minor_id = c.column_id AND ep.class = 1 AND ep.name = 'MS_Description' \
             LEFT JOIN {db}.sys.indexes pk ON pk.object_id = c.object_id AND pk.is_primary_key = 1 \
             LEFT JOIN {db}.sys.index_columns ic ON ic.object_id = c.object_id \
                  AND ic.index_id = pk.index_id AND ic.column_id = c.column_id \
             WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id",
            db = w.db
        );
        let rows = self.meta(&sql, &[&w.object.as_str()]).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let base = cell(r, 1).unwrap_or_default();
                ColumnInfo {
                    name: cell(r, 0).unwrap_or_default(),
                    kind: kind_of(&base),
                    type_name: type_text(
                        &base,
                        int(r, 2).unwrap_or(0),
                        int(r, 3).unwrap_or(0),
                        int(r, 4).unwrap_or(0),
                        cell(r, 9).as_deref(),
                    ),
                    nullable: int(r, 5) == Some(1),
                    default: cell(r, 6),
                    comment: cell(r, 7),
                    primary_key: int(r, 8) == Some(1),
                }
            })
            .collect())
    }

    async fn relation_details(&self, w: &Where<'_>, path: &ObjectPath) -> Result<ObjectDetails> {
        let columns = self.columns(w).await?;
        if columns.is_empty() {
            return Err(not_found(path));
        }
        let ix_rows = self
            .meta(
                &format!(
                    "SELECT i.name, i.is_unique, i.is_primary_key, LOWER(i.type_desc), i.filter_definition, \
                            c.name, ic.is_descending_key \
                     FROM {db}.sys.indexes i \
                     JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                     WHERE i.object_id = OBJECT_ID(@P1) AND i.name IS NOT NULL AND ic.is_included_column = 0 \
                     ORDER BY i.name, ic.key_ordinal",
                    db = w.db
                ),
                &[&w.object.as_str()],
            )
            .await?;
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in &ix_rows {
            let name = cell(r, 0).unwrap_or_default();
            let mut col = cell(r, 5).unwrap_or_default();
            if int(r, 6) == Some(1) {
                col.push_str(" DESC");
            }
            match indexes.last_mut() {
                Some(ix) if ix.name == name => ix.columns.push(col),
                _ => indexes.push(IndexInfo {
                    unique: int(r, 1) == Some(1),
                    primary: int(r, 2) == Some(1),
                    method: cell(r, 3),
                    predicate: cell(r, 4),
                    columns: vec![col],
                    name,
                }),
            }
        }
        let fk_rows = self
            .meta(
                &format!(
                    "SELECT fk.name, SCHEMA_NAME(rt.schema_id), rt.name, pc.name, rc.name \
                     FROM {db}.sys.foreign_keys fk \
                     JOIN {db}.sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                     JOIN {db}.sys.tables rt ON rt.object_id = fk.referenced_object_id \
                     JOIN {db}.sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                     JOIN {db}.sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                     WHERE fk.parent_object_id = OBJECT_ID(@P1) ORDER BY fk.name, fkc.constraint_column_id",
                    db = w.db
                ),
                &[&w.object.as_str()],
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in &fk_rows {
            let name = cell(r, 0).unwrap_or_default();
            let (c, rc) = (
                cell(r, 3).unwrap_or_default(),
                cell(r, 4).unwrap_or_default(),
            );
            match foreign_keys.last_mut() {
                Some(fk) if fk.name == name => {
                    fk.columns.push(c);
                    fk.ref_columns.push(rc);
                }
                _ => foreign_keys.push(ForeignKeyInfo {
                    name,
                    ref_schema: cell(r, 1),
                    ref_table: cell(r, 2).unwrap_or_default(),
                    columns: vec![c],
                    ref_columns: vec![rc],
                }),
            }
        }
        let facts = self
            .meta(
                &format!(
                    "SELECT o.type_desc, \
                            (SELECT SUM(p.rows) FROM {db}.sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)), \
                            (SELECT SUM(a.total_pages) * 8192 FROM {db}.sys.partitions p \
                             JOIN {db}.sys.allocation_units a ON a.container_id = p.partition_id \
                             WHERE p.object_id = o.object_id), \
                            CAST(ep.value AS nvarchar(4000)), o.create_date \
                     FROM {db}.sys.objects o \
                     LEFT JOIN {db}.sys.extended_properties ep ON ep.major_id = o.object_id AND ep.minor_id = 0 \
                          AND ep.class = 1 AND ep.name = 'MS_Description' \
                     WHERE o.object_id = OBJECT_ID(@P1)",
                    db = w.db
                ),
                &[&w.object.as_str()],
            )
            .await?;
        let f = facts.first().ok_or_else(|| not_found(path))?;
        let is_view = cell(f, 0).as_deref() == Some("VIEW");
        Ok(ObjectDetails {
            columns,
            indexes,
            foreign_keys,
            row_estimate: if is_view {
                None
            } else {
                int(f, 1).and_then(|n| u64::try_from(n).ok())
            },
            size_bytes: if is_view {
                None
            } else {
                int(f, 2).and_then(|n| u64::try_from(n).ok())
            },
            comment: cell(f, 3),
            properties: props(vec![
                (
                    "Kind",
                    Some(if is_view { "view" } else { "table" }.to_owned()),
                ),
                ("Created", cell(f, 4)),
            ]),
        })
    }

    async fn definition(&self, w: &Where<'_>) -> Result<Option<String>> {
        let rows = self
            .meta(
                &format!("SELECT OBJECT_DEFINITION(OBJECT_ID(@P1)) FROM {db}.sys.objects WHERE object_id = OBJECT_ID(@P1)", db = w.db),
                &[&w.object.as_str()],
            )
            .await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, 0))
            .map(|d| d.trim().to_owned()))
    }

    async fn table_ddl(&self, w: &Where<'_>, table: &str) -> Result<Option<String>> {
        let db = &w.db;
        let cols = self
            .meta(
                &format!(
                    "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                            c.is_identity, CAST(idc.seed_value AS nvarchar(40)), CAST(idc.increment_value AS nvarchar(40)), \
                            dc.name, dc.definition, cc.definition, cc.is_persisted, \
                            CASE WHEN c.collation_name <> CAST(DATABASEPROPERTYEX(@P2, 'Collation') AS nvarchar(128)) \
                                 THEN c.collation_name END, \
                            CASE WHEN ty.is_user_defined = 1 THEN SCHEMA_NAME(ty.schema_id) END \
                     FROM {db}.sys.columns c \
                     JOIN {db}.sys.types ty ON ty.user_type_id = c.user_type_id \
                     LEFT JOIN {db}.sys.identity_columns idc ON idc.object_id = c.object_id AND idc.column_id = c.column_id \
                     LEFT JOIN {db}.sys.default_constraints dc ON dc.object_id = c.default_object_id \
                     LEFT JOIN {db}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                     WHERE c.object_id = OBJECT_ID(@P1) ORDER BY c.column_id"
                ),
                &[&w.object.as_str(), &db.trim_matches(['[', ']'])],
            )
            .await?;
        if cols.is_empty() {
            return Ok(None);
        }
        let mut lines = Vec::new();
        for r in &cols {
            let name = render::ident(D, &cell(r, 0).unwrap_or_default());
            if let Some(expr) = cell(r, 11) {
                let persisted = if int(r, 12) == Some(1) {
                    " PERSISTED"
                } else {
                    ""
                };
                lines.push(format!("    {name} AS {expr}{persisted}"));
                continue;
            }
            let ty = type_text(
                &cell(r, 1).unwrap_or_default(),
                int(r, 2).unwrap_or(0),
                int(r, 3).unwrap_or(0),
                int(r, 4).unwrap_or(0),
                cell(r, 14).as_deref(),
            );
            let mut line = format!("    {name} {ty}");
            if let Some(coll) = cell(r, 13) {
                line.push_str(&format!(" COLLATE {coll}"));
            }
            if int(r, 6) == Some(1) {
                line.push_str(&format!(
                    " IDENTITY({}, {})",
                    cell(r, 7).unwrap_or_else(|| "1".into()),
                    cell(r, 8).unwrap_or_else(|| "1".into())
                ));
            }
            line.push_str(if int(r, 5) == Some(1) {
                " NULL"
            } else {
                " NOT NULL"
            });
            if let (Some(dn), Some(def)) = (cell(r, 9), cell(r, 10)) {
                line.push_str(&format!(
                    " CONSTRAINT {} DEFAULT {def}",
                    render::ident(D, &dn)
                ));
            }
            lines.push(line);
        }
        // Primary key and unique constraints, with their index type and key order.
        let keys = self
            .meta(
                &format!(
                    "SELECT kc.name, kc.type, i.type_desc, c.name, ic.is_descending_key \
                     FROM {db}.sys.key_constraints kc \
                     JOIN {db}.sys.indexes i ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id \
                     JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                     WHERE kc.parent_object_id = OBJECT_ID(@P1) \
                     ORDER BY CASE kc.type WHEN 'PK' THEN 0 ELSE 1 END, kc.name, ic.key_ordinal"
                ),
                &[&w.object.as_str()],
            )
            .await?;
        let mut key_lines: Vec<(String, String, Vec<String>)> = Vec::new();
        for r in &keys {
            let name = cell(r, 0).unwrap_or_default();
            let col = format!(
                "{} {}",
                render::ident(D, &cell(r, 3).unwrap_or_default()),
                if int(r, 4) == Some(1) { "DESC" } else { "ASC" }
            );
            match key_lines.last_mut() {
                Some((n, _, c)) if *n == name => c.push(col),
                _ => {
                    let kind = if cell(r, 1).as_deref() == Some("PK") {
                        "PRIMARY KEY"
                    } else {
                        "UNIQUE"
                    };
                    let clustered = cell(r, 2).unwrap_or_default();
                    key_lines.push((name, format!("{kind} {clustered}"), vec![col]));
                }
            }
        }
        for (name, kind, cols) in key_lines {
            lines.push(format!(
                "    CONSTRAINT {} {kind} ({})",
                render::ident(D, &name),
                cols.join(", ")
            ));
        }
        let fks = self
            .meta(
                &format!(
                    "SELECT fk.name, SCHEMA_NAME(rt.schema_id), rt.name, pc.name, rc.name, \
                            fk.delete_referential_action_desc, fk.update_referential_action_desc \
                     FROM {db}.sys.foreign_keys fk \
                     JOIN {db}.sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                     JOIN {db}.sys.tables rt ON rt.object_id = fk.referenced_object_id \
                     JOIN {db}.sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                     JOIN {db}.sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                     WHERE fk.parent_object_id = OBJECT_ID(@P1) ORDER BY fk.name, fkc.constraint_column_id"
                ),
                &[&w.object.as_str()],
            )
            .await?;
        let mut fk_lines: Vec<FkLine> = Vec::new();
        for r in &fks {
            let name = cell(r, 0).unwrap_or_default();
            let (c, rc) = (
                render::ident(D, &cell(r, 3).unwrap_or_default()),
                render::ident(D, &cell(r, 4).unwrap_or_default()),
            );
            match fk_lines.last_mut() {
                Some(fk) if fk.name == name => {
                    fk.cols.push(c);
                    fk.ref_cols.push(rc);
                }
                _ => {
                    let target =
                        render::table(D, cell(r, 1).as_deref(), &cell(r, 2).unwrap_or_default());
                    let mut actions = String::new();
                    for (label, idx) in [("DELETE", 5), ("UPDATE", 6)] {
                        let a = cell(r, idx).unwrap_or_default();
                        if a != "NO_ACTION" && !a.is_empty() {
                            actions.push_str(&format!(" ON {label} {}", a.replace('_', " ")));
                        }
                    }
                    fk_lines.push(FkLine {
                        name,
                        target,
                        cols: vec![c],
                        ref_cols: vec![rc],
                        actions,
                    });
                }
            }
        }
        for fk in fk_lines {
            lines.push(format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}",
                render::ident(D, &fk.name),
                fk.cols.join(", "),
                fk.target,
                fk.ref_cols.join(", "),
                fk.actions
            ));
        }
        let checks = self
            .meta(
                &format!(
                    "SELECT name, definition FROM {db}.sys.check_constraints \
                     WHERE parent_object_id = OBJECT_ID(@P1) ORDER BY name"
                ),
                &[&w.object.as_str()],
            )
            .await?;
        for r in &checks {
            lines.push(format!(
                "    CONSTRAINT {} CHECK {}",
                render::ident(D, &cell(r, 0).unwrap_or_default()),
                cell(r, 1).unwrap_or_default()
            ));
        }
        let qualified = render::table(D, Some(w.schema), table);
        let mut out = format!("CREATE TABLE {qualified} (\n{}\n);\n", lines.join(",\n"));
        // Indexes that do not back a constraint.
        let ix = self
            .meta(
                &format!(
                    "SELECT i.name, i.is_unique, i.type_desc, c.name, ic.is_descending_key, ic.is_included_column, \
                            i.filter_definition \
                     FROM {db}.sys.indexes i \
                     JOIN {db}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     JOIN {db}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                     WHERE i.object_id = OBJECT_ID(@P1) AND i.is_primary_key = 0 AND i.is_unique_constraint = 0 \
                       AND i.name IS NOT NULL \
                     ORDER BY i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
                ),
                &[&w.object.as_str()],
            )
            .await?;
        let mut indexes: Vec<IndexDdl> = Vec::new();
        for r in &ix {
            let name = cell(r, 0).unwrap_or_default();
            if indexes.last().is_none_or(|c| c.name != name) {
                let unique = if int(r, 1) == Some(1) { "UNIQUE " } else { "" };
                indexes.push(IndexDdl {
                    name,
                    head: format!("{unique}{}", cell(r, 2).unwrap_or_default()),
                    keys: Vec::new(),
                    include: Vec::new(),
                    filter: cell(r, 6),
                });
            }
            if let Some(c) = indexes.last_mut() {
                let col = render::ident(D, &cell(r, 3).unwrap_or_default());
                if int(r, 5) == Some(1) {
                    c.include.push(col);
                } else {
                    c.keys.push(format!(
                        "{col} {}",
                        if int(r, 4) == Some(1) { "DESC" } else { "ASC" }
                    ));
                }
            }
        }
        for ix in indexes {
            out.push_str(&format!(
                "CREATE {} INDEX {} ON {qualified} ({})",
                ix.head,
                render::ident(D, &ix.name),
                ix.keys.join(", ")
            ));
            if !ix.include.is_empty() {
                out.push_str(&format!(" INCLUDE ({})", ix.include.join(", ")));
            }
            if let Some(f) = ix.filter {
                out.push_str(&format!(" WHERE {f}"));
            }
            out.push_str(";\n");
        }
        Ok(Some(out.trim_end().to_owned()))
    }
}

#[async_trait]
impl Introspect for MsSqlSession {
    async fn children(&self, parent: &ObjectPath, query: &ChildQuery) -> Result<ChildPage> {
        match parent.0.as_slice() {
            [] => {
                let w = Where {
                    db: "[master]".into(),
                    schema: "",
                    object: String::new(),
                };
                let (items, next_cursor) = self
                    .page(
                        "SELECT name, NULL, name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1",
                        &w,
                        query,
                    )
                    .await?;
                Ok(ChildPage {
                    nodes: items
                        .into_iter()
                        .map(|(name, _)| ObjectNode {
                            path: parent.child(ObjectKind::Database, &name),
                            kind: ObjectKind::Database,
                            expandable: true,
                            name,
                            detail: None,
                        })
                        .collect(),
                    next_cursor,
                })
            }
            [
                ..,
                PathPart {
                    kind: ObjectKind::Database,
                    ..
                },
            ] => self.list(parent, "Schemas", None, query).await,
            [
                ..,
                PathPart {
                    kind: ObjectKind::Schema,
                    ..
                },
            ] => Ok(groups(parent, SCHEMA_GROUPS)),
            [
                ..,
                owner,
                PathPart {
                    kind: ObjectKind::Group,
                    name,
                },
            ] => {
                let table = matches!(owner.kind, ObjectKind::Table | ObjectKind::View)
                    .then_some(owner.name.as_str());
                self.list(parent, name, table, query).await
            }
            [
                ..,
                PathPart {
                    kind: ObjectKind::Table,
                    ..
                },
            ] => Ok(groups(
                parent,
                &["Columns", "Indexes", "Constraints", "Triggers"],
            )),
            [
                ..,
                PathPart {
                    kind: ObjectKind::View,
                    ..
                },
            ] => Ok(groups(parent, &["Columns", "Indexes", "Triggers"])),
            _ => Ok(ChildPage::default()),
        }
    }

    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails> {
        let Some(leaf) = path.leaf() else {
            return Ok(ObjectDetails::default());
        };
        match leaf.kind {
            ObjectKind::Table | ObjectKind::View => {
                let w = locate(path, Some(&leaf.name));
                self.relation_details(&w, path).await
            }
            ObjectKind::Column => {
                let table = path
                    .0
                    .iter()
                    .rev()
                    .find(|p| matches!(p.kind, ObjectKind::Table | ObjectKind::View))
                    .map(|p| p.name.as_str());
                let w = locate(path, table);
                let column = self
                    .columns(&w)
                    .await?
                    .into_iter()
                    .find(|c| c.name == leaf.name)
                    .ok_or_else(|| not_found(path))?;
                Ok(ObjectDetails {
                    comment: column.comment.clone(),
                    properties: props(vec![
                        ("Type", Some(column.type_name.clone())),
                        (
                            "Nullable",
                            Some(if column.nullable { "yes" } else { "no" }.into()),
                        ),
                        ("Default", column.default.clone()),
                    ]),
                    columns: vec![column],
                    ..ObjectDetails::default()
                })
            }
            _ => Ok(ObjectDetails::default()),
        }
    }

    async fn ddl(&self, path: &ObjectPath) -> Result<Option<String>> {
        let Some(leaf) = path.leaf() else {
            return Ok(None);
        };
        match leaf.kind {
            ObjectKind::Table => {
                self.table_ddl(&locate(path, Some(&leaf.name)), &leaf.name)
                    .await
            }
            ObjectKind::View | ObjectKind::Function | ObjectKind::Procedure => {
                self.definition(&locate(path, Some(&leaf.name))).await
            }
            ObjectKind::Trigger => self.definition(&locate(path, Some(&leaf.name))).await,
            _ => Ok(None),
        }
    }
}

impl MsSqlSession {
    async fn list(
        &self,
        parent: &ObjectPath,
        group: &str,
        table: Option<&str>,
        q: &ChildQuery,
    ) -> Result<ChildPage> {
        let Some((inner, kind)) = listing(group) else {
            return Ok(ChildPage::default());
        };
        let w = locate(parent, table);
        let (items, next_cursor) = self.page(inner, &w, q).await?;
        Ok(ChildPage {
            nodes: items
                .into_iter()
                .map(|(name, detail)| ObjectNode {
                    path: parent.child(kind, &name),
                    kind,
                    expandable: matches!(
                        kind,
                        ObjectKind::Table | ObjectKind::View | ObjectKind::Schema
                    ),
                    detail: match kind {
                        ObjectKind::Table => {
                            detail.map(|d| d.parse::<f64>().map(rows_detail).unwrap_or(d))
                        }
                        _ => detail,
                    },
                    name,
                })
                .collect(),
            next_cursor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_as_written_in_ddl() {
        assert_eq!(type_text("nvarchar", 100, 0, 0, None), "nvarchar(50)");
        assert_eq!(type_text("varchar", -1, 0, 0, None), "varchar(max)");
        assert_eq!(type_text("decimal", 9, 12, 2, None), "decimal(12, 2)");
        assert_eq!(type_text("datetime2", 8, 27, 7, None), "datetime2(7)");
        assert_eq!(type_text("int", 4, 10, 0, None), "int");
        assert_eq!(
            type_text("money_t", 8, 19, 4, Some("dbo")),
            "[dbo].[money_t]"
        );
    }

    #[test]
    fn filters_escape_brackets_too() {
        assert_eq!(like_pattern("a[1]_"), "%a\\[1]\\_%");
        assert_eq!(kind_of("datetimeoffset"), ValueKind::TimestampTz);
    }
}

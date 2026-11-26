//! The object tree, object details and DDL, read from the system catalogs.
//!
//! Tree pages are keyset-paginated: each listing is ordered by a key (the name, or the column
//! position) in the "C" collation, and the cursor is the last key of the previous page.

use crate::session::{PgSession, kind_of};
use async_trait::async_trait;
use tokio_postgres::types::Type;
use treasury_core::driver::Introspect;
use treasury_core::exec::ValueKind;
use treasury_core::schema::{
    ChildPage, ChildQuery, ColumnInfo, ForeignKeyInfo, IndexInfo, ObjectDetails, ObjectKind,
    ObjectNode, ObjectPath, PathPart,
};
use treasury_core::{Error, ErrorKind, Result};

const DEFAULT_PAGE: u32 = 200;
const MAX_PAGE: u32 = 2_000;

/// Groups under a schema.
const SCHEMA_GROUPS: &[&str] = &[
    "Tables",
    "Views",
    "Materialized views",
    "Functions",
    "Sequences",
    "Types",
];

/// Every listing returns `name, detail, key, schema` and reads `$1` (schema) and `$2` (table).
const SCHEMAS: &str = r"
SELECT n.nspname::text AS name, NULL::text AS detail, n.nspname::text AS key, NULL::text AS schema
FROM pg_namespace n
WHERE n.nspname NOT LIKE 'pg\_toast%' AND n.nspname NOT LIKE 'pg\_temp\_%'";

const EXTENSIONS: &str = r"
SELECT e.extname::text, e.extversion::text, e.extname::text, NULL::text
FROM pg_extension e";

const TABLES: &str = r"
SELECT c.relname::text,
       CASE WHEN c.relkind = 'f' THEN 'foreign' WHEN c.reltuples >= 0 THEN c.reltuples::float8::text END,
       c.relname::text, NULL::text
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relkind IN ('r', 'p', 'f') AND NOT c.relispartition";

const VIEWS: &str = r"
SELECT c.relname::text, NULL::text, c.relname::text, NULL::text
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relkind = 'v'";

const MATVIEWS: &str = r"
SELECT c.relname::text, CASE WHEN c.reltuples >= 0 THEN c.reltuples::float8::text END,
       c.relname::text, NULL::text
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relkind = 'm'";

const FUNCTIONS: &str = r"
SELECT (p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')')::text,
       p.prokind::text,
       (p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')')::text, NULL::text
FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
WHERE n.nspname = $1::name";

const SEQUENCES: &str = r"
SELECT c.relname::text, NULL::text, c.relname::text, NULL::text
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relkind = 'S'";

const TYPES: &str = r"
SELECT t.typname::text,
       CASE t.typtype WHEN 'e' THEN 'enum' WHEN 'd' THEN 'domain' WHEN 'c' THEN 'composite'
                      WHEN 'r' THEN 'range' WHEN 'm' THEN 'multirange' WHEN 'b' THEN 'base' END,
       t.typname::text, NULL::text
FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
WHERE n.nspname = $1::name
  AND (t.typrelid = 0 OR EXISTS (SELECT 1 FROM pg_class c WHERE c.oid = t.typrelid AND c.relkind = 'c'))
  AND NOT (t.typcategory = 'A' AND t.typelem <> 0)
  AND t.typtype <> 'p'";

const COLUMNS: &str = r"
SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), lpad(a.attnum::text, 5, '0'), NULL::text
FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relname = $2::name AND a.attnum > 0 AND NOT a.attisdropped";

const INDEXES: &str = r"
SELECT i.relname::text,
       CASE WHEN x.indisprimary THEN 'primary key' WHEN x.indisunique THEN 'unique' ELSE am.amname::text END,
       i.relname::text, NULL::text
FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_am am ON am.oid = i.relam
JOIN pg_class c ON c.oid = x.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relname = $2::name";

const CONSTRAINTS: &str = r"
SELECT co.conname::text,
       CASE co.contype WHEN 'p' THEN 'primary key' WHEN 'f' THEN 'foreign key' WHEN 'u' THEN 'unique'
                       WHEN 'c' THEN 'check' WHEN 'x' THEN 'exclusion' WHEN 'n' THEN 'not null'
                       WHEN 't' THEN 'trigger' END,
       co.conname::text, NULL::text
FROM pg_constraint co JOIN pg_class c ON c.oid = co.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relname = $2::name";

const TRIGGERS: &str = r"
SELECT t.tgname::text, NULL::text, t.tgname::text, NULL::text
FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1::name AND c.relname = $2::name AND NOT t.tgisinternal";

const PARTITIONS: &str = r"
SELECT c.relname::text, pg_get_expr(c.relpartbound, c.oid), c.relname::text, cn.nspname::text
FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid JOIN pg_namespace cn ON cn.oid = c.relnamespace
JOIN pg_class p ON p.oid = i.inhparent JOIN pg_namespace n ON n.oid = p.relnamespace
WHERE n.nspname = $1::name AND p.relname = $2::name";

/// A `LIKE` pattern matching names that contain `filter`.
fn like_pattern(filter: &str) -> String {
    let mut p = String::from("%");
    for c in filter.chars() {
        if matches!(c, '\\' | '%' | '_') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// `1.2K rows`, from the planner's estimate.
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

/// The logical kind of a column from its (base) type and the type's category.
fn kind_for(oid: u32, category: &str) -> ValueKind {
    if let Some(t) = Type::from_oid(oid) {
        let k = kind_of(&t);
        if k != ValueKind::Other {
            return k;
        }
    }
    match category {
        "B" => ValueKind::Bool,
        "N" => ValueKind::Decimal,
        "S" | "E" => ValueKind::Text,
        "A" => ValueKind::List,
        "C" => ValueKind::Composite,
        "T" => ValueKind::Interval,
        _ => ValueKind::Other,
    }
}

fn not_found(path: &ObjectPath) -> Error {
    let name = path.leaf().map_or("", |l| l.name.as_str());
    Error::new(ErrorKind::Server, format!("\"{name}\" no longer exists."))
}

fn group_kind(group: &str) -> Option<ObjectKind> {
    Some(match group {
        "Tables" | "Partitions" => ObjectKind::Table,
        "Views" => ObjectKind::View,
        "Materialized views" => ObjectKind::MaterializedView,
        "Functions" => ObjectKind::Function,
        "Sequences" => ObjectKind::Sequence,
        "Types" => ObjectKind::Type,
        "Columns" => ObjectKind::Column,
        "Indexes" => ObjectKind::Index,
        "Constraints" => ObjectKind::Constraint,
        "Triggers" => ObjectKind::Trigger,
        "Extensions" => ObjectKind::Extension,
        _ => return None,
    })
}

fn group_query(group: &str) -> Option<&'static str> {
    Some(match group {
        "Tables" => TABLES,
        "Views" => VIEWS,
        "Materialized views" => MATVIEWS,
        "Functions" => FUNCTIONS,
        "Sequences" => SEQUENCES,
        "Types" => TYPES,
        "Columns" => COLUMNS,
        "Indexes" => INDEXES,
        "Constraints" => CONSTRAINTS,
        "Triggers" => TRIGGERS,
        "Partitions" => PARTITIONS,
        "Extensions" => EXTENSIONS,
        _ => return None,
    })
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

struct Item {
    name: String,
    detail: Option<String>,
    schema: Option<String>,
}

/// Owner, kind and the like as label and value, skipping empty ones.
fn props(pairs: Vec<(&str, Option<String>)>) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .filter_map(|(k, v)| v.filter(|v| !v.is_empty()).map(|v| (k.to_owned(), v)))
        .collect()
}

fn ensure_semicolon(sql: &str) -> String {
    let t = sql.trim_end();
    if t.ends_with(';') {
        t.to_owned()
    } else {
        format!("{t};")
    }
}

impl PgSession {
    async fn page(
        &self,
        inner: &str,
        schema: Option<&str>,
        table: Option<&str>,
        q: &ChildQuery,
    ) -> Result<(Vec<Item>, Option<String>)> {
        let limit = i64::from(if q.limit == 0 {
            DEFAULT_PAGE
        } else {
            q.limit.min(MAX_PAGE)
        });
        let sql = format!(
            "SELECT name, detail, key, schema FROM ({inner}) AS items(name, detail, key, schema) \
             WHERE ($3::text IS NULL OR name ILIKE $3) \
               AND ($4::text IS NULL OR key COLLATE \"C\" > $4) \
             ORDER BY key COLLATE \"C\" LIMIT $5"
        );
        let pattern = q
            .filter
            .as_deref()
            .filter(|f| !f.is_empty())
            .map(like_pattern);
        let fetch = limit + 1;
        let rows = self
            .timed(async {
                let st = self
                    .client
                    .prepare_typed(
                        &sql,
                        &[Type::TEXT, Type::TEXT, Type::TEXT, Type::TEXT, Type::INT8],
                    )
                    .await?;
                self.client
                    .query(&st, &[&schema, &table, &pattern, &q.cursor, &fetch])
                    .await
            })
            .await?;
        let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        let mut items = Vec::new();
        let mut last_key = None;
        for row in rows
            .iter()
            .take(usize::try_from(limit).unwrap_or(usize::MAX))
        {
            items.push(Item {
                name: self.col(row, 0)?,
                detail: self.col(row, 1)?,
                schema: self.col(row, 3)?,
            });
            last_key = Some(self.col::<String>(row, 2)?);
        }
        Ok((items, if more { last_key } else { None }))
    }

    async fn list(
        &self,
        parent: &ObjectPath,
        group: &str,
        schema: Option<&str>,
        table: Option<&str>,
        q: &ChildQuery,
    ) -> Result<ChildPage> {
        let (Some(inner), Some(kind)) = (group_query(group), group_kind(group)) else {
            return Ok(ChildPage::default());
        };
        let (items, next_cursor) = self.page(inner, schema, table, q).await?;
        let nodes = items
            .into_iter()
            .map(|it| {
                let kind = match (kind, it.detail.as_deref()) {
                    (ObjectKind::Function, Some("p")) => ObjectKind::Procedure,
                    (k, _) => k,
                };
                let detail = match kind {
                    ObjectKind::Table | ObjectKind::MaterializedView => it
                        .detail
                        .map(|d| d.parse::<f64>().map(rows_detail).unwrap_or(d)),
                    ObjectKind::Function | ObjectKind::Procedure => None,
                    _ => it.detail,
                };
                let path = match it.schema {
                    // Partitions may live in another schema; point at where they are.
                    Some(s) => ObjectPath::root()
                        .child(ObjectKind::Schema, s)
                        .child(ObjectKind::Group, "Tables")
                        .child(kind, &it.name),
                    None => parent.child(kind, &it.name),
                };
                ObjectNode {
                    path,
                    kind,
                    expandable: matches!(
                        kind,
                        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
                    ),
                    name: it.name,
                    detail,
                }
            })
            .collect();
        Ok(ChildPage { nodes, next_cursor })
    }

    async fn rel_oid(&self, schema: &str, name: &str) -> Result<Option<(u32, String)>> {
        let row = self
            .meta_one(
                "SELECT c.oid, c.relkind::text FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema, &name],
            )
            .await?;
        match row {
            Some(r) => Ok(Some((self.col(&r, 0)?, self.col(&r, 1)?))),
            None => Ok(None),
        }
    }

    async fn proc_oid(&self, schema: &str, signature: &str) -> Result<Option<u32>> {
        let row = self
            .meta_one(
                "SELECT p.oid FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 \
                   AND (p.proname || '(' || pg_get_function_identity_arguments(p.oid) || ')') = $2",
                &[&schema, &signature],
            )
            .await?;
        row.map(|r| self.col(&r, 0)).transpose()
    }

    async fn columns(&self, rel: u32) -> Result<Vec<ColumnInfo>> {
        let rows = self
            .meta(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                        CASE WHEN a.attgenerated = '' THEN pg_get_expr(d.adbin, d.adrelid) END, \
                        col_description(a.attrelid, a.attnum), \
                        COALESCE(a.attnum = ANY (pk.conkey), false), \
                        bt.oid, bt.typcategory::text \
                 FROM pg_attribute a \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 JOIN pg_type bt ON bt.oid = CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 LEFT JOIN pg_constraint pk ON pk.conrelid = a.attrelid AND pk.contype = 'p' \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&rel],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(ColumnInfo {
                    name: self.col(r, 0)?,
                    type_name: self.col(r, 1)?,
                    nullable: self.col(r, 2)?,
                    default: self.col(r, 3)?,
                    comment: self.col(r, 4)?,
                    primary_key: self.col(r, 5)?,
                    kind: kind_for(self.col(r, 6)?, &self.col::<String>(r, 7)?),
                })
            })
            .collect()
    }

    async fn relation_details(&self, rel: u32) -> Result<ObjectDetails> {
        let columns = self.columns(rel).await?;
        let indexes = self
            .meta(
                "SELECT i.relname::text, x.indisunique, x.indisprimary, am.amname::text, \
                        pg_get_expr(x.indpred, x.indrelid), \
                        ARRAY(SELECT pg_get_indexdef(x.indexrelid, k, true) \
                              FROM generate_series(1, x.indnkeyatts::int) AS k ORDER BY k) \
                 FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid \
                 JOIN pg_am am ON am.oid = i.relam \
                 WHERE x.indrelid = $1 ORDER BY i.relname",
                &[&rel],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(IndexInfo {
                    name: self.col(r, 0)?,
                    unique: self.col(r, 1)?,
                    primary: self.col(r, 2)?,
                    method: self.col(r, 3)?,
                    predicate: self.col(r, 4)?,
                    columns: self.col(r, 5)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let foreign_keys = self
            .meta(
                "SELECT co.conname::text, rn.nspname::text, rc.relname::text, \
                        ARRAY(SELECT a.attname::text FROM unnest(co.conkey) WITH ORDINALITY AS u(n, o) \
                              JOIN pg_attribute a ON a.attrelid = co.conrelid AND a.attnum = u.n \
                              ORDER BY u.o), \
                        ARRAY(SELECT a.attname::text FROM unnest(co.confkey) WITH ORDINALITY AS u(n, o) \
                              JOIN pg_attribute a ON a.attrelid = co.confrelid AND a.attnum = u.n \
                              ORDER BY u.o) \
                 FROM pg_constraint co JOIN pg_class rc ON rc.oid = co.confrelid \
                 JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
                 WHERE co.conrelid = $1 AND co.contype = 'f' ORDER BY co.conname",
                &[&rel],
            )
            .await?
            .iter()
            .map(|r| {
                Ok(ForeignKeyInfo {
                    name: self.col(r, 0)?,
                    ref_schema: self.col(r, 1)?,
                    ref_table: self.col(r, 2)?,
                    columns: self.col(r, 3)?,
                    ref_columns: self.col(r, 4)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let facts = self
            .meta_one(
                "SELECT c.relkind::text, CASE WHEN c.reltuples >= 0 THEN c.reltuples::float8 END, \
                        CASE WHEN c.relkind IN ('r', 'm', 't') THEN pg_total_relation_size(c.oid) END, \
                        obj_description(c.oid, 'pg_class'), pg_get_userbyid(c.relowner)::text, \
                        c.relpersistence::text, \
                        (SELECT t.spcname::text FROM pg_tablespace t WHERE t.oid = c.reltablespace), \
                        (SELECT format('%I.%I', pn.nspname, p.relname) FROM pg_inherits i \
                         JOIN pg_class p ON p.oid = i.inhparent \
                         JOIN pg_namespace pn ON pn.oid = p.relnamespace \
                         WHERE i.inhrelid = c.oid AND c.relispartition), \
                        CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END, \
                        CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END \
                 FROM pg_class c WHERE c.oid = $1",
                &[&rel],
            )
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Server, "The object no longer exists."))?;
        let relkind: String = self.col(&facts, 0)?;
        let estimate: Option<f64> = self.col(&facts, 1)?;
        let size: Option<i64> = self.col(&facts, 2)?;
        let persistence: String = self.col(&facts, 5)?;
        let kind = match relkind.as_str() {
            "r" => "table",
            "p" => "partitioned table",
            "f" => "foreign table",
            "v" => "view",
            "m" => "materialized view",
            _ => "relation",
        };
        Ok(ObjectDetails {
            columns,
            indexes,
            foreign_keys,
            row_estimate: estimate
                .filter(|_| relkind != "v")
                .map(|n| n.max(0.0).round() as u64),
            size_bytes: size.and_then(|n| u64::try_from(n).ok()),
            comment: self.col(&facts, 3)?,
            properties: props(vec![
                ("Kind", Some(kind.to_owned())),
                ("Owner", self.col(&facts, 4)?),
                (
                    "Persistence",
                    match persistence.as_str() {
                        "u" => Some("unlogged".to_owned()),
                        "t" => Some("temporary".to_owned()),
                        _ => None,
                    },
                ),
                ("Tablespace", self.col(&facts, 6)?),
                ("Partition of", self.col(&facts, 7)?),
                ("Partition bound", self.col(&facts, 8)?),
                ("Partition key", self.col(&facts, 9)?),
            ]),
        })
    }

    async fn table_ddl(&self, rel: u32) -> Result<String> {
        let facts = self
            .meta_one(
                "SELECT format('%I.%I', n.nspname, c.relname), c.relkind::text, c.relpersistence::text, \
                        c.relispartition, \
                        (SELECT format('%I.%I', pn.nspname, p.relname) FROM pg_inherits i \
                         JOIN pg_class p ON p.oid = i.inhparent \
                         JOIN pg_namespace pn ON pn.oid = p.relnamespace \
                         WHERE i.inhrelid = c.oid AND c.relispartition), \
                        CASE WHEN c.relispartition THEN pg_get_expr(c.relpartbound, c.oid) END, \
                        CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END, \
                        ARRAY(SELECT format('%I.%I', pn.nspname, p.relname) FROM pg_inherits i \
                              JOIN pg_class p ON p.oid = i.inhparent \
                              JOIN pg_namespace pn ON pn.oid = p.relnamespace \
                              WHERE i.inhrelid = c.oid AND NOT c.relispartition ORDER BY i.inhseqno), \
                        (SELECT format('%I', s.srvname) FROM pg_foreign_table ft \
                         JOIN pg_foreign_server s ON s.oid = ft.ftserver WHERE ft.ftrelid = c.oid), \
                        (SELECT ft.ftoptions FROM pg_foreign_table ft WHERE ft.ftrelid = c.oid), \
                        quote_literal(obj_description(c.oid, 'pg_class')), \
                        c.reloptions \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
                &[&rel],
            )
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Server, "The table no longer exists."))?;
        let name: String = self.col(&facts, 0)?;
        let relkind: String = self.col(&facts, 1)?;
        let persistence: String = self.col(&facts, 2)?;
        let is_partition: bool = self.col(&facts, 3)?;
        let parent: Option<String> = self.col(&facts, 4)?;
        let bound: Option<String> = self.col(&facts, 5)?;
        let partition_key: Option<String> = self.col(&facts, 6)?;
        let inherits: Vec<String> = self.col(&facts, 7)?;
        let server: Option<String> = self.col(&facts, 8)?;
        let ft_options: Option<Vec<String>> = self.col(&facts, 9)?;
        let comment: Option<String> = self.col(&facts, 10)?;
        let reloptions: Option<Vec<String>> = self.col(&facts, 11)?;

        let columns = self
            .meta(
                "SELECT format('%I', a.attname), format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                        pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text, \
                        CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation THEN \
                          (SELECT format('%I.%I', cn.nspname, co.collname) FROM pg_collation co \
                           JOIN pg_namespace cn ON cn.oid = co.collnamespace WHERE co.oid = a.attcollation) \
                        END, \
                        quote_literal(col_description(a.attrelid, a.attnum)), a.attislocal, \
                        (SELECT sc.relname::text FROM pg_depend dep \
                         JOIN pg_class sc ON sc.oid = dep.objid AND sc.relkind = 'S' \
                         WHERE dep.classid = 'pg_class'::regclass \
                           AND dep.refclassid = 'pg_class'::regclass \
                           AND dep.refobjid = a.attrelid AND dep.refobjsubid = a.attnum \
                           AND dep.deptype = 'a' LIMIT 1) \
                 FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                &[&rel],
            )
            .await?;
        let constraints = self
            .meta(
                "SELECT format('%I', co.conname), pg_get_constraintdef(co.oid) \
                 FROM pg_constraint co \
                 WHERE co.conrelid = $1 AND co.contype IN ('p', 'u', 'c', 'f', 'x') AND co.conislocal \
                 ORDER BY CASE co.contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2 \
                          WHEN 'x' THEN 3 ELSE 4 END, co.conname",
                &[&rel],
            )
            .await?;

        let mut out = String::new();
        let mut lines = Vec::new();
        let mut column_comments = Vec::new();
        for c in &columns {
            let col: String = self.col(c, 0)?;
            let ty: String = self.col(c, 1)?;
            let not_null: bool = self.col(c, 2)?;
            let expr: Option<String> = self.col(c, 3)?;
            let identity: String = self.col(c, 4)?;
            let generated: String = self.col(c, 5)?;
            let collation: Option<String> = self.col(c, 6)?;
            let col_comment: Option<String> = self.col(c, 7)?;
            let local: bool = self.col(c, 8)?;
            let owned_sequence: Option<String> = self.col(c, 9)?;
            // A column that owns the sequence its default draws from is a serial column; written
            // that way, the DDL creates the sequence too.
            let serial = match (&owned_sequence, &expr) {
                (Some(seq), Some(e))
                    if identity.is_empty()
                        && e.starts_with("nextval(")
                        && e.contains(seq.as_str()) =>
                {
                    match ty.as_str() {
                        "integer" => Some("serial"),
                        "bigint" => Some("bigserial"),
                        "smallint" => Some("smallserial"),
                        _ => None,
                    }
                }
                _ => None,
            };
            let (ty, expr) = match serial {
                Some(s) => (s.to_owned(), None),
                None => (ty, expr),
            };
            if let Some(cm) = col_comment {
                column_comments.push(format!("COMMENT ON COLUMN {name}.{col} IS {cm};"));
            }
            if !local {
                continue;
            }
            let mut line = format!("    {col} {ty}");
            if let Some(coll) = collation {
                line.push_str(&format!(" COLLATE {coll}"));
            }
            match (generated.as_str(), identity.as_str(), &expr) {
                ("s", _, Some(e)) => line.push_str(&format!(" GENERATED ALWAYS AS ({e}) STORED")),
                ("v", _, Some(e)) => line.push_str(&format!(" GENERATED ALWAYS AS ({e}) VIRTUAL")),
                (_, "a", _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                (_, "d", _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                (_, _, Some(e)) => line.push_str(&format!(" DEFAULT {e}")),
                _ => {}
            }
            if not_null && identity.is_empty() {
                line.push_str(" NOT NULL");
            }
            lines.push(line);
        }
        let mut late = Vec::new();
        for c in &constraints {
            let cname: String = self.col(c, 0)?;
            let def: String = self.col(c, 1)?;
            if is_partition {
                late.push(format!("ALTER TABLE {name} ADD CONSTRAINT {cname} {def};"));
            } else {
                lines.push(format!("    CONSTRAINT {cname} {def}"));
            }
        }

        let temp = if persistence == "t" {
            "TEMPORARY "
        } else if persistence == "u" {
            "UNLOGGED "
        } else {
            ""
        };
        let foreign = if relkind == "f" { "FOREIGN " } else { "" };
        match (is_partition, parent, bound) {
            (true, Some(parent), Some(bound)) => {
                out.push_str(&format!(
                    "CREATE {temp}TABLE {name} PARTITION OF {parent}\n    {bound}"
                ));
            }
            _ => {
                out.push_str(&format!("CREATE {temp}{foreign}TABLE {name} (\n"));
                out.push_str(&lines.join(",\n"));
                out.push_str("\n)");
                if !inherits.is_empty() {
                    out.push_str(&format!(" INHERITS ({})", inherits.join(", ")));
                }
            }
        }
        if let Some(key) = partition_key {
            out.push_str(&format!(" PARTITION BY {key}"));
        }
        if let Some(server) = server {
            out.push_str(&format!("\nSERVER {server}"));
            if let Some(opts) = ft_options.filter(|o| !o.is_empty()) {
                out.push_str(&format!(" OPTIONS ({})", option_list(&opts)));
            }
        }
        if let Some(opts) = reloptions.filter(|o| !o.is_empty()) {
            out.push_str(&format!(" WITH ({})", reloption_list(&opts)));
        }
        out.push_str(";\n");
        for l in late {
            out.push_str(&l);
            out.push('\n');
        }

        for r in self
            .meta(
                "SELECT pg_get_indexdef(x.indexrelid) FROM pg_index x \
                 JOIN pg_class i ON i.oid = x.indexrelid \
                 WHERE x.indrelid = $1 AND NOT i.relispartition \
                   AND NOT EXISTS (SELECT 1 FROM pg_constraint co WHERE co.conindid = x.indexrelid \
                                   AND co.conrelid = x.indrelid AND co.contype IN ('p', 'u', 'x')) \
                 ORDER BY i.relname",
                &[&rel],
            )
            .await?
        {
            out.push_str(&ensure_semicolon(&self.col::<String>(&r, 0)?));
            out.push('\n');
        }
        for r in self
            .meta(
                "SELECT pg_get_triggerdef(t.oid) FROM pg_trigger t \
                 WHERE t.tgrelid = $1 AND NOT t.tgisinternal AND t.tgparentid = 0 ORDER BY t.tgname",
                &[&rel],
            )
            .await?
        {
            out.push_str(&ensure_semicolon(&self.col::<String>(&r, 0)?));
            out.push('\n');
        }
        if let Some(cm) = comment {
            out.push_str(&format!("COMMENT ON {foreign}TABLE {name} IS {cm};\n"));
        }
        for cm in column_comments {
            out.push_str(&cm);
            out.push('\n');
        }
        Ok(out.trim_end().to_owned())
    }

    async fn view_ddl(&self, rel: u32, materialized: bool) -> Result<String> {
        let row = self
            .meta_one(
                "SELECT format('%I.%I', n.nspname, c.relname), pg_get_viewdef(c.oid, true), \
                        c.relispopulated, quote_literal(obj_description(c.oid, 'pg_class')) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
                &[&rel],
            )
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Server, "The view no longer exists."))?;
        let name: String = self.col(&row, 0)?;
        let def: String = self.col(&row, 1)?;
        let populated: bool = self.col(&row, 2)?;
        let comment: Option<String> = self.col(&row, 3)?;
        let body = def.trim().trim_end_matches(';');
        let mut out = if materialized {
            format!(
                "CREATE MATERIALIZED VIEW {name} AS\n{body}\nWITH {}DATA;\n",
                if populated { "" } else { "NO " }
            )
        } else {
            format!("CREATE OR REPLACE VIEW {name} AS\n{body};\n")
        };
        if materialized {
            for r in self
                .meta(
                    "SELECT pg_get_indexdef(x.indexrelid) FROM pg_index x \
                     JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = $1 ORDER BY i.relname",
                    &[&rel],
                )
                .await?
            {
                out.push_str(&ensure_semicolon(&self.col::<String>(&r, 0)?));
                out.push('\n');
            }
        }
        if let Some(cm) = comment {
            let what = if materialized {
                "MATERIALIZED VIEW"
            } else {
                "VIEW"
            };
            out.push_str(&format!("COMMENT ON {what} {name} IS {cm};\n"));
        }
        Ok(out.trim_end().to_owned())
    }

    async fn sequence_ddl(&self, schema: &str, name: &str) -> Result<Option<String>> {
        let row = self
            .meta_one(
                "SELECT format('CREATE SEQUENCE %I.%I AS %s INCREMENT BY %s MINVALUE %s MAXVALUE %s \
                                START WITH %s CACHE %s%s;', s.schemaname, s.sequencename, s.data_type, \
                               s.increment_by, s.min_value, s.max_value, s.start_value, s.cache_size, \
                               CASE WHEN s.cycle THEN ' CYCLE' ELSE ' NO CYCLE' END), \
                        (SELECT format('ALTER SEQUENCE %I.%I OWNED BY %I.%I.%I;', s.schemaname, \
                                       s.sequencename, tn.nspname, t.relname, a.attname) \
                         FROM pg_class sc JOIN pg_namespace sn ON sn.oid = sc.relnamespace \
                         JOIN pg_depend d ON d.objid = sc.oid AND d.classid = 'pg_class'::regclass \
                              AND d.refclassid = 'pg_class'::regclass AND d.deptype = 'a' \
                         JOIN pg_class t ON t.oid = d.refobjid \
                         JOIN pg_namespace tn ON tn.oid = t.relnamespace \
                         JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = d.refobjsubid \
                         WHERE sn.nspname = s.schemaname AND sc.relname = s.sequencename \
                         LIMIT 1) \
                 FROM pg_sequences s WHERE s.schemaname = $1 AND s.sequencename = $2",
                &[&schema, &name],
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let create: String = self.col(&row, 0)?;
        let owned: Option<String> = self.col(&row, 1)?;
        Ok(Some(match owned {
            Some(o) => format!("{create}\n{o}"),
            None => create,
        }))
    }

    async fn type_ddl(&self, schema: &str, name: &str) -> Result<Option<String>> {
        let Some(row) = self
            .meta_one(
                "SELECT t.oid, t.typtype::text, format('%I.%I', n.nspname, t.typname), t.typrelid, \
                        (SELECT string_agg(quote_literal(e.enumlabel), ', ' ORDER BY e.enumsortorder) \
                         FROM pg_enum e WHERE e.enumtypid = t.oid), \
                        format_type(t.typbasetype, t.typtypmod), t.typnotnull, t.typdefault, \
                        CASE WHEN t.typcollation <> 0 AND t.typcollation <> bt.typcollation THEN \
                          (SELECT format('%I.%I', cn.nspname, co.collname) FROM pg_collation co \
                           JOIN pg_namespace cn ON cn.oid = co.collnamespace WHERE co.oid = t.typcollation) \
                        END \
                 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
                 LEFT JOIN pg_type bt ON bt.oid = t.typbasetype \
                 WHERE n.nspname = $1 AND t.typname = $2",
                &[&schema, &name],
            )
            .await?
        else {
            return Ok(None);
        };
        let oid: u32 = self.col(&row, 0)?;
        let typtype: String = self.col(&row, 1)?;
        let qname: String = self.col(&row, 2)?;
        match typtype.as_str() {
            "e" => {
                let labels: Option<String> = self.col(&row, 4)?;
                Ok(Some(format!(
                    "CREATE TYPE {qname} AS ENUM ({});",
                    labels.unwrap_or_default()
                )))
            }
            "c" => {
                let rel: u32 = self.col(&row, 3)?;
                let fields = self
                    .meta(
                        "SELECT format('    %I %s', a.attname, format_type(a.atttypid, a.atttypmod)) \
                         FROM pg_attribute a WHERE a.attrelid = $1 AND a.attnum > 0 \
                           AND NOT a.attisdropped ORDER BY a.attnum",
                        &[&rel],
                    )
                    .await?
                    .iter()
                    .map(|r| self.col::<String>(r, 0))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Some(format!(
                    "CREATE TYPE {qname} AS (\n{}\n);",
                    fields.join(",\n")
                )))
            }
            "d" => {
                let base: String = self.col(&row, 5)?;
                let not_null: bool = self.col(&row, 6)?;
                let default: Option<String> = self.col(&row, 7)?;
                let collation: Option<String> = self.col(&row, 8)?;
                let mut out = format!("CREATE DOMAIN {qname} AS {base}");
                if let Some(c) = collation {
                    out.push_str(&format!(" COLLATE {c}"));
                }
                if let Some(d) = default {
                    out.push_str(&format!(" DEFAULT {d}"));
                }
                if not_null {
                    out.push_str(" NOT NULL");
                }
                for r in self
                    .meta(
                        "SELECT format('%I', co.conname), pg_get_constraintdef(co.oid) \
                         FROM pg_constraint co WHERE co.contypid = $1 AND co.contype = 'c' \
                         ORDER BY co.conname",
                        &[&oid],
                    )
                    .await?
                {
                    let cname: String = self.col(&r, 0)?;
                    let def: String = self.col(&r, 1)?;
                    out.push_str(&format!("\n    CONSTRAINT {cname} {def}"));
                }
                out.push(';');
                Ok(Some(out))
            }
            "r" => {
                let Some(r) = self
                    .meta_one(
                        "SELECT format_type(r.rngsubtype, NULL), \
                                CASE WHEN NOT opc.opcdefault THEN format('%I.%I', opcn.nspname, opc.opcname) END, \
                                CASE WHEN r.rngcollation <> 0 AND r.rngcollation <> st.typcollation THEN \
                                  (SELECT format('%I.%I', cn.nspname, co.collname) FROM pg_collation co \
                                   JOIN pg_namespace cn ON cn.oid = co.collnamespace \
                                   WHERE co.oid = r.rngcollation) END, \
                                CASE WHEN r.rngcanonical <> 0 THEN r.rngcanonical::regproc::text END, \
                                CASE WHEN r.rngsubdiff <> 0 THEN r.rngsubdiff::regproc::text END, \
                                (SELECT format('%I.%I', mn.nspname, m.typname) FROM pg_type m \
                                 JOIN pg_namespace mn ON mn.oid = m.typnamespace \
                                 WHERE m.oid = r.rngmultitypid) \
                         FROM pg_range r JOIN pg_type st ON st.oid = r.rngsubtype \
                         JOIN pg_opclass opc ON opc.oid = r.rngsubopc \
                         JOIN pg_namespace opcn ON opcn.oid = opc.opcnamespace \
                         WHERE r.rngtypid = $1",
                        &[&oid],
                    )
                    .await?
                else {
                    return Ok(None);
                };
                let mut parts = vec![format!("SUBTYPE = {}", self.col::<String>(&r, 0)?)];
                for (i, label) in [
                    (1, "SUBTYPE_OPCLASS"),
                    (2, "COLLATION"),
                    (3, "CANONICAL"),
                    (4, "SUBTYPE_DIFF"),
                    (5, "MULTIRANGE_TYPE_NAME"),
                ] {
                    if let Some(v) = self.col::<Option<String>>(&r, i)? {
                        parts.push(format!("{label} = {v}"));
                    }
                }
                Ok(Some(format!(
                    "CREATE TYPE {qname} AS RANGE (\n    {}\n);",
                    parts.join(",\n    ")
                )))
            }
            // Base and multirange types are defined in C or by their range.
            _ => Ok(None),
        }
    }
}

/// `key 'value'` pairs from `key=value` strings, for foreign table options.
fn option_list(opts: &[String]) -> String {
    opts.iter()
        .map(|o| {
            let (k, v) = o.split_once('=').unwrap_or((o.as_str(), ""));
            format!("{k} '{}'", v.replace('\'', "''"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `key='value'` pairs for storage parameters.
fn reloption_list(opts: &[String]) -> String {
    opts.iter()
        .map(|o| {
            let (k, v) = o.split_once('=').unwrap_or((o.as_str(), ""));
            format!("{k}='{}'", v.replace('\'', "''"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl Introspect for PgSession {
    async fn children(&self, parent: &ObjectPath, query: &ChildQuery) -> Result<ChildPage> {
        let schema = parent.find(ObjectKind::Schema);
        match parent.0.as_slice() {
            [] => {
                let (items, next_cursor) = self.page(SCHEMAS, None, None, query).await?;
                let mut nodes = Vec::new();
                let unfiltered = query.filter.as_deref().is_none_or(str::is_empty);
                if query.cursor.is_none() && unfiltered {
                    nodes.extend(groups(parent, &["Extensions"]).nodes);
                }
                nodes.extend(items.into_iter().map(|it| ObjectNode {
                    path: parent.child(ObjectKind::Schema, &it.name),
                    kind: ObjectKind::Schema,
                    expandable: true,
                    name: it.name,
                    detail: None,
                }));
                Ok(ChildPage { nodes, next_cursor })
            }
            [
                PathPart {
                    kind: ObjectKind::Group,
                    name,
                },
            ] if name == "Extensions" => self.list(parent, name, None, None, query).await,
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
            ] => match owner.kind {
                ObjectKind::Schema => self.list(parent, name, schema, None, query).await,
                ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                    self.list(parent, name, schema, Some(&owner.name), query)
                        .await
                }
                _ => Ok(ChildPage::default()),
            },
            [
                ..,
                PathPart {
                    kind: ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView,
                    name,
                },
            ] => {
                let Some(schema) = schema else {
                    return Ok(ChildPage::default());
                };
                let relkind = self.rel_oid(schema, name).await?.map(|(_, k)| k);
                let names: &[&str] = match relkind.as_deref() {
                    Some("p") => &[
                        "Columns",
                        "Indexes",
                        "Constraints",
                        "Triggers",
                        "Partitions",
                    ],
                    Some("v") => &["Columns", "Triggers"],
                    Some("m") => &["Columns", "Indexes"],
                    Some(_) => &["Columns", "Indexes", "Constraints", "Triggers"],
                    None => return Err(not_found(parent)),
                };
                Ok(groups(parent, names))
            }
            _ => Ok(ChildPage::default()),
        }
    }

    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails> {
        let Some(leaf) = path.leaf() else {
            return Ok(ObjectDetails::default());
        };
        let schema = path.find(ObjectKind::Schema).unwrap_or("");
        let table = path
            .0
            .iter()
            .rev()
            .skip(1)
            .find(|p| {
                matches!(
                    p.kind,
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
                )
            })
            .map(|p| p.name.as_str());
        let name = leaf.name.as_str();
        match leaf.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                let (oid, _) = self
                    .rel_oid(schema, name)
                    .await?
                    .ok_or_else(|| not_found(path))?;
                self.relation_details(oid).await
            }
            ObjectKind::Column => {
                let (oid, _) = self
                    .rel_oid(schema, table.unwrap_or(""))
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let columns = self.columns(oid).await?;
                let column = columns
                    .into_iter()
                    .find(|c| c.name == name)
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
                        ("Primary key", column.primary_key.then(|| "yes".to_owned())),
                    ]),
                    columns: vec![column],
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let oid = self
                    .proc_oid(schema, name)
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let r = self
                    .meta_one(
                        "SELECT l.lanname::text, pg_get_function_result(p.oid), \
                                pg_get_function_arguments(p.oid), p.provolatile::text, \
                                pg_get_userbyid(p.proowner)::text, obj_description(p.oid, 'pg_proc'), \
                                p.prosecdef \
                         FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang WHERE p.oid = $1",
                        &[&oid],
                    )
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let volatility: String = self.col(&r, 3)?;
                let definer: bool = self.col(&r, 6)?;
                Ok(ObjectDetails {
                    comment: self.col(&r, 5)?,
                    properties: props(vec![
                        ("Arguments", self.col(&r, 2)?),
                        ("Returns", self.col(&r, 1)?),
                        ("Language", self.col(&r, 0)?),
                        (
                            "Volatility",
                            Some(
                                match volatility.as_str() {
                                    "i" => "immutable",
                                    "s" => "stable",
                                    _ => "volatile",
                                }
                                .into(),
                            ),
                        ),
                        ("Security", definer.then(|| "definer".to_owned())),
                        ("Owner", self.col(&r, 4)?),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Sequence => {
                let r = self
                    .meta_one(
                        "SELECT s.data_type::text, s.start_value, s.min_value, s.max_value, \
                                s.increment_by, s.cycle, s.cache_size, s.last_value, s.sequenceowner::text \
                         FROM pg_sequences s WHERE s.schemaname = $1 AND s.sequencename = $2",
                        &[&schema, &name],
                    )
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let num = |i: usize| -> Result<Option<String>> {
                    Ok(self.col::<Option<i64>>(&r, i)?.map(|n| n.to_string()))
                };
                let cycle: bool = self.col(&r, 5)?;
                Ok(ObjectDetails {
                    properties: props(vec![
                        ("Type", self.col(&r, 0)?),
                        ("Last value", num(7)?),
                        ("Start", num(1)?),
                        ("Increment", num(4)?),
                        ("Minimum", num(2)?),
                        ("Maximum", num(3)?),
                        ("Cache", num(6)?),
                        ("Cycles", Some(if cycle { "yes" } else { "no" }.into())),
                        ("Owner", self.col(&r, 8)?),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Type => {
                let r = self
                    .meta_one(
                        "SELECT t.typtype::text, pg_get_userbyid(t.typowner)::text, \
                                obj_description(t.oid, 'pg_type'), \
                                ARRAY(SELECT e.enumlabel::text FROM pg_enum e WHERE e.enumtypid = t.oid \
                                      ORDER BY e.enumsortorder), \
                                CASE WHEN t.typtype = 'd' THEN format_type(t.typbasetype, t.typtypmod) END, \
                                t.typrelid \
                         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
                         WHERE n.nspname = $1 AND t.typname = $2",
                        &[&schema, &name],
                    )
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let typtype: String = self.col(&r, 0)?;
                let labels: Vec<String> = self.col(&r, 3)?;
                let rel: u32 = self.col(&r, 5)?;
                let columns = if typtype == "c" && rel != 0 {
                    self.columns(rel).await?
                } else {
                    Vec::new()
                };
                Ok(ObjectDetails {
                    columns,
                    comment: self.col(&r, 2)?,
                    properties: props(vec![
                        (
                            "Kind",
                            Some(
                                match typtype.as_str() {
                                    "e" => "enum",
                                    "d" => "domain",
                                    "c" => "composite",
                                    "r" => "range",
                                    "m" => "multirange",
                                    _ => "base",
                                }
                                .into(),
                            ),
                        ),
                        ("Values", (!labels.is_empty()).then(|| labels.join(", "))),
                        ("Base type", self.col(&r, 4)?),
                        ("Owner", self.col(&r, 1)?),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Extension => {
                let r = self
                    .meta_one(
                        "SELECT e.extversion::text, n.nspname::text, obj_description(e.oid, 'pg_extension'), \
                                (SELECT a.default_version::text FROM pg_available_extensions a \
                                 WHERE a.name = e.extname) \
                         FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
                         WHERE e.extname = $1",
                        &[&name],
                    )
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let version: String = self.col(&r, 0)?;
                let available: Option<String> = self.col(&r, 3)?;
                Ok(ObjectDetails {
                    comment: self.col(&r, 2)?,
                    properties: props(vec![
                        ("Version", Some(version.clone())),
                        ("Available version", available.filter(|a| *a != version)),
                        ("Schema", self.col(&r, 1)?),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Index => {
                let r = self
                    .meta_one(
                        "SELECT pg_get_indexdef(i.oid), am.amname::text, x.indisvalid, pg_relation_size(i.oid) \
                         FROM pg_class i JOIN pg_namespace n ON n.oid = i.relnamespace \
                         JOIN pg_index x ON x.indexrelid = i.oid JOIN pg_am am ON am.oid = i.relam \
                         WHERE n.nspname = $1 AND i.relname = $2",
                        &[&schema, &name],
                    )
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let valid: bool = self.col(&r, 2)?;
                let size: i64 = self.col(&r, 3)?;
                Ok(ObjectDetails {
                    size_bytes: u64::try_from(size).ok(),
                    properties: props(vec![
                        ("Definition", self.col(&r, 0)?),
                        ("Method", self.col(&r, 1)?),
                        ("Valid", (!valid).then(|| "no, rebuild it".to_owned())),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Constraint | ObjectKind::Trigger => {
                let (oid, _) = self
                    .rel_oid(schema, table.unwrap_or(""))
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let sql = if leaf.kind == ObjectKind::Constraint {
                    "SELECT pg_get_constraintdef(co.oid, true), \
                            CASE WHEN co.convalidated THEN NULL ELSE 'no' END, \
                            obj_description(co.oid, 'pg_constraint') \
                     FROM pg_constraint co WHERE co.conrelid = $1 AND co.conname = $2"
                } else {
                    "SELECT pg_get_triggerdef(t.oid, true), \
                            CASE t.tgenabled WHEN 'D' THEN 'no' WHEN 'R' THEN 'on replicas only' \
                                             WHEN 'A' THEN 'always' END, \
                            obj_description(t.oid, 'pg_trigger') \
                     FROM pg_trigger t WHERE t.tgrelid = $1 AND t.tgname = $2"
                };
                let r = self
                    .meta_one(sql, &[&oid, &name])
                    .await?
                    .ok_or_else(|| not_found(path))?;
                let second = if leaf.kind == ObjectKind::Constraint {
                    "Validated"
                } else {
                    "Enabled"
                };
                Ok(ObjectDetails {
                    comment: self.col(&r, 2)?,
                    properties: props(vec![
                        ("Definition", self.col(&r, 0)?),
                        (second, self.col(&r, 1)?),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Schema => {
                let r = self
                    .meta_one(
                        "SELECT pg_get_userbyid(n.nspowner)::text, obj_description(n.oid, 'pg_namespace') \
                         FROM pg_namespace n WHERE n.nspname = $1",
                        &[&name],
                    )
                    .await?
                    .ok_or_else(|| not_found(path))?;
                Ok(ObjectDetails {
                    comment: self.col(&r, 1)?,
                    properties: props(vec![("Owner", self.col(&r, 0)?)]),
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
        let schema = path.find(ObjectKind::Schema).unwrap_or("");
        let table = path
            .0
            .iter()
            .rev()
            .skip(1)
            .find(|p| {
                matches!(
                    p.kind,
                    ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
                )
            })
            .map(|p| p.name.as_str());
        let name = leaf.name.as_str();
        let one = |sql: &'static str| async move {
            let row = self.meta_one(sql, &[&schema, &name]).await?;
            row.map(|r| self.col::<String>(&r, 0)).transpose()
        };
        match leaf.kind {
            ObjectKind::Schema => {
                let row = self
                    .meta_one(
                        "SELECT format('CREATE SCHEMA %I AUTHORIZATION %I;', n.nspname, \
                                       pg_get_userbyid(n.nspowner)) \
                         FROM pg_namespace n WHERE n.nspname = $1",
                        &[&name],
                    )
                    .await?;
                row.map(|r| self.col::<String>(&r, 0)).transpose()
            }
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                let Some((oid, relkind)) = self.rel_oid(schema, name).await? else {
                    return Err(not_found(path));
                };
                match relkind.as_str() {
                    "v" => self.view_ddl(oid, false).await.map(Some),
                    "m" => self.view_ddl(oid, true).await.map(Some),
                    _ => self.table_ddl(oid).await.map(Some),
                }
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let Some(oid) = self.proc_oid(schema, name).await? else {
                    return Err(not_found(path));
                };
                // Aggregates have no CREATE FUNCTION form.
                let row = self
                    .meta_one(
                        "SELECT CASE WHEN p.prokind = 'a' THEN NULL ELSE pg_get_functiondef(p.oid) END \
                         FROM pg_proc p WHERE p.oid = $1",
                        &[&oid],
                    )
                    .await?;
                Ok(row
                    .map(|r| self.col::<Option<String>>(&r, 0))
                    .transpose()?
                    .flatten()
                    .map(|d| ensure_semicolon(&d)))
            }
            ObjectKind::Sequence => self.sequence_ddl(schema, name).await,
            ObjectKind::Type => self.type_ddl(schema, name).await,
            ObjectKind::Index => Ok(one("SELECT pg_get_indexdef(i.oid) || ';' FROM pg_class i \
                 JOIN pg_namespace n ON n.oid = i.relnamespace \
                 WHERE n.nspname = $1 AND i.relname = $2 AND i.relkind IN ('i', 'I')")
            .await?),
            ObjectKind::Constraint | ObjectKind::Trigger => {
                let Some((oid, _)) = self.rel_oid(schema, table.unwrap_or("")).await? else {
                    return Err(not_found(path));
                };
                let sql = if leaf.kind == ObjectKind::Constraint {
                    "SELECT format('ALTER TABLE %I.%I ADD CONSTRAINT %I %s;', n.nspname, c.relname, \
                                   co.conname, pg_get_constraintdef(co.oid)) \
                     FROM pg_constraint co JOIN pg_class c ON c.oid = co.conrelid \
                     JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE co.conrelid = $1 AND co.conname = $2"
                } else {
                    "SELECT pg_get_triggerdef(t.oid) || ';' FROM pg_trigger t \
                     WHERE t.tgrelid = $1 AND t.tgname = $2"
                };
                let row = self.meta_one(sql, &[&oid, &name]).await?;
                row.map(|r| self.col::<String>(&r, 0)).transpose()
            }
            ObjectKind::Extension => {
                let row = self
                    .meta_one(
                        "SELECT format('CREATE EXTENSION IF NOT EXISTS %I WITH SCHEMA %I VERSION %L;', \
                                       e.extname, n.nspname, e.extversion) \
                         FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
                         WHERE e.extname = $1",
                        &[&name],
                    )
                    .await?;
                row.map(|r| self.col::<String>(&r, 0)).transpose()
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_literally() {
        assert_eq!(like_pattern("a_b%c\\"), "%a\\_b\\%c\\\\%");
        assert_eq!(like_pattern("orders"), "%orders%");
    }

    #[test]
    fn row_estimates_read_short() {
        assert_eq!(rows_detail(0.0), "0 rows");
        assert_eq!(rows_detail(1.0), "1 row");
        assert_eq!(rows_detail(999.0), "999 rows");
        assert_eq!(rows_detail(1_000.0), "1K rows");
        assert_eq!(rows_detail(1_260.0), "1.3K rows");
        assert_eq!(rows_detail(12_400_000.0), "12.4M rows");
        assert_eq!(rows_detail(3_000_000_000.0), "3B rows");
    }

    #[test]
    fn options_are_quoted() {
        assert_eq!(
            option_list(&["schema_name=public".into(), "table_name=it's".into()]),
            "schema_name 'public', table_name 'it''s'"
        );
        assert_eq!(reloption_list(&["fillfactor=70".into()]), "fillfactor='70'");
    }

    #[test]
    fn kinds_fall_back_to_the_type_category() {
        assert_eq!(kind_for(23, "N"), ValueKind::Integer);
        assert_eq!(kind_for(999_999, "E"), ValueKind::Text);
        assert_eq!(kind_for(999_999, "A"), ValueKind::List);
        assert_eq!(kind_for(999_999, "G"), ValueKind::Other);
    }
}

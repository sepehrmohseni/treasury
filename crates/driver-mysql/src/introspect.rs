//! The object tree, details and DDL from information_schema and SHOW CREATE.

use crate::session::{MySqlSession, cell, int};
use async_trait::async_trait;
use mysql_async::Value as My;
use treasury_core::driver::Introspect;
use treasury_core::exec::ValueKind;
use treasury_core::schema::{
    ChildPage, ChildQuery, ColumnInfo, ForeignKeyInfo, IndexInfo, ObjectDetails, ObjectKind,
    ObjectNode, ObjectPath, PathPart,
};
use treasury_core::{Error, ErrorKind, Result};
use treasury_sql::render;

const DEFAULT_PAGE: u32 = 200;
const MAX_PAGE: u32 = 2_000;

/// Every listing selects `name, detail, k` and takes the schema, then the table, as `?`.
const DATABASES: &str = "SELECT SCHEMA_NAME AS name, NULL AS detail, SCHEMA_NAME AS k \
                         FROM information_schema.SCHEMATA";

const TABLES: &str = "SELECT TABLE_NAME AS name, CAST(TABLE_ROWS AS CHAR) AS detail, TABLE_NAME AS k \
                      FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? \
                      AND TABLE_TYPE IN ('BASE TABLE', 'SYSTEM VERSIONED')";

const VIEWS: &str = "SELECT TABLE_NAME AS name, NULL AS detail, TABLE_NAME AS k \
                     FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'VIEW'";

const SEQUENCES: &str = "SELECT TABLE_NAME AS name, NULL AS detail, TABLE_NAME AS k \
                         FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'SEQUENCE'";

const FUNCTIONS: &str = "SELECT ROUTINE_NAME AS name, NULL AS detail, ROUTINE_NAME AS k \
                         FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'FUNCTION'";

const PROCEDURES: &str = "SELECT ROUTINE_NAME AS name, NULL AS detail, ROUTINE_NAME AS k \
                          FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = 'PROCEDURE'";

const EVENTS: &str = "SELECT EVENT_NAME AS name, LOWER(STATUS) AS detail, EVENT_NAME AS k \
                      FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ?";

const COLUMNS: &str = "SELECT COLUMN_NAME AS name, COLUMN_TYPE AS detail, LPAD(ORDINAL_POSITION, 5, '0') AS k \
                       FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

const INDEXES: &str = "SELECT INDEX_NAME AS name, \
                       MAX(CASE WHEN INDEX_NAME = 'PRIMARY' THEN 'primary key' \
                                WHEN NON_UNIQUE = 0 THEN 'unique' ELSE LOWER(INDEX_TYPE) END) AS detail, \
                       INDEX_NAME AS k \
                       FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                       GROUP BY INDEX_NAME";

const CONSTRAINTS: &str = "SELECT CONSTRAINT_NAME AS name, LOWER(CONSTRAINT_TYPE) AS detail, CONSTRAINT_NAME AS k \
                           FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";

const TRIGGERS: &str = "SELECT TRIGGER_NAME AS name, \
                        CONCAT(LOWER(ACTION_TIMING), ' ', LOWER(EVENT_MANIPULATION)) AS detail, TRIGGER_NAME AS k \
                        FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?";

fn group_query(group: &str) -> Option<(&'static str, ObjectKind)> {
    Some(match group {
        "Tables" => (TABLES, ObjectKind::Table),
        "Views" => (VIEWS, ObjectKind::View),
        "Sequences" => (SEQUENCES, ObjectKind::Sequence),
        "Functions" => (FUNCTIONS, ObjectKind::Function),
        "Procedures" => (PROCEDURES, ObjectKind::Procedure),
        "Events" => (EVENTS, ObjectKind::Event),
        "Columns" => (COLUMNS, ObjectKind::Column),
        "Indexes" => (INDEXES, ObjectKind::Index),
        "Constraints" => (CONSTRAINTS, ObjectKind::Constraint),
        "Triggers" => (TRIGGERS, ObjectKind::Trigger),
        _ => return None,
    })
}

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

/// The logical kind from information_schema's DATA_TYPE.
fn kind_of(data_type: &str, column_type: &str) -> ValueKind {
    match data_type {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            ValueKind::Integer
        }
        "float" | "double" | "real" => ValueKind::Float,
        "decimal" | "numeric" => ValueKind::Decimal,
        "date" => ValueKind::Date,
        "datetime" => ValueKind::Timestamp,
        "timestamp" => ValueKind::TimestampTz,
        "time" => ValueKind::Time,
        "json" => ValueKind::Json,
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit" => {
            ValueKind::Binary
        }
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            if column_type.contains("binary") {
                ValueKind::Binary
            } else {
                ValueKind::Text
            }
        }
        "uuid" => ValueKind::Uuid,
        _ => ValueKind::Other,
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

fn text(s: &str) -> My {
    My::Bytes(s.as_bytes().to_vec())
}

impl MySqlSession {
    async fn page(
        &self,
        inner: &str,
        args: Vec<My>,
        q: &ChildQuery,
    ) -> Result<(Vec<(String, Option<String>)>, Option<String>)> {
        let limit = if q.limit == 0 {
            DEFAULT_PAGE
        } else {
            q.limit.min(MAX_PAGE)
        };
        let sql = format!(
            "SELECT name, detail, k FROM ({inner}) AS items \
             WHERE (? IS NULL OR LOWER(name) LIKE LOWER(?)) \
               AND (? IS NULL OR CAST(k AS BINARY) > CAST(? AS BINARY)) \
             ORDER BY CAST(k AS BINARY) LIMIT ?"
        );
        let pattern = q
            .filter
            .as_deref()
            .filter(|f| !f.is_empty())
            .map_or(My::NULL, |f| text(&like_pattern(f)));
        let cursor = q.cursor.as_deref().map_or(My::NULL, text);
        let mut params = args;
        params.extend([
            pattern.clone(),
            pattern,
            cursor.clone(),
            cursor,
            My::UInt(u64::from(limit) + 1),
        ]);
        let rows = self.meta(&sql, params).await?;
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let more = rows.len() > limit;
        let mut items = Vec::new();
        let mut last = None;
        for row in rows.iter().take(limit) {
            items.push((cell(row, 0).unwrap_or_default(), cell(row, 1)));
            last = cell(row, 2);
        }
        Ok((items, if more { last } else { None }))
    }

    fn schema_groups(&self) -> &'static [&'static str] {
        if self.engine == treasury_core::config::Engine::MariaDb {
            &[
                "Tables",
                "Views",
                "Sequences",
                "Functions",
                "Procedures",
                "Events",
            ]
        } else {
            &["Tables", "Views", "Functions", "Procedures", "Events"]
        }
    }

    async fn columns(&self, schema: &str, table: &str) -> Result<Vec<ColumnInfo>> {
        let rows = self
            .meta(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLUMN_COMMENT, \
                        COLUMN_KEY, DATA_TYPE, EXTRA \
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                 ORDER BY ORDINAL_POSITION",
                vec![text(schema), text(table)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let column_type = cell(r, 1).unwrap_or_default();
                ColumnInfo {
                    name: cell(r, 0).unwrap_or_default(),
                    kind: kind_of(&cell(r, 6).unwrap_or_default().to_lowercase(), &column_type),
                    type_name: column_type,
                    nullable: cell(r, 2).as_deref() == Some("YES"),
                    default: cell(r, 3),
                    comment: cell(r, 4).filter(|c| !c.is_empty()),
                    primary_key: cell(r, 5).as_deref() == Some("PRI"),
                }
            })
            .collect())
    }

    async fn table_details(
        &self,
        schema: &str,
        table: &str,
        path: &ObjectPath,
    ) -> Result<ObjectDetails> {
        let facts = self
            .meta(
                "SELECT TABLE_TYPE, ENGINE, TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH, TABLE_COMMENT, \
                        TABLE_COLLATION, AUTO_INCREMENT, CREATE_TIME \
                 FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                vec![text(schema), text(table)],
            )
            .await?;
        let facts = facts.first().ok_or_else(|| not_found(path))?;
        let is_view = cell(facts, 0).as_deref() == Some("VIEW");
        let columns = self.columns(schema, table).await?;
        let args = || vec![text(schema), text(table)];
        // MySQL lists functional index parts in EXPRESSION; MariaDB has no such column.
        let stats = match self
            .meta(
                "SELECT INDEX_NAME, NON_UNIQUE, INDEX_TYPE, \
                        COALESCE(COLUMN_NAME, CONCAT('(', EXPRESSION, ')')) \
                 FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                 ORDER BY INDEX_NAME, SEQ_IN_INDEX",
                args(),
            )
            .await
        {
            Ok(rows) => rows,
            Err(_) => {
                self.meta(
                    "SELECT INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME \
                     FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                     ORDER BY INDEX_NAME, SEQ_IN_INDEX",
                    args(),
                )
                .await?
            }
        };
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in &stats {
            let name = cell(r, 0).unwrap_or_default();
            let column = cell(r, 3).unwrap_or_default();
            match indexes.last_mut() {
                Some(ix) if ix.name == name => ix.columns.push(column),
                _ => indexes.push(IndexInfo {
                    primary: name == "PRIMARY",
                    unique: int(r, 1) == Some(0),
                    method: cell(r, 2).map(|m| m.to_lowercase()),
                    predicate: None,
                    columns: vec![column],
                    name,
                }),
            }
        }
        let fk_rows = self
            .meta(
                "SELECT CONSTRAINT_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME, \
                        COLUMN_NAME, REFERENCED_COLUMN_NAME \
                 FROM information_schema.KEY_COLUMN_USAGE \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL \
                 ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
                vec![text(schema), text(table)],
            )
            .await?;
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in &fk_rows {
            let name = cell(r, 0).unwrap_or_default();
            let (col, ref_col) = (
                cell(r, 3).unwrap_or_default(),
                cell(r, 4).unwrap_or_default(),
            );
            match foreign_keys.last_mut() {
                Some(fk) if fk.name == name => {
                    fk.columns.push(col);
                    fk.ref_columns.push(ref_col);
                }
                _ => foreign_keys.push(ForeignKeyInfo {
                    name,
                    ref_schema: cell(r, 1),
                    ref_table: cell(r, 2).unwrap_or_default(),
                    columns: vec![col],
                    ref_columns: vec![ref_col],
                }),
            }
        }
        Ok(ObjectDetails {
            columns,
            indexes,
            foreign_keys,
            row_estimate: if is_view {
                None
            } else {
                int(facts, 2).and_then(|n| u64::try_from(n).ok())
            },
            size_bytes: if is_view {
                None
            } else {
                int(facts, 3).and_then(|n| u64::try_from(n).ok())
            },
            comment: cell(facts, 4).filter(|c| !c.is_empty() && c != "VIEW"),
            properties: props(vec![
                (
                    "Kind",
                    Some(if is_view { "view" } else { "table" }.to_owned()),
                ),
                ("Engine", cell(facts, 1)),
                ("Collation", cell(facts, 5)),
                ("Next auto-increment", cell(facts, 6)),
                ("Created", cell(facts, 7)),
            ]),
        })
    }

    async fn show_create(&self, sql: String, column: usize) -> Result<Option<String>> {
        let rows = self.meta_text(&sql).await?;
        Ok(rows
            .first()
            .and_then(|r| cell(r, column))
            .map(|d| format!("{};", d.trim_end().trim_end_matches(';'))))
    }
}

#[async_trait]
impl Introspect for MySqlSession {
    async fn children(&self, parent: &ObjectPath, query: &ChildQuery) -> Result<ChildPage> {
        let schema = parent.find(ObjectKind::Database);
        match parent.0.as_slice() {
            [] => {
                let (items, next_cursor) = self.page(DATABASES, vec![], query).await?;
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
            ] => Ok(groups(parent, self.schema_groups())),
            [
                ..,
                owner,
                PathPart {
                    kind: ObjectKind::Group,
                    name,
                },
            ] => {
                let Some((inner, kind)) = group_query(name) else {
                    return Ok(ChildPage::default());
                };
                let mut args = vec![text(schema.unwrap_or(""))];
                if matches!(owner.kind, ObjectKind::Table | ObjectKind::View) {
                    args.push(text(&owner.name));
                }
                let (items, next_cursor) = self.page(inner, args, query).await?;
                Ok(ChildPage {
                    nodes: items
                        .into_iter()
                        .map(|(name, detail)| ObjectNode {
                            path: parent.child(kind, &name),
                            kind,
                            expandable: matches!(kind, ObjectKind::Table | ObjectKind::View),
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
            ] => Ok(groups(parent, &["Columns"])),
            _ => Ok(ChildPage::default()),
        }
    }

    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails> {
        let Some(leaf) = path.leaf() else {
            return Ok(ObjectDetails::default());
        };
        let schema = path.find(ObjectKind::Database).unwrap_or("");
        let table = path
            .0
            .iter()
            .rev()
            .skip(1)
            .find(|p| matches!(p.kind, ObjectKind::Table | ObjectKind::View))
            .map_or("", |p| p.name.as_str());
        let name = leaf.name.as_str();
        match leaf.kind {
            ObjectKind::Table | ObjectKind::View => self.table_details(schema, name, path).await,
            ObjectKind::Column => {
                let column = self
                    .columns(schema, table)
                    .await?
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
                    ]),
                    columns: vec![column],
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let rows = self
                    .meta(
                        "SELECT DTD_IDENTIFIER, ROUTINE_BODY, IS_DETERMINISTIC, SECURITY_TYPE, \
                                SQL_DATA_ACCESS, DEFINER, ROUTINE_COMMENT \
                         FROM information_schema.ROUTINES \
                         WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ? AND ROUTINE_TYPE = ?",
                        vec![
                            text(schema),
                            text(name),
                            text(if leaf.kind == ObjectKind::Function {
                                "FUNCTION"
                            } else {
                                "PROCEDURE"
                            }),
                        ],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found(path))?;
                Ok(ObjectDetails {
                    comment: cell(r, 6).filter(|c| !c.is_empty()),
                    properties: props(vec![
                        ("Returns", cell(r, 0)),
                        ("Language", cell(r, 1).map(|l| l.to_lowercase())),
                        ("Deterministic", cell(r, 2).map(|d| d.to_lowercase())),
                        ("Security", cell(r, 3).map(|d| d.to_lowercase())),
                        ("Data access", cell(r, 4).map(|d| d.to_lowercase())),
                        ("Definer", cell(r, 5)),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Database => {
                let rows = self
                    .meta(
                        "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME, \
                                (SELECT SUM(DATA_LENGTH + INDEX_LENGTH) FROM information_schema.TABLES t \
                                 WHERE t.TABLE_SCHEMA = s.SCHEMA_NAME) \
                         FROM information_schema.SCHEMATA s WHERE SCHEMA_NAME = ?",
                        vec![text(name)],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found(path))?;
                Ok(ObjectDetails {
                    size_bytes: int(r, 2).and_then(|n| u64::try_from(n).ok()),
                    properties: props(vec![
                        ("Character set", cell(r, 0)),
                        ("Collation", cell(r, 1)),
                    ]),
                    ..ObjectDetails::default()
                })
            }
            ObjectKind::Index => {
                let rows = self
                    .meta(
                        "SELECT INDEX_TYPE, NON_UNIQUE, GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX) \
                         FROM information_schema.STATISTICS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = ? GROUP BY INDEX_TYPE, NON_UNIQUE",
                        vec![text(schema), text(table), text(name)],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found(path))?;
                Ok(ObjectDetails {
                    properties: props(vec![
                        ("Method", cell(r, 0).map(|m| m.to_lowercase())),
                        (
                            "Unique",
                            Some(if int(r, 1) == Some(0) { "yes" } else { "no" }.into()),
                        ),
                        ("Columns", cell(r, 2)),
                    ]),
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
        let d = self.dialect();
        let schema = path.find(ObjectKind::Database).unwrap_or("");
        let qualified = render::table(d, Some(schema), &leaf.name);
        match leaf.kind {
            ObjectKind::Database => {
                self.show_create(
                    format!("SHOW CREATE DATABASE {}", render::ident(d, &leaf.name)),
                    1,
                )
                .await
            }
            ObjectKind::Table => {
                self.show_create(format!("SHOW CREATE TABLE {qualified}"), 1)
                    .await
            }
            ObjectKind::View => {
                self.show_create(format!("SHOW CREATE VIEW {qualified}"), 1)
                    .await
            }
            ObjectKind::Sequence => {
                self.show_create(format!("SHOW CREATE SEQUENCE {qualified}"), 1)
                    .await
            }
            ObjectKind::Function => {
                self.show_create(format!("SHOW CREATE FUNCTION {qualified}"), 2)
                    .await
            }
            ObjectKind::Procedure => {
                self.show_create(format!("SHOW CREATE PROCEDURE {qualified}"), 2)
                    .await
            }
            ObjectKind::Trigger => {
                self.show_create(format!("SHOW CREATE TRIGGER {qualified}"), 2)
                    .await
            }
            ObjectKind::Event => {
                self.show_create(format!("SHOW CREATE EVENT {qualified}"), 3)
                    .await
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_information_schema() {
        assert_eq!(kind_of("bigint", "bigint unsigned"), ValueKind::Integer);
        assert_eq!(kind_of("varchar", "varchar(20)"), ValueKind::Text);
        assert_eq!(kind_of("timestamp", "timestamp(6)"), ValueKind::TimestampTz);
        assert_eq!(kind_of("geometry", "geometry"), ValueKind::Other);
    }

    #[test]
    fn filters_match_literally() {
        assert_eq!(like_pattern("a_b%"), "%a\\_b\\%%");
        assert_eq!(rows_detail(2_500.0), "2.5K rows");
    }
}

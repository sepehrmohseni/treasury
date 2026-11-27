//! The key browser: databases, then their keys grouped into a tree by a delimiter (`:` unless
//! the connection says otherwise). Keys are read with SCAN, never KEYS, and a level reads at
//! most [`SCAN_LIMIT`] keys; a row then says there are more, and a filter narrows the reading.

use crate::session::RedisSession;
use async_trait::async_trait;
use std::collections::BTreeMap;
use treasury_core::driver::Introspect;
use treasury_core::exec::ValueKind;
use treasury_core::schema::{
    ChildPage, ChildQuery, ColumnInfo, ObjectDetails, ObjectKind, ObjectNode, ObjectPath,
};
use treasury_core::{Error, ErrorKind, Result};

/// The most keys one level of the tree reads.
pub const SCAN_LIMIT: usize = 20_000;
/// The most SCAN calls one level makes, so a sparse filter on a large database still ends.
const SCAN_CALLS: usize = 400;

/// `text` with the characters SCAN's MATCH treats as patterns escaped.
fn glob(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// What one level of the tree holds: groups of keys sharing the next segment, and keys.
#[derive(Debug, Default, PartialEq)]
struct Level {
    /// Full prefix (with its delimiter) and how many keys it holds.
    groups: BTreeMap<String, u64>,
    keys: Vec<String>,
}

/// Sorts `found` (keys under `prefix`) into the level below `prefix`.
fn level(found: impl IntoIterator<Item = String>, prefix: &str, delimiter: &str) -> Level {
    let mut out = Level::default();
    for key in found {
        let Some(rest) = key.strip_prefix(prefix) else {
            continue;
        };
        match rest.find(delimiter).filter(|_| !delimiter.is_empty()) {
            Some(at) => {
                let group = format!("{prefix}{}", &rest[..at + delimiter.len()]);
                *out.groups.entry(group).or_default() += 1;
            }
            None => out.keys.push(key),
        }
    }
    out.keys.sort();
    out.keys.dedup();
    out
}

fn type_name(t: &str) -> &str {
    match t {
        "ReJSON-RL" => "json",
        other => other,
    }
}

impl RedisSession {
    async fn databases(&self, query: &ChildQuery) -> Result<ChildPage> {
        let mut conn = self.conn().await;
        let info: String = self
            .meta(&mut conn, redis::cmd("INFO").arg("keyspace"))
            .await?;
        let mut dbs: BTreeMap<i64, u64> = BTreeMap::new();
        for line in info.lines() {
            let Some((name, stats)) = line.split_once(':') else {
                continue;
            };
            let Some(n) = name.strip_prefix("db").and_then(|n| n.parse().ok()) else {
                continue;
            };
            let keys = stats
                .split(',')
                .find_map(|kv| kv.strip_prefix("keys="))
                .and_then(|k| k.parse().ok())
                .unwrap_or(0);
            dbs.insert(n, keys);
        }
        // The connection's own database, and 0, even when they hold nothing yet.
        dbs.entry(self.db).or_insert(0);
        dbs.entry(0).or_insert(0);
        let nodes = dbs
            .into_iter()
            .filter(|(n, _)| {
                query
                    .filter
                    .as_deref()
                    .is_none_or(|f| format!("db{n}").contains(f) || n.to_string() == f)
            })
            .map(|(n, keys)| ObjectNode {
                path: ObjectPath::root().child(ObjectKind::Database, n.to_string()),
                kind: ObjectKind::Database,
                name: format!("db{n}"),
                expandable: true,
                detail: Some(match keys {
                    0 => "empty".to_owned(),
                    1 => "1 key".to_owned(),
                    k => format!("{k} keys"),
                }),
            })
            .collect();
        Ok(ChildPage {
            nodes,
            next_cursor: None,
        })
    }

    /// The groups and keys one level below `prefix` in database `db`.
    async fn keys(
        &self,
        parent: &ObjectPath,
        db: i64,
        prefix: &str,
        query: &ChildQuery,
    ) -> Result<ChildPage> {
        let mut conn = self.db_conn(db).await?;
        // A filter finds keys containing it anywhere, so it is not cut off by the prefix.
        let pattern = match query.filter.as_deref().filter(|f| !f.is_empty()) {
            Some(f) => format!("*{}*", glob(f)),
            None => format!("{}*", glob(prefix)),
        };
        let mut found = Vec::new();
        let mut cursor = "0".to_owned();
        let mut calls = 0;
        let mut partial = false;
        loop {
            let (next, keys): (String, Vec<String>) = self
                .meta(
                    &mut conn,
                    redis::cmd("SCAN")
                        .arg(&cursor)
                        .arg("MATCH")
                        .arg(&pattern)
                        .arg("COUNT")
                        .arg(1_000),
                )
                .await?;
            found.extend(keys.into_iter().filter(|k| k.starts_with(prefix)));
            calls += 1;
            if next == "0" {
                break;
            }
            if found.len() >= SCAN_LIMIT || calls >= SCAN_CALLS {
                partial = true;
                break;
            }
            cursor = next;
        }
        let level = level(found, prefix, &self.delimiter);
        let offset: usize = query
            .cursor
            .as_deref()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let limit = usize::try_from(query.limit).unwrap_or(200).max(1);
        let total = level.groups.len() + level.keys.len();
        let mut nodes: Vec<ObjectNode> = level
            .groups
            .iter()
            .map(|(g, n)| ObjectNode {
                path: parent.child(ObjectKind::KeyPrefix, g.clone()),
                kind: ObjectKind::KeyPrefix,
                name: g[prefix.len()..].to_owned(),
                expandable: true,
                detail: Some(if *n == 1 {
                    "1 key".to_owned()
                } else {
                    format!("{n} keys")
                }),
            })
            .chain(level.keys.iter().map(|k| ObjectNode {
                path: parent.child(ObjectKind::Key, k.clone()),
                kind: ObjectKind::Key,
                name: k[prefix.len()..].to_owned(),
                expandable: false,
                detail: None,
            }))
            .skip(offset)
            .take(limit)
            .collect();
        // Each key's type, in one round trip.
        let shown: Vec<String> = nodes
            .iter()
            .filter(|n| n.kind == ObjectKind::Key)
            .filter_map(|n| n.path.leaf().map(|l| l.name.clone()))
            .collect();
        if !shown.is_empty() {
            let mut pipe = redis::pipe();
            for k in &shown {
                pipe.cmd("TYPE").arg(k);
            }
            let types: Vec<String> =
                tokio::time::timeout(self.metadata_timeout, pipe.query_async(&mut conn))
                    .await
                    .map_err(|_| Error::timeout(self.metadata_timeout))?
                    .map_err(|e| self.map(&e))?;
            let mut types = types.into_iter();
            for n in nodes.iter_mut().filter(|n| n.kind == ObjectKind::Key) {
                n.detail = types.next().map(|t| type_name(&t).to_owned());
            }
        }
        let next = offset + nodes.len();
        if partial && next >= total {
            nodes.push(ObjectNode {
                path: parent.child(ObjectKind::Group, "more"),
                kind: ObjectKind::Group,
                name: format!("More than {SCAN_LIMIT} keys here; filter to find the others"),
                expandable: false,
                detail: None,
            });
        }
        Ok(ChildPage {
            nodes,
            next_cursor: (next < total).then(|| next.to_string()),
        })
    }
}

/// The database a path is in.
fn db_of(path: &ObjectPath) -> Result<i64> {
    path.find(ObjectKind::Database)
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| Error::internal("a Redis path without its database"))
}

#[async_trait]
impl Introspect for RedisSession {
    async fn children(&self, parent: &ObjectPath, query: &ChildQuery) -> Result<ChildPage> {
        match parent.leaf().map(|l| (l.kind, l.name.as_str())) {
            None => self.databases(query).await,
            Some((ObjectKind::Database, _)) => self.keys(parent, db_of(parent)?, "", query).await,
            Some((ObjectKind::KeyPrefix, prefix)) => {
                self.keys(parent, db_of(parent)?, prefix, query).await
            }
            _ => Ok(ChildPage::default()),
        }
    }

    async fn details(&self, path: &ObjectPath) -> Result<ObjectDetails> {
        let Some(leaf) = path.leaf().filter(|l| l.kind == ObjectKind::Key) else {
            return Ok(ObjectDetails::default());
        };
        let key = leaf.name.as_str();
        let mut conn = self.db_conn(db_of(path)?).await?;
        let kind: String = self.meta(&mut conn, redis::cmd("TYPE").arg(key)).await?;
        if kind == "none" {
            return Err(Error::new(
                ErrorKind::Server,
                format!("The key \"{key}\" no longer exists."),
            ));
        }
        let ttl: i64 = self.meta(&mut conn, redis::cmd("TTL").arg(key)).await?;
        let length_cmd = match kind.as_str() {
            "string" => Some("STRLEN"),
            "hash" => Some("HLEN"),
            "list" => Some("LLEN"),
            "set" => Some("SCARD"),
            "zset" => Some("ZCARD"),
            "stream" => Some("XLEN"),
            _ => None,
        };
        let length: Option<u64> = match length_cmd {
            Some(c) => self.meta(&mut conn, redis::cmd(c).arg(key)).await.ok(),
            None => None,
        };
        // Both need rights a read-only user may not have; without them they are left out.
        let memory: Option<u64> = self
            .meta(&mut conn, redis::cmd("MEMORY").arg("USAGE").arg(key))
            .await
            .ok();
        let encoding: Option<String> = self
            .meta(&mut conn, redis::cmd("OBJECT").arg("ENCODING").arg(key))
            .await
            .ok();
        let column = |name: &str, kind: ValueKind| ColumnInfo {
            name: name.into(),
            type_name: "string".into(),
            kind,
            nullable: false,
            default: None,
            comment: None,
            primary_key: false,
        };
        let columns = match kind.as_str() {
            "hash" => vec![
                column("field", ValueKind::Text),
                column("value", ValueKind::Text),
            ],
            "set" => vec![column("member", ValueKind::Text)],
            "zset" => vec![
                column("member", ValueKind::Text),
                column("score", ValueKind::Float),
            ],
            "stream" => vec![
                column("id", ValueKind::Text),
                column("fields", ValueKind::Json),
            ],
            _ => vec![column("value", ValueKind::Text)],
        };
        let mut properties = vec![
            ("Type".to_owned(), type_name(&kind).to_owned()),
            (
                "Expires".to_owned(),
                if ttl < 0 {
                    "never".to_owned()
                } else {
                    format!("in {ttl} s")
                },
            ),
        ];
        if let Some(l) = length {
            properties.push(("Length".to_owned(), l.to_string()));
        }
        if let Some(e) = encoding {
            properties.push(("Encoding".to_owned(), e));
        }
        Ok(ObjectDetails {
            columns,
            row_estimate: length,
            size_bytes: memory,
            properties,
            ..ObjectDetails::default()
        })
    }

    async fn ddl(&self, _path: &ObjectPath) -> Result<Option<String>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_group_by_their_next_segment() {
        let keys = ["user:1:name", "user:1:mail", "user:2", "order:9", "plain"].map(String::from);
        let top = level(keys.clone(), "", ":");
        assert_eq!(
            top.groups.into_iter().collect::<Vec<_>>(),
            [("order:".to_owned(), 1), ("user:".to_owned(), 3)]
        );
        assert_eq!(top.keys, ["plain"]);
        let user = level(keys, "user:", ":");
        assert_eq!(user.groups.keys().collect::<Vec<_>>(), ["user:1:"]);
        assert_eq!(user.keys, ["user:2"]);
    }

    #[test]
    fn patterns_are_escaped() {
        assert_eq!(glob("a*b?[c]\\"), "a\\*b\\?\\[c\\]\\\\");
    }
}

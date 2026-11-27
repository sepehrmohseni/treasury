//! The Redis editor language: one command per line, its words split the way redis-cli splits
//! them (double quotes with escapes, single quotes, or bare words). A line starting with `#` is
//! a comment.
//!
//! Classification uses the ACL categories the server reports for each command (`COMMAND`),
//! read when the connection opens: `@write` writes, `@admin` and `@dangerous` are
//! administration, `@read` reads. A built-in table covers servers that do not allow `COMMAND`.
//! Scripts and module commands nobody describes are `Unknown`, which the guardrails treat as
//! writes. FLUSHDB, FLUSHALL, CONFIG, SHUTDOWN and DEBUG never run on production.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use treasury_core::analyze::{
    Analyzer, Classified, CountPlan, Destructive, Flags, LimitPlan, Placeholder, StatementClass,
};

/// Splits `line` into words as redis-cli does.
pub fn words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            return Ok(out);
        };
        let mut word = String::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err("A quoted word is not closed.".into()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => word.push('\n'),
                            Some('r') => word.push('\r'),
                            Some('t') => word.push('\t'),
                            Some('b') => word.push('\u{8}'),
                            Some('a') => word.push('\u{7}'),
                            Some('x') => {
                                let hex: String = chars.by_ref().take(2).collect();
                                let byte = u8::from_str_radix(&hex, 16)
                                    .map_err(|_| format!("\\x{hex} is not a byte."))?;
                                // Commands go to the server as text, so a byte must be one.
                                if !byte.is_ascii() {
                                    return Err(format!(
                                        "\\x{hex} is not a character; Treasury sends commands as text."
                                    ));
                                }
                                word.push(char::from(byte));
                            }
                            Some(c) => word.push(c),
                            None => return Err("A quoted word is not closed.".into()),
                        },
                        Some(c) => word.push(c),
                    }
                }
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err("A quoted word is not closed.".into()),
                        Some('\'') => break,
                        Some('\\') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            word.push('\'');
                        }
                        Some(c) => word.push(c),
                    }
                }
            }
            _ => {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    word.push(c);
                }
            }
        }
        if chars.peek().is_some_and(|c| !c.is_whitespace()) {
            return Err("A quoted word must be followed by a space.".into());
        }
        out.push(word);
    }
}

/// `word` as the editor would need it typed: bare when it can be, quoted otherwise.
pub fn quote(word: &str) -> String {
    let bare = !word.is_empty()
        && word
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\'));
    if bare {
        return word.to_owned();
    }
    let mut out = String::from("\"");
    for c in word.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One command of a script: its byte range and its words, when they split.
pub(crate) struct Piece {
    pub range: Range<usize>,
    pub words: Result<Vec<String>, String>,
}

/// The commands of `script`: its lines that are not empty or comments.
pub(crate) fn pieces(script: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in script.split_inclusive('\n') {
        let start = at;
        at += line.len();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let lead = line.len() - line.trim_start().len();
        let range = start + lead..start + lead + trimmed.len();
        out.push(Piece {
            range,
            words: words(trimmed),
        });
    }
    out
}

/// The ACL categories of each command, as the server describes them. Subcommands are keyed
/// `container|sub`, as the server names them.
#[derive(Debug, Default)]
pub struct CommandTable {
    categories: HashMap<String, Vec<String>>,
}

impl CommandTable {
    /// A table from `(name, categories)` pairs; names are made lower case, categories keep or
    /// lose their `@`.
    pub fn new(entries: impl IntoIterator<Item = (String, Vec<String>)>) -> Self {
        Self {
            categories: entries
                .into_iter()
                .map(|(n, c)| {
                    (
                        n.to_ascii_lowercase(),
                        c.into_iter()
                            .map(|c| c.trim_start_matches('@').to_ascii_lowercase())
                            .collect(),
                    )
                })
                .collect(),
        }
    }

    /// Whether the server described any command; an empty table means the built-in one is used.
    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
    }

    fn get(&self, name: &str, sub: Option<&str>) -> Option<&Vec<String>> {
        sub.and_then(|s| self.categories.get(&format!("{name}|{s}")))
            .or_else(|| self.categories.get(name))
    }
}

/// Commands that read, for servers that do not describe their commands.
const READS: &[&str] = &[
    "get",
    "mget",
    "strlen",
    "getrange",
    "substr",
    "exists",
    "type",
    "ttl",
    "pttl",
    "expiretime",
    "pexpiretime",
    "scan",
    "hget",
    "hgetall",
    "hmget",
    "hlen",
    "hkeys",
    "hvals",
    "hexists",
    "hscan",
    "hstrlen",
    "hrandfield",
    "lrange",
    "llen",
    "lindex",
    "lpos",
    "smembers",
    "sismember",
    "smismember",
    "scard",
    "srandmember",
    "sscan",
    "sinter",
    "sintercard",
    "sunion",
    "sdiff",
    "zrange",
    "zrangebyscore",
    "zrangebylex",
    "zrevrange",
    "zrevrangebyscore",
    "zrevrangebylex",
    "zscore",
    "zmscore",
    "zcard",
    "zcount",
    "zlexcount",
    "zrank",
    "zrevrank",
    "zscan",
    "zrandmember",
    "zdiff",
    "zinter",
    "zunion",
    "xrange",
    "xrevrange",
    "xlen",
    "xread",
    "xinfo",
    "xpending",
    "dbsize",
    "bitcount",
    "bitpos",
    "getbit",
    "bitfield_ro",
    "pfcount",
    "geopos",
    "geodist",
    "geohash",
    "geosearch",
    "georadius_ro",
    "georadiusbymember_ro",
    "lcs",
    "sort_ro",
    "randomkey",
    "touch",
    "dump",
    "object",
    "memory",
    "info",
    "time",
    "lastsave",
    "json.get",
    "json.mget",
    "json.type",
    "json.strlen",
    "json.objkeys",
    "json.objlen",
    "json.arrlen",
    "json.resp",
];

/// Commands that write, for servers that do not describe their commands.
const WRITES: &[&str] = &[
    "set",
    "setnx",
    "setex",
    "psetex",
    "mset",
    "msetnx",
    "append",
    "incr",
    "incrby",
    "incrbyfloat",
    "decr",
    "decrby",
    "getset",
    "getdel",
    "getex",
    "setrange",
    "setbit",
    "bitfield",
    "bitop",
    "del",
    "unlink",
    "expire",
    "pexpire",
    "expireat",
    "pexpireat",
    "persist",
    "rename",
    "renamenx",
    "move",
    "copy",
    "restore",
    "hset",
    "hsetnx",
    "hmset",
    "hdel",
    "hincrby",
    "hincrbyfloat",
    "lpush",
    "rpush",
    "lpushx",
    "rpushx",
    "lpop",
    "rpop",
    "lmpop",
    "blpop",
    "brpop",
    "blmpop",
    "lset",
    "lrem",
    "linsert",
    "ltrim",
    "lmove",
    "blmove",
    "rpoplpush",
    "brpoplpush",
    "sadd",
    "srem",
    "spop",
    "smove",
    "sinterstore",
    "sunionstore",
    "sdiffstore",
    "zadd",
    "zrem",
    "zincrby",
    "zpopmin",
    "zpopmax",
    "bzpopmin",
    "bzpopmax",
    "zmpop",
    "bzmpop",
    "zremrangebyrank",
    "zremrangebyscore",
    "zremrangebylex",
    "zunionstore",
    "zinterstore",
    "zdiffstore",
    "zrangestore",
    "xadd",
    "xdel",
    "xtrim",
    "xgroup",
    "xack",
    "xclaim",
    "xautoclaim",
    "xreadgroup",
    "pfadd",
    "pfmerge",
    "geoadd",
    "georadius",
    "georadiusbymember",
    "geosearchstore",
    "sort",
    "flushdb",
    "flushall",
    "swapdb",
    "json.set",
    "json.del",
    "json.forget",
    "json.mset",
    "json.merge",
    "json.arrappend",
    "json.arrinsert",
    "json.arrpop",
    "json.arrtrim",
    "json.numincrby",
    "json.strappend",
    "json.toggle",
    "json.clear",
];

/// Administration, for servers that do not describe their commands.
const ADMIN: &[&str] = &[
    "config",
    "shutdown",
    "debug",
    "client",
    "acl",
    "save",
    "bgsave",
    "bgrewriteaof",
    "replicaof",
    "slaveof",
    "failover",
    "cluster",
    "module",
    "slowlog",
    "latency",
    "keys",
    "role",
    "monitor",
    "sync",
    "psync",
    "lolwut",
    "readonly",
    "readwrite",
];

/// Never on production, even with writes unlocked.
const FORBIDDEN_IN_PRODUCTION: &[&str] = &["flushdb", "flushall", "config", "shutdown", "debug"];

/// Commands Treasury does not run: they need a connection of their own or never end.
pub(crate) const UNSUPPORTED: &[&str] = &[
    "multi",
    "exec",
    "discard",
    "watch",
    "unwatch",
    "subscribe",
    "psubscribe",
    "ssubscribe",
    "unsubscribe",
    "punsubscribe",
    "sunsubscribe",
    "monitor",
    "sync",
    "psync",
    "hello",
    "reset",
    "quit",
    "auth",
];

/// Commands with subcommands, whose class depends on the subcommand.
const CONTAINERS: &[&str] = &[
    "config", "client", "acl", "cluster", "command", "memory", "object", "script", "function",
    "slowlog", "latency", "module", "xinfo", "xgroup", "pubsub",
];

/// What one command does, from its words.
fn verdict(table: &CommandTable, argv: &[String]) -> (StatementClass, Flags, Option<String>) {
    let Some(name) = argv.first().map(|n| n.to_ascii_lowercase()) else {
        return (StatementClass::Unknown, Flags::default(), None);
    };
    let sub = CONTAINERS
        .contains(&name.as_str())
        .then(|| argv.get(1).map(|s| s.to_ascii_lowercase()))
        .flatten();
    let flags = Flags {
        production_forbidden: FORBIDDEN_IN_PRODUCTION.contains(&name.as_str()),
        destructive: matches!(name.as_str(), "flushdb" | "flushall")
            .then_some(Destructive::Truncate),
        ..Flags::default()
    };
    if UNSUPPORTED.contains(&name.as_str()) {
        return (
            StatementClass::Unknown,
            flags,
            Some(format!(
                "Treasury runs one command at a time on a shared connection, so it does not run {}.",
                name.to_ascii_uppercase()
            )),
        );
    }
    let class = match name.as_str() {
        // Scripts can do anything: unless they are the read-only forms, they count as writes.
        "eval" | "evalsha" | "fcall" | "script" | "function" => {
            return (
                StatementClass::Unknown,
                flags,
                Some("A script can change any key, so Treasury treats it as a write.".into()),
            );
        }
        "eval_ro" | "evalsha_ro" | "fcall_ro" => StatementClass::Read,
        // Harmless, though their categories say neither read nor write.
        "ping" | "echo" | "select" | "command" | "time" | "dbsize" | "info" => StatementClass::Read,
        // KEYS walks every key at once and can stall the server; it is administration.
        "keys" => StatementClass::Admin,
        _ => match table.get(&name, sub.as_deref()) {
            Some(c) if c.iter().any(|c| c == "write") => StatementClass::Write,
            Some(c) if c.iter().any(|c| c == "admin" || c == "dangerous") => StatementClass::Admin,
            Some(c) if c.iter().any(|c| c == "read") => StatementClass::Read,
            Some(_) => StatementClass::Unknown,
            None if table.is_empty() && READS.contains(&name.as_str()) => StatementClass::Read,
            None if table.is_empty() && WRITES.contains(&name.as_str()) => StatementClass::Write,
            None if table.is_empty() && ADMIN.contains(&name.as_str()) => StatementClass::Admin,
            None => StatementClass::Unknown,
        },
    };
    let reason = (class == StatementClass::Unknown).then(|| {
        format!(
            "Treasury does not know the command {}.",
            name.to_ascii_uppercase()
        )
    });
    (class, flags, reason)
}

/// The command and its first argument, for the confirmation dialog.
fn summary(argv: &[String]) -> String {
    argv.iter()
        .take(2)
        .enumerate()
        .map(|(i, w)| {
            if i == 0 {
                w.to_ascii_uppercase()
            } else {
                quote(w)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Redis analyzer, with the command table of the server it was read from.
#[derive(Clone, Default)]
pub struct RedisAnalyzer {
    table: Arc<CommandTable>,
}

impl RedisAnalyzer {
    /// An analyzer using `table`.
    pub fn new(table: CommandTable) -> Self {
        Self {
            table: Arc::new(table),
        }
    }
}

impl Analyzer for RedisAnalyzer {
    fn classify(&self, script: &str) -> Vec<Classified> {
        pieces(script)
            .into_iter()
            .map(|p| match &p.words {
                Ok(argv) => {
                    let (class, flags, reason) = verdict(&self.table, argv);
                    Classified {
                        range: p.range,
                        class,
                        flags,
                        summary: summary(argv),
                        reason,
                    }
                }
                Err(e) => Classified {
                    range: p.range,
                    class: StatementClass::Unknown,
                    flags: Flags::default(),
                    summary: String::new(),
                    reason: Some(e.clone()),
                },
            })
            .collect()
    }

    fn limit(&self, statement: &str, _limit: u64) -> LimitPlan {
        match self.classify(statement).first().map(|c| c.class) {
            Some(StatementClass::Read) => LimitPlan::StopFetching,
            _ => LimitPlan::NotApplicable,
        }
    }

    /// DEL and UNLINK count the keys that exist first, with EXISTS.
    fn count(&self, statement: &str) -> CountPlan {
        let pieces = pieces(statement);
        let [p] = pieces.as_slice() else {
            return CountPlan::NotApplicable;
        };
        let Ok(argv) = &p.words else {
            return CountPlan::NotApplicable;
        };
        match argv.first().map(|n| n.to_ascii_lowercase()).as_deref() {
            Some("del" | "unlink") if argv.len() > 1 => {
                let keys: Vec<String> = argv[1..].iter().map(|k| quote(k)).collect();
                CountPlan::Query(format!("EXISTS {}", keys.join(" ")))
            }
            _ => CountPlan::NotApplicable,
        }
    }

    fn placeholders(&self, _statement: &str) -> Vec<Placeholder> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(line: &str) -> Vec<String> {
        words(line).unwrap()
    }

    #[test]
    fn words_split_as_redis_cli_splits_them() {
        assert_eq!(w("SET  key value"), ["SET", "key", "value"]);
        assert_eq!(
            w(r#"SET "a key" "two\nlines\x41""#),
            ["SET", "a key", "two\nlinesA"]
        );
        assert_eq!(w(r"SET k 'it\'s'"), ["SET", "k", "it's"]);
        assert!(words(r#"SET "open"#).is_err());
        assert!(words(r#"SET "a"b"#).is_err());
        assert!(w("   ").is_empty());
    }

    #[test]
    fn quoting_round_trips() {
        for word in [
            "plain",
            "with space",
            "q\"uote",
            "back\\slash",
            "new\nline",
            "",
        ] {
            assert_eq!(w(&quote(word)), [word], "{word:?}");
        }
        assert_eq!(quote("user:1"), "user:1");
    }

    fn table() -> CommandTable {
        CommandTable::new([
            (
                "get".to_owned(),
                vec!["@read".to_owned(), "@string".to_owned()],
            ),
            ("set".to_owned(), vec!["@write".to_owned()]),
            ("config|get".to_owned(), vec!["@admin".to_owned()]),
            (
                "flushall".to_owned(),
                vec!["@write".to_owned(), "@dangerous".to_owned()],
            ),
            ("client|setname".to_owned(), vec!["@connection".to_owned()]),
        ])
    }

    fn class(t: &CommandTable, line: &str) -> (StatementClass, Flags) {
        let (c, f, _) = verdict(t, &w(line));
        (c, f)
    }

    #[test]
    fn categories_from_the_server_decide() {
        let t = table();
        assert_eq!(class(&t, "GET k").0, StatementClass::Read);
        assert_eq!(class(&t, "set k v").0, StatementClass::Write);
        assert_eq!(class(&t, "CONFIG GET maxmemory").0, StatementClass::Admin);
        assert!(class(&t, "CONFIG GET maxmemory").1.production_forbidden);
        let (c, f) = class(&t, "FLUSHALL");
        assert_eq!(c, StatementClass::Write);
        assert!(f.production_forbidden && f.destructive.is_some());
        // Neither read nor write: not known to be safe.
        assert_eq!(class(&t, "CLIENT SETNAME x").0, StatementClass::Unknown);
        assert_eq!(class(&t, "MODULE.THING k").0, StatementClass::Unknown);
        assert_eq!(class(&t, "PING").0, StatementClass::Read);
    }

    #[test]
    fn without_the_servers_table_the_built_in_one_is_used() {
        let t = CommandTable::default();
        assert_eq!(class(&t, "HGETALL h").0, StatementClass::Read);
        assert_eq!(class(&t, "HSET h f v").0, StatementClass::Write);
        assert_eq!(class(&t, "KEYS *").0, StatementClass::Admin);
        assert_eq!(class(&t, "SOMETHING.NEW x").0, StatementClass::Unknown);
    }

    #[test]
    fn scripts_and_session_commands() {
        let t = table();
        assert_eq!(class(&t, "EVAL \"return 1\" 0").0, StatementClass::Unknown);
        assert_eq!(class(&t, "EVAL_RO \"return 1\" 0").0, StatementClass::Read);
        let (c, _, why) = verdict(&t, &w("MULTI"));
        assert_eq!(c, StatementClass::Unknown);
        assert!(why.unwrap().contains("one command at a time"));
    }

    #[test]
    fn scripts_split_into_lines_and_skip_comments() {
        let a = RedisAnalyzer::new(table());
        let script = "# setup\nSET a 1\n\n  GET a  \n";
        let c = a.classify(script);
        assert_eq!(c.len(), 2);
        assert_eq!(&script[c[1].range.clone()], "GET a");
        assert_eq!(c[0].summary, "SET a");
    }

    #[test]
    fn deletes_count_the_keys_that_exist() {
        let a = RedisAnalyzer::new(table());
        assert_eq!(
            a.count("DEL a \"b c\""),
            CountPlan::Query("EXISTS a \"b c\"".into())
        );
        assert_eq!(a.count("SET a 1"), CountPlan::NotApplicable);
    }
}

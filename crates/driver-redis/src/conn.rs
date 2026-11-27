//! The connection commands go over: to one server, or to a cluster, which sends each command to
//! the node that holds its key.

use redis::aio::MultiplexedConnection;
use redis::cluster::ClusterClient;
use redis::cluster_async::ClusterConnection;
use redis::cluster_routing::{RoutingInfo, SingleNodeRoutingInfo};
use redis::{AsyncConnectionConfig, Client, Cmd, FromRedisValue, Pipeline, RedisResult, Value};
use std::time::Duration;

/// A connection to one server or to a cluster.
#[derive(Clone)]
pub(crate) enum Conn {
    One(MultiplexedConnection),
    Cluster(ClusterConnection),
}

impl Conn {
    pub(crate) async fn query<T: FromRedisValue>(&mut self, cmd: &Cmd) -> RedisResult<T> {
        match self {
            Conn::One(c) => cmd.query_async(c).await,
            Conn::Cluster(c) => cmd.query_async(c).await,
        }
    }

    /// `pipe` in one round trip. Only for one server: a cluster would split it by node.
    pub(crate) async fn pipeline<T: FromRedisValue>(&mut self, pipe: &Pipeline) -> RedisResult<T> {
        match self {
            Conn::One(c) => pipe.query_async(c).await,
            Conn::Cluster(c) => pipe.query_async(c).await,
        }
    }

    /// `cmd` on the cluster node at `host`:`port` (on one server, on that server).
    pub(crate) async fn on_node(&mut self, cmd: Cmd, host: &str, port: u16) -> RedisResult<Value> {
        match self {
            Conn::One(c) => cmd.query_async(c).await,
            Conn::Cluster(c) => {
                c.route_command(
                    cmd,
                    RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                        host: host.to_owned(),
                        port,
                    }),
                )
                .await
            }
        }
    }

    pub(crate) fn is_cluster(&self) -> bool {
        matches!(self, Conn::Cluster(_))
    }
}

/// What opens connections again: a client for one server, or for a cluster.
#[derive(Clone)]
pub(crate) enum Opener {
    One(Box<Client>),
    Cluster(Box<ClusterClient>),
}

impl Opener {
    pub(crate) async fn open(&self, timeout: Duration) -> RedisResult<Conn> {
        match self {
            Opener::One(c) => {
                let options = AsyncConnectionConfig::new()
                    .set_connection_timeout(Some(timeout))
                    .set_response_timeout(None);
                Ok(Conn::One(
                    c.get_multiplexed_async_connection_with_config(&options)
                        .await?,
                ))
            }
            Opener::Cluster(c) => Ok(Conn::Cluster(c.get_async_connection().await?)),
        }
    }

    /// The client of the one server, for stopping its commands from a second connection.
    pub(crate) fn single(&self) -> Option<&Client> {
        match self {
            Opener::One(c) => Some(c),
            Opener::Cluster(_) => None,
        }
    }
}

/// The primaries of a cluster, from `CLUSTER SLOTS`: each range's first node.
pub(crate) fn primaries(slots: &Value) -> Vec<(String, u16)> {
    let items = |v: &Value| match v {
        Value::Array(a) | Value::Set(a) => a.clone(),
        _ => Vec::new(),
    };
    let mut out: Vec<(String, u16)> = Vec::new();
    for range in items(slots) {
        let range = items(&range);
        let Some(node) = range.get(2).map(items) else {
            continue;
        };
        let host = match node.first() {
            Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(Value::SimpleString(s)) => s.clone(),
            _ => continue,
        };
        let Some(Value::Int(port)) = node.get(1) else {
            continue;
        };
        let Ok(port) = u16::try_from(*port) else {
            continue;
        };
        if !out.contains(&(host.clone(), port)) {
            out.push((host, port));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primaries_come_from_the_slot_ranges() {
        let node = |h: &str, p: i64| {
            Value::Array(vec![
                Value::BulkString(h.as_bytes().to_vec()),
                Value::Int(p),
                Value::BulkString(b"id".to_vec()),
            ])
        };
        let slots = Value::Array(vec![
            Value::Array(vec![
                Value::Int(0),
                Value::Int(5460),
                node("10.0.0.1", 6379),
                node("10.0.0.4", 6379),
            ]),
            Value::Array(vec![
                Value::Int(5461),
                Value::Int(10922),
                node("10.0.0.2", 6379),
            ]),
            Value::Array(vec![
                Value::Int(10923),
                Value::Int(16383),
                node("10.0.0.1", 6379),
            ]),
        ]);
        assert_eq!(
            primaries(&slots),
            [("10.0.0.1".to_owned(), 6379), ("10.0.0.2".to_owned(), 6379)]
        );
    }
}

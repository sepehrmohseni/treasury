//! Production guardrails: what must happen before a statement runs. These rules are enforced
//! here, in Rust; the UI only displays the decisions and collects confirmations.

use crate::analyze::{Analyzer, Classified, CountPlan, Destructive, StatementClass, TxControl};
use crate::config::Environment;
use crate::driver::ReadOnlyEnforcement;
use crate::plan::ExplainMode;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long "Unlock writes" lasts.
pub const UNLOCK_DURATION: Duration = Duration::from_secs(15 * 60);

/// Above this many rows an UPDATE or DELETE preview requires typing the connection name.
pub const DEFAULT_COUNT_THRESHOLD: u64 = 1_000;

/// The connection facts the guardrails depend on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The connection's name, which the user types to confirm.
    pub connection_name: String,
    /// Its environment.
    pub environment: Environment,
    /// Whether the session is read-only right now (configured read-only or production, and
    /// writes not unlocked).
    pub read_only: bool,
    /// Who enforces read-only on this engine.
    pub enforcement: ReadOnlyEnforcement,
    /// Row count above which a preview requires the connection name.
    pub count_threshold: u64,
}

/// Why a statement cannot run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// The session is read-only and the statement may write.
    ReadOnly,
    /// The statement would switch off the server's read-only mode.
    ReadOnlyEscape,
    /// Not allowed on production at all.
    ProductionForbidden,
}

/// What the user has to confirm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Confirmation {
    /// Run `query` to count the rows affected and show the count before running. The
    /// statement then needs [`Confirmation::TypeName`] if the count exceeds the threshold.
    CountRows {
        /// The derived count query.
        query: String,
    },
    /// Type the connection name.
    TypeName {
        /// Why, for the dialog.
        reason: NameReason,
    },
}

/// Why the connection name must be typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NameReason {
    /// DROP, TRUNCATE or ALTER on production.
    Destructive {
        /// Which one.
        what: Destructive,
    },
    /// UPDATE or DELETE without WHERE.
    Unfiltered,
    /// The count exceeded the threshold.
    ManyRows {
        /// The count.
        count: u64,
    },
    /// The preview could not be derived.
    NoPreview {
        /// Why not.
        detail: String,
    },
}

/// The decision for one statement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Run it.
    Allow,
    /// Do not run it.
    Block {
        /// Why.
        reason: BlockReason,
    },
    /// Run it after these confirmations, in order.
    Confirm {
        /// The steps.
        steps: Vec<Confirmation>,
    },
}

/// One statement and its decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewed {
    /// The analysis.
    pub statement: Classified,
    /// The decision.
    pub decision: Decision,
}

/// Reviews every statement of `script`.
pub fn review(policy: &Policy, analyzer: &dyn Analyzer, script: &str) -> Vec<Reviewed> {
    analyzer
        .classify(script)
        .into_iter()
        .map(|statement| {
            let text = script.get(statement.range.clone()).unwrap_or("");
            let decision = decide(policy, &statement, || analyzer.count(text));
            Reviewed {
                statement,
                decision,
            }
        })
        .collect()
}

/// The decision for one classified statement. `count` derives the preview query when needed.
pub fn decide(
    policy: &Policy,
    statement: &Classified,
    count: impl FnOnce() -> CountPlan,
) -> Decision {
    let flags = &statement.flags;
    let production = policy.environment == Environment::Production;
    if production && flags.production_forbidden {
        return Decision::Block {
            reason: BlockReason::ProductionForbidden,
        };
    }
    if policy.read_only {
        if flags.read_only_escape
            || matches!(
                flags.transaction,
                Some(TxControl::Begin {
                    read_write: Some(true)
                })
            )
        {
            return Decision::Block {
                reason: BlockReason::ReadOnlyEscape,
            };
        }
        let blocked = match policy.enforcement {
            // The server refuses writes itself; statements Treasury cannot classify go to it.
            ReadOnlyEnforcement::Server => matches!(
                statement.class,
                StatementClass::Write | StatementClass::Ddl | StatementClass::Admin
            ),
            ReadOnlyEnforcement::Client => statement.class.may_write(),
        };
        if blocked {
            return Decision::Block {
                reason: BlockReason::ReadOnly,
            };
        }
        return Decision::Allow;
    }

    let mut steps = Vec::new();
    if production && let Some(what) = flags.destructive {
        steps.push(Confirmation::TypeName {
            reason: NameReason::Destructive { what },
        });
    }
    // Unknown counts as a write: an UPDATE or DELETE the parser could not read gets no preview,
    // so it needs the connection name like any other form the rewriter cannot handle.
    if policy.environment != Environment::Local
        && matches!(
            statement.class,
            StatementClass::Write | StatementClass::Unknown
        )
    {
        match count() {
            CountPlan::Query(query) => {
                steps.push(Confirmation::CountRows { query });
                if flags.unfiltered {
                    steps.push(Confirmation::TypeName {
                        reason: NameReason::Unfiltered,
                    });
                }
            }
            CountPlan::Underivable { reason } => steps.push(Confirmation::TypeName {
                reason: NameReason::NoPreview { detail: reason },
            }),
            CountPlan::NotApplicable => {}
        }
    }
    if steps.is_empty() {
        Decision::Allow
    } else {
        Decision::Confirm { steps }
    }
}

/// After a count preview: whether the user must also type the connection name.
pub fn after_count(policy: &Policy, count: u64) -> Option<Confirmation> {
    (count > policy.count_threshold).then_some(Confirmation::TypeName {
        reason: NameReason::ManyRows { count },
    })
}

/// Whether `typed` confirms the connection. Surrounding spaces are ignored; case is not.
pub fn name_matches(policy: &Policy, typed: &str) -> bool {
    !policy.connection_name.is_empty() && typed.trim() == policy.connection_name.trim()
}

/// The plan mode actually used: plans that execute the statement only run for reads.
pub fn explain_mode(class: StatementClass, requested: ExplainMode) -> ExplainMode {
    match (requested, class) {
        (ExplainMode::Analyze, StatementClass::Read) => ExplainMode::Analyze,
        _ => ExplainMode::Estimate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::Flags;

    fn policy(env: Environment, read_only: bool, enforcement: ReadOnlyEnforcement) -> Policy {
        Policy {
            connection_name: "orders-prod".into(),
            environment: env,
            read_only,
            enforcement,
            count_threshold: DEFAULT_COUNT_THRESHOLD,
        }
    }

    fn st(class: StatementClass, flags: Flags) -> Classified {
        Classified {
            range: 0..1,
            class,
            flags,
            summary: String::new(),
            reason: None,
        }
    }

    fn no_count() -> CountPlan {
        CountPlan::NotApplicable
    }

    #[test]
    fn read_only_server_blocks_known_writes_and_escapes() {
        let p = policy(Environment::Production, true, ReadOnlyEnforcement::Server);
        let block = |c, f| decide(&p, &st(c, f), no_count);
        assert_eq!(
            block(StatementClass::Read, Flags::default()),
            Decision::Allow
        );
        assert_eq!(
            block(StatementClass::Write, Flags::default()),
            Decision::Block {
                reason: BlockReason::ReadOnly
            }
        );
        // The server is the authority for what Treasury cannot classify.
        assert_eq!(
            block(StatementClass::Unknown, Flags::default()),
            Decision::Allow
        );
        let escape = Flags {
            read_only_escape: true,
            ..Flags::default()
        };
        assert_eq!(
            block(StatementClass::Admin, escape),
            Decision::Block {
                reason: BlockReason::ReadOnlyEscape
            }
        );
        let begin_rw = Flags {
            transaction: Some(TxControl::Begin {
                read_write: Some(true),
            }),
            ..Flags::default()
        };
        assert_eq!(
            block(StatementClass::Read, begin_rw),
            Decision::Block {
                reason: BlockReason::ReadOnlyEscape
            }
        );
    }

    #[test]
    fn read_only_client_blocks_unknown() {
        let p = policy(Environment::Production, true, ReadOnlyEnforcement::Client);
        assert_eq!(
            decide(&p, &st(StatementClass::Unknown, Flags::default()), no_count),
            Decision::Block {
                reason: BlockReason::ReadOnly
            }
        );
        assert_eq!(
            decide(&p, &st(StatementClass::Read, Flags::default()), no_count),
            Decision::Allow
        );
    }

    #[test]
    fn unknown_statements_get_the_write_preview() {
        // An UPDATE the parser could not read: no count, so the name is required.
        let p = policy(Environment::Production, false, ReadOnlyEnforcement::Server);
        let underivable = || CountPlan::Underivable {
            reason: "Treasury could not parse it.".into(),
        };
        assert_eq!(
            decide(
                &p,
                &st(StatementClass::Unknown, Flags::default()),
                underivable
            ),
            Decision::Confirm {
                steps: vec![Confirmation::TypeName {
                    reason: NameReason::NoPreview {
                        detail: "Treasury could not parse it.".into()
                    }
                }]
            }
        );
        // A procedure call parses but is no UPDATE or DELETE: it runs once writes are unlocked.
        assert_eq!(
            decide(&p, &st(StatementClass::Unknown, Flags::default()), no_count),
            Decision::Allow
        );
        // On a local database nothing is previewed.
        let local = policy(Environment::Local, false, ReadOnlyEnforcement::Server);
        assert_eq!(
            decide(
                &local,
                &st(StatementClass::Unknown, Flags::default()),
                underivable
            ),
            Decision::Allow
        );
    }

    #[test]
    fn unlocked_production_confirms_destructive_ddl_by_name() {
        let p = policy(Environment::Production, false, ReadOnlyEnforcement::Server);
        let drop = Flags {
            destructive: Some(Destructive::Drop),
            ..Flags::default()
        };
        assert_eq!(
            decide(&p, &st(StatementClass::Ddl, drop), no_count),
            Decision::Confirm {
                steps: vec![Confirmation::TypeName {
                    reason: NameReason::Destructive {
                        what: Destructive::Drop
                    }
                }]
            }
        );
        let staging = policy(Environment::Staging, false, ReadOnlyEnforcement::Server);
        let drop = Flags {
            destructive: Some(Destructive::Drop),
            ..Flags::default()
        };
        assert_eq!(
            decide(&staging, &st(StatementClass::Ddl, drop), no_count),
            Decision::Allow
        );
    }

    #[test]
    fn updates_off_local_get_a_count_preview() {
        let p = policy(Environment::Staging, false, ReadOnlyEnforcement::Server);
        let d = decide(&p, &st(StatementClass::Write, Flags::default()), || {
            CountPlan::Query("SELECT COUNT(*) FROM t WHERE a = 1".into())
        });
        assert_eq!(
            d,
            Decision::Confirm {
                steps: vec![Confirmation::CountRows {
                    query: "SELECT COUNT(*) FROM t WHERE a = 1".into()
                }]
            }
        );
        let unfiltered = Flags {
            unfiltered: true,
            ..Flags::default()
        };
        let d = decide(&p, &st(StatementClass::Write, unfiltered), || {
            CountPlan::Query("q".into())
        });
        assert!(matches!(&d, Decision::Confirm { steps } if steps.len() == 2));
        let d = decide(&p, &st(StatementClass::Write, Flags::default()), || {
            CountPlan::Underivable {
                reason: "UPDATE with ORDER BY".into(),
            }
        });
        assert!(
            matches!(&d, Decision::Confirm { steps } if matches!(steps[0], Confirmation::TypeName { .. }))
        );
        let local = policy(Environment::Local, false, ReadOnlyEnforcement::Server);
        assert_eq!(
            decide(&local, &st(StatementClass::Write, Flags::default()), || {
                CountPlan::Query("q".into())
            }),
            Decision::Allow
        );
    }

    #[test]
    fn large_counts_need_the_name() {
        let p = policy(Environment::Staging, false, ReadOnlyEnforcement::Server);
        assert_eq!(after_count(&p, 1_000), None);
        assert_eq!(
            after_count(&p, 1_001),
            Some(Confirmation::TypeName {
                reason: NameReason::ManyRows { count: 1_001 }
            })
        );
    }

    #[test]
    fn some_commands_never_run_on_production() {
        let p = policy(Environment::Production, false, ReadOnlyEnforcement::Client);
        let flush = Flags {
            production_forbidden: true,
            ..Flags::default()
        };
        assert_eq!(
            decide(&p, &st(StatementClass::Admin, flush), no_count),
            Decision::Block {
                reason: BlockReason::ProductionForbidden
            }
        );
        let dev = policy(Environment::Development, false, ReadOnlyEnforcement::Client);
        let flush = Flags {
            production_forbidden: true,
            ..Flags::default()
        };
        assert_eq!(
            decide(&dev, &st(StatementClass::Admin, flush), no_count),
            Decision::Allow
        );
    }

    #[test]
    fn names_must_match_exactly() {
        let p = policy(Environment::Production, false, ReadOnlyEnforcement::Server);
        assert!(name_matches(&p, " orders-prod "));
        assert!(!name_matches(&p, "Orders-Prod"));
        assert!(!name_matches(&p, ""));
    }

    #[test]
    fn analyze_only_for_reads() {
        assert_eq!(
            explain_mode(StatementClass::Read, ExplainMode::Analyze),
            ExplainMode::Analyze
        );
        assert_eq!(
            explain_mode(StatementClass::Write, ExplainMode::Analyze),
            ExplainMode::Estimate
        );
        assert_eq!(
            explain_mode(StatementClass::Unknown, ExplainMode::Analyze),
            ExplainMode::Estimate
        );
    }
}

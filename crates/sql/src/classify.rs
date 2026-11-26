//! Classifying parsed statements. Every rule leans towards caution: what Treasury cannot place
//! is `Unknown`, and `Unknown` is treated as a write.

use crate::dialect::Rules;
use crate::functions;
use sqlparser::ast::{
    CopyTarget, Expr, FunctionArguments, ObjectName, Query, Reset, SetExpr, Statement, TableFactor,
    TransactionAccessMode, TransactionMode, Visit, Visitor, visit_expressions, visit_statements,
};
use std::ops::ControlFlow;
use treasury_core::analyze::{Destructive, Flags, StatementClass, TxControl};

/// The verdict for one statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Its class.
    pub class: StatementClass,
    /// Its flags.
    pub flags: Flags,
    /// Why it is unknown.
    pub reason: Option<String>,
}

impl Verdict {
    fn of(class: StatementClass) -> Self {
        Self {
            class,
            flags: Flags::default(),
            reason: None,
        }
    }

    fn unknown(reason: &str) -> Self {
        Self {
            class: StatementClass::Unknown,
            flags: Flags::default(),
            reason: Some(reason.to_owned()),
        }
    }

    fn destructive(class: StatementClass, what: Destructive) -> Self {
        let mut v = Self::of(class);
        v.flags.destructive = Some(what);
        v
    }

    /// Combines two verdicts: the more dangerous class wins and flags accumulate.
    pub fn merge(mut self, other: Verdict) -> Verdict {
        if other.class > self.class {
            self.class = other.class;
            self.reason = other.reason.or(self.reason);
        }
        let f = &mut self.flags;
        let o = other.flags;
        f.locks |= o.locks;
        f.transaction = f.transaction.or(o.transaction);
        f.read_only_escape |= o.read_only_escape;
        f.destructive = f.destructive.or(o.destructive);
        f.unfiltered |= o.unfiltered;
        f.executes |= o.executes;
        f.production_forbidden |= o.production_forbidden;
        self
    }
}

/// Session settings that are safe to change on a read-only session.
const HARMLESS_SETTINGS: &[&str] = &[
    "search_path",
    "timezone",
    "time zone",
    "datestyle",
    "intervalstyle",
    "client_encoding",
    "application_name",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "extra_float_digits",
    "work_mem",
    "names",
    "character_set_client",
    "character_set_results",
    "character_set_connection",
    "collation_connection",
    "time_zone",
    "max_execution_time",
    "sql_select_limit",
    "net_read_timeout",
    "net_write_timeout",
    "wait_timeout",
    "query_tag",
    "nocount",
    "language",
    "dateformat",
    "datefirst",
    "textsize",
    "ansi_nulls",
    "quoted_identifier",
    "arithabort",
    "statistics",
    "showplan_xml",
    "showplan_text",
    "showplan_all",
    "statistics_xml",
];

/// Settings that control read-only mode; changing or resetting them escapes it.
const READ_ONLY_SETTINGS: &[&str] = &[
    "default_transaction_read_only",
    "transaction_read_only",
    "tx_read_only",
    "read_only",
    "super_read_only",
    "session_characteristics",
];

fn setting_name(name: &ObjectName) -> String {
    let full = name.to_string().to_ascii_lowercase();
    let last = full.rsplit('.').next().unwrap_or(&full);
    last.trim_matches(['"', '`', '[', ']', '@']).to_owned()
}

/// Classifies one parsed statement.
pub fn statement(rules: &Rules, st: &Statement) -> Verdict {
    use StatementClass as C;
    match st {
        Statement::Query(q) => query(rules, q),
        Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::Merge(_)
        | Statement::LoadData { .. }
        | Statement::CopyIntoSnowflake { .. }
        | Statement::NOTIFY { .. }
        | Statement::Put { .. }
        | Statement::Remove(_) => {
            let mut v = Verdict::of(C::Write);
            if let Statement::Update(u) = st {
                v.flags.unfiltered = u.selection.is_none();
            }
            if let Statement::Delete(d) = st {
                v.flags.unfiltered = d.selection.is_none();
            }
            v
        }
        Statement::Lock(_) | Statement::LockTables { .. } => {
            let mut v = Verdict::of(C::Write);
            v.flags.locks = true;
            v
        }
        Statement::Copy { to, target, .. } => {
            if !*to {
                Verdict::of(C::Write)
            } else if matches!(target, CopyTarget::Stdout) {
                Verdict::of(C::Read)
            } else {
                // COPY ... TO 'file' or PROGRAM writes on the server.
                Verdict::of(C::Admin)
            }
        }
        Statement::Explain {
            analyze,
            statement: inner,
            ..
        } => {
            let inner_v = self::statement(rules, inner);
            if *analyze {
                let mut v = inner_v;
                v.flags.executes = true;
                v
            } else {
                Verdict::of(C::Read)
            }
        }
        Statement::ExplainTable { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowProcessList { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowCharset(_)
        | Statement::ShowObjects(_)
        | Statement::ShowTables { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowCollation { .. }
        | Statement::Use(_)
        | Statement::Fetch { .. }
        | Statement::Close { .. }
        | Statement::Open(_)
        | Statement::LISTEN { .. }
        | Statement::UNLISTEN { .. }
        | Statement::Assert { .. }
        | Statement::Print(_)
        | Statement::RaisError { .. }
        | Statement::Throw(_)
        | Statement::WaitFor(_)
        | Statement::List(_)
        | Statement::Deallocate { .. } => Verdict::of(C::Read),
        Statement::Declare { stmts, .. } => {
            // A cursor declaration reads; T-SQL variable declarations change nothing.
            let mut v = Verdict::of(C::Read);
            for d in stmts {
                if let Some(q) = &d.for_query {
                    v = v.merge(query(rules, q));
                }
            }
            v
        }
        Statement::StartTransaction {
            modes, statements, ..
        } => {
            if !statements.is_empty() {
                return Verdict::unknown("A BEGIN ... END block runs several statements.");
            }
            let read_write = modes.iter().find_map(|m| match m {
                TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => Some(true),
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => Some(false),
                TransactionMode::IsolationLevel(_) => None,
            });
            let mut v = Verdict::of(C::Read);
            v.flags.transaction = Some(TxControl::Begin { read_write });
            v
        }
        Statement::Commit { .. } => tx(TxControl::Commit),
        Statement::Rollback { savepoint, .. } => tx(if savepoint.is_some() {
            TxControl::Savepoint
        } else {
            TxControl::Rollback
        }),
        Statement::Savepoint { .. } | Statement::ReleaseSavepoint { .. } => {
            tx(TxControl::Savepoint)
        }
        Statement::Set(set) => set_statement(rules, set),
        Statement::Reset(r) => match &r.reset {
            Reset::ALL => escape(C::Admin),
            Reset::SessionAuthorization => Verdict::of(C::Admin),
            Reset::ConfigurationParameter(name) => {
                let n = setting_name(name);
                if READ_ONLY_SETTINGS.contains(&n.as_str()) {
                    escape(C::Admin)
                } else if HARMLESS_SETTINGS.contains(&n.as_str()) {
                    Verdict::of(C::Read)
                } else {
                    Verdict::of(C::Admin)
                }
            }
        },
        Statement::Truncate(_) => Verdict::destructive(C::Ddl, Destructive::Truncate),
        Statement::Drop { .. }
        | Statement::DropFunction(_)
        | Statement::DropDomain(_)
        | Statement::DropProcedure { .. }
        | Statement::DropSecret { .. }
        | Statement::DropPolicy(_)
        | Statement::DropRule(_)
        | Statement::DropConnector { .. }
        | Statement::DropExtension(_)
        | Statement::DropOperator(_)
        | Statement::DropOperatorFamily(_)
        | Statement::DropOperatorClass(_)
        | Statement::DropTrigger(_) => Verdict::destructive(C::Ddl, Destructive::Drop),
        Statement::AlterTable(_)
        | Statement::AlterSchema(_)
        | Statement::AlterIndex { .. }
        | Statement::AlterView(_)
        | Statement::AlterFunction(_)
        | Statement::AlterType(_)
        | Statement::AlterCollation(_)
        | Statement::AlterOperator(_)
        | Statement::AlterOperatorFamily(_)
        | Statement::AlterOperatorClass(_)
        | Statement::AlterTextSearch(_)
        | Statement::AlterPolicy(_)
        | Statement::AlterTrigger(_)
        | Statement::AlterConnector { .. } => Verdict::destructive(C::Ddl, Destructive::Alter),
        // CREATE OR REPLACE throws away what was there.
        Statement::CreateTable(ct) if ct.or_replace => {
            Verdict::destructive(C::Ddl, Destructive::Drop)
        }
        Statement::CreateView(cv) if cv.or_replace => {
            Verdict::destructive(C::Ddl, Destructive::Alter)
        }
        Statement::CreateView(_)
        | Statement::CreateTable(_)
        | Statement::CreateVirtualTable { .. }
        | Statement::CreateIndex(_)
        | Statement::CreateForeignTable(_)
        | Statement::CreatePolicy(_)
        | Statement::CreateRule(_)
        | Statement::CreateOperator(_)
        | Statement::CreateOperatorFamily(_)
        | Statement::CreateOperatorClass(_)
        | Statement::CreateTextSearch(_)
        | Statement::CreateAggregate(_)
        | Statement::CreateSchema { .. }
        | Statement::CreateDatabase { .. }
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::CreateProcedure { .. }
        | Statement::CreateMacro { .. }
        | Statement::CreateStage { .. }
        | Statement::CreateFileFormat { .. }
        | Statement::CreateSequence { .. }
        | Statement::CreateDomain(_)
        | Statement::CreateType { .. }
        | Statement::CreateExtension(_)
        | Statement::CreateCollation(_)
        | Statement::Comment { .. }
        | Statement::RenameTable(_)
        | Statement::RefreshMaterializedView(_) => Verdict::of(C::Ddl),
        Statement::Grant(_)
        | Statement::GrantRole(_)
        | Statement::Deny(_)
        | Statement::Revoke(_)
        | Statement::RevokeRole(_)
        | Statement::AlterDefaultPrivileges(_)
        | Statement::CreateRole(_)
        | Statement::CreateUser(_)
        | Statement::AlterUser(_)
        | Statement::AlterRole { .. }
        | Statement::CreateSecret { .. }
        | Statement::CreateServer(_)
        | Statement::CreateForeignDataWrapper(_)
        | Statement::CreateUserMapping(_)
        | Statement::CreateConnector(_)
        | Statement::CreateWarehouse(_)
        | Statement::Kill { .. }
        | Statement::Vacuum(_)
        | Statement::Analyze(_)
        | Statement::OptimizeTable { .. }
        | Statement::Msck(_)
        | Statement::Discard { .. }
        | Statement::Flush { .. }
        | Statement::Pragma { .. }
        | Statement::Install { .. }
        | Statement::Load { .. }
        | Statement::Directory { .. }
        | Statement::AttachDatabase { .. }
        | Statement::AttachDuckDBDatabase { .. }
        | Statement::DetachDuckDBDatabase { .. }
        | Statement::Cache { .. }
        | Statement::UNCache { .. }
        | Statement::Unload { .. }
        | Statement::ExportData(_)
        | Statement::UnlockTables
        | Statement::AlterSession { .. } => Verdict::of(C::Admin),
        Statement::Call(_)
        | Statement::Execute { .. }
        | Statement::Prepare { .. }
        | Statement::If(_)
        | Statement::While(_)
        | Statement::Case(_)
        | Statement::Raise(_)
        | Statement::Return(_) => Verdict::unknown(
            "It runs code Treasury cannot see into (a procedure, a prepared statement or a block).",
        ),
    }
}

fn tx(control: TxControl) -> Verdict {
    let mut v = Verdict::of(StatementClass::Read);
    v.flags.transaction = Some(control);
    v
}

fn escape(class: StatementClass) -> Verdict {
    let mut v = Verdict::of(class);
    v.flags.read_only_escape = true;
    v
}

fn set_statement(rules: &Rules, set: &sqlparser::ast::Set) -> Verdict {
    use StatementClass as C;
    use sqlparser::ast::Set as S;
    let names: Vec<(String, Option<&Expr>, bool)> = match set {
        S::SingleAssignment {
            scope,
            variable,
            values,
            ..
        } => {
            let global = scope
                .as_ref()
                .is_some_and(|s| s.to_string().eq_ignore_ascii_case("GLOBAL"));
            vec![(setting_name(variable), values.first(), global)]
        }
        S::ParenthesizedAssignments { variables, values } => variables
            .iter()
            .zip(values.iter().map(Some).chain(std::iter::repeat(None)))
            .map(|(v, e)| (setting_name(v), e, false))
            .collect(),
        S::MultipleAssignments { assignments } => assignments
            .iter()
            .map(|a| {
                let global = a
                    .scope
                    .as_ref()
                    .is_some_and(|s| s.to_string().eq_ignore_ascii_case("GLOBAL"));
                (setting_name(&a.name), Some(&a.value), global)
            })
            .collect(),
        S::SetTimeZone { .. } | S::SetNames { .. } | S::SetNamesDefault {} => {
            return Verdict::of(C::Read);
        }
        S::SetSessionParam(_) => return Verdict::of(C::Read),
        S::SetRole { .. } | S::SetSessionAuthorization(_) => return Verdict::of(C::Admin),
        S::SetTransaction { modes, .. } => {
            let rw = modes.iter().any(|m| {
                matches!(
                    m,
                    TransactionMode::AccessMode(TransactionAccessMode::ReadWrite)
                )
            });
            return if rw {
                escape(C::Admin)
            } else {
                Verdict::of(C::Read)
            };
        }
    };
    let mut verdict = Verdict::of(C::Read);
    for (name, value, global) in names {
        let v = if global {
            Verdict::of(C::Admin)
        } else if READ_ONLY_SETTINGS.contains(&name.as_str()) {
            // Setting read-only on is fine; anything else may switch it off.
            let turns_on = value.is_some_and(|e| {
                matches!(
                    e.to_string().to_ascii_lowercase().trim_matches('\''),
                    "on" | "true" | "1" | "yes"
                )
            });
            if turns_on {
                Verdict::of(C::Read)
            } else {
                escape(C::Admin)
            }
        } else if HARMLESS_SETTINGS.contains(&name.as_str()) {
            Verdict::of(C::Read)
        } else if name.is_empty() {
            Verdict::unknown("The SET statement could not be read.")
        } else {
            // A user variable (`SET @x = 1`), a session variable or configuration value in
            // Snowflake and Databricks (both only last for the session), or a setting Treasury
            // does not know.
            let session_only = matches!(
                rules.dialect,
                treasury_core::driver::SqlDialect::Snowflake
                    | treasury_core::driver::SqlDialect::Databricks
            );
            if session_only || set_variable_is_user_var(set) {
                Verdict::of(C::Read)
            } else {
                Verdict::of(C::Admin)
            }
        };
        verdict = verdict.merge(v);
    }
    verdict
}

fn set_variable_is_user_var(set: &sqlparser::ast::Set) -> bool {
    use sqlparser::ast::Set as S;
    match set {
        S::SingleAssignment { variable, .. } => {
            let t = variable.to_string();
            t.starts_with('@') && !t.starts_with("@@")
        }
        _ => false,
    }
}

/// T-SQL table hints that take or hold locks the way `FOR UPDATE` does.
const LOCK_HINTS: &[&str] = &[
    "UPDLOCK",
    "XLOCK",
    "TABLOCKX",
    "HOLDLOCK",
    "SERIALIZABLE",
    "REPEATABLEREAD",
];

#[derive(Default)]
struct LockHints {
    found: bool,
}

impl Visitor for LockHints {
    type Break = ();

    fn pre_visit_table_factor(&mut self, tf: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table {
            with_hints, args, ..
        } = tf
        {
            let mut hints: Vec<String> = with_hints.iter().map(|h| h.to_string()).collect();
            // The deprecated `FROM t (UPDLOCK)` form parses as table function arguments.
            if let Some(a) = args {
                hints.extend(a.args.iter().map(|x| x.to_string()));
            }
            if hints
                .iter()
                .any(|h| LOCK_HINTS.contains(&h.trim().to_ascii_uppercase().as_str()))
            {
                self.found = true;
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

fn query(rules: &Rules, q: &Query) -> Verdict {
    use StatementClass as C;
    let mut v = Verdict::of(C::Read);
    let mut hints = LockHints::default();
    let _ = q.visit(&mut hints);
    if !q.locks.is_empty() || hints.found {
        v.class = C::Write;
        v.flags.locks = true;
    }
    // Data-modifying statements anywhere inside: CTEs, nested set expressions.
    let mut nested_write = false;
    let _ = visit_statements(q, |s| {
        if !matches!(s, Statement::Query(_)) {
            nested_write = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    if matches!(
        q.body.as_ref(),
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_)
    ) {
        nested_write = true;
    }
    if nested_write {
        v.class = C::Write;
    }
    if select_into(&q.body) {
        // SELECT ... INTO creates a table.
        v = v.merge(Verdict::of(C::Ddl));
    }
    v.merge(function_calls(rules, q))
}

fn select_into(body: &SetExpr) -> bool {
    match body {
        SetExpr::Select(s) => s.into.is_some(),
        SetExpr::Query(q) => select_into(&q.body),
        SetExpr::SetOperation { left, right, .. } => select_into(left) || select_into(right),
        _ => false,
    }
}

fn function_calls(rules: &Rules, q: &Query) -> Verdict {
    use StatementClass as C;
    let mut v = Verdict::of(C::Read);
    let _ = visit_expressions(q, |e| {
        match e {
            Expr::Function(f) => {
                let name = f
                    .name
                    .0
                    .last()
                    .map(|p| p.to_string())
                    .unwrap_or_default()
                    .trim_matches(['"', '`', '[', ']'])
                    .to_ascii_lowercase();
                let effect = functions::effect(&name);
                let class = match effect {
                    Some(c) => c,
                    None if rules.functions_may_write => C::Unknown,
                    None => C::Read,
                };
                let mut fv = Verdict::of(class);
                if class == C::Unknown {
                    fv.reason = Some(format!(
                        "It calls {name}(), which may change data; Treasury cannot tell."
                    ));
                }
                if name == "set_config" && first_arg_mentions_read_only(f) {
                    fv.flags.read_only_escape = true;
                }
                v = std::mem::replace(&mut v, Verdict::of(C::Read)).merge(fv);
            }
            Expr::CompoundIdentifier(parts) => {
                // Oracle and Snowflake sequences: seq.NEXTVAL.
                if parts
                    .last()
                    .is_some_and(|p| p.value.eq_ignore_ascii_case("nextval"))
                {
                    v = std::mem::replace(&mut v, Verdict::of(C::Read))
                        .merge(Verdict::of(C::Write));
                }
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
    v
}

fn first_arg_mentions_read_only(f: &sqlparser::ast::Function) -> bool {
    match &f.args {
        FunctionArguments::List(list) => list.args.first().is_some_and(|a| {
            let text = a.to_string().to_ascii_lowercase();
            text.contains("read_only")
        }),
        _ => true,
    }
}

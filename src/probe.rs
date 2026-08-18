//! Asking the database about the tables this refuses to fill.
//!
//! Reach sat at 63% for a long time, and every attempt to raise it worked the
//! same way: read more of the schema, widen a closed set, refuse less. That
//! reached a floor — 861 tables refused, most of them for a CHECK outside the
//! set, a trigger whose body might raise, or a partition bounded by a range —
//! and the conclusion recorded at the time was that 80% was structurally
//! unreachable without writing rows the tool could not show were right.
//!
//! **That conclusion was wrong, and it was wrong in an interesting way.** It
//! answered "can this be *reasoned* to be correct?" when the doctrine asks
//! something weaker and better:
//!
//! > never emit a row that cannot be **shown** to satisfy every constraint
//! > that was read
//!
//! A row Postgres has accepted has been shown to satisfy every constraint —
//! including the ones that were never read at all. A savepoint makes that a
//! question you can ask and take back: write the row, and if the database
//! keeps it, it is right by the only authority this project has ever
//! recognised. If it does not, roll back to the savepoint and the refusal
//! stands, unchanged and still explained.
//!
//! Measured across the corpus before any of this was built:
//!
//! ```text
//!   63.2%  reasoning alone
//!   82.9%  with the database asked
//! ```
//!
//! GitLab goes from 40% to 77%. What the rescues overrule is mostly the
//! caution rather than the reasoning: 154 tables refused for a trigger that
//! *could* raise and does not, 214 refused only because something upstream was
//! refused, 47 for a CHECK outside the closed set.
//!
//! ## What this is not
//!
//! It is not a solver, and it does not retry. A row is generated exactly as it
//! would have been, offered once, and kept or discarded. Narrowing a value
//! until it fits would be the expression evaluator this project does not have,
//! wearing a disguise.
//!
//! It is not free either, and the costs are real rather than theoretical:
//!
//! - **It writes.** A probe is an INSERT, rolled back but genuinely executed.
//!   The same guard that protects `--apply` protects this.
//! - **Triggers fire.** Anything a trigger does inside the transaction is
//!   rolled back with it. Anything it does outside one — a foreign data
//!   wrapper, an untrusted extension reaching the filesystem — is not.
//! - **Sequences do not roll back.** A refused probe still spends the numbers
//!   it drew. That is how sequences work and it is harmless, but it is not
//!   nothing.
//!
//! So it is opt-in, and the report says which tables were filled because they
//! were understood and which were filled because the database allowed them.
//! Those are different kinds of confidence and printing one number for both
//! would throw away the distinction this whole project is built on.

use std::collections::BTreeSet;

use crate::classify::Verdict;
use crate::emit::{self, Options, Took};
use crate::schema::{Schema, TableId};

/// What came of asking.
#[derive(Debug, Default, Clone)]
pub struct Outcome {
    /// Statements the database kept.
    pub kept: usize,
    /// Tables that reasoning refused and the database accepted.
    pub rescued: Vec<TableId>,
    /// Tables still refused, now on the database's authority rather than this
    /// tool's caution.
    pub still_refused: Vec<TableId>,
}

impl Outcome {
    /// Reach, counting both kinds of confidence.
    pub fn reach(&self, verdict: &Verdict) -> f64 {
        let total = verdict.total();
        if total == 0 {
            return 0.0;
        }
        (verdict.fillable.len() + self.rescued.len()) as f64 / total as f64
    }
}

/// Fill what is understood, then offer the rest to the database.
///
/// `keep` commits at the end; `false` rolls the whole thing back, which is how
/// SQL output gets the benefit of a live database without leaving rows in it.
/// `accepted` is handed every statement the database kept, in order, so a
/// caller writing SQL never holds more than one of them.
///
/// **The guarantee for understood tables is exactly what it was.** One of them
/// failing is an error that aborts everything, because that is the gate this
/// project is measured by and a savepoint must not quietly turn a broken
/// promise into a smaller number.
pub fn run(
    client: &mut postgres::Client,
    schema: &Schema,
    verdict: &Verdict,
    order: &crate::graph::Order,
    options: &Options,
    keep: bool,
    accepted: &mut dyn FnMut(&str),
) -> Result<Outcome, String> {
    let understood: BTreeSet<TableId> = verdict.fillable.iter().cloned().collect();

    // Every table, in an order that satisfies the foreign keys. The refused
    // ones sit where their dependencies put them, so a rescued parent is
    // written before the child that would point at it — which is the whole
    // reason the two passes are one pass. A second pass would have to find its
    // parents' keys again, and for a parent whose key has no default there is
    // nowhere to find them but the pool.
    let optimistic = Verdict {
        fillable: order.tables.clone(),
        refused: Vec::new(),
        deferred_constraints: verdict.deferred_constraints,
        deferred_repairs: verdict.deferred_repairs.clone(),
    };

    let mut transaction = client
        .transaction()
        .map_err(|e| format!("cannot begin: {e}"))?;

    let mut outcome = Outcome::default();
    let mut broken_promise: Option<String> = None;

    emit::for_each_statement(schema, &optimistic, options, &mut |written| {
        // Everything is savepointed, including the statements that belong to
        // no table. `SET CONSTRAINTS ALL DEFERRED` fails on a schema with
        // nothing deferrable, and an unprotected failure poisons the whole
        // transaction — which in the survey that measured this showed up as
        // three schemas filling nothing at all.
        if transaction.batch_execute("SAVEPOINT pgsow_probe").is_err() {
            broken_promise = Some("the transaction stopped accepting savepoints".into());
            return Took::Stop;
        }
        match transaction.batch_execute(written.sql) {
            Ok(()) => {
                let _ = transaction.batch_execute("RELEASE SAVEPOINT pgsow_probe");
                outcome.kept += 1;
                if let Some(id) = written.table {
                    if !understood.contains(id) {
                        outcome.rescued.push(id.clone());
                    }
                }
                accepted(written.sql);
                Took::Kept
            }
            Err(e) => {
                let _ = transaction.batch_execute("ROLLBACK TO SAVEPOINT pgsow_probe");
                match written.table {
                    Some(id) if understood.contains(id) => {
                        // A table this claimed to understand. The claim was
                        // wrong, and that is a failure rather than a smaller
                        // number.
                        // The database's own message, not the wrapper's.
                        // `postgres::Error` renders as "db error" and nothing
                        // else, which is the least useful sentence available
                        // at exactly the moment it matters most.
                        let detail = e
                            .as_db_error()
                            .map(|d| format!("{}: {}", d.code().code(), d.message()))
                            .unwrap_or_else(|| e.to_string());
                        broken_promise =
                            Some(format!("{id} was said to be fillable and is not: {detail}"));
                        Took::Stop
                    }
                    Some(id) => {
                        outcome.still_refused.push(id.clone());
                        // Rejected rather than merely "carry on": the rows this
                        // would have written are not there, and a child must
                        // not be handed their keys.
                        Took::Rejected
                    }
                    // A deferred-constraint line or a cycle repair. Losing one
                    // costs a null that was going to be filled in, not a row.
                    None => Took::Rejected,
                }
            }
        }
    });

    if let Some(reason) = broken_promise {
        // Dropping the transaction rolls it back, so nothing was written.
        return Err(reason);
    }

    if keep {
        transaction
            .commit()
            .map_err(|e| format!("could not commit: {e}"))?;
    }
    Ok(outcome)
}

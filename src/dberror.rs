//! What a `postgres::Error` actually said.
//!
//! `postgres::Error` renders as the word `db error` and nothing else, so
//! `format!("{e}")` produces lines like `cannot connect: db error` and
//! `could not commit: db error`. Both are true and neither is any use.
//!
//! This lived in the binary, where it fixed the messages the CLI prints and
//! left the ones the library produces. `probe` reported a failed commit as
//! `could not commit: db error` for exactly that reason, on a schema where the
//! real message named the constraint. One copy now, reachable from both.

/// The database's own message, with its detail and hint when it gave any.
///
/// When the failure never reached the database — a refused connection, a bad
/// host — there is no message to quote, so the root of the source chain is
/// used instead. That is the layer that knows what went wrong.
pub fn explain(e: &postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let mut out = db.message().to_string();
            if let Some(detail) = db.detail() {
                out.push_str(&format!("\n       {detail}"));
            }
            if let Some(hint) = db.hint() {
                out.push_str(&format!("\n       {hint}"));
            }
            out
        }
        None => {
            let mut cause: &dyn std::error::Error = e;
            while let Some(next) = cause.source() {
                cause = next;
            }
            cause.to_string()
        }
    }
}

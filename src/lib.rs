//! Read a Postgres schema, work out what can be filled, and refuse the rest.
//!
//! The logic lives here rather than in the binary so that integration tests
//! can drive it against a real database — a test can only reach a library
//! crate, and the tests that matter for this tool are the ones where Postgres
//! itself decides whether the answer was right.
//!
//! **The rule the whole thing is built on:** never emit a row that cannot be
//! shown to satisfy every constraint that was read. See `classify`.

pub mod checks;
pub mod classify;
pub mod dsn;
pub mod emit;
pub mod filter;
pub mod generate;
pub mod graph;
pub mod indexes;
pub mod introspect;
pub mod schema;
pub mod volume;

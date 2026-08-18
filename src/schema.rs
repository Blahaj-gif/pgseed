//! What a schema *is*, as far as generating data for it is concerned.
//!
//! Deliberately not a faithful model of Postgres. It records the things that
//! decide whether a row can be produced — the type, whether null is allowed,
//! what it points at, what must be unique — and records everything else as a
//! reason to refuse rather than as a field to interpret later.
//!
//! The distinction that matters most in this file is between a constraint this
//! tool can *satisfy* and one it can merely *see*. A CHECK constraint is read,
//! quoted and used to refuse the table. It is never approximated, because a
//! partial understanding of `total > 0` that silently mishandles
//! `total > 0 OR status = 'void'` produces rows that look right and are not.

use std::collections::BTreeMap;
use std::fmt;

/// Where a table lives and what it is called, kept together because a bare
/// name is ambiguous the moment anybody uses two schemas.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableId {
    pub schema: String,
    pub name: String,
}

impl TableId {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }

    /// Quoted for SQL. Always quoted, never conditionally: an identifier that
    /// needs quoting and does not get it is a syntax error at best, and at
    /// worst a different table.
    pub fn quoted(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }
}

impl fmt::Display for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Unqualified for `public`, because that is what a person calls it.
        if self.schema == "public" {
            write!(f, "{}", self.name)
        } else {
            write!(f, "{}.{}", self.schema, self.name)
        }
    }
}

/// A Postgres identifier as SQL. Doubling an embedded quote is the whole of
/// the escaping rule, and getting it wrong on a table called `o"rders` is an
/// injection rather than a typo.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// What this tool knows how to produce a value for.
///
/// The catch-all is the point. An unrecognised type is not guessed at and not
/// filled with a plausible string; it is carried through as `Unsupported` so
/// the table can be refused with the type's real name in the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    Boolean,
    /// Width in bytes, so `smallint` is not handed a value that overflows it.
    Integer {
        bytes: u8,
    },
    /// `numeric(p, s)`; `None` where the column declares no limit.
    Numeric {
        precision: Option<i32>,
        scale: Option<i32>,
    },
    Float {
        bytes: u8,
    },
    /// `varchar(n)` and `char(n)` carry their limit; `text` does not.
    Text {
        max_length: Option<i32>,
    },
    Uuid,
    Date,
    Time,
    Timestamp {
        with_zone: bool,
    },
    Interval,
    Json {
        binary: bool,
    },
    Bytea,
    /// `inet`, `cidr`, `macaddr`. Ordinary Postgres, and their absence refused
    /// a DNS server's entire schema for one column.
    Network {
        kind: NetworkKind,
    },
    /// A user-defined enum, with its labels in declaration order.
    ///
    /// `qualified` is the schema-qualified, quoted name — what has to be
    /// written to cast an array of these. `None` when two schemas define an
    /// enum of the same name, because then the bare name this was looked up
    /// by does not identify one of them.
    Enum {
        name: String,
        qualified: Option<String>,
        labels: Vec<String>,
    },
    /// A domain wraps another type and may add constraints of its own. The
    /// inner type is kept so the value can be produced; `has_constraint` is
    /// what forces a refusal, since a domain constraint is a CHECK by another
    /// name.
    Domain {
        name: String,
        inner: Box<ColumnType>,
        has_constraint: bool,
    },
    Array {
        of: Box<ColumnType>,
        dimensions: i32,
    },
    /// Anything else. Carries the type name Postgres reported so the refusal
    /// can say `type "geometry"` rather than "an unsupported type".
    Unsupported {
        name: String,
    },
}

impl ColumnType {
    /// Whether a value can be produced for this type at all.
    pub fn is_generatable(&self) -> bool {
        match self {
            ColumnType::Unsupported { .. } => false,
            ColumnType::Domain {
                has_constraint,
                inner,
                ..
            } => !has_constraint && inner.is_generatable(),
            ColumnType::Array { of, .. } => of.is_generatable(),
            _ => true,
        }
    }

    /// The Postgres name for this type, where writing one is unambiguous.
    ///
    /// Needed because `ARRAY['x']` types itself as `text[]` the moment it is
    /// written, and `text[]` does not implicitly become `jsonb[]` or `inet[]`.
    /// The array has to say what it is.
    ///
    /// `None` for domains and unrecognised types, and for an enum whose name
    /// is ambiguous across schemas — naming one then would be a guess.
    pub fn sql_name(&self) -> Option<String> {
        Some(
            match self {
                ColumnType::Boolean => "boolean",
                ColumnType::Integer { bytes: 2 } => "smallint",
                ColumnType::Integer { bytes: 8 } => "bigint",
                ColumnType::Integer { .. } => "integer",
                ColumnType::Numeric { .. } => "numeric",
                ColumnType::Float { bytes: 4 } => "real",
                ColumnType::Float { .. } => "double precision",
                ColumnType::Text { .. } => "text",
                ColumnType::Uuid => "uuid",
                ColumnType::Date => "date",
                ColumnType::Time => "time",
                ColumnType::Timestamp { with_zone: true } => "timestamptz",
                ColumnType::Timestamp { .. } => "timestamp",
                ColumnType::Interval => "interval",
                ColumnType::Json { binary: true } => "jsonb",
                ColumnType::Json { .. } => "json",
                ColumnType::Bytea => "bytea",
                ColumnType::Network {
                    kind: NetworkKind::Inet,
                } => "inet",
                ColumnType::Network {
                    kind: NetworkKind::Cidr,
                } => "cidr",
                ColumnType::Network {
                    kind: NetworkKind::MacAddr,
                } => "macaddr",
                // An enum can be named when it is unambiguous, which is what
                // lets `ARRAY['sad']::public."mood"[]` be written at all.
                ColumnType::Enum { qualified, .. } => return qualified.clone(),
                ColumnType::Domain { .. }
                | ColumnType::Array { .. }
                | ColumnType::Unsupported { .. } => return None,
            }
            .to_string(),
        )
    }

    /// The name to use when explaining a refusal.
    pub fn describe(&self) -> String {
        match self {
            ColumnType::Unsupported { name } => name.clone(),
            ColumnType::Domain { name, .. } => name.clone(),
            ColumnType::Enum { name, .. } => name.clone(),
            ColumnType::Array { of, .. } => format!("{}[]", of.describe()),
            other => format!("{other:?}").to_lowercase(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkKind {
    Inet,
    Cidr,
    MacAddr,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_: ColumnType,
    pub nullable: bool,
    /// A column with a default can be omitted from the insert entirely, which
    /// is usually the right thing for `id` and `created_at`.
    pub has_default: bool,
    /// Whether that default reads from a sequence. A sequence default is
    /// already distinct on every row; a constant one — `DEFAULT 1` — is the
    /// same value every time, which matters the moment the column is UNIQUE.
    pub default_is_sequence: bool,
    /// `GENERATED ALWAYS AS IDENTITY` and generated columns must *not* be
    /// written to; an insert that names them fails outright.
    pub is_generated: bool,
    pub position: i32,
}

/// A foreign key, kept whole because composite keys are ordinary and handling
/// only single-column keys would refuse a large share of real schemas.
#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub references: TableId,
    pub referenced_columns: Vec<String>,
    /// A cycle can be broken through a deferrable constraint by deferring it
    /// inside the transaction. Whether that is possible is a property of the
    /// constraint, not a choice this tool gets to make.
    pub deferrable: bool,
}

impl ForeignKey {
    /// Whether this key can hold NULL, which is the cheapest way out of a
    /// cycle: insert the row without the reference, then fill it in.
    ///
    /// Nullable in the catalogue is not the end of the question. A column
    /// carrying `CHECK (col IS NOT NULL)` is not nullable in any sense that
    /// matters, and a schema that adds its not-nulls that way — GitLab does,
    /// because it avoids rewriting a large table — would otherwise have every
    /// one of them read as an invitation to write NULL.
    pub fn is_optional(&self, table: &Table) -> bool {
        self.columns.iter().all(|c| {
            table.column(c).is_some_and(|col| col.nullable) && !table.check_forbids_null(c)
        })
    }
}

#[derive(Debug, Clone)]
pub struct UniqueKey {
    pub name: String,
    pub columns: Vec<String>,
    pub is_primary: bool,
}

/// A CHECK constraint, read and never solved.
///
/// `definition` is what `pg_get_constraintdef` returned, kept verbatim so a
/// refusal can quote the actual rule rather than a paraphrase of it.
#[derive(Debug, Clone)]
pub struct CheckConstraint {
    pub name: String,
    pub definition: String,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub id: TableId,
    pub columns: Vec<Column>,
    pub foreign_keys: Vec<ForeignKey>,
    pub unique_keys: Vec<UniqueKey>,
    pub checks: Vec<CheckConstraint>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Whether a CHECK on this table says the column may not be null.
    ///
    /// `ALTER TABLE ... SET NOT NULL` takes an exclusive lock and rewrites, so
    /// large schemas add the same rule as a CHECK instead. It is the same rule.
    pub fn check_forbids_null(&self, column: &str) -> bool {
        self.checks.iter().any(|check| {
            matches!(
                crate::checks::interpret(&check.definition),
                crate::checks::Meaning::NotNull { column: c } if c == column
            )
        })
    }

    /// Whether a CHECK on this table is satisfied by leaving the column NULL,
    /// and so obliges the generator to.
    ///
    /// The counterpart of `check_forbids_null`, and the reason both exist: a
    /// table can carry one of each over the same column, and then no row
    /// satisfies both. GitLab's `ai_tool_rules` says three columns may each be
    /// null-or-one-of-a-list, and separately that at least one of the three
    /// must not be null.
    pub fn check_forces_null(&self, column: &str) -> bool {
        self.checks.iter().any(|check| {
            matches!(
                crate::checks::interpret(&check.definition),
                crate::checks::Meaning::MustBeNull { column: c } if c == column
            )
        })
    }

    /// The columns a row must actually supply: not generated, and either
    /// required or worth filling. A column with a default is left to the
    /// database, which is both simpler and more likely to be what the schema
    /// author intended.
    pub fn columns_to_write(&self) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|c| !c.is_generated && (!c.has_default || self.default_will_not_do(c)))
            .collect()
    }

    /// Whether leaving this column to its default would break a unique key.
    ///
    /// `resource_version INTEGER NOT NULL DEFAULT 1 UNIQUE` is a real column
    /// in a real schema. Omitting it gives every row the value 1, and the
    /// second row is rejected. A sequence default is fine — that is what a
    /// sequence is for — and a generated column is never written at all.
    fn default_will_not_do(&self, column: &Column) -> bool {
        !column.default_is_sequence
            && self
                .unique_keys
                .iter()
                .any(|k| k.columns.contains(&column.name))
    }

    pub fn primary_key(&self) -> Option<&UniqueKey> {
        self.unique_keys.iter().find(|k| k.is_primary)
    }
}

/// Every table read, keyed so that iteration order is stable — which matters,
/// because a topological sort with ties broken by hash order would produce a
/// different plan on every run and destroy reproducibility.
#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub tables: BTreeMap<TableId, Table>,
}

impl Schema {
    pub fn get(&self, id: &TableId) -> Option<&Table> {
        self.tables.get(id)
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identifier_with_a_quote_in_it_is_escaped_not_broken() {
        // A table called o"rders is legal, rare, and an injection if the
        // quoting is done by concatenation.
        assert_eq!(quote_ident("o\"rders"), "\"o\"\"rders\"");
        assert_eq!(quote_ident("orders"), "\"orders\"");
    }

    #[test]
    fn a_table_in_public_prints_without_the_schema() {
        // What a person calls it, in every message they will read.
        assert_eq!(TableId::new("public", "orders").to_string(), "orders");
        assert_eq!(
            TableId::new("billing", "orders").to_string(),
            "billing.orders"
        );
    }

    #[test]
    fn a_table_is_always_fully_qualified_in_sql() {
        assert_eq!(
            TableId::new("public", "orders").quoted(),
            "\"public\".\"orders\""
        );
    }

    #[test]
    fn an_unknown_type_is_not_generatable_and_keeps_its_name() {
        let t = ColumnType::Unsupported {
            name: "geometry".into(),
        };
        assert!(!t.is_generatable());
        assert_eq!(t.describe(), "geometry");
    }

    #[test]
    fn a_domain_carrying_a_constraint_is_refused_like_a_check() {
        // A domain constraint is a CHECK wearing a different hat, and
        // satisfying it needs the same expression solving this does not do.
        let constrained = ColumnType::Domain {
            name: "positive_int".into(),
            inner: Box::new(ColumnType::Integer { bytes: 4 }),
            has_constraint: true,
        };
        assert!(!constrained.is_generatable());

        let plain = ColumnType::Domain {
            name: "email".into(),
            inner: Box::new(ColumnType::Text { max_length: None }),
            has_constraint: false,
        };
        assert!(plain.is_generatable());
    }

    #[test]
    fn an_array_of_something_unknown_is_unknown() {
        let t = ColumnType::Array {
            of: Box::new(ColumnType::Unsupported {
                name: "geometry".into(),
            }),
            dimensions: 1,
        };
        assert!(!t.is_generatable());
        assert_eq!(t.describe(), "geometry[]");
    }

    fn table_with(columns: Vec<Column>) -> Table {
        Table {
            id: TableId::new("public", "t"),
            columns,
            foreign_keys: vec![],
            unique_keys: vec![],
            checks: vec![],
        }
    }

    fn column(name: &str, nullable: bool, has_default: bool, generated: bool) -> Column {
        Column {
            name: name.into(),
            type_: ColumnType::Integer { bytes: 4 },
            nullable,
            has_default,
            // Fixtures without a unique key never reach the sequence question.
            default_is_sequence: false,
            is_generated: generated,
            position: 1,
        }
    }

    #[test]
    fn generated_and_defaulted_columns_are_left_to_the_database() {
        // Writing to GENERATED ALWAYS fails outright, and overriding a default
        // is usually the opposite of what the schema author wanted.
        let t = table_with(vec![
            column("id", false, true, true),
            column("created_at", false, true, false),
            column("amount", false, false, false),
        ]);
        let names: Vec<&str> = t
            .columns_to_write()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["amount"]);
    }

    #[test]
    fn a_foreign_key_is_optional_only_when_every_column_of_it_is() {
        // A composite key with one NOT NULL column cannot be left out, so it
        // cannot be used to break a cycle.
        let t = table_with(vec![
            column("a", true, false, false),
            column("b", false, false, false),
        ]);
        let both_nullable = ForeignKey {
            name: "fk".into(),
            columns: vec!["a".into()],
            references: TableId::new("public", "other"),
            referenced_columns: vec!["id".into()],
            deferrable: false,
        };
        let one_required = ForeignKey {
            columns: vec!["a".into(), "b".into()],
            ..both_nullable.clone()
        };
        assert!(both_nullable.is_optional(&t));
        assert!(!one_required.is_optional(&t));
    }
}

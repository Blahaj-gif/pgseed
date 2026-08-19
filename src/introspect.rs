//! Reading the schema out of Postgres.
//!
//! From `pg_catalog`, not `information_schema`. The standard views are easier
//! to read and lose exactly the things this needs: `information_schema` will
//! not give you a CHECK constraint's expression, hides constraints on tables
//! you do not own, and flattens identity and generated columns into a form
//! that cannot be told apart. `pg_get_constraintdef` is worth the uglier SQL.
//!
//! Everything here is one query per *kind* of thing rather than one query per
//! table. A schema of forty tables should cost five round trips, not two
//! hundred, and the ordering of the results is made deterministic in SQL so
//! the rest of the program never has to sort to stay reproducible.

use std::collections::BTreeMap;

use postgres::Client;

use crate::schema::{
    quote_ident, CheckConstraint, Column, ColumnType, ForeignKey, Schema, Table, TableId, UniqueKey,
};

/// Ordinary tables in the named schemas. Views, matviews, partitions and
/// foreign tables are excluded: a view cannot be inserted into, and a
/// partition is filled through its parent.
const TABLES_SQL: &str = "
    SELECT n.nspname, c.relname
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p')
      AND NOT c.relispartition
      AND n.nspname = ANY($1)
    ORDER BY n.nspname, c.relname";

/// Columns, with everything that decides whether and how to write one.
///
/// `attidentity` marks GENERATED ... AS IDENTITY and `attgenerated` marks a
/// generated column; both must be left out of an insert entirely rather than
/// merely defaulted, because naming them is an error rather than an override.
const COLUMNS_SQL: &str = "
    SELECT n.nspname, c.relname, a.attname, a.attnum,
           a.attnotnull, a.atthasdef,
           (a.attidentity <> '' OR a.attgenerated <> '') AS generated,
           COALESCE(pg_get_expr(ad.adbin, ad.adrelid) LIKE 'nextval(%', false)
             AS default_is_sequence,
           t.typname, t.typtype, a.atttypmod, a.attndims,
           bt.typname AS base_typname, bt.typtype AS base_typtype,
           EXISTS (SELECT 1 FROM pg_constraint dc
                   WHERE dc.contypid = t.oid AND dc.contype = 'c') AS domain_checked,
           et.typname AS element_typname, et.typtype AS element_typtype
    FROM pg_attribute a
    JOIN pg_class c ON c.oid = a.attrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_type t ON t.oid = a.atttypid
    LEFT JOIN pg_type bt ON bt.oid = t.typbasetype
    LEFT JOIN pg_type et ON et.oid = t.typelem
    LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum
    WHERE c.relkind IN ('r', 'p')
      AND NOT c.relispartition
      AND n.nspname = ANY($1)
      AND a.attnum > 0
      AND NOT a.attisdropped
    ORDER BY n.nspname, c.relname, a.attnum";

/// Every constraint that matters, with its definition text.
///
/// `conkey` and `confkey` are attribute-number arrays, resolved to names in
/// SQL so the caller never has to hold a second lookup table. The ordinality
/// join keeps composite-key columns in their declared order, which is the
/// order the referencing and referenced sides have to agree on.
const CONSTRAINTS_SQL: &str = "
    SELECT n.nspname, c.relname, con.conname, con.contype,
           con.condeferrable,
           pg_get_constraintdef(con.oid) AS definition,
           (SELECT array_agg(att.attname ORDER BY k.ord)
              FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
              JOIN pg_attribute att
                ON att.attrelid = con.conrelid AND att.attnum = k.attnum
           ) AS columns,
           fn.nspname AS ref_schema, fc.relname AS ref_table,
           (SELECT array_agg(att.attname ORDER BY k.ord)
              FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
              JOIN pg_attribute att
                ON att.attrelid = con.confrelid AND att.attnum = k.attnum
           ) AS ref_columns
    FROM pg_constraint con
    JOIN pg_class c ON c.oid = con.conrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN pg_class fc ON fc.oid = con.confrelid
    LEFT JOIN pg_namespace fn ON fn.oid = fc.relnamespace
    WHERE con.contype IN ('p', 'f', 'u', 'c', 'x')
      AND n.nspname = ANY($1)
    ORDER BY n.nspname, c.relname, con.conname";

/// Enum labels, in declaration order — which is the order a person means when
/// they say "the first status".
/// Indexes that constrain what can be written, and that no constraint backs.
///
/// Two kinds, and the second is easy to miss.
///
/// A **unique index** is a uniqueness requirement exactly as binding as
/// `UNIQUE (name)`, and it lives in `pg_index` rather than `pg_constraint` —
/// so reading only constraints missed 1,397 of them across the nine corpus
/// schemas, GitLab alone accounting for 1,046.
///
/// An **expression index** constrains the data whether or not it enforces
/// uniqueness, because the expression is evaluated on every row inserted.
/// Discourse indexes `((data)::jsonb ->> 'display_username')` on a `varchar`
/// column, and that index is not unique, but an ordinary word written to
/// `data` fails to cast and the insert is rejected. An index that cannot be
/// violated can still refuse a row.
///
/// `CREATE UNIQUE INDEX ... ON t (name)` is a uniqueness requirement exactly
/// as binding as `UNIQUE (name)`, and it lives in `pg_index` rather than
/// `pg_constraint` — so reading only constraints missed 1,397 of them across
/// the nine corpus schemas, GitLab alone accounting for 1,046.
///
/// The exclusion is only for constraints that *own* an index — a primary key,
/// a unique constraint, an exclusion constraint — because those arrive through
/// `pg_constraint` already. A foreign key also fills in `conindid`, pointing at
/// the unique index on the **referenced** side that it validates against, and
/// excluding on that made every referenced unique index invisible. GitLab's
/// `index_oauth_applications_on_uid` is one, and it duplicated on the second
/// row because of it.
///
/// `indnkeyatts` rather than `indnatts`: columns added with INCLUDE are stored
/// in the index and are not part of what it makes unique. `indkey` is an
/// `int2vector`, which is subscripted from zero — so the slice ends at
/// `indnkeyatts - 1`, and ending it at `indnkeyatts` took one column too many
/// and made every INCLUDE index look like an expression index.
const UNIQUE_INDEXES_SQL: &str = "
    SELECT n.nspname, c.relname, i.relname AS index_name,
           (SELECT array_agg(a.attname ORDER BY k.ord)
              FROM unnest(ix.indkey[0:ix.indnkeyatts - 1]) WITH ORDINALITY AS k(attnum, ord)
              JOIN pg_attribute a
                ON a.attrelid = c.oid AND a.attnum = k.attnum AND NOT a.attisdropped
           ) AS columns,
           ix.indnkeyatts,
           ix.indisunique AS is_unique,
           (ix.indexprs IS NOT NULL) AS has_expression,
           pg_get_indexdef(ix.indexrelid) AS definition
    FROM pg_index ix
    JOIN pg_class c ON c.oid = ix.indrelid
    JOIN pg_class i ON i.oid = ix.indexrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE (ix.indisunique OR ix.indexprs IS NOT NULL)
      AND ix.indislive
      AND n.nspname = ANY($1)
      AND NOT EXISTS (
            SELECT 1 FROM pg_constraint con
            WHERE con.conindid = ix.indexrelid
              AND con.contype IN ('p', 'u', 'x'))
    ORDER BY n.nspname, c.relname, i.relname";

/// Row-level triggers that fire on insert, with the body of the function they
/// call.
///
/// Anything that fires on insert, at row level or statement level. Statement
/// level was excluded at first on the reasoning that it fires once regardless
/// of what is in the rows — which is true and beside the point, because
/// GitLab's copies the whole `NEW TABLE` into a partitioned table and the
/// routing fails. `tgtype` bit 2 is INSERT; `tgisinternal` marks the triggers
/// Postgres creates to implement foreign keys, already read as constraints.
const TRIGGERS_SQL: &str = "
    SELECT n.nspname, c.relname, t.tgname, p.prosrc
    FROM pg_trigger t
    JOIN pg_class c ON c.oid = t.tgrelid
    JOIN pg_proc p ON p.oid = t.tgfoid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE NOT t.tgisinternal
      AND (t.tgtype & 4) <> 0
      AND n.nspname = ANY($1)
    ORDER BY n.nspname, c.relname, t.tgname";

/// Partitioned tables, their key, and the bounds of every partition under
/// them.
///
/// A partitioned parent holds no rows of its own, so a row that falls outside
/// every partition is refused. Reading them is what stops everything that
/// references one from being refused for pointing at a table nobody read.
const PARTITIONS_SQL: &str = "
    SELECT n.nspname, c.relname,
           pg_get_partkeydef(c.oid) AS key,
           (SELECT array_agg(pg_get_expr(child.relpartbound, child.oid))
              FROM pg_inherits i
              JOIN pg_class child ON child.oid = i.inhrelid
             WHERE i.inhparent = c.oid) AS bounds,
           -- A partition may carry rules of its own that the parent does not.
           -- GitLab's `project_uploads` has a CHECK the parent `uploads` has
           -- no sign of, and a row routed into it is judged by that CHECK.
           (SELECT count(*)
              FROM pg_inherits i
              JOIN pg_constraint con ON con.conrelid = i.inhrelid
             WHERE i.inhparent = c.oid AND con.contype = 'c'
               -- Locally defined only. Postgres copies a parent's CHECK down
               -- to every partition, and `coninhcount > 0` marks those — they
               -- are the parent's rule, already read, not a new one the
               -- partition adds.
               AND con.coninhcount = 0) AS own_rules
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind = 'p'
      AND NOT c.relispartition
      AND n.nspname = ANY($1)";

const ENUMS_SQL: &str = "
    SELECT t.typname, n.nspname, e.enumlabel
    FROM pg_enum e
    JOIN pg_type t ON t.oid = e.enumtypid
    JOIN pg_namespace n ON n.oid = t.typnamespace
    ORDER BY t.typname, n.nspname, e.enumsortorder";

/// The `pg_type` columns that decide what a column's type is.
///
/// A struct rather than eight positional arguments, three of which are `&str`
/// and two of which are `Option<(&str, &str)>` — a pair that is easy to hand
/// over in the wrong order and impossible to notice having done so.
#[derive(Debug, Clone, Copy)]
pub struct TypeRow<'a> {
    pub typname: &'a str,
    pub typtype: &'a str,
    /// `atttypmod`: the declared length or precision, offset by four.
    pub typmod: i32,
    /// `attndims`: how many array dimensions were declared.
    pub ndims: i32,
    /// For a domain, the type it wraps, as (name, kind).
    pub base: Option<(&'a str, &'a str)>,
    /// Whether a domain adds a constraint of its own.
    pub domain_checked: bool,
    /// For an array, the type of its elements, as (name, kind).
    pub element: Option<(&'a str, &'a str)>,
}

impl<'a> TypeRow<'a> {
    /// A plain named type: no length, no dimensions, nothing wrapped. What the
    /// recursive calls need when they descend into a domain or an array.
    pub fn plain(typname: &'a str, typtype: &'a str, typmod: i32) -> TypeRow<'a> {
        TypeRow {
            typname,
            typtype,
            typmod,
            ndims: 0,
            base: None,
            domain_checked: false,
            element: None,
        }
    }
}

/// Map what Postgres calls a type to what this tool can do about it.
///
/// A pure function, and unit tested, because it is the part most likely to be
/// quietly wrong and the part that does not need a database to check. An
/// unrecognised name becomes `Unsupported` carrying that name rather than a
/// guess: a column this cannot generate must refuse its table by name.
pub fn map_type(row: TypeRow<'_>, enums: &BTreeMap<String, EnumType>) -> ColumnType {
    let TypeRow {
        typname,
        typtype,
        typmod,
        ndims,
        base,
        domain_checked,
        element,
    } = row;
    // Enum, before anything else: its name is user-chosen and could collide
    // with a built-in.
    if typtype == "e" {
        let found = enums.get(typname).cloned().unwrap_or_default();
        return ColumnType::Enum {
            name: typname.to_string(),
            qualified: found.qualified,
            labels: found.labels,
        };
    }

    // Domain: generate for the underlying type, unless the domain adds a
    // constraint, which is a CHECK by another name and refused like one.
    if typtype == "d" {
        let inner = match base {
            Some((base_name, base_type)) => {
                map_type(TypeRow::plain(base_name, base_type, typmod), enums)
            }
            None => ColumnType::Unsupported {
                name: typname.to_string(),
            },
        };
        return ColumnType::Domain {
            name: typname.to_string(),
            inner: Box::new(inner),
            has_constraint: domain_checked,
        };
    }

    // Arrays: Postgres names them with a leading underscore.
    if let Some(stripped) = typname.strip_prefix('_') {
        let of = match element {
            Some((el_name, el_type)) => map_type(TypeRow::plain(el_name, el_type, typmod), enums),
            None => map_type(TypeRow::plain(stripped, "b", typmod), enums),
        };
        return ColumnType::Array {
            of: Box::new(of),
            dimensions: if ndims > 0 { ndims } else { 1 },
        };
    }

    // `atttypmod` carries the declared length or precision, offset by four
    // for the length header. -1 means unlimited.
    let length = if typmod > 4 { Some(typmod - 4) } else { None };

    match typname {
        "bool" => ColumnType::Boolean,
        "int2" => ColumnType::Integer { bytes: 2 },
        "int4" => ColumnType::Integer { bytes: 4 },
        "int8" => ColumnType::Integer { bytes: 8 },
        "float4" => ColumnType::Float { bytes: 4 },
        "float8" => ColumnType::Float { bytes: 8 },
        "numeric" => {
            let (precision, scale) = if typmod > 4 {
                let packed = typmod - 4;
                (Some(packed >> 16), Some(packed & 0xffff))
            } else {
                (None, None)
            };
            ColumnType::Numeric { precision, scale }
        }
        "varchar" | "bpchar" => ColumnType::Text { max_length: length },
        "text" | "name" | "citext" => ColumnType::Text { max_length: None },
        "uuid" => ColumnType::Uuid,
        "date" => ColumnType::Date,
        "time" | "timetz" => ColumnType::Time,
        "timestamp" => ColumnType::Timestamp { with_zone: false },
        "timestamptz" => ColumnType::Timestamp { with_zone: true },
        "interval" => ColumnType::Interval,
        "json" => ColumnType::Json { binary: false },
        "jsonb" => ColumnType::Json { binary: true },
        "bytea" => ColumnType::Bytea,
        "inet" => ColumnType::Network {
            kind: crate::schema::NetworkKind::Inet,
        },
        "cidr" => ColumnType::Network {
            kind: crate::schema::NetworkKind::Cidr,
        },
        "macaddr" | "macaddr8" => ColumnType::Network {
            kind: crate::schema::NetworkKind::MacAddr,
        },
        other => ColumnType::Unsupported {
            name: other.to_string(),
        },
    }
}

/// Read every table in the named schemas.
pub fn read(client: &mut Client, schemas: &[String]) -> Result<Schema, postgres::Error> {
    let enums = read_enums(client)?;
    let mut schema = Schema::default();

    for row in client.query(TABLES_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        schema.tables.insert(
            id.clone(),
            Table {
                id,
                columns: vec![],
                foreign_keys: vec![],
                unique_keys: vec![],
                checks: vec![],
            },
        );
    }

    for row in client.query(COLUMNS_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(table) = schema.tables.get_mut(&id) else {
            continue;
        };

        let typname: String = row.get("typname");
        let typtype: i8 = row.get("typtype");
        let base_typname: Option<String> = row.get("base_typname");
        let base_typtype: Option<i8> = row.get("base_typtype");
        let element_typname: Option<String> = row.get("element_typname");
        let element_typtype: Option<i8> = row.get("element_typtype");

        let base_typtype = base_typtype.map(|c| (c as u8 as char).to_string());
        let element_typtype = element_typtype.map(|c| (c as u8 as char).to_string());

        let type_ = map_type(
            TypeRow {
                typname: &typname,
                typtype: &(typtype as u8 as char).to_string(),
                typmod: row.get::<_, i32>("atttypmod"),
                // `attndims` is int2 in the catalog, not int4. Reading it as
                // the wrong width is a deserialisation error rather than a
                // silent misread, which is the good kind of wrong — and
                // exactly the kind only a real database finds.
                ndims: row.get::<_, i16>("attndims") as i32,
                base: base_typname.as_deref().zip(base_typtype.as_deref()),
                domain_checked: row.get::<_, bool>("domain_checked"),
                element: element_typname.as_deref().zip(element_typtype.as_deref()),
            },
            &enums,
        );

        table.columns.push(Column {
            name: row.get("attname"),
            type_,
            nullable: !row.get::<_, bool>("attnotnull"),
            has_default: row.get::<_, bool>("atthasdef"),
            default_is_sequence: row.get::<_, bool>("default_is_sequence"),
            is_generated: row.get::<_, bool>("generated"),
            position: row.get::<_, i16>("attnum") as i32,
        });
    }

    // Unique indexes, which are a uniqueness requirement as binding as a
    // unique constraint and are not stored as one.
    for row in client.query(UNIQUE_INDEXES_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(table) = schema.tables.get_mut(&id) else {
            continue;
        };
        let name: String = row.get("index_name");
        let definition: String = row.get("definition");
        let unique: bool = row.get("is_unique");

        // An index over an expression — `lower(email)` — is a rule about a
        // computed value, and making the underlying column distinct does not
        // make the expression distinct. There is no closed form for it here,
        // so it is recorded as a check and refuses its table, the same as any
        // other rule this cannot show it satisfies.
        match crate::indexes::interpret(&definition) {
            // Plain columns. Unique makes them a key; not unique asks nothing.
            crate::indexes::Requirement::Columns(columns) => {
                if unique {
                    table.unique_keys.push(UniqueKey {
                        name,
                        columns,
                        is_primary: false,
                    });
                }
            }
            // Every expression is `lower(col)`, which cannot fail — so a
            // non-unique one constrains nothing at all. A unique one is a key
            // over the underlying columns, because every string generated here
            // is lower case already and `lower(col)` is therefore `col`. That
            // last part is recorded rather than assumed: the lowercase rule
            // goes in as a check the generator honours, so the day the word
            // list gains a capital letter this still holds.
            crate::indexes::Requirement::Lowered { columns, lowered } => {
                for column in lowered {
                    table.checks.push(CheckConstraint {
                        name: format!("{name}:lower({column})"),
                        definition: format!("CHECK (({column} = lower({column})))"),
                    });
                }
                if unique {
                    table.unique_keys.push(UniqueKey {
                        name,
                        columns,
                        is_primary: false,
                    });
                }
            }
            // Every key is an expression that cannot fail. A non-unique index
            // enforces nothing, so it constrains nothing and is ignored; a
            // unique one is asking whether those expressions can be made
            // distinct, which is a different question and not one this can
            // answer.
            crate::indexes::Requirement::Harmless => {
                if unique {
                    table.checks.push(CheckConstraint { name, definition });
                }
            }
            // Anything else refuses the table, quoting the index back. An
            // expression that cannot be read might fail on every row, and an
            // index that enforces no uniqueness at all can still reject one.
            crate::indexes::Requirement::Unknown => {
                table.checks.push(CheckConstraint { name, definition });
            }
        }
    }

    // Triggers that can stop an insert. Recorded as checks, because that is
    // exactly what they are — a rule over the row that this cannot satisfy —
    // and because the refusal then quotes the trigger by name like any other.
    // Partitioned tables: either every row lands somewhere, or the table is
    // refused. Recorded as checks so the refusal reads like every other one.
    let mut partitioned_tables: Vec<TableId> = Vec::new();
    for row in client.query(PARTITIONS_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        partitioned_tables.push(id.clone());
        let Some(table) = schema.tables.get_mut(&id) else {
            continue;
        };
        let key: String = row.get("key");
        let bounds: Vec<String> = row.try_get("bounds").unwrap_or_default();
        let own_rules: i64 = row.try_get("own_rules").unwrap_or(0);

        // A partition carrying its own CHECK judges the rows routed into it by
        // a rule the parent never mentions, and nothing here reads it.
        if own_rules > 0 {
            table.checks.push(CheckConstraint {
                name: format!("{}:partitions", id.name),
                definition: format!(
                    "PARTITION BY {key} — {own_rules} of its partitions carry                      constraints of their own, which this does not read"
                ),
            });
            continue;
        }

        match crate::partitions::interpret(&key, &bounds) {
            crate::partitions::Routing::Anything => {}
            crate::partitions::Routing::OneOf { column, values } => {
                // The same shape `checks` already knows, so it goes in as one
                // rather than as a second way of saying it.
                table.checks.push(CheckConstraint {
                    name: format!("{}:partitions", id.name),
                    definition: format!("CHECK (({column} = ANY (ARRAY[{}])))", values.join(", ")),
                });
            }
            crate::partitions::Routing::Unknown if bounds.is_empty() => {
                // Not "this could not be read": there is nothing to read. A
                // partitioned table with no partitions attached takes no row
                // from anybody, and a reader staring at the refusal deserves
                // to know it is not about them or about this tool. GitLab
                // declares 101 such parents and creates the partitions at
                // runtime; 83 of the corpus's 106 are like this.
                table.checks.push(CheckConstraint {
                    name: format!("{}:partitions", id.name),
                    definition: format!(
                        "PARTITION BY {key} with no partitions attached — no row can                          land anywhere, so this table holds nothing until one exists"
                    ),
                });
            }
            crate::partitions::Routing::Unknown => {
                table.checks.push(CheckConstraint {
                    name: format!("{}:partitions", id.name),
                    definition: format!(
                        "PARTITION BY {key} — this cannot show a row lands in any partition"
                    ),
                });
            }
        }
    }

    for row in client.query(CONSTRAINTS_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(table) = schema.tables.get_mut(&id) else {
            continue;
        };

        let name: String = row.get("conname");
        let kind = row.get::<_, i8>("contype") as u8 as char;
        let columns: Vec<String> = row.try_get("columns").unwrap_or_default();

        match kind {
            'p' | 'u' => table.unique_keys.push(UniqueKey {
                name,
                columns,
                is_primary: kind == 'p',
            }),
            'f' => {
                let ref_schema: Option<String> = row.get("ref_schema");
                let ref_table: Option<String> = row.get("ref_table");
                if let (Some(s), Some(t)) = (ref_schema, ref_table) {
                    table.foreign_keys.push(ForeignKey {
                        name,
                        columns,
                        references: TableId::new(s, t),
                        referenced_columns: row.try_get("ref_columns").unwrap_or_default(),
                        deferrable: row.get::<_, bool>("condeferrable"),
                    });
                }
            }
            // An exclusion constraint is read as a check, which is what it is:
            // a rule over the row that this cannot prove it satisfies. GitLab
            // has four, and two of them build `daterange(start_date, due_date)`
            // out of two columns generated independently — so half the time
            // the range came out backwards. Not seeing a constraint is not the
            // same as satisfying it.
            'c' | 'x' => table.checks.push(CheckConstraint {
                name,
                definition: row.get("definition"),
            }),
            _ => {}
        }
    }

    // Triggers last: whether one interferes depends on which columns are
    // written, and that depends on the unique keys, which arrive with the
    // constraints above. Reading them earlier meant asking the question before
    // the answer existed.
    let mut written_by_triggers: Vec<(String, String, TableId)> = Vec::new();
    for row in client.query(TRIGGERS_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(table) = schema.tables.get_mut(&id) else {
            continue;
        };
        let name: String = row.get("tgname");
        let body: String = row.get("prosrc");
        let written: Vec<String> = table
            .columns_to_write()
            .iter()
            .map(|c| c.name.to_lowercase())
            .collect();
        if crate::triggers::interferes(&body, &written) {
            table.checks.push(CheckConstraint {
                name: name.clone(),
                definition: format!(
                    "TRIGGER {name} raises on insert, rewrites the row, or writes elsewhere"
                ),
            });
        }
        for target in crate::triggers::writes_to(&body) {
            written_by_triggers.push((target, name.clone(), id.clone()));
        }
    }

    for (target, trigger, source) in written_by_triggers {
        // Unqualified, so matched by name across the schemas that were read.
        let ids: Vec<TableId> = schema
            .tables
            .keys()
            .filter(|id| id.name.eq_ignore_ascii_case(&target))
            .cloned()
            .collect();

        let partitioned: Vec<TableId> = ids
            .iter()
            .filter(|id| partitioned_tables.contains(id))
            .cloned()
            .collect();

        if ids.is_empty() || !partitioned.is_empty() {
            // The target was never read — a partitioned table, most often. The
            // write may fail for reasons invisible from here, so the table
            // carrying the trigger cannot be filled.
            if let Some(table) = schema.tables.get_mut(&source) {
                table.checks.push(CheckConstraint {
                    name: format!("{trigger}:writes"),
                    definition: format!(
                        "TRIGGER {trigger} writes rows into {target}, which was not                          read or is partitioned — the write cannot be shown to land"
                    ),
                });
            }
            if ids.is_empty() {
                continue;
            }
        }

        // The target was read, so the write itself is as safe as filling it —
        // but the rows it receives are not ones this counted.
        for id in ids {
            if let Some(table) = schema.tables.get_mut(&id) {
                table.checks.push(CheckConstraint {
                    name: format!("{trigger}:writes"),
                    definition: format!("TRIGGER {trigger} writes rows into this table"),
                });
            }
        }
    }

    Ok(schema)
}

/// An enum's labels, and the name that can be written for it in SQL.
///
/// `qualified` is `None` where two schemas define an enum of the same name:
/// columns are looked up by bare type name, so at that point the bare name
/// does not say which one, and writing either would be a guess.
#[derive(Debug, Clone, Default)]
pub struct EnumType {
    pub qualified: Option<String>,
    pub labels: Vec<String>,
}

fn read_enums(client: &mut Client) -> Result<BTreeMap<String, EnumType>, postgres::Error> {
    let mut seen: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for row in client.query(ENUMS_SQL, &[])? {
        seen.entry(row.get::<_, String>(0))
            .or_default()
            .entry(row.get::<_, String>(1))
            .or_default()
            .push(row.get::<_, String>(2));
    }

    let mut out: BTreeMap<String, EnumType> = BTreeMap::new();
    for (typname, by_schema) in seen {
        let ambiguous = by_schema.len() > 1;
        // Labels from the first schema either way: a value still has to be
        // produced, and only the *name* is in doubt.
        let (namespace, labels) = by_schema.into_iter().next().expect("non-empty");
        out.insert(
            typname.clone(),
            EnumType {
                qualified: (!ambiguous)
                    .then(|| format!("{}.{}", quote_ident(&namespace), quote_ident(&typname))),
                labels,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_enums() -> BTreeMap<String, EnumType> {
        BTreeMap::new()
    }

    fn map(typname: &str, typmod: i32) -> ColumnType {
        map_type(TypeRow::plain(typname, "b", typmod), &no_enums())
    }

    #[test]
    fn the_ordinary_types_are_recognised() {
        assert_eq!(map("int4", -1), ColumnType::Integer { bytes: 4 });
        assert_eq!(map("int8", -1), ColumnType::Integer { bytes: 8 });
        assert_eq!(map("bool", -1), ColumnType::Boolean);
        assert_eq!(map("uuid", -1), ColumnType::Uuid);
        assert_eq!(map("jsonb", -1), ColumnType::Json { binary: true });
        assert_eq!(
            map("timestamptz", -1),
            ColumnType::Timestamp { with_zone: true }
        );
    }

    #[test]
    fn a_declared_length_is_carried_so_a_value_can_fit_it() {
        // varchar(20) arrives as atttypmod 24: the length plus a four-byte
        // header. Writing 30 characters into it is a runtime error.
        assert_eq!(
            map("varchar", 24),
            ColumnType::Text {
                max_length: Some(20)
            }
        );
        assert_eq!(map("varchar", -1), ColumnType::Text { max_length: None });
        assert_eq!(map("text", -1), ColumnType::Text { max_length: None });
    }

    #[test]
    fn numeric_precision_and_scale_are_unpacked() {
        // numeric(10,2) packs both into one integer: precision in the high
        // half, scale in the low. Getting this backwards puts the decimal
        // point in the wrong place on every money column in the database.
        let packed = ((10 << 16) | 2) + 4;
        assert_eq!(
            map("numeric", packed),
            ColumnType::Numeric {
                precision: Some(10),
                scale: Some(2)
            }
        );
        assert_eq!(
            map("numeric", -1),
            ColumnType::Numeric {
                precision: None,
                scale: None
            }
        );
    }

    #[test]
    fn an_unknown_type_keeps_its_name_for_the_refusal_message() {
        assert_eq!(
            map("geometry", -1),
            ColumnType::Unsupported {
                name: "geometry".into()
            }
        );
    }

    #[test]
    fn an_enum_carries_its_labels_in_declaration_order() {
        let mut enums = BTreeMap::new();
        enums.insert(
            "status".to_string(),
            EnumType {
                qualified: Some("\"public\".\"status\"".into()),
                labels: vec!["pending".to_string(), "shipped".to_string()],
            },
        );
        let t = map_type(TypeRow::plain("status", "e", -1), &enums);
        match t {
            ColumnType::Enum {
                name,
                labels,
                qualified,
            } => {
                assert_eq!(name, "status");
                assert_eq!(labels, vec!["pending", "shipped"]);
                // The qualified name is what an array of these is cast to.
                assert_eq!(qualified.as_deref(), Some("\"public\".\"status\""));
            }
            other => panic!("expected an enum, got {other:?}"),
        }
    }

    #[test]
    fn a_domain_without_a_check_is_generated_as_its_base_type() {
        let t = map_type(
            TypeRow {
                base: Some(("text", "b")),
                ..TypeRow::plain("email", "d", -1)
            },
            &no_enums(),
        );
        assert!(t.is_generatable());
        match &t {
            ColumnType::Domain {
                inner,
                has_constraint,
                ..
            } => {
                assert_eq!(**inner, ColumnType::Text { max_length: None });
                assert!(!has_constraint);
            }
            other => panic!("expected a domain, got {other:?}"),
        }
    }

    #[test]
    fn a_domain_with_a_check_is_not_generatable() {
        // `CREATE DOMAIN positive AS int CHECK (VALUE > 0)` needs the same
        // expression solving a table CHECK does, so it gets the same answer.
        let t = map_type(
            TypeRow {
                base: Some(("int4", "b")),
                domain_checked: true,
                ..TypeRow::plain("positive", "d", -1)
            },
            &no_enums(),
        );
        assert!(!t.is_generatable());
    }

    #[test]
    fn an_array_is_recognised_by_its_leading_underscore() {
        let t = map_type(
            TypeRow {
                ndims: 1,
                element: Some(("int4", "b")),
                ..TypeRow::plain("_int4", "b", -1)
            },
            &no_enums(),
        );
        match t {
            ColumnType::Array { of, dimensions } => {
                assert_eq!(*of, ColumnType::Integer { bytes: 4 });
                assert_eq!(dimensions, 1);
            }
            other => panic!("expected an array, got {other:?}"),
        }
    }

    #[test]
    fn an_array_of_an_unknown_type_is_still_unknown() {
        let t = map_type(
            TypeRow {
                ndims: 1,
                element: Some(("geometry", "b")),
                ..TypeRow::plain("_geometry", "b", -1)
            },
            &no_enums(),
        );
        assert!(!t.is_generatable());
        assert_eq!(t.describe(), "geometry[]");
    }
}

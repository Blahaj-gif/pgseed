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
    quote_ident, CheckConstraint, Column, ColumnType, ForeignKey, Schema, Table, TableId,
    UniqueKey,
};

/// Ordinary tables in the named schemas. Views, matviews, partitions and
/// foreign tables are excluded: a view cannot be inserted into, and a
/// partition is filled through its parent.
const TABLES_SQL: &str = "
    SELECT n.nspname, c.relname
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind = 'r'
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
    WHERE c.relkind = 'r'
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
/// column, and that index is not unique — but an ordinary word written to
/// `data` fails to cast and the insert is rejected. An index that cannot be
/// violated can still refuse a row.
///
/// `CREATE UNIQUE INDEX ... ON t (name)` is a uniqueness requirement exactly
/// as binding as `UNIQUE (name)`, and it lives in `pg_index` rather than
/// `pg_constraint` — so reading only constraints missed 1,397 of them across
/// the nine corpus schemas, GitLab alone accounting for 1,046.
///
/// `indnkeyatts` rather than `indnatts`: columns added with INCLUDE are stored
/// in the index and are not part of what it makes unique.
const UNIQUE_INDEXES_SQL: &str = "
    SELECT n.nspname, c.relname, i.relname AS index_name,
           (SELECT array_agg(a.attname ORDER BY k.ord)
              FROM unnest(ix.indkey[0:ix.indnkeyatts]) WITH ORDINALITY AS k(attnum, ord)
              JOIN pg_attribute a
                ON a.attrelid = c.oid AND a.attnum = k.attnum AND NOT a.attisdropped
           ) AS columns,
           ix.indnkeyatts,
           ix.indisunique AS is_unique,
           (ix.indexprs IS NOT NULL) AS has_expression
    FROM pg_index ix
    JOIN pg_class c ON c.oid = ix.indrelid
    JOIN pg_class i ON i.oid = ix.indexrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE (ix.indisunique OR ix.indexprs IS NOT NULL)
      AND ix.indislive
      AND n.nspname = ANY($1)
      AND NOT EXISTS (
            SELECT 1 FROM pg_constraint con WHERE con.conindid = ix.indexrelid)
    ORDER BY n.nspname, c.relname, i.relname";

const ENUMS_SQL: &str = "
    SELECT t.typname, n.nspname, e.enumlabel
    FROM pg_enum e
    JOIN pg_type t ON t.oid = e.enumtypid
    JOIN pg_namespace n ON n.oid = t.typnamespace
    ORDER BY t.typname, n.nspname, e.enumsortorder";

/// Map what Postgres calls a type to what this tool can do about it.
///
/// A pure function, and unit tested, because it is the part most likely to be
/// quietly wrong and the part that does not need a database to check. An
/// unrecognised name becomes `Unsupported` carrying that name rather than a
/// guess: a column this cannot generate must refuse its table by name.
pub fn map_type(
    typname: &str,
    typtype: &str,
    typmod: i32,
    ndims: i32,
    base: Option<(&str, &str)>,
    domain_checked: bool,
    element: Option<(&str, &str)>,
    enums: &BTreeMap<String, EnumType>,
) -> ColumnType {
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
            Some((base_name, base_type)) => map_type(
                base_name, base_type, typmod, 0, None, false, None, enums,
            ),
            None => ColumnType::Unsupported { name: typname.to_string() },
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
            Some((el_name, el_type)) => {
                map_type(el_name, el_type, typmod, 0, None, false, None, enums)
            }
            None => map_type(stripped, "b", typmod, 0, None, false, None, enums),
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
        "inet" => ColumnType::Network { kind: crate::schema::NetworkKind::Inet },
        "cidr" => ColumnType::Network { kind: crate::schema::NetworkKind::Cidr },
        "macaddr" | "macaddr8" => {
            ColumnType::Network { kind: crate::schema::NetworkKind::MacAddr }
        }
        other => ColumnType::Unsupported { name: other.to_string() },
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
            Table { id, columns: vec![], foreign_keys: vec![], unique_keys: vec![], checks: vec![] },
        );
    }

    for row in client.query(COLUMNS_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(table) = schema.tables.get_mut(&id) else { continue };

        let typname: String = row.get("typname");
        let typtype: i8 = row.get("typtype");
        let base_typname: Option<String> = row.get("base_typname");
        let base_typtype: Option<i8> = row.get("base_typtype");
        let element_typname: Option<String> = row.get("element_typname");
        let element_typtype: Option<i8> = row.get("element_typtype");

        let base_typtype = base_typtype.map(|c| (c as u8 as char).to_string());
        let element_typtype = element_typtype.map(|c| (c as u8 as char).to_string());

        let type_ = map_type(
            &typname,
            &(typtype as u8 as char).to_string(),
            row.get::<_, i32>("atttypmod"),
            // `attndims` is int2 in the catalog, not int4. Reading it as the
            // wrong width is a deserialisation error rather than a silent
            // misread, which is the good kind of wrong — and exactly the kind
            // only a real database finds.
            row.get::<_, i16>("attndims") as i32,
            base_typname.as_deref().zip(base_typtype.as_deref()),
            row.get::<_, bool>("domain_checked"),
            element_typname.as_deref().zip(element_typtype.as_deref()),
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
        let Some(table) = schema.tables.get_mut(&id) else { continue };
        let name: String = row.get("index_name");
        let columns: Vec<String> = row.try_get("columns").unwrap_or_default();
        let key_columns: i16 = row.get("indnkeyatts");

        // An index over an expression — `lower(email)` — is a rule about a
        // computed value, and making the underlying column distinct does not
        // make the expression distinct. There is no closed form for it here,
        // so it is recorded as a check and refuses its table, the same as any
        // other rule this cannot show it satisfies.
        // An index over an expression — `lower(email)`, `(data)::jsonb ->>
        // 'x'` — is a rule about a computed value. Making the underlying
        // column distinct does not make the expression distinct, and an
        // expression that fails to evaluate rejects the row outright. There is
        // no closed form for either here, so it is recorded as a check and
        // refuses its table like any other rule this cannot show it satisfies.
        if row.get::<_, bool>("has_expression") || columns.len() != key_columns as usize {
            table.checks.push(CheckConstraint {
                name: name.clone(),
                definition: format!("INDEX {name} over an expression"),
            });
            continue;
        }

        // Not unique and not an expression: it constrains nothing, and only
        // arrived here because the query asks for both kinds at once.
        if !row.get::<_, bool>("is_unique") {
            continue;
        }

        // A partial index constrains only the rows matching its predicate.
        // Treating it as constraining every row is *stricter* than the real
        // rule, and stricter is the safe direction: rows that are all distinct
        // satisfy a requirement that only some of them be.
        table.unique_keys.push(UniqueKey { name, columns, is_primary: false });
    }

    for row in client.query(CONSTRAINTS_SQL, &[&schemas])? {
        let id = TableId::new(row.get::<_, String>(0), row.get::<_, String>(1));
        let Some(table) = schema.tables.get_mut(&id) else { continue };

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
                qualified: (!ambiguous).then(|| {
                    format!("{}.{}", quote_ident(&namespace), quote_ident(&typname))
                }),
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
        map_type(typname, "b", typmod, 0, None, false, None, &no_enums())
    }

    #[test]
    fn the_ordinary_types_are_recognised() {
        assert_eq!(map("int4", -1), ColumnType::Integer { bytes: 4 });
        assert_eq!(map("int8", -1), ColumnType::Integer { bytes: 8 });
        assert_eq!(map("bool", -1), ColumnType::Boolean);
        assert_eq!(map("uuid", -1), ColumnType::Uuid);
        assert_eq!(map("jsonb", -1), ColumnType::Json { binary: true });
        assert_eq!(map("timestamptz", -1), ColumnType::Timestamp { with_zone: true });
    }

    #[test]
    fn a_declared_length_is_carried_so_a_value_can_fit_it() {
        // varchar(20) arrives as atttypmod 24: the length plus a four-byte
        // header. Writing 30 characters into it is a runtime error.
        assert_eq!(map("varchar", 24), ColumnType::Text { max_length: Some(20) });
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
            ColumnType::Numeric { precision: Some(10), scale: Some(2) }
        );
        assert_eq!(map("numeric", -1), ColumnType::Numeric { precision: None, scale: None });
    }

    #[test]
    fn an_unknown_type_keeps_its_name_for_the_refusal_message() {
        assert_eq!(
            map("geometry", -1),
            ColumnType::Unsupported { name: "geometry".into() }
        );
    }

    #[test]
    fn an_enum_carries_its_labels_in_declaration_order() {
        let mut enums = BTreeMap::new();
        enums.insert("status".to_string(), EnumType {
            qualified: Some("\"public\".\"status\"".into()),
            labels: vec!["pending".to_string(), "shipped".to_string()],
        });
        let t = map_type("status", "e", -1, 0, None, false, None, &enums);
        match t {
            ColumnType::Enum { name, labels, qualified } => {
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
        let t = map_type("email", "d", -1, 0, Some(("text", "b")), false, None, &no_enums());
        assert!(t.is_generatable());
        match &t {
            ColumnType::Domain { inner, has_constraint, .. } => {
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
        let t = map_type("positive", "d", -1, 0, Some(("int4", "b")), true, None, &no_enums());
        assert!(!t.is_generatable());
    }

    #[test]
    fn an_array_is_recognised_by_its_leading_underscore() {
        let t = map_type("_int4", "b", -1, 1, None, false, Some(("int4", "b")), &no_enums());
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
        let t = map_type("_geometry", "b", -1, 1, None, false, Some(("geometry", "b")), &no_enums());
        assert!(!t.is_generatable());
        assert_eq!(t.describe(), "geometry[]");
    }
}

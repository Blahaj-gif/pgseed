//! The test the whole project exists for.
//!
//! Generate, apply to a real Postgres, and let the database decide. There is
//! no assertion here about whether the data *looks* right, because that is not
//! a thing this tool claims. The claim is narrower and checkable: **every row
//! it writes satisfies every constraint the schema declares**, and Postgres is
//! the only authority on that.
//!
//! A rejected row is a test failure, not a warning. Nothing else in this suite
//! matters as much: a seed tool that inserts rows the real system would have
//! refused is worse than no seed tool, because everything downstream is then
//! tested against data that could not exist.

mod harness;

use harness::Db;
use pgsow::{classify, emit, graph, introspect};

/// Create the schema, generate for it, apply the result, and report what
/// Postgres made of it.
fn seed(ddl: &str, rows: usize) -> (Db, String) {
    let db = Db::start();
    db.apply(ddl);

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);
    let sql = emit::sql(&schema, &verdict, &emit::Options { seed: 1, rows });

    if let Err(e) = db.client().batch_execute(&sql) {
        panic!("Postgres rejected generated data — that is the whole failure this \
                project is about.\n\nerror: {e}\n\nsql:\n{sql}");
    }
    (db, sql)
}

fn count(db: &Db, table: &str) -> i64 {
    db.client()
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

#[test]
fn an_ordinary_schema_fills_and_the_database_accepts_every_row() {
    let (db, _) = seed(
        "CREATE TABLE users (
             id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             email      varchar(255) NOT NULL UNIQUE,
             nickname   text,
             active     boolean NOT NULL,
             created_at timestamptz NOT NULL DEFAULT now()
         );
         CREATE TABLE orders (
             id       int PRIMARY KEY,
             user_id  bigint NOT NULL REFERENCES users(id),
             total    numeric(10,2) NOT NULL,
             placed   date NOT NULL
         );",
        25,
    );
    assert_eq!(count(&db, "users"), 25);
    assert_eq!(count(&db, "orders"), 25);
}

#[test]
fn every_foreign_key_written_points_at_a_row_that_exists() {
    // Postgres would have rejected a dangling key outright, so this is really
    // asking a second question: did anything get quietly written as NULL to
    // dodge the constraint?
    let (db, _) = seed(
        "CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (
             id int PRIMARY KEY,
             user_id int NOT NULL REFERENCES users(id)
         );",
        30,
    );
    let orphans: i64 = db
        .client()
        .query_one(
            "SELECT count(*) FROM orders o
             WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.id = o.user_id)",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(orphans, 0);
    assert_eq!(count(&db, "orders"), 30);
}

#[test]
fn the_checks_it_claimed_to_satisfy_are_actually_satisfied() {
    // The strongest test here. Every one of these constraints was accepted by
    // `classify` on the strength of a promise made in `checks`, and Postgres
    // enforces all of them on insert. If any promise were empty, this fails.
    let (db, _) = seed(
        "CREATE TABLE things (
             id       int PRIMARY KEY,
             name     text NOT NULL,
             size     int NOT NULL,
             digest   bytea NOT NULL,
             note     text,
             CONSTRAINT name_length  CHECK (char_length(name) <= 12),
             CONSTRAINT size_positive CHECK (size > 0),
             CONSTRAINT digest_width CHECK (octet_length(digest) = 20),
             CONSTRAINT note_escape  CHECK ((note IS NULL) OR (char_length(note) <= 3))
         );",
        40,
    );
    assert_eq!(count(&db, "things"), 40);

    // And the nullable escape was actually taken, rather than the second
    // branch being satisfied by luck.
    let nulls: i64 = db
        .client()
        .query_one("SELECT count(*) FROM things WHERE note IS NULL", &[])
        .unwrap()
        .get(0);
    assert_eq!(nulls, 40, "the MustBeNull obligation was not honoured");
}

#[test]
fn a_composite_foreign_key_names_one_real_parent_row() {
    // Drawing each column of a composite key independently would name a pair
    // that never existed, and Postgres would reject it.
    let (db, _) = seed(
        "CREATE TABLE parent (
             tenant_id int, code text,
             PRIMARY KEY (tenant_id, code)
         );
         CREATE TABLE child (
             id int PRIMARY KEY,
             c_tenant int NOT NULL, c_code text NOT NULL,
             FOREIGN KEY (c_tenant, c_code) REFERENCES parent (tenant_id, code)
         );",
        20,
    );
    assert_eq!(count(&db, "child"), 20);
}

#[test]
fn enums_arrays_and_uuids_survive_the_round_trip() {
    let (db, _) = seed(
        "CREATE TYPE status AS ENUM ('pending', 'shipped', 'cancelled');
         CREATE TABLE parcels (
             id      uuid PRIMARY KEY,
             state   status NOT NULL,
             tags    text[] NOT NULL,
             weight  real NOT NULL,
             seen_at timestamp NOT NULL
         );",
        15,
    );
    assert_eq!(count(&db, "parcels"), 15);
}

#[test]
fn a_self_referencing_table_fills() {
    // `employees.manager_id -> employees` is ordinary, and the manager column
    // is nullable so the cycle breaks trivially.
    let (db, _) = seed(
        "CREATE TABLE employees (
             id         int PRIMARY KEY,
             name       text NOT NULL,
             manager_id int REFERENCES employees(id)
         );",
        10,
    );
    assert_eq!(count(&db, "employees"), 10);
}

#[test]
fn a_refused_table_is_left_completely_untouched() {
    // The doctrine, checked from the outside: a table this could not promise
    // to satisfy gets no rows at all, rather than a best effort.
    let (db, sql) = seed(
        "CREATE TABLE ok (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE targets (
             id int PRIMARY KEY,
             user_id int, group_id int,
             CONSTRAINT one_owner CHECK (num_nonnulls(user_id, group_id) = 1)
         );",
        10,
    );
    assert!(!sql.contains("targets"), "a refused table appeared in the SQL");
    assert_eq!(count(&db, "ok"), 10);
    assert_eq!(count(&db, "targets"), 0);
}

#[test]
fn the_same_seed_produces_the_same_database_twice() {
    let ddl = "CREATE TABLE users (
                   id int PRIMARY KEY,
                   email text NOT NULL UNIQUE,
                   score numeric(6,2) NOT NULL
               );";
    let (_a, first) = seed(ddl, 20);
    let (_b, second) = seed(ddl, 20);
    assert_eq!(first, second, "the same seed produced different SQL");
}

#[test]
fn network_types_and_a_lowercase_constraint_round_trip() {
    // PowerDNS in miniature: the schema that scored 29% because of one `inet`
    // column and four copies of a lowercase rule. Postgres is strict about
    // `cidr` in particular — the host bits must be zero — so this fails loudly
    // if the generator is careless.
    let (db, _) = seed(
        "CREATE TABLE hosts (
             id      int PRIMARY KEY,
             name    varchar(255) NOT NULL,
             ip      inet NOT NULL,
             subnet  cidr NOT NULL,
             mac     macaddr NOT NULL,
             CONSTRAINT c_lowercase_name CHECK (((name)::text = lower((name)::text)))
         );",
        20,
    );
    assert_eq!(count(&db, "hosts"), 20);
}

#[test]
fn applying_directly_writes_the_same_rows_the_sql_would_have() {
    // Two ways in, one implementation behind both. If they drifted, the one
    // nobody tested would be the one somebody ran.
    let ddl = "CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL UNIQUE);
               CREATE TABLE orders (
                   id int PRIMARY KEY,
                   user_id int NOT NULL REFERENCES users(id)
               );";
    let db = Db::start();
    db.apply(ddl);

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);
    let options = emit::Options { seed: 3, rows: 12 };

    emit::apply(&mut client, &schema, &verdict, &options).expect("apply failed");
    assert_eq!(count(&db, "users"), 12);
    assert_eq!(count(&db, "orders"), 12);
}

#[test]
fn a_failed_apply_leaves_the_database_exactly_as_it_was() {
    // All or nothing. A seed that stopped halfway leaves a database that looks
    // populated and is not — the failure this tool is shaped against.
    let db = Db::start();
    db.apply("CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL UNIQUE);");

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);

    // A row already sitting on the primary key this is about to generate.
    db.client()
        .batch_execute("INSERT INTO users (id, email) VALUES (0, 'taken@example.com');")
        .unwrap();

    let failed = emit::apply(&mut client, &schema, &verdict, &emit::Options { seed: 1, rows: 5 });
    assert!(failed.is_err(), "a primary key collision should have failed");
    assert_eq!(count(&db, "users"), 1, "the transaction did not roll back");
}

#[test]
fn a_cycle_broken_with_a_null_is_filled_in_afterwards() {
    // Breaking the cycle is not the end of the job. A `manager_id` that is
    // null on every row is valid and has modelled nothing.
    let db = Db::start();
    db.apply(
        "CREATE TABLE employees (
             id         int PRIMARY KEY,
             name       text NOT NULL,
             manager_id int REFERENCES employees(id)
         );",
    );

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);
    emit::apply(&mut client, &schema, &verdict, &emit::Options { seed: 1, rows: 10 }).unwrap();

    let with_manager: i64 = db.client()
        .query_one("SELECT count(*) FROM employees WHERE manager_id IS NOT NULL", &[])
        .unwrap()
        .get(0);
    assert_eq!(with_manager, 9, "every row but the root should report to somebody");

    // And exactly one root, or it is a closed loop rather than a hierarchy.
    let roots: i64 = db.client()
        .query_one("SELECT count(*) FROM employees WHERE manager_id IS NULL", &[])
        .unwrap()
        .get(0);
    assert_eq!(roots, 1);
}

/// A unique column whose type has fewer values than the rows asked for.
///
/// Not a hypothetical: a `bool` holds two values and an enum holds as many as
/// it was declared with. Asking for fifty distinct ones is not a hard problem,
/// it is an impossible one, and the only honest answers are to write fewer
/// rows or to refuse the table. Writing fifty and letting the database throw
/// the lot out is the one answer that is definitely wrong.
#[test]
fn a_unique_column_cannot_hold_more_values_than_its_type_has() {
    let (db, _) = seed(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');
         CREATE TABLE flags (id int PRIMARY KEY, on_off bool UNIQUE);
         CREATE TABLE moods (id int PRIMARY KEY, m mood UNIQUE);",
        20,
    );
    // Exactly the domain, not merely no more than it — `<=` would also pass
    // on a table it gave up on and left empty, which is a different answer
    // wearing the same number.
    assert_eq!(count(&db, "flags"), 2, "a bool holds two rows, both of them");
    assert_eq!(count(&db, "moods"), 3, "an enum of three labels holds three");
}

/// A join table can hold at most as many rows as there are pairs to join.
#[test]
fn a_join_table_cannot_have_more_rows_than_the_pairs_available() {
    let (db, _) = seed(
        "CREATE TABLE users (id int PRIMARY KEY);
         CREATE TABLE roles (id int PRIMARY KEY, name bool UNIQUE);
         CREATE TABLE grants (
             user_id int REFERENCES users(id),
             role_id int REFERENCES roles(id),
             PRIMARY KEY (user_id, role_id)
         );",
        20,
    );
    // `roles.name` is a unique bool, so roles caps itself at 2 and that cap
    // travels: 20 users x 2 roles is 40 pairs, of which 20 were asked for.
    assert_eq!(count(&db, "users"), 20);
    assert_eq!(count(&db, "roles"), 2, "the cap on a parent is the point");
    assert_eq!(count(&db, "grants"), 20, "20 asked for, 40 available");

    // And they are 20 *distinct* pairs, which is the thing the odometer buys.
    let distinct: i64 = db
        .client()
        .query_one("SELECT count(*) FROM (SELECT DISTINCT user_id, role_id FROM grants) d", &[])
        .unwrap()
        .get(0);
    assert_eq!(distinct, 20, "the pairs repeated");
}

/// A composite unique key has to be distinct as a tuple.
///
/// Only single-column keys were ever handled, so `PRIMARY KEY (first, last)`
/// drew both names from a sixteen-word list and collided almost at once. Taken
/// straight from PostgREST's test schema, where it did.
#[test]
fn a_composite_key_does_not_repeat_a_pair() {
    let (db, _) = seed(
        "CREATE TABLE employees (
             first_name text,
             last_name  text,
             salary     numeric,
             PRIMARY KEY (first_name, last_name)
         );
         CREATE TABLE account_data (
             user_id           text NOT NULL,
             account_data_type text NOT NULL,
             content           text,
             CONSTRAINT account_data_uniqueness UNIQUE (user_id, account_data_type)
         );",
        30,
    );
    assert_eq!(count(&db, "employees"), 30);
    assert_eq!(count(&db, "account_data"), 30);
}

/// A constant default cannot serve a unique constraint.
///
/// `resource_version INTEGER NOT NULL DEFAULT 1 UNIQUE` is Hasura's, and
/// leaving it to its default gave every row the value 1. A sequence default is
/// a different matter and must still be left alone, or the sequence and the
/// rows disagree about what has been used.
#[test]
fn a_constant_default_under_a_unique_key_is_written_rather_than_defaulted() {
    let (db, sql) = seed(
        "CREATE TABLE metadata (
             id               serial PRIMARY KEY,
             resource_version integer NOT NULL DEFAULT 1 UNIQUE,
             payload          text
         );",
        6,
    );
    assert_eq!(count(&db, "metadata"), 6);
    assert!(sql.contains("resource_version"), "it was left to the default");
    assert!(!sql.contains("\"id\""), "a sequence default should be left alone: {sql}");

    // The sequence and the rows must still agree, or the next real insert
    // collides with a row this wrote.
    let next: i32 = db
        .client()
        .query_one("SELECT nextval('metadata_id_seq')::int", &[])
        .unwrap()
        .get(0);
    assert!(next > 6, "the sequence fell behind the rows: {next}");
}

/// Arrays of things that are not text.
///
/// `ARRAY['x']` is a `text[]` from the moment it is written, and `text[]` does
/// not implicitly become `jsonb[]` or `inet[]`. Three of the nine real schemas
/// were rejected on exactly this. The enum and domain cases are in here not
/// because they are known to work but because this is where it gets found out.
#[test]
fn an_array_carries_the_type_of_what_is_in_it() {
    let (db, _) = seed(
        "CREATE TYPE mood AS ENUM ('sad', 'ok');
         CREATE DOMAIN short AS text;
         CREATE TABLE arrays (
             id      int PRIMARY KEY,
             blobs   jsonb[],
             docs    json[],
             hosts   inet[],
             nets    cidr[],
             stamps  timestamptz[],
             ids     uuid[],
             counts  bigint[],
             moods   mood[],
             notes   short[]
         );",
        4,
    );
    assert_eq!(count(&db, "arrays"), 4);
}

/// An exclusion constraint is a rule, whether or not it is read.
///
/// GitLab builds `daterange(start_date, due_date)` inside an EXCLUDE and this
/// generated the two dates independently, so half the time the range came out
/// backwards and Postgres refused it. The constraint was never read at all —
/// `contype = 'x'` was not in the query — and a constraint that is not seen is
/// not thereby satisfied.
#[test]
fn an_exclusion_constraint_refuses_its_table_rather_than_guessing() {
    let db = Db::start();
    db.apply(
        "CREATE EXTENSION IF NOT EXISTS btree_gist;
         CREATE TABLE sprints (
             id         int PRIMARY KEY,
             group_id   int NOT NULL,
             start_date date,
             due_date   date,
             EXCLUDE USING gist (
                 group_id WITH =,
                 daterange(start_date, due_date, '[]'::text) WITH &&
             )
         );
         CREATE TABLE plain (id int PRIMARY KEY);",
    );

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);

    assert!(verdict.is_refused(&pgsow::schema::TableId::new("public", "sprints")));
    assert_eq!(verdict.fillable.len(), 1, "the plain table is untouched by this");

    let (_, reasons) = verdict.refused.iter().find(|(t, _)| t.name == "sprints").unwrap();
    let text = reasons[0].explain();
    assert!(text.contains("EXCLUDE"), "the rule should be quoted back: {text}");
}

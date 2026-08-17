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

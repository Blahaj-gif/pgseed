//! Three things that are only worth knowing if they are known at the edges.
//!
//! The corpus measures *reach* on schemas nobody here wrote, which is the
//! honest number and a blunt one: a construct appearing twice in twenty
//! schemas is one schema away from being untested, and a construct appearing
//! never is tested only by DDL this project wrote. These are the deliberate
//! exception. They are hand-written on purpose, and they earn it by testing
//! shapes the corpus is *thin* on rather than shapes it covers well.
//!
//! Each of the three answers a question that a percentage cannot:
//!
//! 1. **All-or-nothing constraints.** A rule where a row is either wholly
//!    right or wholly wrong, with no partial credit and no near miss. These
//!    are where a "mostly correct" generator does its quietest damage.
//! 2. **Circular foreign keys at scale.** Two tables in a ring is a unit test.
//!    Ten is a different question, and so are two rings sharing a table.
//! 3. **Determinism, including under `--probe`.** Probing consults a live
//!    database, so it is the one part of this tool whose output could depend on
//!    something other than the seed. If it does, the promise is broken.

mod harness;

use harness::Db;
use pgsow::{classify, emit, graph, introspect};

/// Read a schema, classify it, and produce the SQL, without applying it.
fn plan(db: &Db, rows: usize) -> (pgsow::schema::Schema, classify::Verdict, String) {
    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);
    let sql = emit::sql(&schema, &verdict, &emit::Options::flat(1, rows));
    (schema, verdict, sql)
}

fn refused(verdict: &classify::Verdict, name: &str) -> bool {
    verdict.refused.iter().any(|(id, _)| id.name == name)
}

fn fillable(verdict: &classify::Verdict, name: &str) -> bool {
    verdict.fillable.iter().any(|id| id.name == name)
}

fn count(db: &Db, table: &str) -> i64 {
    db.client()
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

// ===========================================================================
// 1. All-or-nothing constraints
// ===========================================================================

#[test]
fn every_spelling_of_exactly_one_is_satisfied_and_the_database_agrees() {
    // Four ways of writing the same obligation, one of which this project
    // only learned to read because Plausible refused a table over it. If any
    // of them were merely *believed* to be satisfied, the database says so
    // here rather than in somebody's staging environment.
    let db = Db::start();
    db.apply(
        "CREATE TABLE by_function (
             id int PRIMARY KEY,
             a text, b text,
             CONSTRAINT by_function_one CHECK (num_nonnulls(a, b) = 1)
         );
         CREATE TABLE by_inequality (
             id int PRIMARY KEY,
             a text, b text,
             CONSTRAINT by_inequality_one CHECK ((a IS NULL) <> (b IS NULL))
         );
         CREATE TABLE by_longhand (
             id int PRIMARY KEY,
             a text, b text,
             CONSTRAINT by_longhand_one CHECK (
                 ((a IS NOT NULL) AND (b IS NULL)) OR ((a IS NULL) AND (b IS NOT NULL)))
         );
         CREATE TABLE by_longhand_three (
             id int PRIMARY KEY,
             a text, b text, c text,
             CONSTRAINT by_longhand_three_one CHECK (
                 ((a IS NOT NULL) AND (b IS NULL) AND (c IS NULL)) OR
                 ((b IS NOT NULL) AND (a IS NULL) AND (c IS NULL)) OR
                 ((c IS NOT NULL) AND (a IS NULL) AND (b IS NULL)))
         );",
    );

    let (_, verdict, sql) = plan(&db, 6);
    for table in [
        "by_function",
        "by_inequality",
        "by_longhand",
        "by_longhand_three",
    ] {
        assert!(fillable(&verdict, table), "{table} was refused");
    }
    db.client()
        .batch_execute(&sql)
        .expect("Postgres accepts it");
    assert_eq!(count(&db, "by_longhand_three"), 6);
}

#[test]
fn an_all_or_nothing_rule_with_no_satisfying_row_is_refused_rather_than_attempted() {
    // Each of these is a rule that cannot be met, and the failure mode being
    // guarded against is filling the table anyway because each *part* looked
    // familiar.
    let db = Db::start();
    db.apply(
        // Exactly one of two columns, both of which the catalogue insists on.
        "CREATE TABLE impossible_pair (
             id int PRIMARY KEY,
             a text NOT NULL, b text NOT NULL,
             CONSTRAINT impossible_pair_one CHECK (num_nonnulls(a, b) = 1)
         );
         -- A floor above a ceiling: twelve characters into eight.
         CREATE TABLE impossible_width (
             id int PRIMARY KEY,
             code varchar(8) NOT NULL,
             CONSTRAINT impossible_width_min CHECK (char_length(code) >= 12)
         );
         -- Not a complete cover: `c` may never hold the value, which is a
         -- narrower rule than exactly-one and not one this can satisfy.
         CREATE TABLE partial_cover (
             id int PRIMARY KEY,
             a text, b text, c text,
             CONSTRAINT partial_cover_one CHECK (
                 ((a IS NOT NULL) AND (b IS NULL) AND (c IS NULL)) OR
                 ((b IS NOT NULL) AND (a IS NULL) AND (c IS NULL)))
         );
         -- An exclusion constraint is a check by another name and is refused.
         -- One column only: `room WITH =` would need btree_gist, and the point
         -- here is the constraint rather than the operator class.
         CREATE TABLE booked (
             id int PRIMARY KEY,
             during tsrange NOT NULL,
             EXCLUDE USING gist (during WITH &&)
         );",
    );

    let (_, verdict, sql) = plan(&db, 5);
    for table in [
        "impossible_pair",
        "impossible_width",
        "partial_cover",
        "booked",
    ] {
        assert!(refused(&verdict, table), "{table} should have been refused");
        assert!(
            !sql.contains(&format!("\"{table}\"")),
            "{table} was refused and written anyway"
        );
    }
}

#[test]
fn a_partial_unique_index_is_at_least_as_strict_as_the_real_one() {
    // `UNIQUE ... WHERE deleted_at IS NULL` constrains only some rows. Reading
    // the predicate is not attempted; the index is treated as a plain unique
    // key, which is *stricter* than the truth and therefore safe. Worth an
    // explicit test, because the direction of that error is the whole reason
    // it is acceptable, and a later change that made it looser would not fail
    // anything else.
    let db = Db::start();
    db.apply(
        "CREATE TABLE accounts (
             id         int PRIMARY KEY,
             email      text NOT NULL,
             deleted_at timestamptz
         );
         CREATE UNIQUE INDEX accounts_live_email ON accounts (email) WHERE deleted_at IS NULL;",
    );
    let (_, verdict, sql) = plan(&db, 20);
    assert!(fillable(&verdict, "accounts"));
    db.client().batch_execute(&sql).unwrap();
    assert_eq!(
        db.client()
            .query_one("SELECT count(DISTINCT email) FROM accounts", &[])
            .unwrap()
            .get::<_, i64>(0),
        20,
        "every email distinct, which satisfies the partial index outright"
    );
}

// ===========================================================================
// 2. Circular foreign keys, at scale
// ===========================================================================

/// A ring of `n` tables, each pointing at the next, with the closing key
/// nullable so the cycle can be broken.
fn ring(n: usize, nullable_close: bool) -> String {
    let mut ddl = String::new();
    for i in 0..n {
        ddl.push_str(&format!(
            "CREATE TABLE ring{i} (id int PRIMARY KEY, next_id int{});\n",
            if i + 1 == n && !nullable_close {
                " NOT NULL"
            } else {
                ""
            }
        ));
    }
    for i in 0..n {
        ddl.push_str(&format!(
            "ALTER TABLE ring{i} ADD CONSTRAINT ring{i}_next FOREIGN KEY (next_id) \
             REFERENCES ring{}(id);\n",
            (i + 1) % n
        ));
    }
    ddl
}

#[test]
fn a_ring_of_ten_tables_is_broken_filled_and_then_repaired() {
    // Two tables in a cycle is a unit test. Ten is a different question: the
    // break has to happen once rather than ten times, the order has to be
    // stable, and the nulls left behind have to be filled in afterwards. A
    // `manager_id` that is null on every row is valid SQL and has modelled
    // nothing.
    let db = Db::start();
    db.apply(&ring(10, true));

    let (_, verdict, sql) = plan(&db, 8);
    assert_eq!(verdict.refused.len(), 0, "a nullable ring is breakable");
    assert_eq!(verdict.fillable.len(), 10);
    db.client()
        .batch_execute(&sql)
        .expect("Postgres accepts it");

    for i in 0..10 {
        assert_eq!(count(&db, &format!("ring{i}")), 8);
        let unfilled: i64 = db
            .client()
            .query_one(
                &format!("SELECT count(*) FROM ring{i} WHERE next_id IS NULL"),
                &[],
            )
            .unwrap()
            .get(0);
        assert!(
            unfilled < 8,
            "ring{i} was left entirely null, so the cycle was broken and never repaired"
        );
    }
}

#[test]
fn a_rigid_ring_is_refused_and_says_which_constraint_would_have_to_change() {
    // Every key NOT NULL and none deferrable: no order of single-row inserts
    // satisfies it, and the right answer is to say so and name the thing a
    // person could change.
    let db = Db::start();
    let mut ddl = String::new();
    for i in 0..4 {
        ddl.push_str(&format!(
            "CREATE TABLE rigid{i} (id int PRIMARY KEY, next_id int NOT NULL);\n"
        ));
    }
    for i in 0..4 {
        ddl.push_str(&format!(
            "ALTER TABLE rigid{i} ADD CONSTRAINT rigid{i}_next FOREIGN KEY (next_id) \
             REFERENCES rigid{}(id);\n",
            (i + 1) % 4
        ));
    }
    db.apply(&ddl);

    let (_, verdict, sql) = plan(&db, 5);
    assert_eq!(verdict.fillable.len(), 0);
    assert_eq!(verdict.refused.len(), 4);
    let explained = verdict
        .refused
        .iter()
        .flat_map(|(_, reasons)| reasons.iter().map(|r| r.explain()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        explained.contains("rigid"),
        "the refusal must name a constraint to change: {explained}"
    );
    assert!(!sql.contains("INSERT INTO \"public\".\"rigid"));
}

#[test]
fn a_deferrable_ring_is_filled_by_deferring_rather_than_by_nulling() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE left_side  (id int PRIMARY KEY, other int NOT NULL);
         CREATE TABLE right_side (id int PRIMARY KEY, other int NOT NULL);
         ALTER TABLE left_side  ADD CONSTRAINT left_other FOREIGN KEY (other)
             REFERENCES right_side(id) DEFERRABLE INITIALLY IMMEDIATE;
         ALTER TABLE right_side ADD CONSTRAINT right_other FOREIGN KEY (other)
             REFERENCES left_side(id) DEFERRABLE INITIALLY IMMEDIATE;",
    );
    let (_, verdict, sql) = plan(&db, 6);
    assert_eq!(verdict.refused.len(), 0);
    assert!(sql.contains("SET CONSTRAINTS ALL DEFERRED"));
    db.client()
        .batch_execute(&sql)
        .expect("Postgres accepts it");
    assert_eq!(count(&db, "left_side"), 6);
    assert_eq!(count(&db, "right_side"), 6);
}

#[test]
fn two_rings_sharing_a_table_are_both_broken() {
    // A table in two cycles at once. Breaking one ring must not leave the
    // other unbroken, and the shared table must be written exactly once.
    let db = Db::start();
    db.apply(
        "CREATE TABLE hub (id int PRIMARY KEY, a_id int, b_id int);
         CREATE TABLE spoke_a (id int PRIMARY KEY, hub_id int NOT NULL);
         CREATE TABLE spoke_b (id int PRIMARY KEY, hub_id int NOT NULL);
         ALTER TABLE spoke_a ADD CONSTRAINT spoke_a_hub FOREIGN KEY (hub_id) REFERENCES hub(id);
         ALTER TABLE spoke_b ADD CONSTRAINT spoke_b_hub FOREIGN KEY (hub_id) REFERENCES hub(id);
         ALTER TABLE hub ADD CONSTRAINT hub_a FOREIGN KEY (a_id) REFERENCES spoke_a(id);
         ALTER TABLE hub ADD CONSTRAINT hub_b FOREIGN KEY (b_id) REFERENCES spoke_b(id);",
    );
    let (_, verdict, sql) = plan(&db, 5);
    assert_eq!(verdict.refused.len(), 0, "both rings have a nullable side");
    assert_eq!(sql.matches("INSERT INTO \"public\".\"hub\"").count(), 1);
    db.client()
        .batch_execute(&sql)
        .expect("Postgres accepts it");
    assert_eq!(count(&db, "hub"), 5);
    assert_eq!(count(&db, "spoke_a"), 5);
    assert_eq!(count(&db, "spoke_b"), 5);
}

#[test]
fn a_table_that_points_at_itself_fills_without_pointing_at_a_row_that_is_not_there() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE staff (
             id         int PRIMARY KEY,
             manager_id int REFERENCES staff(id),
             full_name  text NOT NULL
         );",
    );
    let (_, verdict, sql) = plan(&db, 12);
    assert!(fillable(&verdict, "staff"));
    db.client().batch_execute(&sql).unwrap();
    let orphans: i64 = db
        .client()
        .query_one(
            "SELECT count(*) FROM staff s WHERE s.manager_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM staff p WHERE p.id = s.manager_id)",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(orphans, 0);
}

// ===========================================================================
// 3. Determinism, including under probing
// ===========================================================================

const SHAPED_LIKE_AN_APPLICATION: &str = "
    CREATE TABLE companies (
        id   serial PRIMARY KEY,
        name text NOT NULL,
        country_code char(2) NOT NULL
    );
    CREATE TABLE users (
        id         serial PRIMARY KEY,
        company_id int NOT NULL REFERENCES companies(id),
        email      varchar(255) NOT NULL UNIQUE,
        first_name text NOT NULL,
        last_name  text NOT NULL
    );
    CREATE TABLE tickets (
        id      serial PRIMARY KEY,
        user_id int NOT NULL REFERENCES users(id),
        slug    text NOT NULL CONSTRAINT tickets_slug_shape CHECK (slug ~ '^[a-z-]+[0-9]*$'),
        title   text NOT NULL
    );
";

#[test]
fn the_same_seed_gives_the_same_bytes_on_a_schema_with_names_in_it() {
    // Determinism was tested before the column names were read. Reading them
    // added a second source of values, and a second source is a second thing
    // that could drift.
    let db = Db::start();
    db.apply(SHAPED_LIKE_AN_APPLICATION);
    let (_, _, first) = plan(&db, 25);
    let (_, _, again) = plan(&db, 25);
    assert_eq!(first, again);
    assert!(
        first.contains('@'),
        "the names really were read: {first:.400}"
    );
}

#[test]
fn probing_twice_reaches_the_same_verdict_and_writes_the_same_rows() {
    // The one part of this tool whose answer could depend on something other
    // than the seed, because it consults a live database. Two fresh databases,
    // the same schema, the same seed: the rescued set and the accepted SQL
    // have to match, or `--probe` has quietly given up determinism in exchange
    // for reach.
    let run = || {
        let db = Db::start();
        db.apply(SHAPED_LIKE_AN_APPLICATION);
        let mut client = db.client();
        let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
        let order = graph::order(&schema);
        let verdict = classify::classify(&schema, &order);
        let mut sql = String::new();
        let outcome = pgsow::probe::run(
            &mut client,
            &schema,
            &verdict,
            &order,
            &emit::Options::flat(1, 15),
            false,
            &mut |statement| {
                sql.push_str(statement);
                sql.push('\n');
            },
        )
        .expect("nothing understood should fail");
        let rescued: Vec<String> = outcome.rescued.iter().map(|id| id.name.clone()).collect();
        (rescued, sql)
    };

    let (first_rescued, first_sql) = run();
    let (again_rescued, again_sql) = run();
    assert_eq!(first_rescued, again_rescued);
    assert_eq!(first_sql, again_sql);
    // And the regular expression really was outside the closed set, so this
    // is testing a rescue rather than a table that was fillable all along.
    assert_eq!(first_rescued, vec!["tickets".to_string()]);
}

#[test]
fn probing_writes_the_understood_rows_exactly_as_a_plain_apply_would() {
    // `--probe` must be a strict superset. If it changed what the understood
    // tables hold, the two reach numbers would not be comparable and the
    // report would be putting two different measurements side by side.
    let plain = {
        let db = Db::start();
        db.apply(SHAPED_LIKE_AN_APPLICATION);
        let (_, _, sql) = plan(&db, 15);
        sql.lines()
            .filter(|line| line.trim_start().starts_with('('))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };

    let probed = {
        let db = Db::start();
        db.apply(SHAPED_LIKE_AN_APPLICATION);
        let mut client = db.client();
        let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
        let order = graph::order(&schema);
        let verdict = classify::classify(&schema, &order);
        let mut sql = String::new();
        pgsow::probe::run(
            &mut client,
            &schema,
            &verdict,
            &order,
            &emit::Options::flat(1, 15),
            false,
            &mut |statement| {
                sql.push_str(statement);
                sql.push('\n');
            },
        )
        .unwrap();
        sql.lines()
            .filter(|line| line.trim_start().starts_with('('))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };

    // Everything the plain run wrote is in the probed run, in the same form.
    for row in &plain {
        assert!(
            probed.contains(row),
            "probing changed a row the plain run wrote:\n{row}"
        );
    }
    assert!(probed.len() > plain.len(), "probing should add rows");
}

#[test]
fn a_single_row_lock_table_holds_exactly_one_row() {
    // Synapse's idiom, and the shape that caught the closed set out. A column
    // constrained to one value and made unique can hold exactly one row, so
    // understanding the constraint is only half the job — the row count has to
    // follow from it, or the tool asks for five rows and the database refuses
    // the second.
    let db = Db::start();
    db.apply(
        "CREATE TABLE stream_position (
             lock            character(1) DEFAULT 'X'::bpchar NOT NULL,
             stream_ordering bigint,
             CONSTRAINT stream_position_lock_check CHECK ((lock = 'X'::bpchar))
         );
         ALTER TABLE ONLY stream_position
             ADD CONSTRAINT stream_position_lock_key UNIQUE (lock);",
    );
    let (_, verdict, sql) = plan(&db, 5);
    assert!(fillable(&verdict, "stream_position"));
    db.client()
        .batch_execute(&sql)
        .expect("Postgres accepts it");
    assert_eq!(count(&db, "stream_position"), 1);
}

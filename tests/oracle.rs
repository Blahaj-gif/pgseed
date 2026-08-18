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
    let sql = emit::sql(&schema, &verdict, &emit::Options::flat(1, rows));

    if let Err(e) = db.client().batch_execute(&sql) {
        panic!(
            "Postgres rejected generated data — that is the whole failure this \
                project is about.\n\nerror: {e}\n\nsql:\n{sql}"
        );
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
        // A regular expression: a real rule with no closed form here, short of
        // the expression evaluator this deliberately does not have. It was
        // num_nonnulls until that shape joined the closed set, at which point
        // this test stopped being about a refused table and said so.
        "CREATE TABLE ok (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE targets (
             id int PRIMARY KEY,
             code text NOT NULL,
             CONSTRAINT one_owner CHECK ((code ~ '^[A-Z]{3}-[0-9]{4}$'))
         );",
        10,
    );
    assert!(
        !sql.contains("targets"),
        "a refused table appeared in the SQL"
    );
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
    let options = emit::Options::flat(3, 12);

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

    let failed = emit::apply(&mut client, &schema, &verdict, &emit::Options::flat(1, 5));
    assert!(
        failed.is_err(),
        "a primary key collision should have failed"
    );
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
    emit::apply(&mut client, &schema, &verdict, &emit::Options::flat(1, 10)).unwrap();

    let with_manager: i64 = db
        .client()
        .query_one(
            "SELECT count(*) FROM employees WHERE manager_id IS NOT NULL",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        with_manager, 9,
        "every row but the root should report to somebody"
    );

    // And exactly one root, or it is a closed loop rather than a hierarchy.
    let roots: i64 = db
        .client()
        .query_one(
            "SELECT count(*) FROM employees WHERE manager_id IS NULL",
            &[],
        )
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
    assert_eq!(
        count(&db, "flags"),
        2,
        "a bool holds two rows, both of them"
    );
    assert_eq!(
        count(&db, "moods"),
        3,
        "an enum of three labels holds three"
    );
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
        .query_one(
            "SELECT count(*) FROM (SELECT DISTINCT user_id, role_id FROM grants) d",
            &[],
        )
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
    assert!(
        sql.contains("resource_version"),
        "it was left to the default"
    );
    assert!(
        !sql.contains("\"id\""),
        "a sequence default should be left alone: {sql}"
    );

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
    assert_eq!(
        verdict.fillable.len(),
        1,
        "the plain table is untouched by this"
    );

    let (_, reasons) = verdict
        .refused
        .iter()
        .find(|(t, _)| t.name == "sprints")
        .unwrap();
    let text = reasons[0].explain();
    assert!(
        text.contains("EXCLUDE"),
        "the rule should be quoted back: {text}"
    );
}

/// The speed gate, pre-registered: fourteen tables at fifty rows, under 2s.
///
/// Measured over generation only. Starting a Postgres and running the DDL is
/// not what the gate was about, and folding it in would measure the harness.
#[test]
fn fourteen_tables_at_fifty_rows_generate_inside_the_budget() {
    let ddl = std::fs::read_to_string("tests/speed.sql").expect("the fixture");
    let db = Db::start();
    db.apply(&ddl);

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    assert_eq!(schema.len(), 14, "the gate is about fourteen tables");

    let started = std::time::Instant::now();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);
    let sql = emit::sql(&schema, &verdict, &emit::Options::flat(1, 50));
    let elapsed = started.elapsed();

    println!(
        "  14 tables x 50 rows: {:?} for {} bytes",
        elapsed,
        sql.len()
    );
    assert!(
        elapsed.as_secs_f64() < 2.0,
        "the gate is 2s, this took {elapsed:?}"
    );

    // And the output still has to be real, or the timing measured nothing.
    db.client()
        .batch_execute(&sql)
        .expect("its own output did not run");
    assert_eq!(count(&db, "order_items"), 50);
}

/// Determinism, now that a row count can depend on another table.
///
/// The per-cell stream was the answer to this: a value comes from
/// hash(seed, table, column, row) rather than a running stream, so adding a
/// table cannot shift values elsewhere. That was true when every table got a
/// flat fifty. It needs re-asking now that `volume` lets one table's row count
/// depend on its parents — the promise is worth only as much as its last test.
#[test]
fn adding_a_table_does_not_disturb_the_ones_already_there() {
    let base = "CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL UNIQUE);
                CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL REFERENCES users(id));";

    let render = |ddl: &str| {
        let db = Db::start();
        db.apply(ddl);
        let mut client = db.client();
        let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
        let order = graph::order(&schema);
        let verdict = classify::classify(&schema, &order);
        emit::sql(&schema, &verdict, &emit::Options::flat(7, 12))
    };

    let before = render(base);
    let after = render(&format!(
        "{base} CREATE TABLE unrelated (id int PRIMARY KEY, note text);"
    ));

    let users_before = before
        .lines()
        .skip_while(|l| !l.contains("\"users\""))
        .take(13);
    let users_after = after
        .lines()
        .skip_while(|l| !l.contains("\"users\""))
        .take(13);
    assert!(
        users_before.eq(users_after),
        "adding an unrelated table moved the values in users"
    );

    // And the same input twice is the same bytes, which is the simpler half.
    assert_eq!(render(base), before, "two runs at one seed differed");
}

/// A unique column too narrow to hold the rows asked for.
///
/// `varying_columns` picks a column whose type has no small domain, and text
/// qualifies — but `varchar(4)` is text with four characters to play with, and
/// "alpha-37" truncated to four is "alph" every time. `volume` does not treat
/// a length limit as a bound, so nothing caps the row count either. Whether
/// this is a real hole is a question with an answer, so here it is asked.
#[test]
fn a_narrow_unique_column_is_not_quietly_overfilled() {
    let (db, _) = seed(
        "CREATE TABLE codes (id int PRIMARY KEY, code varchar(4) NOT NULL UNIQUE);
         CREATE TABLE tight (id int PRIMARY KEY, c varchar(1) NOT NULL UNIQUE);
         CREATE TABLE checked (
             id int PRIMARY KEY,
             c  text NOT NULL UNIQUE,
             CONSTRAINT c_short CHECK ((char_length(c) <= 2))
         );",
        50,
    );
    // Four base-36 characters hold well over fifty values, so all fifty fit.
    assert_eq!(count(&db, "codes"), 50);
    // One character holds 36, and asking for fifty is the impossible question.
    assert_eq!(
        count(&db, "tight"),
        36,
        "a single character holds 36 of them"
    );
    // The same limit written as a CHECK is the same limit.
    assert_eq!(count(&db, "checked"), 50, "36 * 36 leaves room for fifty");
}

/// The shapes the closed set was widened to, judged by Postgres.
///
/// Each was chosen from a survey of what the nine real schemas actually write
/// rather than from what seemed likely: `num_nonnulls(...) = 1` was 78 of the
/// 277 constraints this did not understand, `jsonb_typeof` 53, a byte ceiling
/// 37, an array length 19.
#[test]
fn the_widened_check_shapes_produce_rows_the_database_keeps() {
    let (db, sql) = seed(
        "CREATE TABLE owned (
             id           int PRIMARY KEY,
             group_id     int,
             project_id   int,
             CONSTRAINT one_owner CHECK ((num_nonnulls(group_id, project_id) = 1))
         );
         CREATE TABLE triple (
             id              int PRIMARY KEY,
             namespace_id    int,
             organization_id int,
             project_id      int,
             CONSTRAINT one_of_three
                 CHECK ((num_nonnulls(namespace_id, organization_id, project_id) = 1))
         );
         CREATE TABLE shapes (
             id      int PRIMARY KEY,
             filter  jsonb NOT NULL,
             items   jsonb NOT NULL,
             label   jsonb NOT NULL,
             CONSTRAINT f_obj CHECK ((jsonb_typeof(filter) = 'object'::text)),
             CONSTRAINT i_arr CHECK ((jsonb_typeof(items) = 'array'::text)),
             CONSTRAINT l_str CHECK ((jsonb_typeof(label) = 'string'::text))
         );
         CREATE TABLE widths (
             id    int PRIMARY KEY,
             iv    bytea NOT NULL,
             note  text NOT NULL,
             tags  text[],
             CONSTRAINT iv_max   CHECK ((octet_length(iv) <= 12)),
             CONSTRAINT note_max CHECK ((octet_length(note) <= 6)),
             CONSTRAINT tag_max  CHECK ((cardinality(tags) <= 20))
         );",
        20,
    );
    for table in ["owned", "triple", "shapes", "widths"] {
        assert_eq!(count(&db, table), 20, "{table}");
    }

    // Postgres accepted them, so the constraints hold. This also checks the
    // *choice* was made where it was supposed to be: the first column carries
    // the value and the rest are null, rather than all of them being null and
    // the constraint happening to be satisfied some other way.
    let filled: i64 = db
        .client()
        .query_one("SELECT count(*) FROM owned WHERE group_id IS NOT NULL", &[])
        .unwrap()
        .get(0);
    assert_eq!(filled, 20, "the chosen column should hold the value");
    assert!(sql.contains("NULL"), "the others should be written null");
}

/// A group that cannot have exactly one non-null is refused, not attempted.
#[test]
fn two_columns_that_both_must_hold_a_value_cannot_have_one_between_them() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE impossible (
             id int PRIMARY KEY,
             a  int NOT NULL,
             b  int NOT NULL,
             CONSTRAINT one CHECK ((num_nonnulls(a, b) = 1))
         );",
    );
    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);
    assert!(
        verdict.fillable.is_empty(),
        "there is no choice to make here"
    );
    assert!(verdict.refused[0].1[0].explain().contains("num_nonnulls"));
}

/// A composite key where no single column has room to carry it.
///
/// The rule was "one column of the tuple varies and that makes the tuple
/// distinct", which needs a column with room to spare. A boolean and a
/// three-label enum have none: varying either alone gives two or three
/// distinct pairs and then repeats. The columns are walked as digits instead —
/// the boolean flips every row, the enum advances every second row — and the
/// row count is capped at the six combinations that exist.
#[test]
fn a_composite_key_of_narrow_columns_is_walked_rather_than_guessed() {
    let (db, _) = seed(
        "CREATE TYPE state AS ENUM ('new', 'open', 'done');
         CREATE TABLE narrow (
             flag   boolean NOT NULL,
             status state   NOT NULL,
             note   text,
             PRIMARY KEY (flag, status)
         );",
        50,
    );
    assert_eq!(count(&db, "narrow"), 6, "two booleans times three labels");

    // All six, not the same one six times.
    let pairs: i64 = db
        .client()
        .query_one(
            "SELECT count(*) FROM (SELECT DISTINCT flag, status FROM narrow) d",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(pairs, 6);
}

/// Two composite keys sharing a column that neither can spare.
///
/// One odometer cannot advance a column at two different rates, so this is
/// refused by name rather than satisfying the first key and hoping the second
/// falls out.
#[test]
fn two_keys_that_contend_for_one_column_are_refused_rather_than_hoped_over() {
    let db = Db::start();
    db.apply(
        "CREATE TYPE colour AS ENUM ('red', 'green');
         CREATE TABLE contended (
             a colour NOT NULL,
             b colour NOT NULL,
             c colour NOT NULL,
             PRIMARY KEY (a, b),
             CONSTRAINT other UNIQUE (b, c)
         );
         CREATE TABLE roomy (
             a colour NOT NULL,
             b text    NOT NULL,
             c text    NOT NULL,
             PRIMARY KEY (a, b),
             CONSTRAINT fine UNIQUE (b, c)
         );",
    );
    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);

    let (_, reasons) = verdict
        .refused
        .iter()
        .find(|(t, _)| t.name == "contended")
        .expect("two narrow keys contending should be refused");
    assert!(
        reasons[0].explain().contains("share a column"),
        "{:?}",
        reasons[0]
    );

    // And a table whose keys each have a column with room to spare is fine:
    // each carries its own key and they never contend.
    assert!(
        verdict.fillable.iter().any(|t| t.name == "roomy"),
        "this one should not have been caught by the same rule"
    );
}

/// A cycle repaired on a table keyed by uuid.
///
/// The repair picked its root row with `min(id)`, and `min()` has no overload
/// for `uuid`. Lago keys all 137 of its tables that way, and said so the first
/// time it was measured.
#[test]
fn a_cycle_is_repaired_on_a_table_keyed_by_something_min_cannot_take() {
    let (db, _) = seed(
        "CREATE TABLE charges (
             id uuid PRIMARY KEY,
             parent_id uuid REFERENCES charges(id),
             name text NOT NULL
         );",
        8,
    );
    assert_eq!(count(&db, "charges"), 8);
    let roots: i64 = db
        .client()
        .query_one("SELECT count(*) FROM charges WHERE parent_id IS NULL", &[])
        .unwrap()
        .get(0);
    assert_eq!(roots, 1, "every row but the root should point somewhere");
}

/// Two rules that cannot both be kept.
///
/// GitLab's `ai_tool_rules` says each of three permission columns may be
/// `NULL or one of a list` — which this satisfies by writing NULL — and
/// separately that at least one of the three must not be null. Read one at a
/// time both are satisfiable; together no row satisfies them, and the table
/// has to be refused rather than attempted.
#[test]
fn a_column_obliged_to_be_null_cannot_also_be_the_one_holding_a_value() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE ai_tool_rules (
             id bigint PRIMARY KEY,
             web_access   smallint,
             local_access smallint,
             CONSTRAINT web_enum   CHECK (((web_access IS NULL) OR (web_access = ANY (ARRAY[0, 1])))),
             CONSTRAINT local_enum CHECK (((local_access IS NULL) OR (local_access = ANY (ARRAY[0, 1])))),
             CONSTRAINT has_permission CHECK (((web_access IS NOT NULL) OR (local_access IS NOT NULL)))
         );
         CREATE TABLE fine (
             id bigint PRIMARY KEY,
             a smallint,
             b smallint,
             CONSTRAINT a_enum CHECK (((a IS NULL) OR (a = ANY (ARRAY[0, 1])))),
             CONSTRAINT one_of CHECK (((a IS NOT NULL) OR (b IS NOT NULL)))
         );",
    );

    let mut client = db.client();
    let schema = introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = graph::order(&schema);
    let verdict = classify::classify(&schema, &order);

    assert!(
        verdict.is_refused(&pgsow::schema::TableId::new("public", "ai_tool_rules")),
        "no row satisfies both rules, so the table must be named"
    );
    // `b` is free, so one column can still hold the value and this one works.
    assert!(
        verdict.fillable.iter().any(|t| t.name == "fine"),
        "one unconstrained column is enough: {:?}",
        verdict.refused
    );
}

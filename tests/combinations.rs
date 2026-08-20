//! Flags in combination, because that is where the last bug lived.
//!
//! `--probe` offered every table in the database no matter what `--include`
//! asked for. Neither flag was untested; the gap was between them. Every test
//! that passed `--include` did not probe, and every test that probed asked for
//! everything, so nothing in this repository ever ran the pair — and it took a
//! different project using the tool to notice.
//!
//! So this file is organised by *pair*, not by flag. The ones that matter are
//! the pairs where one flag selects tables and the other writes or destroys
//! them, because that is where being wrong costs somebody data.
//!
//! `cargo test --test combinations`

mod harness;

use harness::Db;

/// Two tables and a foreign key: `child` points at `parent`. Enough to tell a
/// selection that is respected from one that is quietly widened, since the FK
/// is the thing that widens it.
const PAIR: &str = "
    CREATE TABLE parent (id serial PRIMARY KEY, name text NOT NULL);
    CREATE TABLE child  (id serial PRIMARY KEY,
                         parent_id integer NOT NULL REFERENCES parent(id),
                         note text NOT NULL);
    CREATE TABLE bystander (id serial PRIMARY KEY, name text NOT NULL);
";

struct Ran {
    ok: bool,
    out: String,
    err: String,
}

fn pgseed(db: &Db, args: &[&str]) -> Ran {
    let mut all = vec!["--dsn", &db.url];
    all.extend_from_slice(args);
    let done = std::process::Command::new(env!("CARGO_BIN_EXE_pgseed"))
        .args(&all)
        .output()
        .expect("pgseed should run");
    Ran {
        ok: done.status.success(),
        out: String::from_utf8_lossy(&done.stdout).into_owned(),
        err: String::from_utf8_lossy(&done.stderr).into_owned(),
    }
}

fn count(db: &Db, table: &str) -> i64 {
    db.client()
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

fn fill(db: &Db) {
    db.apply(
        "INSERT INTO parent (name) SELECT 'p' || g FROM generate_series(1, 4) g;
         INSERT INTO child (parent_id, note) SELECT 1, 'c' || g FROM generate_series(1, 4) g;
         INSERT INTO bystander (name) SELECT 'b' || g FROM generate_series(1, 4) g;",
    );
}

// ---------------------------------------------------------------------------
// --probe × the selection flags. The pair that was broken.

#[test]
fn probe_does_not_reach_a_table_that_include_left_out() {
    let db = Db::start();
    db.apply(PAIR);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "3",
            "--allow-nonempty",
            "--probe",
            "--include",
            "parent",
        ],
    );
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    assert_eq!(
        count(&db, "parent"),
        3,
        "the included table should be filled"
    );
    assert_eq!(
        count(&db, "bystander"),
        0,
        "--include named one table and --probe filled another"
    );
}

#[test]
fn probe_does_not_reach_a_table_that_exclude_named() {
    let db = Db::start();
    db.apply(PAIR);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "3",
            "--allow-nonempty",
            "--probe",
            "--exclude",
            "bystander",
        ],
    );
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    assert_eq!(
        count(&db, "bystander"),
        0,
        "--exclude promises never, and --probe wrote there anyway"
    );
}

// ---------------------------------------------------------------------------
// --truncate × the selection flags. The pair where being wrong destroys data.

#[test]
fn truncate_leaves_a_table_that_include_did_not_name() {
    let db = Db::start();
    db.apply(PAIR);
    fill(&db);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "2",
            "--truncate",
            "--include",
            "bystander",
        ],
    );
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    assert_eq!(
        count(&db, "parent"),
        4,
        "--include named only bystander and --truncate emptied an unrelated table"
    );
}

/// The one worth knowing the answer to.
///
/// Emptying `parent` requires emptying `child`, because Postgres will not
/// truncate a table something references. So `--include parent --truncate`
/// reaches `child` whatever the user said about it. That is defensible — the
/// row was not going to survive either way — but `--exclude`'s help says
/// "Never these tables", and a promise the tool cannot keep should be a
/// promise it does not print.
#[test]
fn truncate_says_out_loud_when_a_foreign_key_widens_what_it_empties() {
    let db = Db::start();
    db.apply(PAIR);
    fill(&db);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "2",
            "--truncate",
            "--include",
            "parent",
            "--exclude",
            "child",
        ],
    );
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    let said = format!("{}{}", ran.out, ran.err);
    let emptied = count(&db, "child") == 0;
    if emptied {
        assert!(
            said.contains("child"),
            "child was emptied despite --exclude and nothing said so. Output was:\n{said}"
        );
    }
    assert_eq!(
        count(&db, "bystander"),
        4,
        "bystander references nothing and was not asked for"
    );
}

// ---------------------------------------------------------------------------
// --rows overrides × the selection flags.

#[test]
fn a_rows_override_for_an_excluded_table_is_reported_not_silently_dropped() {
    let db = Db::start();
    db.apply(PAIR);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "2",
            "--rows",
            "bystander=9",
            "--allow-nonempty",
            "--include",
            "parent",
        ],
    );
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    assert_eq!(count(&db, "bystander"), 0, "it was not included");
    assert!(
        ran.err.contains("bystander"),
        "an override that matched nothing must say so, or a typo looks like it worked. \
         stderr was:\n{}",
        ran.err
    );
}

// ---------------------------------------------------------------------------
// --plan × the selection flags. Reporting, not writing, but a report that
// counts tables nobody asked about is a report that cannot be acted on.

#[test]
fn plan_writes_nothing_even_when_probe_is_asked_for() {
    let db = Db::start();
    db.apply(PAIR);
    fill(&db);

    let ran = pgseed(&db, &["--plan", "--probe", "--rows", "3"]);
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    assert_eq!(count(&db, "parent"), 4, "--plan must not write");
    assert_eq!(count(&db, "child"), 4, "--plan must not write");
    assert_eq!(count(&db, "bystander"), 4, "--plan must not write");
}

#[test]
fn plan_does_not_count_a_table_the_selection_left_out() {
    let db = Db::start();
    db.apply(PAIR);

    let ran = pgseed(&db, &["--plan", "--include", "parent"]);
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    let said = format!("{}{}", ran.out, ran.err);
    assert!(
        !said.contains("bystander") || said.contains("left out"),
        "a table nobody asked about appeared in the plan as something other \
         than a table left out:\n{said}"
    );
}

/// Asking for a child table and getting nothing.
///
/// `--include child` cannot be satisfied without rows in `parent`, because the
/// foreign key is NOT NULL. Before `--probe` was scoped to the selection it
/// filled `parent` anyway, as a side effect of filling everything, and that
/// accident was the only reason `--include` on a child ever worked.
#[test]
fn include_on_a_child_table_reaches_the_parent_it_needs() {
    let db = Db::start();
    db.apply(PAIR);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "4",
            "--allow-nonempty",
            "--probe",
            "--include",
            "child",
        ],
    );
    assert!(ran.ok, "pgseed failed: {}", ran.err);

    let child = count(&db, "child");
    assert_eq!(
        child, 4,
        "asked for child and got {child} rows. stdout:\n{}\nstderr:\n{}",
        ran.out, ran.err
    );
    assert_eq!(
        count(&db, "bystander"),
        0,
        "bystander is not needed by child and must stay untouched"
    );
}

/// Widening must not override an explicit refusal.
///
/// `--include child --exclude parent` asks for something that cannot be
/// satisfied: the key is NOT NULL and its only source was just forbidden.
/// The answer has to be a stated refusal, not a row invented to fill the gap
/// and not a table written to after being named to `--exclude`.
#[test]
fn an_excluded_parent_is_not_written_to_in_order_to_satisfy_a_child() {
    let db = Db::start();
    db.apply(PAIR);

    let ran = pgseed(
        &db,
        &[
            "--apply",
            "--rows",
            "4",
            "--allow-nonempty",
            "--probe",
            "--include",
            "child",
            "--exclude",
            "parent",
        ],
    );

    assert_eq!(
        count(&db, "parent"),
        0,
        "parent was named to --exclude and was written to anyway"
    );
    let said = format!("{}{}", ran.out, ran.err);
    assert!(
        said.contains("parent") || said.contains("child"),
        "an unsatisfiable selection must say which table and why:\n{said}"
    );
}

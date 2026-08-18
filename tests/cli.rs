//! The binary, driven the way a person drives it.
//!
//! Everything else here tests the library. This checks the two things only the
//! command line can get wrong: that SQL goes to stdout while the report goes
//! to stderr — so `pgsow > seed.sql` produces a file that actually runs — and
//! that the exit codes mean what the README says they mean.

mod harness;

use std::process::Command;

use harness::Db;

fn run(url: &str, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_pgsow"))
        .arg("--dsn")
        .arg(url)
        .args(args)
        .output()
        .expect("could not run pgsow");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn rows(db: &Db, table: &str) -> i64 {
    db.client()
        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
        .unwrap()
        .get(0)
}

#[test]
fn the_two_streams_are_kept_apart_and_the_exit_codes_mean_something() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL);
         CREATE TABLE hard (
             id int PRIMARY KEY, a int, b int,
             -- Genuinely outside the closed set, and meant to stay there.
             -- This was num_nonnulls(a, b) = 1 until that shape was
             -- understood, at which point the table stopped being refused
             -- and this test said so.
             CONSTRAINT one_of CHECK ((a > 0) OR (b > 0))
         );",
    );

    // Redirecting stdout must give a file that runs, with no prose in it.
    let (stdout, stderr, code) = run(&db.url, &["--rows", "3"]);
    assert!(stdout.starts_with("BEGIN;"), "{stdout}");
    assert!(stdout.contains("INSERT INTO"));
    assert!(!stdout.contains("pgsow:"), "the report leaked into the SQL");
    assert!(
        !stdout.contains("\"hard\""),
        "a refused table appeared in the SQL"
    );

    // The refusal is on stderr, and names the constraint rather than
    // apologising in general terms.
    assert!(stderr.contains("one_of"), "{stderr}");
    assert_eq!(code, 1, "something was refused, so this is 1 rather than 0");

    // And the SQL it produced is real: the database takes it.
    db.client()
        .batch_execute(&stdout)
        .expect("its own output did not run");
    assert_eq!(rows(&db, "users"), 3);
}

#[test]
fn plan_reports_and_writes_nothing_at_all() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE users (id int PRIMARY KEY);
         CREATE TABLE flags (id int PRIMARY KEY, on_off bool UNIQUE);",
    );
    let (stdout, stderr, _) = run(&db.url, &["--plan", "--rows", "9"]);
    assert!(stdout.is_empty(), "--plan produced SQL: {stdout}");
    assert!(stderr.contains("would fill"));

    // A table that cannot hold what was asked for says so in the plan, rather
    // than quietly returning a third of it.
    assert!(
        stderr.contains("capped"),
        "the cap went unmentioned: {stderr}"
    );
    let flags = stderr.lines().find(|l| l.contains("flags")).unwrap_or("");
    assert!(
        flags.contains(" 2 "),
        "a unique bool holds 2, not more: {flags}"
    );
    assert!(
        stderr
            .lines()
            .any(|l| l.contains("users") && !l.contains("capped")),
        "an unbounded table should not be reported as capped: {stderr}"
    );
}

#[test]
fn apply_writes_rows_and_truncate_lets_it_be_run_twice() {
    let db = Db::start();
    db.apply("CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL);");

    let (stdout, stderr, code) = run(&db.url, &["--apply", "--rows", "7"]);
    assert!(stdout.is_empty(), "--apply should not also print SQL");
    assert!(stderr.contains("applied"), "{stderr}");
    assert_eq!(code, 0);
    assert_eq!(rows(&db, "users"), 7);

    // The same seed generates the same primary keys, so a second run collides.
    // That is determinism working, not a bug — and the reason --truncate is
    // the flag that makes a repeated run mean anything.
    let (_, _, again) = run(&db.url, &["--apply", "--rows", "7"]);
    assert_eq!(
        again, 2,
        "a second run should fail on keys it already wrote"
    );
    assert_eq!(
        rows(&db, "users"),
        7,
        "the failed run must have rolled back"
    );

    let (_, _, third) = run(&db.url, &["--apply", "--truncate", "--rows", "7"]);
    assert_eq!(third, 0);
    assert_eq!(
        rows(&db, "users"),
        7,
        "truncate leaves exactly one run's worth"
    );
}

#[test]
fn an_unreachable_database_says_so_rather_than_panicking() {
    let (_, stderr, code) = run("postgres://nobody@127.0.0.1:1/nothing", &[]);
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot connect"), "{stderr}");
}

#[test]
fn rows_can_be_set_per_table_and_a_typo_is_reported() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE users (id int PRIMARY KEY);
         CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL REFERENCES users(id));",
    );
    let (_, stderr, code) = run(
        &db.url,
        &[
            "--apply", "--rows", "4", "--rows", "order*=9", "--rows", "nosuch=3",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(rows(&db, "users"), 4);
    assert_eq!(rows(&db, "orders"), 9, "the override should have applied");

    // A pattern that matched nothing is a typo, and a typo that is silently
    // ignored produces a run which looks like it worked and did something else.
    assert!(
        stderr.contains("nosuch=3"),
        "the typo went unmentioned: {stderr}"
    );
}

#[test]
fn include_and_exclude_choose_the_tables_and_exclude_wins() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE users (id int PRIMARY KEY);
         CREATE TABLE orders (id int PRIMARY KEY);
         CREATE TABLE order_items (id int PRIMARY KEY);",
    );
    let (_, stderr, code) = run(
        &db.url,
        &[
            "--apply",
            "--rows",
            "3",
            "--include",
            "order*",
            "--exclude",
            "order_items",
        ],
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(rows(&db, "orders"), 3);
    assert_eq!(rows(&db, "order_items"), 0, "excluded, so untouched");
    assert_eq!(rows(&db, "users"), 0, "not included, so untouched");

    // A table left out is not the same answer as a table refused, and the
    // report must not blur them.
    assert!(stderr.contains("left out by"), "{stderr}");
    assert!(
        !stderr.contains("refused:"),
        "nothing here was refused: {stderr}"
    );
}

#[test]
fn out_writes_the_sql_to_a_file_and_stdout_stays_empty() {
    let db = Db::start();
    db.apply("CREATE TABLE users (id int PRIMARY KEY);");
    let path = std::env::temp_dir().join("pgsow_cli_out.sql");
    let _ = std::fs::remove_file(&path);

    let (stdout, _, code) = run(&db.url, &["--rows", "5", "--out", path.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(stdout.is_empty(), "--out should not also print: {stdout}");

    let written = std::fs::read_to_string(&path).expect("the file");
    assert!(written.starts_with("BEGIN;"));
    db.client()
        .batch_execute(&written)
        .expect("the file should run");
    assert_eq!(rows(&db, "users"), 5);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn writing_over_rows_that_are_already_there_needs_saying_so() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE users (id int PRIMARY KEY);
         INSERT INTO users VALUES (9999);",
    );

    // The guard that does not depend on a hostname: an empty database is a
    // scratch database whatever it is called, and a populated one might be
    // anything at all.
    let (_, stderr, code) = run(&db.url, &["--apply", "--rows", "3"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already hold rows"), "{stderr}");
    assert_eq!(rows(&db, "users"), 1, "it must not have written anything");

    // And it says which flag to reach for.
    assert!(stderr.contains("--truncate"), "{stderr}");
    assert!(stderr.contains("--allow-nonempty"), "{stderr}");

    let (_, stderr, code) = run(&db.url, &["--apply", "--rows", "3", "--allow-nonempty"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(rows(&db, "users"), 4, "one that was there and three more");
}

#[test]
fn writing_somewhere_that_is_not_this_machine_needs_saying_so() {
    // Refused before connecting, so nothing is touched and the hostname does
    // not even have to resolve. Not a claim that the far end is production —
    // only that it is not here, which is a fact about the string.
    let (_, stderr, code) = run("postgres://u@db.example.com:5432/app", &["--apply"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("not this machine"), "{stderr}");
    assert!(stderr.contains("--remote"), "{stderr}");

    // Reading is not writing, so the same connection string without --apply
    // gets as far as failing to connect, which is a different error.
    let (_, stderr, code) = run("postgres://u@db.example.com:5432/app", &["--plan"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot connect"), "{stderr}");
}

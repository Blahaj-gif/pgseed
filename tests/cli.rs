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
             CONSTRAINT one_of CHECK (num_nonnulls(a, b) = 1)
         );",
    );

    // Redirecting stdout must give a file that runs, with no prose in it.
    let (stdout, stderr, code) = run(&db.url, &["--rows", "3"]);
    assert!(stdout.starts_with("BEGIN;"), "{stdout}");
    assert!(stdout.contains("INSERT INTO"));
    assert!(!stdout.contains("pgsow:"), "the report leaked into the SQL");
    assert!(!stdout.contains("\"hard\""), "a refused table appeared in the SQL");

    // The refusal is on stderr, and names the constraint rather than
    // apologising in general terms.
    assert!(stderr.contains("one_of"), "{stderr}");
    assert_eq!(code, 1, "something was refused, so this is 1 rather than 0");

    // And the SQL it produced is real: the database takes it.
    db.client().batch_execute(&stdout).expect("its own output did not run");
    assert_eq!(rows(&db, "users"), 3);
}

#[test]
fn plan_reports_and_writes_nothing_at_all() {
    let db = Db::start();
    db.apply("CREATE TABLE users (id int PRIMARY KEY);");
    let (stdout, stderr, _) = run(&db.url, &["--plan"]);
    assert!(stdout.is_empty(), "--plan produced SQL: {stdout}");
    assert!(stderr.contains("would fill"));
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
    assert_eq!(again, 2, "a second run should fail on keys it already wrote");
    assert_eq!(rows(&db, "users"), 7, "the failed run must have rolled back");

    let (_, _, third) = run(&db.url, &["--apply", "--truncate", "--rows", "7"]);
    assert_eq!(third, 0);
    assert_eq!(rows(&db, "users"), 7, "truncate leaves exactly one run's worth");
}

#[test]
fn an_unreachable_database_says_so_rather_than_panicking() {
    let (_, stderr, code) = run("postgres://nobody@127.0.0.1:1/nothing", &[]);
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot connect"), "{stderr}");
}

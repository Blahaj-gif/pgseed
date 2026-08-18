//! A real Postgres, for tests that need one.
//!
//! Downloaded and started by `postgresql_embedded` rather than installed, and
//! rather than run in Docker. That is not a convenience: the whole correctness
//! argument of this tool is that *the database adjudicates*, so the tests need
//! a database and not a model of one, and a test suite that only runs where
//! somebody remembered to install Postgres is a test suite that stops running.
#![allow(dead_code)]

use std::time::Duration;

use postgresql_embedded::blocking::PostgreSQL;
use postgresql_embedded::Settings;

/// How long to allow for setup. The default is fifteen seconds, which is fine
/// for starting a server that is already unpacked and nowhere near enough for
/// the first run, where the binaries are downloaded and `initdb` builds a
/// cluster from nothing — on Windows that alone can take a minute.
const SETUP_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Db {
    postgres: PostgreSQL,
    pub url: String,
}

impl Db {
    /// Start a server and create an empty database on it.
    pub fn start() -> Db {
        let settings = Settings {
            timeout: Some(SETUP_TIMEOUT),
            ..Settings::default()
        };
        let mut postgres = PostgreSQL::new(settings);
        postgres
            .setup()
            .expect("could not set up an embedded postgres");
        postgres
            .start()
            .expect("could not start the embedded postgres");
        postgres
            .create_database("pgsow_test")
            .expect("could not create the test database");
        let url = postgres.settings().url("pgsow_test");
        Db { postgres, url }
    }

    pub fn client(&self) -> postgres::Client {
        postgres::Client::connect(&self.url, postgres::NoTls)
            .expect("could not connect to the embedded postgres")
    }

    /// Run some DDL, so a test can state the schema it is about.
    pub fn apply(&self, sql: &str) {
        self.client().batch_execute(sql).expect("DDL failed");
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        let _ = self.postgres.stop();
    }
}

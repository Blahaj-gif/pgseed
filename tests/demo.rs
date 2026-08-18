//! What the output looks like, on a schema shaped like an application.
//!
//! Every other test here asks whether the database accepted the rows. This one
//! asks the question the database cannot: would anybody want to look at them.
//! It is a survey, not an assertion — it prints, and reading it is the check.
//!
//! `cargo test --test demo -- --ignored --nocapture`

mod harness;

use harness::Db;

const SCHEMA: &str = r#"
CREATE TABLE companies (
    id            serial PRIMARY KEY,
    name          text NOT NULL,
    country_code  char(2) NOT NULL,
    website_url   text,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id            serial PRIMARY KEY,
    company_id    integer NOT NULL REFERENCES companies(id),
    email         varchar(255) NOT NULL UNIQUE,
    username      varchar(40) NOT NULL UNIQUE,
    first_name    text NOT NULL,
    last_name     text NOT NULL,
    display_name  text,
    locale        varchar(8) NOT NULL DEFAULT 'en-US',
    timezone      text NOT NULL,
    avatar_path   text,
    last_login_ip text,
    api_token     varchar(48),
    state         varchar(16) NOT NULL,
    CONSTRAINT users_state_known CHECK (state = ANY (ARRAY['active','suspended','invited']))
);

CREATE TABLE documents (
    id            bigserial PRIMARY KEY,
    author_id     integer NOT NULL REFERENCES users(id),
    title         text NOT NULL,
    slug          varchar(64) NOT NULL UNIQUE,
    description   text,
    body_html     text,
    file_name     text,
    content_type  varchar(100),
    checksum      char(64),
    version       varchar(20) NOT NULL,
    label_color   char(7),
    CONSTRAINT documents_title_not_empty CHECK (char_length(title) > 0)
);
"#;

#[test]
#[ignore]
fn what_the_rows_look_like() {
    let db = Db::start();
    let mut client = db.client();
    client.batch_execute(SCHEMA).expect("the demo schema loads");

    let read = pgsow::introspect::read(&mut client, &["public".to_string()]).expect("read");
    let order = pgsow::graph::order(&read);
    let verdict = pgsow::classify::classify(&read, &order);
    for (id, reasons) in &verdict.refused {
        for reason in reasons {
            println!("REFUSED {id}: {}", reason.explain());
        }
    }
    let sql = pgsow::emit::sql(&read, &verdict, &pgsow::emit::Options::flat(1, 4));
    println!("\n{sql}");

    // And the database still adjudicates, because a demo that does not apply
    // is a screenshot rather than evidence.
    client.batch_execute(&sql).expect("Postgres accepts it");
}

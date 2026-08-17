//! Reading a real schema out of a real Postgres.
//!
//! Everything here would pass against a mock and mean nothing. The point is
//! that Postgres itself produced the catalog rows, so a wrong assumption about
//! `atttypmod` or `conkey` shows up as a failure rather than as agreement with
//! my own beliefs about them.

mod harness;

use harness::Db;

#[test]
fn a_real_schema_reads_back_the_way_the_ddl_described_it() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE users (
             id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             email       varchar(255) NOT NULL UNIQUE,
             nickname    text,
             created_at  timestamptz NOT NULL DEFAULT now()
         );",
    );

    let mut client = db.client();
    let schema = pgsow::introspect::read(&mut client, &["public".to_string()])
        .expect("introspection failed");

    assert_eq!(schema.len(), 1);
    let table = schema
        .get(&pgsow::schema::TableId::new("public", "users"))
        .expect("users is missing");

    // The identity column must be recognised as generated: naming it in an
    // INSERT is an error, not an override.
    let id = table.column("id").unwrap();
    assert!(id.is_generated, "identity column not recognised as generated");

    // varchar(255) has to arrive with its limit, or a longer value is written
    // and rejected at runtime.
    let email = table.column("email").unwrap();
    assert!(!email.nullable);
    assert_eq!(
        email.type_,
        pgsow::schema::ColumnType::Text { max_length: Some(255) }
    );

    let nickname = table.column("nickname").unwrap();
    assert!(nickname.nullable);

    // A defaulted column is left to the database.
    let created = table.column("created_at").unwrap();
    assert!(created.has_default);

    assert!(table.primary_key().is_some());
    assert_eq!(table.unique_keys.len(), 2, "primary key and the unique on email");
}

#[test]
fn a_check_constraint_arrives_with_its_expression_and_refuses_the_table() {
    // The reason this reads pg_catalog rather than information_schema: the
    // standard views will not give you the expression at all, and without it a
    // refusal cannot quote the rule a person needs to see.
    let db = Db::start();
    db.apply(
        "CREATE TABLE invoices (
             id     int PRIMARY KEY,
             total  numeric(10,2) NOT NULL,
             CONSTRAINT invoices_total_positive CHECK (total > 0)
         );",
    );

    let mut client = db.client();
    let schema = pgsow::introspect::read(&mut client, &["public".to_string()]).unwrap();
    let table = schema.get(&pgsow::schema::TableId::new("public", "invoices")).unwrap();

    assert_eq!(table.checks.len(), 1);
    assert_eq!(table.checks[0].name, "invoices_total_positive");
    assert!(table.checks[0].definition.contains("total"), "{:?}", table.checks[0]);

    // numeric(10,2) has to come back with both halves unpacked, or every money
    // column in the database gets its decimal point in the wrong place.
    assert_eq!(
        table.column("total").unwrap().type_,
        pgsow::schema::ColumnType::Numeric { precision: Some(10), scale: Some(2) }
    );

    let order = pgsow::graph::order(&schema);
    let verdict = pgsow::classify::classify(&schema, &order);
    assert!(verdict.fillable.is_empty());
    assert!(verdict.refused[0].1[0].explain().contains("invoices_total_positive"));
}

#[test]
fn a_composite_foreign_key_keeps_its_columns_in_the_declared_order() {
    // The referencing and referenced sides have to agree on order. Sorting
    // them by name, or by attribute number, silently pairs the wrong columns
    // and produces rows that reference the wrong parent.
    let db = Db::start();
    db.apply(
        "CREATE TABLE parent (
             tenant_id int, code text,
             PRIMARY KEY (tenant_id, code)
         );
         CREATE TABLE child (
             id int PRIMARY KEY,
             c_code text NOT NULL, c_tenant int NOT NULL,
             CONSTRAINT child_parent_fk FOREIGN KEY (c_tenant, c_code)
                 REFERENCES parent (tenant_id, code)
         );",
    );

    let mut client = db.client();
    let schema = pgsow::introspect::read(&mut client, &["public".to_string()]).unwrap();
    let child = schema.get(&pgsow::schema::TableId::new("public", "child")).unwrap();

    let fk = &child.foreign_keys[0];
    assert_eq!(fk.columns, vec!["c_tenant", "c_code"]);
    assert_eq!(fk.referenced_columns, vec!["tenant_id", "code"]);

    let order = pgsow::graph::order(&schema);
    let names: Vec<&str> = order.tables.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["parent", "child"], "parent must be inserted first");
}

#[test]
fn a_deferrable_cycle_is_recognised_as_deferrable() {
    let db = Db::start();
    db.apply(
        "CREATE TABLE a (id int PRIMARY KEY, b_id int NOT NULL);
         CREATE TABLE b (id int PRIMARY KEY, a_id int NOT NULL);
         ALTER TABLE a ADD CONSTRAINT a_b_fk FOREIGN KEY (b_id) REFERENCES b(id)
             DEFERRABLE INITIALLY DEFERRED;
         ALTER TABLE b ADD CONSTRAINT b_a_fk FOREIGN KEY (a_id) REFERENCES a(id)
             DEFERRABLE INITIALLY DEFERRED;",
    );

    let mut client = db.client();
    let schema = pgsow::introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = pgsow::graph::order(&schema);

    assert_eq!(order.cycles.len(), 1);
    match &order.cycles[0].strategy {
        pgsow::graph::CycleStrategy::Deferred { constraints } => {
            assert_eq!(constraints.len(), 2);
        }
        other => panic!("expected deferral, got {other:?}"),
    }
    assert!(order.blocked().is_empty());
    assert_eq!(pgsow::classify::classify(&schema, &order).fillable.len(), 2);
}

#[test]
fn a_rigid_cycle_is_refused_against_a_real_database() {
    // NOT NULL on both sides, neither deferrable. Postgres itself will not let
    // you insert into either table, which is what makes refusing correct
    // rather than defeatist.
    let db = Db::start();
    db.apply(
        "CREATE TABLE x (id int PRIMARY KEY, y_id int NOT NULL);
         CREATE TABLE y (id int PRIMARY KEY, x_id int NOT NULL);
         ALTER TABLE x ADD CONSTRAINT x_y_fk FOREIGN KEY (y_id) REFERENCES y(id);
         ALTER TABLE y ADD CONSTRAINT y_x_fk FOREIGN KEY (x_id) REFERENCES x(id);",
    );

    let mut client = db.client();
    let schema = pgsow::introspect::read(&mut client, &["public".to_string()]).unwrap();
    let order = pgsow::graph::order(&schema);
    let verdict = pgsow::classify::classify(&schema, &order);

    assert!(verdict.fillable.is_empty());
    assert_eq!(verdict.refused.len(), 2);
    let reason = verdict.refused[0].1[0].explain();
    assert!(reason.contains("deferrable") && reason.contains("nullable"), "{reason}");

    // And prove the refusal was right: Postgres rejects the insert too.
    let failed = db.client().batch_execute("INSERT INTO x (id, y_id) VALUES (1, 1);");
    assert!(failed.is_err(), "the database should have rejected this as well");
}

#[test]
fn enums_domains_and_arrays_come_back_as_themselves() {
    let db = Db::start();
    db.apply(
        "CREATE TYPE status AS ENUM ('pending', 'shipped', 'cancelled');
         CREATE DOMAIN email AS text;
         CREATE DOMAIN positive AS int CHECK (VALUE > 0);
         CREATE TABLE things (
             id       int PRIMARY KEY,
             state    status NOT NULL,
             contact  email NOT NULL,
             tags     text[] NOT NULL,
             amount   positive NOT NULL
         );",
    );

    let mut client = db.client();
    let schema = pgsow::introspect::read(&mut client, &["public".to_string()]).unwrap();
    let t = schema.get(&pgsow::schema::TableId::new("public", "things")).unwrap();

    match &t.column("state").unwrap().type_ {
        pgsow::schema::ColumnType::Enum { labels, .. } => {
            assert_eq!(labels, &vec!["pending", "shipped", "cancelled"]);
        }
        other => panic!("expected an enum, got {other:?}"),
    }

    assert!(t.column("contact").unwrap().type_.is_generatable(), "a plain domain is fine");
    assert!(t.column("tags").unwrap().type_.is_generatable(), "text[] is fine");

    // A domain carrying a CHECK is a CHECK by another name, and gets the same
    // answer: refused rather than approximated.
    assert!(!t.column("amount").unwrap().type_.is_generatable());

    let order = pgsow::graph::order(&schema);
    let verdict = pgsow::classify::classify(&schema, &order);
    assert!(verdict.refused[0].1.iter().any(|r| r.explain().contains("positive")));
}

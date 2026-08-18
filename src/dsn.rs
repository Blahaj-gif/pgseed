//! Where this is pointed, and the one question worth asking before it writes.
//!
//! The plan called this "the guard against pointing at prod". The tempting
//! version of that guard reads the database name for words like `prod` and
//! refuses, which is a guess dressed as a safety feature: it stops nobody who
//! called their database `main`, and it annoys everybody whose local copy is
//! called `myapp_production_dump`.
//!
//! So the guard here is built on facts rather than on inferences about names.
//! Two of them:
//!
//!   1. **Is the host this machine?** A connection to `localhost` is a
//!      different kind of risk from one to a hostname somewhere else. That is
//!      a property of the string, not an opinion about it.
//!   2. **Do the target tables already hold rows?** An empty database is a
//!      scratch database whatever it is called. A populated one might be
//!      anything, and seeding on top of real data is nearly always an
//!      accident — see `filter::already_populated`.
//!
//! Neither is a claim about whether a database is production. Both are
//! questions a person can answer instantly and a tool cannot, which is why
//! they are asked rather than assumed.

/// The host a connection string names, as far as the guard is concerned.
///
/// Deliberately not a full URI parser. Postgres accepts both a URI and a
/// keyword string, and getting the general case exactly right is not what this
/// needs — it needs to know when it is *certain* the host is local, and to say
/// so is not otherwise.
pub fn host(dsn: &str) -> Option<String> {
    // Keyword form: `host=db.example.com port=5432 dbname=app`.
    if !dsn.contains("://") {
        return dsn
            .split_whitespace()
            .find_map(|pair| pair.strip_prefix("host="))
            .map(|h| h.trim().to_string());
    }

    // URI form: `postgres://user:pass@host:port/db?opts`. The authority is
    // between the scheme and the first `/`, and the host is after the last `@`
    // — passwords may contain almost anything, `@` included.
    let after_scheme = dsn.split_once("://")?.1;
    let authority = after_scheme
        .split(['/', '?'])
        .next()
        .filter(|a| !a.is_empty())?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);

    // A bracketed IPv6 literal keeps its colons; anything else splits on the
    // last one to drop the port.
    let host = if let Some(inner) = host_port.strip_prefix('[') {
        inner.split_once(']').map(|(h, _)| h).unwrap_or(inner)
    } else {
        host_port.rsplit_once(':').map_or(host_port, |(h, _)| h)
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// Whether the connection certainly goes nowhere but this machine.
///
/// The default when the host cannot be read is `false`. A guard that fails
/// open is not a guard, and being wrong in this direction costs one extra
/// flag while being wrong in the other costs somebody's data.
pub fn is_local(dsn: &str) -> bool {
    match host(dsn) {
        // In the keyword form, no host at all means a Unix socket on this
        // machine — the most local thing there is. In the URI form it means
        // the authority could not be read, which is not the same and must not
        // be treated as though it were.
        None => !dsn.contains("://"),
        Some(host) => {
            let host = host.to_ascii_lowercase();
            // Exact names, not prefixes. `localhost.evil.com` starts with
            // "localhost" and resolves to somebody else's machine; my own test
            // caught this one, which is the argument for writing it down.
            host.starts_with('/')
                || host == "localhost"
                || host.ends_with(".localhost")
                || host == "::1"
                || host == "0:0:0:0:0:0:0:1"
                || is_loopback_v4(&host)
        }
    }
}

/// Whether a dotted-quad names the 127.0.0.0/8 loopback range.
///
/// Checked as four numbers rather than as the text `127.`, because
/// `127.example.com` is a perfectly legal hostname belonging to somebody else.
fn is_loopback_v4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) && parts[0] == "127"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_gives_up_its_host_without_the_port_or_the_credentials() {
        assert_eq!(
            host("postgres://db.example.com/app").as_deref(),
            Some("db.example.com")
        );
        assert_eq!(
            host("postgres://u:p@db.example.com:5432/app").as_deref(),
            Some("db.example.com")
        );
        assert_eq!(
            host("postgresql://localhost:5432/app").as_deref(),
            Some("localhost")
        );
        // A password may contain an `@`, so the split is on the *last* one.
        assert_eq!(
            host("postgres://u:p@ss@real.host/app").as_deref(),
            Some("real.host")
        );
        assert_eq!(host("postgres://[::1]:5432/app").as_deref(), Some("::1"));
    }

    #[test]
    fn the_keyword_form_works_too() {
        assert_eq!(
            host("host=db.example.com port=5432 dbname=app").as_deref(),
            Some("db.example.com")
        );
        assert_eq!(host("dbname=app user=me"), None);
    }

    #[test]
    fn local_is_recognised_and_everything_else_is_not() {
        for dsn in [
            "postgres://localhost/app",
            "postgres://127.0.0.1:5432/app",
            "postgres://[::1]/app",
            "host=/var/run/postgresql dbname=app",
            "dbname=app",
        ] {
            assert!(is_local(dsn), "{dsn}");
        }
        for dsn in [
            "postgres://db.example.com/app",
            "postgres://10.0.0.5/app",
            "host=db.internal dbname=app",
            // Named to look local, and belonging to somebody else. Both of
            // these were passing until the test said otherwise.
            "postgres://localhost.evil.com/app",
            "postgres://127.example.com/app",
        ] {
            assert!(!is_local(dsn), "{dsn}");
        }
    }

    #[test]
    fn a_host_that_cannot_be_read_is_treated_as_remote() {
        // A guard that fails open is not a guard. Being wrong this way costs
        // one flag; being wrong the other way costs somebody their data.
        assert!(!is_local("postgres://"));
        assert!(!is_local("postgres:// /app"));
    }
}

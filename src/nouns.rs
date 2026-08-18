//! Values that look like the thing the column is named after.
//!
//! Everything here used to come from sixteen NATO words, on the reasoning that
//! this tool produces *valid* data rather than realistic data and that saying
//! so plainly beats implying otherwise. That reasoning is right about
//! correctness and wrong about use: a seed tool whose `email` column holds
//! `bravo` and whose `description` holds `foxtrot` is not one anybody puts in
//! front of a screenshot, and the incumbent it is measured against ships a
//! whole library for exactly this.
//!
//! So: **the same closed-set discipline as `checks` and `indexes`, applied to
//! column names.** A name is matched against exact shapes — the whole name,
//! its last two segments, its last one, and either lands on a known noun or
//! does not. Nothing is inferred from a substring, because `description_id` is
//! not a description and `key_id` is not a key.
//!
//! ## Where the names came from
//!
//! Not from memory. `tests/columns.rs` counts every text-typed column in the
//! twenty corpus schemas — 5,509 of them — and ranks the names. The list this
//! module covers is the head of that ranking:
//!
//! ```text
//!   666 name    463 id      290 type    230 key     217 path
//!   172 url     163 desc.   134 file     88 etag     79 token
//!    78 email    75 value    73 message  70 version  69 code
//! ```
//!
//! A handful of nouns here — city, street, postal code, company — are barely
//! in that ranking, because twenty open-source backend schemas are mostly
//! infrastructure and not a CRM. They are covered anyway, and this is the one
//! place in the project where something is included on judgement rather than
//! on measurement. Said out loud rather than buried.
//!
//! ## What must stay true
//!
//! Three properties, each of which the rest of the project already leans on:
//!
//! - **ASCII only.** `checks` treats `octet_length(col) <= N` and
//!   `char_length(col) <= N` as the same ceiling, which is true exactly while
//!   every generated string is ASCII. One accented surname would make that
//!   silently false and start producing rows Postgres rejects on a byte limit.
//!   Asserted by a test over every list in this file.
//! - **Distinct on demand.** A column under a unique key is generated with a
//!   `step`, and the value must then differ for every step — provably, not
//!   probably. The lists are read as an odometer and a cycle count is appended
//!   once they are exhausted, so `(digits, cycle)` reconstructs the step and
//!   two steps cannot collide.
//! - **It fits, or it declines.** Every noun returns `None` rather than
//!   producing something too long for the column, and the caller falls back to
//!   the plain generator, which is built for tight limits. A truncated email
//!   address is not a better email address.

use rand::Rng;
use rand_chacha::ChaCha8Rng;

/// What a column appears to hold, when its name says so beyond doubt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Noun {
    GivenName,
    FamilyName,
    PersonName,
    Username,
    Email,
    Company,
    City,
    Country,
    CountryCode,
    Region,
    Street,
    PostalCode,
    Phone,
    Url,
    Host,
    Domain,
    Path,
    FileName,
    Extension,
    MimeType,
    Slug,
    Title,
    Sentence,
    Html,
    Label,
    ClassName,
    State,
    Action,
    Method,
    Scope,
    Provider,
    Branch,
    Version,
    Currency,
    Locale,
    Timezone,
    Color,
    UserAgent,
    Encoding,
    Ip,
    Uuid,
    /// An opaque identifier: a key that is text rather than a number.
    Ident,
    /// Hexadecimal of a given width — a digest, an etag, a token, a secret.
    Hex(usize),
}

const GIVEN: [&str; 48] = [
    "Ada", "Amara", "Anton", "Bea", "Callum", "Camila", "Dara", "Dmitri", "Elena", "Elliot",
    "Farrah", "Felix", "Gita", "Grant", "Hana", "Hugo", "Imani", "Ines", "Jonas", "Junia", "Kai",
    "Karim", "Lena", "Louis", "Maeve", "Mateo", "Nadia", "Noah", "Olan", "Ottilie", "Priya",
    "Pavel", "Quentin", "Rania", "Rowan", "Sana", "Silas", "Tamsin", "Theo", "Ursula", "Vidya",
    "Viktor", "Wren", "Wesley", "Xanthe", "Yusuf", "Zara", "Zeke",
];

/// One apostrophe in here on purpose. Every value this project emits goes
/// through `Literal::text`, which doubles quotes, and a list with nothing to
/// escape leaves that path exercised only by a test that made its own input.
const FAMILY: [&str; 48] = [
    "Achebe",
    "Adeyemi",
    "Ballard",
    "Bianchi",
    "Castellan",
    "Chen",
    "Delacroix",
    "Duarte",
    "Espinoza",
    "Ellery",
    "Fairbairn",
    "Fontaine",
    "Gallagher",
    "Ghosh",
    "Halloran",
    "Hidalgo",
    "Ibarra",
    "Ingram",
    "Jarosz",
    "Jimenez",
    "Kowalski",
    "Kirilenko",
    "Lindqvist",
    "Lockhart",
    "Marchetti",
    "Mbeki",
    "Nakamura",
    "Novak",
    "O'Brien",
    "Okonkwo",
    "Pereira",
    "Prasad",
    "Quintero",
    "Rahman",
    "Rosales",
    "Sandoval",
    "Silvestri",
    "Tanaka",
    "Thorne",
    "Ueda",
    "Valentin",
    "Vasquez",
    "Whitfield",
    "Wojcik",
    "Xiong",
    "Yamada",
    "Zambrano",
    "Zielinski",
];

const COMPANY_HEAD: [&str; 24] = [
    "Alder",
    "Beacon",
    "Cobalt",
    "Driftwood",
    "Everline",
    "Foundry",
    "Granite",
    "Harbour",
    "Ironwood",
    "Junction",
    "Kestrel",
    "Lantern",
    "Meridian",
    "Northgate",
    "Overland",
    "Pinegrove",
    "Quarry",
    "Redwood",
    "Saltmarsh",
    "Tidewater",
    "Umberline",
    "Vantage",
    "Westbrook",
    "Yarrow",
];

const COMPANY_TAIL: [&str; 12] = [
    "Analytics",
    "Freight",
    "Logistics",
    "Systems",
    "Foods",
    "Labs",
    "Robotics",
    "Media",
    "Textiles",
    "Brewing",
    "Instruments",
    "Consulting",
];

const CITY: [&str; 32] = [
    "Adelaide",
    "Antwerp",
    "Bergen",
    "Bristol",
    "Cordoba",
    "Dunedin",
    "Eindhoven",
    "Fukuoka",
    "Galway",
    "Gothenburg",
    "Halifax",
    "Innsbruck",
    "Jaipur",
    "Kaunas",
    "Leuven",
    "Ljubljana",
    "Malmo",
    "Nagoya",
    "Oaxaca",
    "Portland",
    "Quito",
    "Rotterdam",
    "Salvador",
    "Sapporo",
    "Tampere",
    "Trieste",
    "Uppsala",
    "Utrecht",
    "Valencia",
    "Wellington",
    "Yogyakarta",
    "Zagreb",
];

const COUNTRY: [&str; 16] = [
    "Argentina",
    "Australia",
    "Belgium",
    "Brazil",
    "Canada",
    "Denmark",
    "Finland",
    "Ireland",
    "Japan",
    "Kenya",
    "Mexico",
    "Netherlands",
    "Norway",
    "Portugal",
    "Uruguay",
    "Vietnam",
];

/// Deliberately the same order as `COUNTRY`, so a row with both columns is not
/// obviously incoherent. It is not guaranteed coherent either — nothing here
/// knows that two columns belong together — and that limit is stated in the
/// module docs rather than papered over.
const COUNTRY_CODE: [&str; 16] = [
    "AR", "AU", "BE", "BR", "CA", "DK", "FI", "IE", "JP", "KE", "MX", "NL", "NO", "PT", "UY", "VN",
];

const REGION: [&str; 12] = [
    "Aberdeenshire",
    "Bavaria",
    "Catalonia",
    "Drenthe",
    "Emilia",
    "Flanders",
    "Gelderland",
    "Hokkaido",
    "Jalisco",
    "Kanto",
    "Limburg",
    "Ontario",
];

const STREET: [&str; 24] = [
    "Alder Lane",
    "Beckett Row",
    "Carlisle Street",
    "Dunmore Way",
    "Elmfield Road",
    "Fennel Court",
    "Granary Walk",
    "Hawthorn Close",
    "Ivyleaf Terrace",
    "Juniper Rise",
    "Kilburn Street",
    "Larkspur Avenue",
    "Marlow Crescent",
    "Netherby Road",
    "Orchard Gate",
    "Pembroke Place",
    "Quayside Walk",
    "Rushmere Drive",
    "Sedgefield Lane",
    "Thornbury Road",
    "Ulverston Way",
    "Vestry Court",
    "Wharfedale Road",
    "Yewtree Close",
];

/// For the things a schema names that are not people: queues, projects, plans,
/// tags, environments. 666 columns in the corpus end in `name` and most of
/// them mean this rather than a person.
const THING: [&str; 32] = [
    "Aurora",
    "Bramble",
    "Cascade",
    "Cinder",
    "Driftwood",
    "Ember",
    "Fathom",
    "Fernway",
    "Glimmer",
    "Halcyon",
    "Hollow",
    "Ivory",
    "Juniper",
    "Kindling",
    "Lantern",
    "Loam",
    "Marble",
    "Nimbus",
    "Orchard",
    "Pebble",
    "Quill",
    "Ridge",
    "Saffron",
    "Slate",
    "Thicket",
    "Trellis",
    "Umbra",
    "Verdant",
    "Willow",
    "Winnow",
    "Yonder",
    "Zephyr",
];

const TITLE: [&str; 24] = [
    "Quarterly revenue review",
    "Onboarding checklist",
    "Incident postmortem",
    "Migration runbook",
    "Warehouse capacity plan",
    "Customer retention notes",
    "Release announcement",
    "Security review findings",
    "Hiring loop feedback",
    "Vendor comparison",
    "Roadmap for next quarter",
    "Support escalation policy",
    "Data retention proposal",
    "Pricing experiment results",
    "Accessibility audit",
    "Backup restore drill",
    "Cost breakdown by team",
    "Partner integration brief",
    "Field trial summary",
    "Inventory reconciliation",
    "Latency investigation",
    "Design critique notes",
    "Compliance questionnaire",
    "Weekly operations digest",
];

const SENTENCE: [&str; 24] = [
    "Filed during the weekly review and left open for follow-up.",
    "Superseded by the newer revision but kept for reference.",
    "Applies to every region except the two under migration.",
    "Raised by support and confirmed by the on-call engineer.",
    "Held back until the dependent service finishes rolling out.",
    "Approved with a note about the reporting deadline.",
    "Reproduced twice on staging and once in production.",
    "Written up after the outage and shared with the team.",
    "Counts toward the quarterly target for the retail segment.",
    "Waiting on the vendor to confirm the revised delivery date.",
    "Replaces the manual process that ran every Friday.",
    "Verified against the archived export from last month.",
    "Deferred until the schema change lands in the next release.",
    "Trimmed down from the longer draft after the review.",
    "Flagged for a second look before the audit begins.",
    "Covers only the accounts created after the cutover.",
    "Attached to the ticket so the history stays in one place.",
    "Marked complete once the checklist finished cleanly.",
    "Left in place while the replacement is being trialled.",
    "Reconciled against the ledger with no discrepancy found.",
    "Escalated after the second missed response window.",
    "Recorded here because the original channel is read-only.",
    "Scheduled to run overnight when the load is lowest.",
    "Kept short on purpose so it fits in the summary view.",
];

const SLUG_WORD: [&str; 24] = [
    "annual",
    "backup",
    "billing",
    "carrier",
    "channel",
    "cutover",
    "dispatch",
    "export",
    "fallback",
    "gateway",
    "handoff",
    "inbound",
    "invoice",
    "ledger",
    "manifest",
    "outbound",
    "payload",
    "quarterly",
    "refund",
    "rollout",
    "shipment",
    "transfer",
    "vendor",
    "warehouse",
];

const PATH_HEAD: [&str; 8] = [
    "app/models",
    "config/environments",
    "lib/tasks",
    "public/uploads",
    "src/components",
    "storage/archive",
    "var/log",
    "workspace/reports",
];

const FILE_STEM: [&str; 16] = [
    "annual-report",
    "audit-log",
    "backup",
    "balance-sheet",
    "customer-list",
    "delivery-note",
    "invoice",
    "manifest",
    "packing-slip",
    "payroll",
    "purchase-order",
    "receipt",
    "shipment",
    "statement",
    "timesheet",
    "warehouse-scan",
];

const EXTENSION: [&str; 12] = [
    "pdf", "csv", "png", "jpg", "zip", "json", "xml", "txt", "xlsx", "docx", "webp", "svg",
];

const MIME: [&str; 12] = [
    "application/pdf",
    "text/csv",
    "image/png",
    "image/jpeg",
    "application/zip",
    "application/json",
    "application/xml",
    "text/plain",
    "application/vnd.ms-excel",
    "text/html",
    "image/webp",
    "image/svg+xml",
];

const STATE: [&str; 12] = [
    "pending",
    "active",
    "queued",
    "running",
    "succeeded",
    "failed",
    "cancelled",
    "expired",
    "archived",
    "draft",
    "published",
    "suspended",
];

const ACTION: [&str; 12] = [
    "create", "update", "delete", "publish", "archive", "restore", "approve", "reject", "assign",
    "import", "export", "refund",
];

const METHOD: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

const SCOPE: [&str; 8] = [
    "read",
    "write",
    "admin",
    "openid",
    "profile",
    "email",
    "offline_access",
    "read:packages",
];

const PROVIDER: [&str; 8] = [
    "github",
    "gitlab",
    "google",
    "okta",
    "azuread",
    "bitbucket",
    "keycloak",
    "saml",
];

const BRANCH: [&str; 8] = [
    "main",
    "develop",
    "release/2.4",
    "hotfix/billing",
    "feature/import-csv",
    "feature/audit-trail",
    "chore/dependencies",
    "spike/latency",
];

const CURRENCY: [&str; 12] = [
    "USD", "EUR", "GBP", "JPY", "CAD", "AUD", "CHF", "SEK", "NOK", "BRL", "INR", "MXN",
];

const LOCALE: [&str; 12] = [
    "en-US", "en-GB", "de-DE", "fr-FR", "es-ES", "pt-BR", "nl-NL", "sv-SE", "ja-JP", "ko-KR",
    "zh-CN", "it-IT",
];

const TIMEZONE: [&str; 12] = [
    "UTC",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Madrid",
    "America/New_York",
    "America/Chicago",
    "America/Sao_Paulo",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Asia/Kolkata",
    "Australia/Sydney",
    "Pacific/Auckland",
];

const COLOR: [&str; 12] = [
    "#1f2937", "#2563eb", "#0f766e", "#b91c1c", "#c2410c", "#a16207", "#4d7c0f", "#0369a1",
    "#6d28d9", "#be185d", "#334155", "#065f46",
];

/// `utf8` rather than the more usual `utf-8` on purpose: a carry is appended
/// as `-N`, and `utf` carrying 8 would be the same string as `utf-8` carrying
/// nothing. This list never carries — `Encoding` declines instead — but the
/// test that enforces the property does not know that, and an entry that is
/// only safe because of a branch elsewhere is the kind of coupling this
/// project keeps finding and regretting.
const ENCODING: [&str; 8] = [
    "utf8", "gzip", "base64", "json", "identity", "deflate", "binary", "ascii",
];

const CLASS_NAME: [&str; 16] = [
    "User",
    "Project",
    "Invoice",
    "Shipment",
    "Account",
    "Comment",
    "Attachment",
    "Subscription",
    "Webhook",
    "Notification",
    "AuditEvent",
    "Membership",
    "Payment",
    "Document",
    "Schedule",
    "Warehouse",
];

const USER_AGENT: [&str; 6] = [
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0 Safari/537.36",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64; rv:126.0) Gecko/20100101 Firefox/126.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 Version/17.5 Mobile Safari/604.1",
    "curl/8.6.0",
    "python-requests/2.31.0",
];

/// Reserved by RFC 2606 for exactly this purpose, and therefore incapable of
/// reaching a real inbox or a real web server. Not decoration: generated data
/// ends up in staging systems, and staging systems send mail.
const DOMAIN: [&str; 3] = ["example.com", "example.org", "example.net"];

/// Which noun a column name lands on, if any.
///
/// Three passes, most specific first: the whole name, its last two segments,
/// then its last one. A name that matches nothing returns `None` and is filled
/// the way everything used to be.
pub fn of(column: &str) -> Option<Noun> {
    let name = column.trim_matches('_').to_ascii_lowercase();
    if let Some(noun) = exact(&name) {
        return Some(noun);
    }

    let parts: Vec<&str> = name.split('_').filter(|p| !p.is_empty()).collect();
    let last = *parts.last()?;

    if parts.len() >= 2 {
        let pair = format!("{}_{}", parts[parts.len() - 2], last);
        if let Some(noun) = exact(&pair) {
            return Some(noun);
        }
        // `name` on its own is ambiguous and its qualifier settles it: a
        // `file_name` and a `first_name` and a `queue_name` are three
        // different things and 666 columns in the corpus end this way.
        if last == "name" {
            return Some(qualified_name(parts[parts.len() - 2]));
        }
    }

    tail(last)
}

/// Names that only mean one thing when taken whole, including the ones written
/// without underscores — `userid`, `displayname`, `mimetype` — which are how
/// Mattermost and Synapse spell them.
fn exact(name: &str) -> Option<Noun> {
    Some(match name {
        "content_type" | "mime_type" | "mimetype" | "media_type" => Noun::MimeType,
        "user_agent" | "useragent" | "browser" => Noun::UserAgent,
        "ip_address" | "ipaddress" | "remote_ip" | "client_ip" | "ip" | "inet" => Noun::Ip,
        "email_address" | "emailaddress" | "e_mail" => Noun::Email,
        "street_address" | "address_line_1" | "address1" | "street" => Noun::Street,
        "postal_code" | "postcode" | "zip_code" | "zip" | "zipcode" => Noun::PostalCode,
        "country_code" | "countrycode" | "iso_code" | "iso_country" => Noun::CountryCode,
        "currency_code" | "currencycode" => Noun::Currency,
        "phone_number" | "phonenumber" | "telephone" | "mobile" | "msisdn" => Noun::Phone,
        "display_name" | "displayname" | "full_name" | "fullname" | "real_name" => Noun::PersonName,
        "first_name" | "firstname" | "given_name" | "givenname" | "forename" => Noun::GivenName,
        "last_name" | "lastname" | "family_name" | "familyname" | "surname" => Noun::FamilyName,
        "user_name" | "username" | "login" | "handle" | "nickname" | "nick" | "screen_name" => {
            Noun::Username
        }
        "file_name" | "filename" | "basename" | "original_filename" => Noun::FileName,
        "host_name" | "hostname" | "fqdn" | "server_name" => Noun::Host,
        "class_name" | "classname" | "model_type" | "target_type" | "owner_type"
        | "resource_type" | "subject_type" | "commentable_type" | "noteable_type" => {
            Noun::ClassName
        }
        "time_zone" | "timezone" | "tz" => Noun::Timezone,
        "http_method" | "request_method" | "verb" => Noun::Method,
        "user_id" | "userid" | "channel_id" | "channelid" | "team_id" | "teamid" | "room_id"
        | "creator_id" | "creatorid" | "remote_id" | "remoteid" | "device_id" | "session_id" => {
            Noun::Ident
        }
        _ => return None,
    })
}

/// What `*_name` means, decided by the word in front of it.
fn qualified_name(qualifier: &str) -> Noun {
    match qualifier {
        "first" | "given" | "fore" => Noun::GivenName,
        "last" | "family" | "sur" | "sir" => Noun::FamilyName,
        "full" | "real" | "display" | "author" | "owner" | "contact" | "person" | "customer"
        | "recipient" | "sender" | "assignee" | "member" => Noun::PersonName,
        "user" | "login" | "account" | "screen" | "nick" => Noun::Username,
        "file" | "attachment" | "upload" | "document" | "asset" | "original" => Noun::FileName,
        "host" | "server" | "node" | "machine" | "worker" | "instance" | "pod" => Noun::Host,
        "domain" | "site" => Noun::Domain,
        "company" | "organisation" | "organization" | "org" | "vendor" | "supplier" | "brand"
        | "merchant" | "tenant" | "workspace" => Noun::Company,
        "city" | "town" | "locality" => Noun::City,
        "country" => Noun::Country,
        "region" | "state" | "province" | "county" => Noun::Region,
        "class" | "model" | "type" | "entity" => Noun::ClassName,
        "table" | "column" | "index" | "schema" | "database" | "queue" | "topic" | "job"
        | "task" | "role" | "permission" | "scope" | "setting" | "config" | "flag" | "feature"
        | "metric" | "field" | "key" | "variable" | "function" | "extension" | "bucket" => {
            Noun::Slug
        }
        // A project, a plan, a channel, a tag — the ordinary case, and the one
        // 666 corpus columns are mostly made of.
        _ => Noun::Label,
    }
}

/// What the final segment of a name means on its own.
fn tail(last: &str) -> Option<Noun> {
    Some(match last {
        "name" => Noun::Label,
        "email" | "mail" => Noun::Email,
        "username" => Noun::Username,
        "company" | "organisation" | "organization" | "org" | "vendor" | "supplier" => {
            Noun::Company
        }
        "city" | "town" => Noun::City,
        "country" => Noun::Country,
        "region" | "province" | "county" => Noun::Region,
        "url" | "uri" | "link" | "href" | "endpoint" | "webhook" | "callback" | "redirect" => {
            Noun::Url
        }
        "host" | "hostname" => Noun::Host,
        "domain" => Noun::Domain,
        "path" | "directory" | "folder" | "location" | "prefix" => Noun::Path,
        "file" | "filename" | "attachment" => Noun::FileName,
        "extension" | "ext" | "format" => Noun::Extension,
        "slug" | "identifier" | "namespace" | "handle" | "alias" => Noun::Slug,
        "title" | "subject" | "headline" | "summary" | "label" | "caption" => Noun::Title,
        "description" | "message" | "reason" | "comment" | "note" | "notes" | "body" | "text"
        | "content" | "error" | "failure" | "details" | "detail" | "excerpt" => Noun::Sentence,
        "html" => Noun::Html,
        "state" | "status" | "phase" | "stage" | "result" | "outcome" | "disposition" => {
            Noun::State
        }
        "action" | "operation" | "event" | "activity" => Noun::Action,
        // 290 columns end in `type` and most are not class names —
        // `event_type`, `notification_type`, `data_type` hold lowercase words
        // like `invoice_refunded`. A slug is what those look like; the ones
        // that really are class names (`model_type`, `target_type`) are named
        // exactly, above.
        "type" | "kind" | "category" => Noun::Slug,
        "method" | "verb" => Noun::Method,
        "scope" | "scopes" | "permission" | "audience" => Noun::Scope,
        "provider" | "strategy" | "issuer" => Noun::Provider,
        "branch" | "ref" | "revision" => Noun::Branch,
        "version" | "release" | "tag" => Noun::Version,
        "currency" => Noun::Currency,
        "locale" | "language" | "lang" => Noun::Locale,
        "timezone" | "tz" => Noun::Timezone,
        "color" | "colour" | "hex" => Noun::Color,
        "encoding" | "charset" | "compression" => Noun::Encoding,
        "uuid" | "guid" => Noun::Uuid,
        "ip" | "addr" => Noun::Ip,
        "id" | "xid" | "userid" | "channelid" | "teamid" | "remoteid" | "creatorid" => Noun::Ident,
        // Opaque bytes rendered as text. The widths are the real ones: a SHA-1
        // is 40 hex characters and a SHA-256 is 64, and a column declared
        // `char(40)` holding 64 of them is a rejected row.
        "sha" | "sha1" | "signature" => Noun::Hex(40),
        "digest" | "checksum" | "sha256" | "fingerprint" => Noun::Hex(64),
        "hash" | "etag" | "iv" | "nonce" | "salt" => Noun::Hex(32),
        "token" | "secret" | "key" | "credential" | "password" | "encrypted" | "cipher" => {
            Noun::Hex(48)
        }
        _ => return None,
    })
}

/// The digits of `step` read against a set of list lengths, plus whatever is
/// left over.
///
/// This is what makes a realistic value safe on a unique column. Reading the
/// step as an odometer over the lists means `(digits, carry)` reconstructs it
/// exactly, so two different steps cannot produce the same digits, and the
/// carry is `None` until the lists are genuinely exhausted, which for a
/// forty-eight by forty-eight pair is 2,304 rows in.
fn indices(rng: &mut ChaCha8Rng, step: Option<usize>, lengths: &[usize]) -> (Vec<usize>, usize) {
    match step {
        // Nothing has to be distinct, so roll.
        None => (lengths.iter().map(|n| rng.gen_range(0..*n)).collect(), 0),
        Some(step) => {
            let mut digits = Vec::with_capacity(lengths.len());
            let mut rest = step;
            let mut mix = 0usize;
            for (place, length) in lengths.iter().enumerate() {
                let raw = rest % length;
                rest /= length;
                // Each digit is turned by the ones before it. Without this the
                // slow digits sit still — the first forty-eight emails were
                // all `<something>.achebe@`, and the first twenty-four slugs
                // all ended `-annual` — because a plain odometer only moves
                // its second wheel once the first has gone all the way round.
                //
                // Still one-to-one: `mix` depends only on the raw digits
                // already read, so peeling them off in order recovers every
                // one of them, and two steps therefore cannot land on the same
                // digits. The nine-thousand-step test is the check on that.
                digits.push((raw + mix) % length);
                mix = mix
                    .wrapping_mul(7)
                    .wrapping_add(raw.wrapping_mul(place + 3))
                    .wrapping_add(1);
            }
            (digits, rest)
        }
    }
}

/// The carry, spelled out, or nothing at all while it is zero.
///
/// Hyphenated rather than run straight on, so that reading the value backwards
/// finds one unambiguous split. `Portland` plus a carry of `12` and `Portland1`
/// plus a carry of `2` would otherwise be the same string, and the second of
/// those is only impossible because no list happens to hold an entry ending in
/// a digit — a coincidence, and one a test now refuses to rely on.
fn suffix(carry: usize) -> String {
    if carry == 0 {
        String::new()
    } else {
        format!("-{carry}")
    }
}

/// A value for this noun, or `None` if it cannot produce one that fits.
///
/// `step` is `Some` exactly when the value has to be distinct from every other
/// step's, and `limit` is the tightest of the column's declared width and any
/// CHECK ceiling on it.
pub fn render(
    noun: Noun,
    rng: &mut ChaCha8Rng,
    step: Option<usize>,
    identity: usize,
    limit: Option<usize>,
) -> Option<String> {
    // A column that describes a person reads the row's own position when it is
    // not obliged to be distinct, so that `first_name`, `last_name`, `email`
    // and `display_name` in one row describe the same person instead of four
    // different ones. Everything else keeps rolling, because a `status` column
    // that walked its list in order would be visibly synthetic in a way a
    // random one is not.
    let distinct = step.is_some();
    let step = if describes_a_person(noun) {
        Some(step.unwrap_or(identity))
    } else {
        step
    };
    // Agreement is available exactly when distinctness was not demanded.
    let text = build(noun, rng, step, describes_a_person(noun) && !distinct)?;
    // The last word on it. Every branch above tries to fit, and this is what
    // makes "tries" into "did" — a value that does not fit is refused here and
    // the caller falls back to the generator built for tight columns.
    match limit {
        Some(limit) if text.chars().count() > limit => None,
        _ => Some(text),
    }
}

fn build(noun: Noun, rng: &mut ChaCha8Rng, step: Option<usize>, coherent: bool) -> Option<String> {
    Some(match noun {
        Noun::GivenName if coherent => {
            // The same two wheels every other person-shaped noun reads, so
            // that the carry lands in the same place too: `Ada-1` beside
            // `Ada Ballard` is exactly the disagreement this is here to stop.
            let (i, carry) = indices(rng, step, &[GIVEN.len(), FAMILY.len()]);
            format!("{}{}", GIVEN[i[0]], suffix(carry))
        }
        Noun::GivenName => {
            let (i, carry) = indices(rng, step, &[GIVEN.len()]);
            format!("{}{}", GIVEN[i[0]], suffix(carry))
        }
        Noun::FamilyName if coherent => {
            // The second wheel, which is where `PersonName`, `Email` and
            // `Username` keep the surname — so a row's `last_name` is the same
            // surname its `display_name` and its email address use.
            let (i, carry) = indices(rng, step, &[GIVEN.len(), FAMILY.len()]);
            format!("{}{}", FAMILY[i[1]], suffix(carry))
        }
        Noun::FamilyName => {
            // Under a unique key, where being distinct comes before agreeing
            // with the column next door: reading the second wheel would repeat
            // itself forty-eight times before it turned.
            let (i, carry) = indices(rng, step, &[FAMILY.len()]);
            format!("{}{}", FAMILY[i[0]], suffix(carry))
        }
        Noun::PersonName => {
            let (i, carry) = indices(rng, step, &[GIVEN.len(), FAMILY.len()]);
            format!("{} {}{}", GIVEN[i[0]], FAMILY[i[1]], suffix(carry))
        }
        Noun::Username => {
            let (i, carry) = indices(rng, step, &[GIVEN.len(), FAMILY.len()]);
            // The whole given name rather than its initial. An initial throws
            // away exactly the information that makes two steps different —
            // `Ada Achebe` and `Amara Achebe` both became `aachebe` — and a
            // unique column then repeated on its second row.
            format!(
                "{}.{}{}",
                slugify(GIVEN[i[0]]),
                slugify(FAMILY[i[1]]),
                suffix(carry)
            )
        }
        Noun::Email => {
            // Two wheels, not three, so the carry falls where `PersonName` and
            // `GivenName` put theirs. The domain is chosen from the two digits
            // rather than being a wheel of its own for the same reason.
            let (i, carry) = indices(rng, step, &[GIVEN.len(), FAMILY.len()]);
            format!(
                "{}.{}{}@{}",
                slugify(GIVEN[i[0]]),
                slugify(FAMILY[i[1]]),
                suffix(carry),
                DOMAIN[(i[0] + i[1]) % DOMAIN.len()]
            )
        }
        Noun::Company => {
            let (i, carry) = indices(rng, step, &[COMPANY_HEAD.len(), COMPANY_TAIL.len()]);
            format!(
                "{} {}{}",
                COMPANY_HEAD[i[0]],
                COMPANY_TAIL[i[1]],
                suffix(carry)
            )
        }
        Noun::City => pick(rng, step, &CITY),
        Noun::Country => pick(rng, step, &COUNTRY),
        Noun::CountryCode => {
            // Two characters is the whole point of the column, so a carry has
            // nowhere to go: past sixteen distinct values this declines and
            // the plain generator, which knows how to fill a `char(2)`, takes
            // over.
            let (i, carry) = indices(rng, step, &[COUNTRY_CODE.len()]);
            if carry > 0 {
                return None;
            }
            COUNTRY_CODE[i[0]].to_string()
        }
        Noun::Region => pick(rng, step, &REGION),
        Noun::Street => {
            let (i, carry) = indices(rng, step, &[240, STREET.len()]);
            format!("{} {}{}", i[0] + 1, STREET[i[1]], suffix(carry))
        }
        Noun::PostalCode => {
            let (i, carry) = indices(rng, step, &[90_000]);
            if carry > 0 {
                return None;
            }
            format!("{:05}", i[0] + 10_000)
        }
        Noun::Phone => {
            // 555-0100 through 555-0199 is the block reserved for fiction, and
            // it holds exactly one hundred numbers. Past that this declines
            // rather than inventing a number that rings somebody.
            let (i, carry) = indices(rng, step, &[100]);
            if carry > 0 {
                return None;
            }
            format!("+1-555-{:04}", i[0] + 100)
        }
        Noun::Url => {
            let (i, carry) = indices(rng, step, &[DOMAIN.len(), SLUG_WORD.len(), SLUG_WORD.len()]);
            format!(
                "https://{}/{}/{}{}",
                DOMAIN[i[0]],
                SLUG_WORD[i[1]],
                SLUG_WORD[i[2]],
                suffix(carry)
            )
        }
        Noun::Host => {
            let (i, carry) = indices(rng, step, &[SLUG_WORD.len(), DOMAIN.len()]);
            format!("{}{}.{}", SLUG_WORD[i[0]], suffix(carry), DOMAIN[i[1]])
        }
        Noun::Domain => {
            let (i, carry) = indices(rng, step, &[DOMAIN.len()]);
            if carry == 0 {
                DOMAIN[i[0]].to_string()
            } else {
                // A subdomain, because a hostname cannot begin with a hyphen
                // and the carry has to go somewhere legal.
                format!("s{carry}.{}", DOMAIN[i[0]])
            }
        }
        Noun::Path => {
            let (i, carry) = indices(
                rng,
                step,
                &[PATH_HEAD.len(), FILE_STEM.len(), EXTENSION.len()],
            );
            format!(
                "{}/{}{}.{}",
                PATH_HEAD[i[0]],
                FILE_STEM[i[1]],
                suffix(carry),
                EXTENSION[i[2]]
            )
        }
        Noun::FileName => {
            let (i, carry) = indices(rng, step, &[FILE_STEM.len(), EXTENSION.len()]);
            format!("{}{}.{}", FILE_STEM[i[0]], suffix(carry), EXTENSION[i[1]])
        }
        Noun::Extension => {
            let (i, carry) = indices(rng, step, &[EXTENSION.len()]);
            if carry > 0 {
                return None;
            }
            EXTENSION[i[0]].to_string()
        }
        Noun::MimeType => {
            let (i, carry) = indices(rng, step, &[MIME.len()]);
            if carry > 0 {
                return None;
            }
            MIME[i[0]].to_string()
        }
        Noun::Slug => {
            let (i, carry) = indices(rng, step, &[SLUG_WORD.len(), SLUG_WORD.len()]);
            format!("{}-{}{}", SLUG_WORD[i[0]], SLUG_WORD[i[1]], suffix(carry))
        }
        Noun::Title => pick(rng, step, &TITLE),
        Noun::Sentence => pick(rng, step, &SENTENCE),
        Noun::Html => {
            let (i, carry) = indices(rng, step, &[SENTENCE.len()]);
            format!("<p>{}{}</p>", SENTENCE[i[0]], suffix(carry))
        }
        Noun::Label => {
            let (i, carry) = indices(rng, step, &[THING.len(), COMPANY_TAIL.len()]);
            format!("{} {}{}", THING[i[0]], COMPANY_TAIL[i[1]], suffix(carry))
        }
        Noun::ClassName => pick(rng, step, &CLASS_NAME),
        Noun::State => pick(rng, step, &STATE),
        Noun::Action => pick(rng, step, &ACTION),
        Noun::Method => {
            let (i, carry) = indices(rng, step, &[METHOD.len()]);
            if carry > 0 {
                return None;
            }
            METHOD[i[0]].to_string()
        }
        Noun::Scope => pick(rng, step, &SCOPE),
        Noun::Provider => pick(rng, step, &PROVIDER),
        Noun::Branch => pick(rng, step, &BRANCH),
        Noun::Version => {
            let (i, carry) = indices(rng, step, &[6, 12, 20]);
            format!("{}.{}.{}{}", i[0], i[1], i[2], suffix(carry))
            // `suffix` hyphenates, so this reads as a prerelease tag rather
            // than running into the patch number.
        }
        Noun::Currency => {
            let (i, carry) = indices(rng, step, &[CURRENCY.len()]);
            if carry > 0 {
                return None;
            }
            CURRENCY[i[0]].to_string()
        }
        Noun::Locale => {
            let (i, carry) = indices(rng, step, &[LOCALE.len()]);
            if carry > 0 {
                return None;
            }
            LOCALE[i[0]].to_string()
        }
        Noun::Timezone => {
            let (i, carry) = indices(rng, step, &[TIMEZONE.len()]);
            if carry > 0 {
                return None;
            }
            TIMEZONE[i[0]].to_string()
        }
        Noun::Color => {
            let (i, carry) = indices(rng, step, &[COLOR.len()]);
            if carry > 0 {
                return None;
            }
            COLOR[i[0]].to_string()
        }
        Noun::UserAgent => pick(rng, step, &USER_AGENT),
        Noun::Encoding => {
            let (i, carry) = indices(rng, step, &[ENCODING.len()]);
            if carry > 0 {
                return None;
            }
            ENCODING[i[0]].to_string()
        }
        // TEST-NET-1, reserved by RFC 5737 for documentation and examples, so
        // no generated row names a host that belongs to somebody.
        Noun::Ip => {
            const BLOCK: [&str; 3] = ["192.0.2", "198.51.100", "203.0.113"];
            let (i, carry) = indices(rng, step, &[254, BLOCK.len()]);
            if carry > 0 {
                return None;
            }
            format!("{}.{}", BLOCK[i[1]], i[0] + 1)
        }
        Noun::Uuid => {
            let head: u64 = rng.gen();
            let tail: u64 = match step {
                Some(step) => step as u64,
                None => rng.gen(),
            };
            let hex = format!("{head:016x}{tail:016x}");
            format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            )
        }
        Noun::Ident => hex_of(26, rng, step)?,
        Noun::Hex(width) => hex_of(width, rng, step)?,
    })
}

/// Whether two of these in one row ought to agree with each other.
///
/// The lists are read positionally, so `GivenName` and `Email` at the same
/// odometer position name the same person by construction — there is no
/// cross-column bookkeeping here, only two readings of one number.
fn describes_a_person(noun: Noun) -> bool {
    matches!(
        noun,
        Noun::GivenName | Noun::FamilyName | Noun::PersonName | Noun::Username | Noun::Email
    )
}

/// One entry from a list, with a counter appended once the list runs out.
fn pick(rng: &mut ChaCha8Rng, step: Option<usize>, list: &[&str]) -> String {
    let (i, carry) = indices(rng, step, &[list.len()]);
    format!("{}{}", list[i[0]], suffix(carry))
}

/// `width` hex characters, with the step spelled out in the last of them.
///
/// The step goes in the tail rather than being hashed in, because a unique
/// column needs distinctness that can be shown rather than distinctness that
/// is overwhelmingly likely. Where the step will not fit in the width at all,
/// this declines.
fn hex_of(width: usize, rng: &mut ChaCha8Rng, step: Option<usize>) -> Option<String> {
    let tag = match step {
        // Zero-padded to a fixed width, which is the whole of why this is
        // correct. A tag that grows with the step shortens the random prefix
        // as it does, and `<25 random chars>b` collided with
        // `<24 random chars>b0` on the 176th row of a text primary key.
        Some(step) => {
            let places = width.min(16);
            let text = format!("{step:0places$x}");
            if text.len() > places {
                // More rows than the column has room to number.
                return None;
            }
            text
        }
        None => String::new(),
    };
    if tag.len() > width {
        return None;
    }
    let mut out = String::with_capacity(width);
    for _ in 0..width - tag.len() {
        out.push(char::from_digit(rng.gen_range(0..16), 16)?);
    }
    out.push_str(&tag);
    Some(out)
}

/// A name reduced to what an email address or a login can hold.
fn slugify(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    fn rng() -> ChaCha8Rng {
        ChaCha8Rng::seed_from_u64(9)
    }

    /// Every list in this file, for the properties that hold across all of
    /// them. Kept as one place so a list added later cannot quietly skip them.
    fn all_lists() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            ("GIVEN", GIVEN.to_vec()),
            ("FAMILY", FAMILY.to_vec()),
            ("COMPANY_HEAD", COMPANY_HEAD.to_vec()),
            ("COMPANY_TAIL", COMPANY_TAIL.to_vec()),
            ("CITY", CITY.to_vec()),
            ("COUNTRY", COUNTRY.to_vec()),
            ("COUNTRY_CODE", COUNTRY_CODE.to_vec()),
            ("REGION", REGION.to_vec()),
            ("STREET", STREET.to_vec()),
            ("THING", THING.to_vec()),
            ("TITLE", TITLE.to_vec()),
            ("SENTENCE", SENTENCE.to_vec()),
            ("SLUG_WORD", SLUG_WORD.to_vec()),
            ("PATH_HEAD", PATH_HEAD.to_vec()),
            ("FILE_STEM", FILE_STEM.to_vec()),
            ("EXTENSION", EXTENSION.to_vec()),
            ("MIME", MIME.to_vec()),
            ("STATE", STATE.to_vec()),
            ("ACTION", ACTION.to_vec()),
            ("METHOD", METHOD.to_vec()),
            ("SCOPE", SCOPE.to_vec()),
            ("PROVIDER", PROVIDER.to_vec()),
            ("BRANCH", BRANCH.to_vec()),
            ("CURRENCY", CURRENCY.to_vec()),
            ("LOCALE", LOCALE.to_vec()),
            ("TIMEZONE", TIMEZONE.to_vec()),
            ("COLOR", COLOR.to_vec()),
            ("ENCODING", ENCODING.to_vec()),
            ("CLASS_NAME", CLASS_NAME.to_vec()),
            ("USER_AGENT", USER_AGENT.to_vec()),
            ("DOMAIN", DOMAIN.to_vec()),
        ]
    }

    #[test]
    fn every_word_is_ascii() {
        // `checks` reads `octet_length(col) <= N` as the same ceiling as
        // `char_length(col) <= N`, which is true only while this holds. One
        // accented surname would make that silently false, and the rows it
        // produced would be rejected by a constraint the tool believed it had
        // satisfied.
        for (name, list) in all_lists() {
            for word in list {
                assert!(
                    word.is_ascii(),
                    "{name} contains a non-ASCII entry: {word:?}"
                );
            }
        }
    }

    #[test]
    fn no_list_holds_two_words_that_differ_only_in_case() {
        // A unique index over `lower(col)` is read as a unique key over `col`
        // on the strength of the generated values being distinct after
        // lowercasing. Two entries differing only in case would break that
        // without breaking anything visible.
        for (name, list) in all_lists() {
            let mut seen = std::collections::BTreeSet::new();
            for word in list {
                assert!(
                    seen.insert(word.to_ascii_lowercase()),
                    "{name} holds two entries that are the same lowercased: {word:?}"
                );
            }
        }
    }

    #[test]
    fn a_unique_column_gets_a_distinct_value_every_time() {
        // The property the whole odometer exists for. Every noun, a thousand
        // steps, no repeats, and no reliance on the values being unlikely to
        // collide, which is not what a unique constraint asks.
        let nouns = [
            Noun::GivenName,
            Noun::FamilyName,
            Noun::PersonName,
            Noun::Username,
            Noun::Email,
            Noun::Company,
            Noun::City,
            Noun::Country,
            Noun::Region,
            Noun::Street,
            Noun::Url,
            Noun::Host,
            Noun::Domain,
            Noun::Path,
            Noun::FileName,
            Noun::Slug,
            Noun::Title,
            Noun::Sentence,
            Noun::Html,
            Noun::Label,
            Noun::ClassName,
            Noun::State,
            Noun::Action,
            Noun::Scope,
            Noun::Provider,
            Noun::Branch,
            Noun::Version,
            Noun::UserAgent,
            Noun::Uuid,
            Noun::Ident,
            Noun::Hex(40),
        ];
        for noun in nouns {
            let mut seen = std::collections::BTreeSet::new();
            // Far enough past the product of the list lengths that the carry
            // is genuinely exercised: an email is 48 by 48 by 3, so anything
            // under 6,912 rows never reaches it and never tests it.
            for step in 0..9000 {
                let value = render(noun, &mut rng(), Some(step), 0, None)
                    .unwrap_or_else(|| panic!("{noun:?} declined at step {step} with no limit"));
                assert!(
                    seen.insert(value.to_ascii_lowercase()),
                    "{noun:?} repeated itself at step {step}"
                );
            }
        }
    }

    #[test]
    fn no_entry_ends_where_a_carry_could_be_mistaken_for_part_of_it() {
        // `suffix` hyphenates the carry, so the value splits unambiguously as
        // long as no entry already ends in a hyphen and digits. Asserted
        // rather than assumed, because `release/2.4` is already in one of
        // these lists and the next one added might end in `-2`.
        for (name, list) in all_lists() {
            for word in list {
                let tail = word.rsplit('-').next().unwrap_or(word);
                assert!(
                    !(word.contains('-')
                        && !tail.is_empty()
                        && tail.chars().all(|c| c.is_ascii_digit())),
                    "{name} holds {word:?}, which a carry could not be told apart from"
                );
            }
        }
    }

    #[test]
    fn a_column_too_narrow_for_the_value_is_declined_rather_than_truncated() {
        // Truncating `ada.lovelace@example.com` into a `varchar(8)` produces
        // `ada.love`, which is not an email address and is not distinct from
        // the next one either. Declining hands the column back to the plain
        // generator, which is built for exactly this.
        assert_eq!(render(Noun::Email, &mut rng(), None, 0, Some(8)), None);
        assert!(render(Noun::Email, &mut rng(), None, 0, Some(64)).is_some());
        // And a noun that has nowhere to put a counter says so.
        assert_eq!(
            render(Noun::CountryCode, &mut rng(), Some(99), 0, None),
            None
        );
        assert!(render(Noun::CountryCode, &mut rng(), Some(3), 0, None).is_some());
    }

    #[test]
    fn a_name_is_read_from_its_last_segments_and_not_from_a_substring() {
        assert_eq!(of("email"), Some(Noun::Email));
        assert_eq!(of("user_email"), Some(Noun::Email));
        assert_eq!(of("billing_contact_email"), Some(Noun::Email));
        assert_eq!(of("first_name"), Some(Noun::GivenName));
        assert_eq!(of("file_name"), Some(Noun::FileName));
        assert_eq!(of("queue_name"), Some(Noun::Slug));
        assert_eq!(of("project_name"), Some(Noun::Label));
        assert_eq!(of("content_type"), Some(Noun::MimeType));
        assert_eq!(of("relative_path"), Some(Noun::Path));
        assert_eq!(of("upstream_etag"), Some(Noun::Hex(32)));

        // `description_id` is a foreign key to a table of descriptions, and
        // filling it with a sentence would be worse than filling it with
        // nothing. A substring match would get this wrong.
        assert_eq!(of("description_id"), Some(Noun::Ident));
        assert_eq!(of("email_verified_at"), None);
        assert_eq!(of("weight"), None);
        assert_eq!(of(""), None);
    }

    #[test]
    fn the_columns_that_describe_a_person_describe_the_same_one() {
        // The visible half of this work. A row whose `first_name` is Ada and
        // whose `email` is `amara.adeyemi@` is worse than one that says
        // `bravo` twice, because it looks right and is not.
        for identity in 0..200usize {
            let given = render(Noun::GivenName, &mut rng(), None, identity, None).unwrap();
            let family = render(Noun::FamilyName, &mut rng(), None, identity, None).unwrap();
            let full = render(Noun::PersonName, &mut rng(), None, identity, None).unwrap();
            let email = render(Noun::Email, &mut rng(), None, identity, None).unwrap();
            let username = render(Noun::Username, &mut rng(), None, identity, None).unwrap();

            assert_eq!(full, format!("{given} {family}"), "at {identity}");
            let local = email.split('@').next().unwrap();
            assert_eq!(local, username, "at {identity}");
            assert_eq!(
                local,
                format!("{}.{}", slugify(&given), slugify(&family)),
                "at {identity}"
            );
        }
    }

    #[test]
    fn the_slow_wheels_turn_too() {
        // A plain odometer leaves its second digit alone until the first has
        // gone all the way round, which made the first forty-eight emails
        // share a surname and the first twenty-four slugs share a word. This
        // is the check that they now move.
        let surnames: std::collections::BTreeSet<String> = (0..12)
            .map(|step| render(Noun::FamilyName, &mut rng(), None, step, None).unwrap())
            .collect();
        assert!(surnames.len() > 6, "surnames barely moved: {surnames:?}");

        let slugs: std::collections::BTreeSet<String> = (0..12)
            .map(|step| {
                let slug = render(Noun::Slug, &mut rng(), Some(step), 0, None).unwrap();
                slug.split('-').next_back().unwrap().to_string()
            })
            .collect();
        assert!(slugs.len() > 6, "slug tails barely moved: {slugs:?}");
    }

    #[test]
    fn the_same_step_always_gives_the_same_value() {
        for step in [None, Some(0), Some(7), Some(5000)] {
            let first = render(Noun::Email, &mut rng(), step, 0, None);
            let again = render(Noun::Email, &mut rng(), step, 0, None);
            assert_eq!(first, again);
        }
    }

    #[test]
    fn nothing_generated_can_reach_a_real_person() {
        // Reserved ranges only: RFC 2606 for the domains, the 555-01xx block
        // for the numbers, RFC 5737 for the addresses. Generated data ends up
        // in staging systems that send mail and make requests.
        for step in 0..200usize {
            if let Some(email) = render(Noun::Email, &mut rng(), Some(step), 0, None) {
                assert!(
                    DOMAIN.iter().any(|d| email.ends_with(&format!("@{d}"))),
                    "{email} is not on a reserved domain"
                );
            }
            if let Some(phone) = render(Noun::Phone, &mut rng(), Some(step), 0, None) {
                assert!(phone.starts_with("+1-555-01"), "{phone} is not reserved");
            }
            if let Some(url) = render(Noun::Url, &mut rng(), Some(step), 0, None) {
                assert!(
                    DOMAIN
                        .iter()
                        .any(|d| url.starts_with(&format!("https://{d}/"))),
                    "{url} is not on a reserved domain"
                );
            }
        }
    }
}

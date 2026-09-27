# Tontoo SQLKit

Dependency-light SQLite-compatible engine for TontooOS. Exposes a `rusqlite`-compatible subset (`Connection`, `Statement`, `params!`, `Row`, `Transaction`) over a pure-Rust engine with streaming iterators and atomic file snapshots. Built so CoreData and other frameworks can drop `rusqlite` / `libsqlite3` without changing call sites.

Status: basis engine (not yet used anywhere). `CREATE TABLE` / `CREATE INDEX`, `INSERT` (`OR REPLACE` / `OR IGNORE`), `SELECT` with `WHERE col = ?` filters, `UPDATE`, `DELETE`, `PRAGMA`, transactions, and file persistence work. JOIN, sub-selects, triggers, views, and the full B-Tree page layout are roadmap items.

## Made for TontooOS

Explore more at https://github.com/TontooOS/Libs

Full docs: [wiki/MAIN.md](wiki/MAIN.md)

## Adding to Your Project

Add to your `Cargo.toml`:

```toml
[dependencies]
sdk = { path = "/Library/System/sdk", features = ["SQLKit"] }
```

Then at the crate root:

```rust
sdk::preinclude!();
use sqlkit::{params, Connection};
```

Direct dependency (development):

```toml
[dependencies]
sqlkit = { path = "../SQLKit" }
```

Quick start:

```rust
use sqlkit::{params, Connection};

let conn = Connection::open_in_memory().unwrap();
conn.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, title TEXT)").unwrap();
conn.execute("INSERT INTO notes (id, title) VALUES (?1, ?2)", params!["a", "Hello"]).unwrap();
let title: String = conn.query_row(
    "SELECT title FROM notes WHERE id = ?1",
    params!["a"],
    |row| row.get(0),
).unwrap();
assert_eq!(title, "Hello");
```

## License

TCL v26.1

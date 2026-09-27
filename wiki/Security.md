# Security

Threat model, input bounds, filesystem rules, and FFI guarantees. SQLKit
parses untrusted SQL text and reads untrusted database files, so every
input path is bounded and every failure surfaces as `Err`, never a panic.

## Threat Model

| Attacker capability | Defense |
|---|---|
| Malicious SQL text | Byte/statement/chain/parameter bounds, checked parsing (see [Parser.md](Parser.md)) |
| Malicious `.sqlite` file | Magic validation, checked arithmetic, cycle guards, 256 MiB cap (see [Pager.md](Pager.md)) |
| Symlinked sidecars | Symlink refusal on tmp/journal paths |
| World-readable secrets | Owner-only (`0600`) file creation |
| Malicious C caller | Null checks, panic boundary (see [FFI.md](FFI.md)) |

## Input Bounds

| Bound | Constant | Value |
|---|---|---|
| Single statement | `MAX_STATEMENT_BYTES` | 1 MiB |
| Whole batch | `MAX_BATCH_BYTES` | 8 MiB |
| Statements per batch | `MAX_BATCH_STATEMENTS` | 10_000 |
| `AND` / `OR` / `HAVING` / `UNION` / `JOIN` / `GROUP BY` chain | `MAX_NESTING_DEPTH` | 128 |
| Sub-select nesting | `MAX_NESTING_DEPTH` | 128 levels |
| Bound parameters | `MAX_PARAMS` | 999 (SQLite parity) |
| Foreign file size | `MAX_FOREIGN_FILE_BYTES` | 256 MiB |

All bounds return `Err(SqlError::Parse)` or `Err(SqlError::Unsupported)`;
`tests/security.rs` covers each bound plus garbage, truncated, and
non-UTF8 inputs.

### `params`

```rust
let p = params![1i32, "hello"];
```

Built-in `ToSql` implementations are infallible. A custom `ToSql` that
returns `Err` panics inside `params!`; such implementations must be
infallible or callers must build `Vec<Value>` by hand. Custom `ToSql`
implementations must not reenter the same `Connection` (the engine uses
`RefCell` interior mutability, which panics on reentrant borrow instead
of corrupting state).

## Unsafe Inventory

`unsafe` exists only in `src/ffi.rs` (raw-pointer contract handling). No
other module contains `unsafe`. FFI entry points catch Rust panics at the
boundary and convert them to error returns (`-4`); a panic never unwinds
into C.

## Filesystem Rules

| Rule | Implementation |
|---|---|
| Temp files | Unique `<db>.<pid>.<counter>.sqlkit-tmp` names, `O_EXCL` creation, `0600` at creation |
| Journal | Fixed `<db>-journal` name (recovery must find it), symlinks refused, `0600` at creation |
| Database file | `0600` at creation on unix; rename preserves the temp mode |
| Symlinks | Refused for tmp/journal paths; attacker-planted links fail the write with an error |
| Directory sync | `fsync` after journal write, image rename, and journal delete |
| Old content | Cookie/counter read from a 128-byte prefix; journal payload streams in 64 KiB chunks (large files never load fully into RAM) |

## Error Hygiene

Untrusted text echoed in errors is cut to 128 characters
(`MAX_ERROR_ECHO_CHARS`) with a `...[truncated]` suffix: token rendering
in parse errors and value rendering in conversion errors. File bytes are
never echoed; corrupt files yield static `SqliteFailure` code 11 messages.

## Usage / Example

```rust
use sqlkit::Connection;

// Oversized input fails closed, without allocating the payload.
let big = format!("SELECT '{}'", "x".repeat(2_000_000));
assert!(Connection::open_in_memory().unwrap().execute_batch(&big).is_err());
```

## Cross References

- [Parser.md](Parser.md) – bound constants and grammar limits
- [Pager.md](Pager.md) – file validation, journal behavior, WAL policy
- [FFI.md](FFI.md) – C return codes including `-4` for internal failures

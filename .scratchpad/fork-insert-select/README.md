# Spike: INSERT INTO ... SELECT via diesel DSL

**Question.** Can fork copy `messages` rows inside the database, or must every
payload (base64 images included) be pulled through the server process?

**Method.** Appended `insert_from_select.rs` to `app/coda_server/src/storage.rs`
and ran `cargo check -p coda_server`. Source file restored afterwards.

**Result.** Compiles. Two corrections found along the way:
- `into_columns` is a method on `InsertStatement`, not on the select — it goes
  *after* `.values(...)`, not chained onto the query.
- The literal new `session_id` in the select list needs
  `IntoSql::into_sql::<Text>()` so diesel types the bind parameter.

**Implication.** Message copying stays in the database. Checkpoints still need
the load-compute-insert path, because `message_count` has to be recomputed.

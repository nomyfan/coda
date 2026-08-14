//! A typed `jsonb` column.
//!
//! diesel ships `Jsonb` support only for `serde_json::Value`, which would force
//! every read and write of the six json columns through an untyped intermediate.
//! `Json<T>` restores the type: the column is declared `Jsonb` in `schema.rs`, and
//! the Rust side names the type it actually holds, so `payload` is a `Message`
//! rather than a `Value` someone remembers to convert.

use diesel::deserialize::{self, FromSql, FromSqlRow};
use diesel::expression::AsExpression;
use diesel::pg::{Pg, PgValue};
use diesel::serialize::{self, IsNull, Output, ToSql};
use diesel::sql_types::Jsonb;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::ser::{CharEscape, CompactFormatter, Formatter};
use std::io::Write;

/// PostgreSQL prefixes the `jsonb` wire format with a version byte. Version 1 is
/// the only one that has ever existed; anything else means we are talking to
/// something that is not the PostgreSQL we think it is.
const JSONB_VERSION: u8 = 1;

/// U+FFFD is what `from_utf8_lossy` already left for the undecodable bytes a NUL
/// usually travels with, so one blob of garbage keeps looking like one blob.
const NUL_REPLACEMENT: &str = "\u{fffd}";

/// Writes U+FFFD wherever serde_json would have escaped a NUL.
///
/// A `String` may hold a NUL — it is valid UTF-8, so `from_utf8_lossy` passes it
/// through — but `jsonb` keeps its strings as `text`, which cannot, so PostgreSQL
/// rejects the escape with "unsupported Unicode escape sequence" and fails the
/// whole statement. One NUL in a tool result cost the entire checkpoint.
///
/// Substituting is lossy by necessity: `jsonb` cannot store the byte under any
/// encoding, and the `json` type that could has no equality operator, which the
/// `resume_point` comparisons need.
struct ScrubNul;

impl Formatter for ScrubNul {
    fn write_char_escape<W>(
        &mut self,
        writer: &mut W,
        char_escape: CharEscape,
    ) -> std::io::Result<()>
    where
        W: ?Sized + Write,
    {
        if matches!(char_escape, CharEscape::AsciiControl(0)) {
            return writer.write_all(NUL_REPLACEMENT.as_bytes());
        }
        CompactFormatter.write_char_escape(writer, char_escape)
    }
}

/// `serde_json::to_writer`, minus the NULs PostgreSQL would refuse. Keys escape
/// through the same path as values, so a NUL is scrubbed wherever it sits.
fn write_scrubbed_json<W, T>(writer: W, value: &T) -> serde_json::Result<()>
where
    W: Write,
    T: ?Sized + Serialize,
{
    value.serialize(&mut serde_json::Serializer::with_formatter(
        writer, ScrubNul,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = Jsonb)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> ToSql<Jsonb, Pg> for Json<T>
where
    T: Serialize + std::fmt::Debug,
{
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Pg>) -> serialize::Result {
        out.write_all(&[JSONB_VERSION])?;
        write_scrubbed_json(out, &self.0)
            .map(|()| IsNull::No)
            .map_err(Into::into)
    }
}

impl<T> FromSql<Jsonb, Pg> for Json<T>
where
    T: DeserializeOwned,
{
    fn from_sql(value: PgValue<'_>) -> deserialize::Result<Self> {
        let bytes = value.as_bytes();
        let version = bytes
            .first()
            .ok_or("received an empty jsonb value from the server")?;
        if *version != JSONB_VERSION {
            return Err(format!("unsupported jsonb encoding version {version}").into());
        }
        serde_json::from_slice(&bytes[1..])
            .map(Json)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrub(value: &serde_json::Value) -> String {
        let mut out = Vec::new();
        write_scrubbed_json(&mut out, value).expect("writing to a Vec cannot fail");
        String::from_utf8(out).expect("the formatter only ever emits UTF-8")
    }

    #[test]
    fn a_nul_in_a_string_becomes_the_replacement_character() {
        assert_eq!(
            scrub(&serde_json::json!({ "output": "\u{0}ELF\u{0}stripped" })),
            "{\"output\":\"\u{fffd}ELF\u{fffd}stripped\"}"
        );
    }

    #[test]
    fn a_nul_in_a_key_is_scrubbed_too() {
        assert_eq!(
            scrub(&serde_json::json!({ "a\u{0}b": 1 })),
            "{\"a\u{fffd}b\":1}"
        );
    }

    /// Only the NUL changes; everything else stays byte-for-byte serde_json.
    #[test]
    fn every_other_escape_is_left_alone() {
        let value = serde_json::json!({
            "quote": "a\"b",
            "backslash": "a\\b",
            "newline": "a\nb",
            "tab": "a\tb",
            "control": "a\u{1}b",
            "unicode": "café ☕",
            "nested": [1, true, null, { "k": "v" }],
        });
        assert_eq!(scrub(&value), serde_json::to_string(&value).unwrap());
    }
}

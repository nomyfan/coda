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
use std::io::Write;

/// PostgreSQL prefixes the `jsonb` wire format with a version byte. Version 1 is
/// the only one that has ever existed; anything else means we are talking to
/// something that is not the PostgreSQL we think it is.
const JSONB_VERSION: u8 = 1;

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
        serde_json::to_writer(out, &self.0)
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

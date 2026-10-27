//! Short, time-sortable identifiers for every row the API exposes.
//!
//! An id is 16 lowercase base36 characters: 9 encode the creation time in
//! milliseconds (good until the year 5188) and 7 are random (~36 bits). Ids
//! made in the same millisecond collide with odds of about 1 in 78 billion,
//! and the primary key turns the rare collision into an error instead of a
//! mix-up. Sorting ids sorts rows by creation time.
//!
//! The database generates the same format with `new_id()` (see the migration),
//! so ids minted in SQL and in Rust are interchangeable.

use std::fmt;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub const LEN: usize = 16;
const TIME_LEN: usize = 9;
const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id([u8; LEN]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId;

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid id: expected {LEN} characters of 0-9 a-z")
    }
}

impl std::error::Error for InvalidId {}

impl Id {
    /// All zeros: a fixed id for "no row" (e.g. the instance-level key scope).
    pub const ZERO: Id = Id([b'0'; LEN]);

    pub fn new() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut random = [0u8; LEN - TIME_LEN];
        getrandom::fill(&mut random).expect("OS random number generator");
        Self::from_parts(ms, random)
    }

    fn from_parts(ms: u64, random: [u8; LEN - TIME_LEN]) -> Self {
        let mut out = [b'0'; LEN];
        let mut t = ms;
        for slot in out[..TIME_LEN].iter_mut().rev() {
            *slot = ALPHABET[(t % 36) as usize];
            t /= 36;
        }
        // A byte mod 36 is very slightly biased toward the first 4 symbols;
        // irrelevant for uniqueness.
        for (slot, b) in out[TIME_LEN..].iter_mut().zip(random) {
            *slot = ALPHABET[(b % 36) as usize];
        }
        Id(out)
    }

    pub fn as_str(&self) -> &str {
        // Only ever built from ASCII alphanumerics.
        std::str::from_utf8(&self.0).expect("ids are ASCII")
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl FromStr for Id {
    type Err = InvalidId;

    fn from_str(s: &str) -> Result<Self, InvalidId> {
        let bytes: [u8; LEN] = s.as_bytes().try_into().map_err(|_| InvalidId)?;
        if bytes
            .iter()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        {
            Ok(Id(bytes))
        } else {
            Err(InvalidId)
        }
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.as_str())
    }
}

impl serde::Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "sqlx")]
mod sql {
    use sqlx::encode::IsNull;
    use sqlx::error::BoxDynError;
    use sqlx::postgres::{PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueRef};
    use sqlx::{Decode, Encode, Postgres, Type};

    use super::Id;

    impl Type<Postgres> for Id {
        fn type_info() -> PgTypeInfo {
            <&str as Type<Postgres>>::type_info()
        }

        fn compatible(ty: &PgTypeInfo) -> bool {
            <&str as Type<Postgres>>::compatible(ty)
        }
    }

    impl PgHasArrayType for Id {
        fn array_type_info() -> PgTypeInfo {
            <&str as PgHasArrayType>::array_type_info()
        }
    }

    impl Encode<'_, Postgres> for Id {
        fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
            <&str as Encode<Postgres>>::encode(self.as_str(), buf)
        }
    }

    impl<'r> Decode<'r, Postgres> for Id {
        fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
            Ok(<&str as Decode<Postgres>>::decode(value)?.parse()?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_valid_and_time_sortable() {
        let a = Id::from_parts(1_700_000_000_000, [255; 7]);
        let b = Id::from_parts(1_700_000_000_001, [0; 7]);
        assert!(a < b);
        assert_eq!(a.as_str().len(), LEN);
        assert_eq!(a.as_str().parse::<Id>().unwrap(), a);

        let fresh = Id::new();
        assert!(fresh > b && fresh != Id::new());
    }

    #[test]
    fn parsing_is_strict() {
        for bad in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123456789ABCDEF",
            "../../etc/passwd",
            "0123456789abcde-",
        ] {
            assert!(bad.parse::<Id>().is_err(), "{bad:?}");
        }
        let json: Id = serde_json::from_str("\"0000000000000000\"").unwrap();
        assert_eq!(json, Id::ZERO);
        assert!(serde_json::from_str::<Id>("\"0190d6c0-0000-7000-8000-000000000000\"").is_err());
    }
}

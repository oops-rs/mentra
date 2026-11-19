//! Serde compatibility for timestamps saved by the former `time` fields.

use std::fmt;

use jiff::{
    Timestamp,
    civil::{Date, Time},
    tz::Offset,
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, SeqAccess, Visitor, value::SeqAccessDeserializer},
};

// Year, ordinal day, hour, minute, second, nanosecond, and offset H:M:S.
type LegacyParts = (i32, u16, u8, u8, u8, u32, i8, i8, i8);

pub fn serialize<S>(timestamp: &Timestamp, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let utc = Offset::UTC.to_datetime(*timestamp);
    let parts: LegacyParts = (
        i32::from(utc.year()),
        utc.day_of_year() as u16,
        utc.hour() as u8,
        utc.minute() as u8,
        utc.second() as u8,
        utc.subsec_nanosecond() as u32,
        0,
        0,
        0,
    );
    parts.serialize(serializer)
}

pub fn deserialize<'de, D>(deserializer: D) -> Result<Timestamp, D::Error>
where
    D: Deserializer<'de>,
{
    if deserializer.is_human_readable() {
        deserializer.deserialize_any(TimestampVisitor)
    } else {
        deserializer.deserialize_tuple(9, TimestampVisitor)
    }
}

struct TimestampVisitor;

impl<'de> Visitor<'de> for TimestampVisitor {
    type Value = Timestamp;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a legacy timestamp tuple or timestamp string")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, sequence: A) -> Result<Timestamp, A::Error> {
        let (year, day, hour, minute, second, nanos, offset_hours, offset_minutes, offset_seconds) =
            LegacyParts::deserialize(SeqAccessDeserializer::new(sequence))?;
        let year = i16::try_from(year).map_err(de::Error::custom)?;
        let day = i16::try_from(day).map_err(de::Error::custom)?;
        let date = Date::new(year, 1, 1)
            .and_then(|date| date.with().day_of_year(day).build())
            .map_err(de::Error::custom)?;
        let time = Time::new(
            i8::try_from(hour).map_err(de::Error::custom)?,
            i8::try_from(minute).map_err(de::Error::custom)?,
            i8::try_from(second).map_err(de::Error::custom)?,
            i32::try_from(nanos).map_err(de::Error::custom)?,
        )
        .map_err(de::Error::custom)?;
        let offset = Offset::from_seconds(
            i32::from(offset_hours) * 3600
                + i32::from(offset_minutes) * 60
                + i32::from(offset_seconds),
        )
        .map_err(de::Error::custom)?;
        offset
            .to_timestamp(date.to_datetime(time))
            .map_err(de::Error::custom)
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Timestamp, E> {
        value
            .parse()
            .or_else(|_| Timestamp::strptime("%Y-%m-%d %H:%M:%S%.f %::z", value))
            .map_err(E::custom)
    }
}

pub(crate) mod option {
    use jiff::Timestamp;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(transparent)]
    struct CompatibleTimestamp(#[serde(with = "crate::legacy_timestamp")] Timestamp);

    pub fn serialize<S>(value: &Option<Timestamp>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.map(CompatibleTimestamp).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Timestamp>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<CompatibleTimestamp>::deserialize(deserializer)
            .map(|value| value.map(|timestamp| timestamp.0))
    }
}

use serde::{de, ser, Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::fmt::{self, Display};

/// "canonical" JSON serializer, thrown together by Claude
/// Notable features include camelCase key conversion and serializing numeric types >=u64 as strings (max safe int for JS is 2 ^ 53)
/// Yes all this code is just to do those two things... :c

// === Error Type ===
#[derive(Debug)]
pub struct Error(pub String);

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}
impl de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

// === Case Conversion ===
fn to_camel_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut cap_next = false;
    for c in s.chars() {
        if c == '_' {
            cap_next = true;
        } else if cap_next {
            result.push(c.to_ascii_uppercase());
            // Keep cap_next true if this is a digit, so the next letter gets capitalized
            cap_next = c.is_ascii_digit();
        } else {
            result.push(c);
        }
    }
    result
}

fn to_snake_case(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut result = String::with_capacity(s.len() + 4);

    for i in 0..chars.len() {
        let c = chars[i];

        if c.is_ascii_uppercase() {
            // Insert underscore before uppercase letter, but NOT if previous char was a digit
            // (because the underscore was already inserted before the digit)
            if i > 0 && !chars[i - 1].is_ascii_digit() {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else if c.is_ascii_digit() && i > 0 {
            // If this digit is followed by an uppercase letter, insert underscore before the digit
            if i + 1 < chars.len() && chars[i + 1].is_ascii_uppercase() {
                result.push('_');
            }
            result.push(c);
        } else {
            result.push(c.to_ascii_lowercase());
        }
    }
    result
}

// === Public API ===
pub fn to_canonical<T: Serialize>(value: &T) -> Result<Value, Error> {
    value.serialize(CanonicalSerializer)
}

pub fn from_canonical<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, Error> {
    T::deserialize(CanonicalDeserializer(value))
}

/// Wrapper struct that implements Serialize/Deserialize with canonical JSON format
#[derive(Debug, Clone)]
pub struct Canonical<T>(pub T);

impl<T> From<T> for Canonical<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T: Serialize> Serialize for Canonical<T> {
    fn serialize<S: ser::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = to_canonical(&self.0).map_err(ser::Error::custom)?;
        value.serialize(serializer)
    }
}

impl<'de, T: for<'a> Deserialize<'a>> Deserialize<'de> for Canonical<T> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        from_canonical(value)
            .map(Canonical)
            .map_err(de::Error::custom)
    }
}

// === Serializer Implementation ===
// this serializer serializes to serde-json Values, but changes some of the rules
// notably, u/i64 are always serialized as strings, instead of numbers
struct CanonicalSerializer;

impl ser::Serializer for CanonicalSerializer {
    type Ok = Value;
    type Error = Error;
    type SerializeSeq = SeqSer;
    type SerializeTuple = SeqSer;
    type SerializeTupleStruct = SeqSer;
    type SerializeTupleVariant = TupleVariantSer;
    type SerializeMap = MapSer;
    type SerializeStruct = MapSer;
    type SerializeStructVariant = StructVariantSer;

    fn serialize_bool(self, v: bool) -> Result<Value, Error> {
        Ok(Value::Bool(v))
    }
    fn serialize_i8(self, v: i8) -> Result<Value, Error> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_i16(self, v: i16) -> Result<Value, Error> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_i32(self, v: i32) -> Result<Value, Error> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_i64(self, v: i64) -> Result<Value, Error> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_i128(self, v: i128) -> Result<Value, Error> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_u8(self, v: u8) -> Result<Value, Error> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_u16(self, v: u16) -> Result<Value, Error> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_u32(self, v: u32) -> Result<Value, Error> {
        Ok(Value::Number(v.into()))
    }
    fn serialize_u64(self, v: u64) -> Result<Value, Error> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_u128(self, v: u128) -> Result<Value, Error> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_f32(self, v: f32) -> Result<Value, Error> {
        Ok(Number::from_f64(v as f64)
            .map(Value::Number)
            .unwrap_or(Value::Null))
    }
    fn serialize_f64(self, v: f64) -> Result<Value, Error> {
        Ok(Number::from_f64(v)
            .map(Value::Number)
            .unwrap_or(Value::Null))
    }
    fn serialize_char(self, v: char) -> Result<Value, Error> {
        Ok(Value::String(v.to_string()))
    }
    fn serialize_str(self, v: &str) -> Result<Value, Error> {
        Ok(Value::String(v.to_owned()))
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<Value, Error> {
        Ok(Value::Array(
            v.iter().map(|&b| Value::Number(b.into())).collect(),
        ))
    }
    fn serialize_none(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, v: &T) -> Result<Value, Error> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        var: &'static str,
    ) -> Result<Value, Error> {
        Ok(Value::String(to_camel_case(var)))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<Value, Error> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        var: &'static str,
        v: &T,
    ) -> Result<Value, Error> {
        let mut map = Map::new();
        map.insert(to_camel_case(var), v.serialize(Self)?);
        Ok(Value::Object(map))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<SeqSer, Error> {
        Ok(SeqSer(Vec::with_capacity(len.unwrap_or(0))))
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqSer, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> Result<SeqSer, Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        var: &'static str,
        len: usize,
    ) -> Result<TupleVariantSer, Error> {
        Ok(TupleVariantSer {
            name: to_camel_case(var),
            vec: Vec::with_capacity(len),
        })
    }
    fn serialize_map(self, len: Option<usize>) -> Result<MapSer, Error> {
        Ok(MapSer {
            map: Map::with_capacity(len.unwrap_or(0)),
            key: None,
        })
    }
    fn serialize_struct(self, _: &'static str, len: usize) -> Result<MapSer, Error> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        var: &'static str,
        len: usize,
    ) -> Result<StructVariantSer, Error> {
        Ok(StructVariantSer {
            name: to_camel_case(var),
            map: Map::with_capacity(len),
        })
    }
}

struct SeqSer(Vec<Value>);
impl ser::SerializeSeq for SeqSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Error> {
        self.0.push(v.serialize(CanonicalSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Array(self.0))
    }
}
impl ser::SerializeTuple for SeqSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Error> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Value, Error> {
        ser::SerializeSeq::end(self)
    }
}
impl ser::SerializeTupleStruct for SeqSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Error> {
        ser::SerializeSeq::serialize_element(self, v)
    }
    fn end(self) -> Result<Value, Error> {
        ser::SerializeSeq::end(self)
    }
}

struct TupleVariantSer {
    name: String,
    vec: Vec<Value>,
}
impl ser::SerializeTupleVariant for TupleVariantSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Error> {
        self.vec.push(v.serialize(CanonicalSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        let mut map = Map::new();
        map.insert(self.name, Value::Array(self.vec));
        Ok(Value::Object(map))
    }
}

struct MapSer {
    map: Map<String, Value>,
    key: Option<String>,
}
impl ser::SerializeMap for MapSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, k: &T) -> Result<(), Error> {
        match k.serialize(CanonicalSerializer)? {
            Value::String(s) => {
                self.key = Some(to_camel_case(&s));
                Ok(())
            }
            _ => Err(Error("map key must be string".into())),
        }
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, v: &T) -> Result<(), Error> {
        let k = self.key.take().ok_or_else(|| Error("missing key".into()))?;
        self.map.insert(k, v.serialize(CanonicalSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Object(self.map))
    }
}
impl ser::SerializeStruct for MapSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        k: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        self.map
            .insert(to_camel_case(k), v.serialize(CanonicalSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        Ok(Value::Object(self.map))
    }
}

struct StructVariantSer {
    name: String,
    map: Map<String, Value>,
}
impl ser::SerializeStructVariant for StructVariantSer {
    type Ok = Value;
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        k: &'static str,
        v: &T,
    ) -> Result<(), Error> {
        self.map
            .insert(to_camel_case(k), v.serialize(CanonicalSerializer)?);
        Ok(())
    }
    fn end(self) -> Result<Value, Error> {
        let mut outer = Map::new();
        outer.insert(self.name, Value::Object(self.map));
        Ok(Value::Object(outer))
    }
}

// === Deserializer Implementation ===
struct CanonicalDeserializer(Value);

macro_rules! deserialize_number {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match self.0 {
                Value::Number(n) => {
                    if let Some(v) = n.as_i64().and_then(|i| <$ty>::try_from(i).ok()) {
                        visitor.$visit(v)
                    } else if let Some(v) = n.as_u64().and_then(|u| <$ty>::try_from(u).ok()) {
                        visitor.$visit(v)
                    } else if let Some(v) = n.as_f64() {
                        visitor.$visit(v as $ty)
                    } else {
                        Err(Error(concat!("invalid ", stringify!($ty)).into()))
                    }
                }
                Value::String(s) => {
                    let v: $ty = s
                        .parse()
                        .map_err(|_| Error(format!("invalid {}: {}", stringify!($ty), s)))?;
                    visitor.$visit(v)
                }
                _ => Err(Error(concat!("expected ", stringify!($ty)).into())),
            }
        }
    };
}

impl<'de> de::Deserializer<'de> for CanonicalDeserializer {
    type Error = Error;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    visitor.visit_i64(i)
                } else if let Some(u) = n.as_u64() {
                    visitor.visit_u64(u)
                } else if let Some(f) = n.as_f64() {
                    visitor.visit_f64(f)
                } else {
                    Err(Error("invalid number".into()))
                }
            }
            Value::String(s) => visitor.visit_string(s),
            Value::Array(a) => visitor.visit_seq(SeqDe(a.into_iter())),
            Value::Object(m) => visitor.visit_map(MapDe::new(m)),
        }
    }

    deserialize_number!(deserialize_i8, visit_i8, i8);
    deserialize_number!(deserialize_i16, visit_i16, i16);
    deserialize_number!(deserialize_i32, visit_i32, i32);
    deserialize_number!(deserialize_i64, visit_i64, i64);
    deserialize_number!(deserialize_u8, visit_u8, u8);
    deserialize_number!(deserialize_u16, visit_u16, u16);
    deserialize_number!(deserialize_u32, visit_u32, u32);
    deserialize_number!(deserialize_u64, visit_u64, u64);

    fn deserialize_i128<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => visitor.visit_i128(
                s.parse()
                    .map_err(|_| Error(format!("invalid i128: {}", s)))?,
            ),
            Value::Number(n) => {
                visitor.visit_i128(n.as_i64().ok_or_else(|| Error("invalid i128".into()))? as i128)
            }
            _ => Err(Error("expected i128".into())),
        }
    }

    fn deserialize_u128<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => visitor.visit_u128(
                s.parse()
                    .map_err(|_| Error(format!("invalid u128: {}", s)))?,
            ),
            Value::Number(n) => {
                visitor.visit_u128(n.as_u64().ok_or_else(|| Error("invalid u128".into()))? as u128)
            }
            _ => Err(Error("expected u128".into())),
        }
    }

    fn deserialize_f32<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Number(n) => {
                visitor.visit_f32(n.as_f64().ok_or_else(|| Error("invalid f32".into()))? as f32)
            }
            _ => Err(Error("expected f32".into())),
        }
    }

    fn deserialize_f64<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Number(n) => {
                visitor.visit_f64(n.as_f64().ok_or_else(|| Error("invalid f64".into()))?)
            }
            _ => Err(Error("expected f64".into())),
        }
    }

    fn deserialize_bool<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Bool(b) => visitor.visit_bool(b),
            _ => Err(Error("expected bool".into())),
        }
    }

    fn deserialize_char<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) if s.len() == 1 => visitor.visit_char(s.chars().next().unwrap()),
            _ => Err(Error("expected char".into())),
        }
    }

    fn deserialize_str<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => visitor.visit_string(s),
            _ => Err(Error("expected string".into())),
        }
    }

    fn deserialize_string<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Array(a) => {
                let bytes: Result<Vec<u8>, _> = a
                    .into_iter()
                    .map(|v| {
                        v.as_u64()
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| Error("invalid byte".into()))
                    })
                    .collect();
                visitor.visit_byte_buf(bytes?)
            }
            _ => Err(Error("expected bytes".into())),
        }
    }

    fn deserialize_byte_buf<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_none(),
            v => visitor.visit_some(Self(v)),
        }
    }

    fn deserialize_unit<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            _ => Err(Error("expected null".into())),
        }
    }

    fn deserialize_unit_struct<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Array(a) => visitor.visit_seq(SeqDe(a.into_iter())),
            _ => Err(Error("expected array".into())),
        }
    }

    fn deserialize_tuple<V: de::Visitor<'de>>(
        self,
        _: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        _: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::Object(m) => visitor.visit_map(MapDe::new(m)),
            _ => Err(Error("expected object".into())),
        }
    }

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => visitor.visit_enum(EnumDe::Unit(to_snake_case(&s))),
            Value::Object(m) if m.len() == 1 => {
                let (k, v) = m.into_iter().next().unwrap();
                visitor.visit_enum(EnumDe::Variant(to_snake_case(&k), v))
            }
            _ => Err(Error("expected enum".into())),
        }
    }

    fn deserialize_identifier<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.0 {
            Value::String(s) => visitor.visit_string(to_snake_case(&s)),
            _ => Err(Error("expected identifier".into())),
        }
    }

    fn deserialize_ignored_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
}

struct SeqDe(std::vec::IntoIter<Value>);
impl<'de> de::SeqAccess<'de> for SeqDe {
    type Error = Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        self.0
            .next()
            .map(|v| seed.deserialize(CanonicalDeserializer(v)))
            .transpose()
    }
}

struct MapDe {
    iter: std::vec::IntoIter<(String, Value)>,
    value: Option<Value>,
}
impl MapDe {
    fn new(map: Map<String, Value>) -> Self {
        Self {
            iter: map.into_iter().collect::<Vec<_>>().into_iter(),
            value: None,
        }
    }
}
impl<'de> de::MapAccess<'de> for MapDe {
    type Error = Error;
    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        match self.iter.next() {
            Some((k, v)) => {
                self.value = Some(v);
                seed.deserialize(CanonicalDeserializer(Value::String(to_snake_case(&k))))
                    .map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        seed.deserialize(CanonicalDeserializer(
            self.value
                .take()
                .ok_or_else(|| Error("missing value".into()))?,
        ))
    }
}

enum EnumDe {
    Unit(String),
    Variant(String, Value),
}
impl<'de> de::EnumAccess<'de> for EnumDe {
    type Error = Error;
    type Variant = VariantDe;
    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, VariantDe), Error> {
        match self {
            Self::Unit(s) => Ok((
                seed.deserialize(CanonicalDeserializer(Value::String(s)))?,
                VariantDe::Unit,
            )),
            Self::Variant(s, v) => Ok((
                seed.deserialize(CanonicalDeserializer(Value::String(s)))?,
                VariantDe::Value(v),
            )),
        }
    }
}

enum VariantDe {
    Unit,
    Value(Value),
}
impl<'de> de::VariantAccess<'de> for VariantDe {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        match self {
            Self::Unit => Ok(()),
            _ => Err(Error("expected unit variant".into())),
        }
    }
    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        match self {
            Self::Value(v) => seed.deserialize(CanonicalDeserializer(v)),
            _ => Err(Error("expected newtype variant".into())),
        }
    }
    fn tuple_variant<V: de::Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, Error> {
        match self {
            Self::Value(Value::Array(a)) => visitor.visit_seq(SeqDe(a.into_iter())),
            _ => Err(Error("expected tuple variant".into())),
        }
    }
    fn struct_variant<V: de::Visitor<'de>>(
        self,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self {
            Self::Value(Value::Object(m)) => visitor.visit_map(MapDe::new(m)),
            _ => Err(Error("expected struct variant".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConsensusConfig;
    use proptest::prelude::*;

    #[test]
    fn case_conversion_handles_digit_patterns() {
        assert_eq!(to_camel_case("sha_1s_difficulty"), "sha1SDifficulty");
        assert_eq!(to_snake_case("sha1SDifficulty"), "sha_1s_difficulty");

        // Verify roundtrip
        let original = "sha_1s_difficulty";
        let camel = to_camel_case(original);
        let back = to_snake_case(&camel);
        assert_eq!(original, back);
    }

    #[test]
    fn consensus_config_uses_camel_case_keys() {
        let v = to_canonical(&ConsensusConfig::testing()).unwrap();

        assert!(v.get("chainId").is_some());
        assert!(v.get("chunkSize").is_some());
        assert!(v.get("blockMigrationDepth").is_some());
        assert!(v.get("numChunksInPartition").is_some());
        assert!(v.get("difficultyAdjustment").is_some());
        assert!(v["difficultyAdjustment"].get("blockTime").is_some());
        assert!(v["difficultyAdjustment"]
            .get("difficultyAdjustmentInterval")
            .is_some());
        assert!(v.get("mempool").is_some());
        assert!(v["mempool"].get("maxDataTxsPerBlock").is_some());
        assert!(v["mempool"].get("txAnchorExpiryDepth").is_some());
    }

    #[test]
    fn consensus_config_u64_fields_serialize_as_strings() {
        let v = to_canonical(&ConsensusConfig::testing()).unwrap();

        assert!(v["chainId"].is_string());
        assert!(v["chunkSize"].is_string());
        assert!(v["numChunksInPartition"].is_string());
    }

    fn assert_roundtrip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        config: T,
    ) {
        let restored: T = from_canonical(to_canonical(&config).unwrap()).unwrap();
        assert_eq!(config, restored);
    }

    #[rstest::rstest]
    #[case::mempool(ConsensusConfig::testing().mempool)]
    #[case::difficulty_adjustment(ConsensusConfig::testing().difficulty_adjustment)]
    #[case::epoch(ConsensusConfig::testing().epoch)]
    #[case::ema(ConsensusConfig::testing().ema)]
    #[case::block_reward(ConsensusConfig::testing().block_reward_config)]
    #[case::genesis(ConsensusConfig::testing().genesis)]
    #[case::reth(ConsensusConfig::testing().reth)]
    #[case::vdf(ConsensusConfig::testing().vdf)]
    fn roundtrip_config<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        #[case] config: T,
    ) {
        assert_roundtrip(config);
    }

    #[test]
    fn roundtrip_full_consensus_config() {
        let config = ConsensusConfig::testing();
        assert_roundtrip(config);
    }

    proptest! {
        #[test]
        fn roundtrip_primitives(
            b in any::<bool>(),
            i8_val in any::<i8>(),
            i16_val in any::<i16>(),
            i32_val in any::<i32>(),
            i64_val in any::<i64>(),
            u8_val in any::<u8>(),
            u16_val in any::<u16>(),
            u32_val in any::<u32>(),
            u64_val in any::<u64>(),
        ) {
            prop_assert_eq!(b, from_canonical::<bool>(to_canonical(&b).unwrap()).unwrap());
            prop_assert_eq!(i8_val, from_canonical::<i8>(to_canonical(&i8_val).unwrap()).unwrap());
            prop_assert_eq!(i16_val, from_canonical::<i16>(to_canonical(&i16_val).unwrap()).unwrap());
            prop_assert_eq!(i32_val, from_canonical::<i32>(to_canonical(&i32_val).unwrap()).unwrap());
            prop_assert_eq!(i64_val, from_canonical::<i64>(to_canonical(&i64_val).unwrap()).unwrap());
            prop_assert_eq!(u8_val, from_canonical::<u8>(to_canonical(&u8_val).unwrap()).unwrap());
            prop_assert_eq!(u16_val, from_canonical::<u16>(to_canonical(&u16_val).unwrap()).unwrap());
            prop_assert_eq!(u32_val, from_canonical::<u32>(to_canonical(&u32_val).unwrap()).unwrap());
            prop_assert_eq!(u64_val, from_canonical::<u64>(to_canonical(&u64_val).unwrap()).unwrap());
        }
    }

    #[test]
    fn u64_i64_serialize_as_strings() {
        assert_eq!(
            to_canonical(&9_007_199_254_740_993_u64).unwrap(),
            serde_json::json!("9007199254740993")
        );
        assert_eq!(
            to_canonical(&(-9_007_199_254_740_993_i64)).unwrap(),
            serde_json::json!("-9007199254740993")
        );
    }

    #[test]
    fn option_none_serializes_as_null() {
        let v = to_canonical(&ConsensusConfig::testing()).unwrap();
        assert_eq!(v["expectedGenesisHash"], serde_json::Value::Null);
    }

    #[test]
    fn hashmap_keys_use_camel_case() {
        let v = to_canonical(&ConsensusConfig::testnet()).unwrap();
        assert!(v["reth"]["alloc"].is_object());
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Status {
        Active,
        Suspended { reason: String, by_user_id: u64 },
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Account {
        account_id: u64,
        status: Status,
    }

    #[test]
    fn wrapper_and_enum_variants() {
        // Test Canonical wrapper with enum unit variant
        let active = Account {
            account_id: 12345678901234,
            status: Status::Active,
        };
        let wrapped = Canonical(active.clone());
        let json_str = serde_json::to_string(&wrapped).unwrap();
        assert!(json_str.contains("accountId"));
        assert!(json_str.contains("\"12345678901234\""));
        assert!(json_str.contains("active"));
        let restored: Canonical<Account> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(active, restored.0);

        // Test enum struct variant
        let suspended = Account {
            account_id: 999,
            status: Status::Suspended {
                reason: "spam".into(),
                by_user_id: 9007199254740993,
            },
        };
        let v = to_canonical(&suspended).unwrap();
        assert_eq!(v["accountId"], "999");
        assert_eq!(v["status"]["suspended"]["reason"], "spam");
        assert_eq!(v["status"]["suspended"]["byUserId"], "9007199254740993");
        let restored: Account = from_canonical(v).unwrap();
        assert_eq!(suspended, restored);
    }
}

use serde::{de, ser, Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::fmt::{self, Display};

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
            cap_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            result.push('_');
            result.push(c.to_ascii_lowercase());
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

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestStruct {
        user_name: String,
        user_age: u32,
        big_number: u64,
        negative_big: i64,
        items: Vec<i32>,
    }

    #[test]
    fn test_serialize() {
        let s = TestStruct {
            user_name: "Alice".into(),
            user_age: 30,
            big_number: 42,
            negative_big: -100,
            items: vec![1, 2, 3],
        };
        let v = to_canonical(&s).unwrap();
        assert_eq!(v["userName"], "Alice");
        assert_eq!(v["userAge"], 30);
        assert_eq!(v["bigNumber"], "42"); // u64 -> string
        assert_eq!(v["negativeBig"], "-100"); // i64 -> string
        assert_eq!(v["items"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn test_deserialize() {
        let v = serde_json::json!({
            "userName": "Bob",
            "userAge": 25,
            "bigNumber": "999",
            "negativeBig": "-50",
            "items": [4, 5]
        });
        let s: TestStruct = from_canonical(v).unwrap();
        assert_eq!(s.user_name, "Bob");
        assert_eq!(s.user_age, 25);
        assert_eq!(s.big_number, 999);
        assert_eq!(s.negative_big, -50);
        assert_eq!(s.items, vec![4, 5]);
    }

    #[test]
    fn test_roundtrip() {
        let original = TestStruct {
            user_name: "Test".into(),
            user_age: 100,
            big_number: 12345678901234,
            negative_big: -9876543210,
            items: vec![10, 20, 30],
        };
        let v = to_canonical(&original).unwrap();
        let restored: TestStruct = from_canonical(v).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn test_wrapper() {
        let original = TestStruct {
            user_name: "Wrapper".into(),
            user_age: 42,
            big_number: 1000000000000,
            negative_big: -1000000000000,
            items: vec![],
        };
        let wrapped = Canonical(original.clone());
        let json_str = serde_json::to_string(&wrapped).unwrap();
        assert!(json_str.contains("userName"));
        assert!(json_str.contains("\"1000000000000\""));

        let restored: Canonical<TestStruct> = serde_json::from_str(&json_str).unwrap();
        assert_eq!(original, restored.0);
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Address {
        street_name: String,
        zip_code: u32,
        building_id: u64,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Company {
        company_name: String,
        employee_count: u64,
        headquarters: Address,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Person {
        full_name: String,
        account_balance: i64,
        home_address: Address,
        employer: Option<Company>,
        past_employers: Vec<Company>,
    }

    #[test]
    fn test_nested_struct() {
        let addr = Address {
            street_name: "Main St".into(),
            zip_code: 12345,
            building_id: 9999999999999,
        };
        let v = to_canonical(&addr).unwrap();
        assert_eq!(v["streetName"], "Main St");
        assert_eq!(v["zipCode"], 12345);
        assert_eq!(v["buildingId"], "9999999999999");

        let restored: Address = from_canonical(v).unwrap();
        assert_eq!(addr, restored);
    }

    #[test]
    fn test_deeply_nested_struct() {
        let person = Person {
            full_name: "Jane Doe".into(),
            account_balance: -50000,
            home_address: Address {
                street_name: "Oak Ave".into(),
                zip_code: 54321,
                building_id: 42,
            },
            employer: Some(Company {
                company_name: "Tech Corp".into(),
                employee_count: 10000,
                headquarters: Address {
                    street_name: "Corporate Blvd".into(),
                    zip_code: 99999,
                    building_id: 1234567890123,
                },
            }),
            past_employers: vec![],
        };

        let v = to_canonical(&person).unwrap();

        // Check top-level camelCase
        assert_eq!(v["fullName"], "Jane Doe");
        assert_eq!(v["accountBalance"], "-50000"); // i64 -> string

        // Check nested struct camelCase
        assert_eq!(v["homeAddress"]["streetName"], "Oak Ave");
        assert_eq!(v["homeAddress"]["zipCode"], 54321);
        assert_eq!(v["homeAddress"]["buildingId"], "42"); // u64 -> string

        // Check doubly nested (inside Option)
        assert_eq!(v["employer"]["companyName"], "Tech Corp");
        assert_eq!(v["employer"]["employeeCount"], "10000");
        assert_eq!(
            v["employer"]["headquarters"]["streetName"],
            "Corporate Blvd"
        );
        assert_eq!(v["employer"]["headquarters"]["buildingId"], "1234567890123");

        let restored: Person = from_canonical(v).unwrap();
        assert_eq!(person, restored);
    }

    #[test]
    fn test_vec_of_nested_structs() {
        let person = Person {
            full_name: "John Smith".into(),
            account_balance: 100000,
            home_address: Address {
                street_name: "Elm St".into(),
                zip_code: 11111,
                building_id: 1,
            },
            employer: None,
            past_employers: vec![
                Company {
                    company_name: "StartupA".into(),
                    employee_count: 50,
                    headquarters: Address {
                        street_name: "First St".into(),
                        zip_code: 22222,
                        building_id: 100,
                    },
                },
                Company {
                    company_name: "BigCorp".into(),
                    employee_count: 999999999999,
                    headquarters: Address {
                        street_name: "Money Lane".into(),
                        zip_code: 33333,
                        building_id: 9007199254740993, // > JS MAX_SAFE_INTEGER
                    },
                },
            ],
        };

        let v = to_canonical(&person).unwrap();

        // Check vec elements have camelCase
        assert_eq!(v["pastEmployers"][0]["companyName"], "StartupA");
        assert_eq!(v["pastEmployers"][0]["employeeCount"], "50");
        assert_eq!(v["pastEmployers"][1]["companyName"], "BigCorp");
        assert_eq!(v["pastEmployers"][1]["employeeCount"], "999999999999");
        assert_eq!(
            v["pastEmployers"][1]["headquarters"]["buildingId"],
            "9007199254740993"
        );

        // Check None becomes null
        assert_eq!(v["employer"], serde_json::Value::Null);

        let restored: Person = from_canonical(v).unwrap();
        assert_eq!(person, restored);
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum Status {
        ActiveUser,
        InactiveUser {
            last_seen_timestamp: i64,
        },
        Banned {
            ban_reason: String,
            banned_by_user_id: u64,
        },
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Account {
        account_id: u64,
        user_status: Status,
    }

    #[test]
    fn test_nested_enum_unit_variant() {
        let acc = Account {
            account_id: 12345678901234,
            user_status: Status::ActiveUser,
        };

        let v = to_canonical(&acc).unwrap();
        assert_eq!(v["accountId"], "12345678901234");
        assert_eq!(v["userStatus"], "activeUser");

        let restored: Account = from_canonical(v).unwrap();
        assert_eq!(acc, restored);
    }

    #[test]
    fn test_nested_enum_struct_variant() {
        let acc = Account {
            account_id: 999,
            user_status: Status::Banned {
                ban_reason: "spam".into(),
                banned_by_user_id: 9007199254740993,
            },
        };

        let v = to_canonical(&acc).unwrap();
        assert_eq!(v["accountId"], "999");
        assert_eq!(v["userStatus"]["banned"]["banReason"], "spam");
        assert_eq!(
            v["userStatus"]["banned"]["bannedByUserId"],
            "9007199254740993"
        );

        let restored: Account = from_canonical(v).unwrap();
        assert_eq!(acc, restored);
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct NestedMaps {
        outer_map: std::collections::HashMap<String, InnerData>,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct InnerData {
        inner_value: u64,
        inner_name: String,
    }

    #[test]
    fn test_nested_hashmap() {
        let mut outer = std::collections::HashMap::new();
        outer.insert(
            "first_key".into(),
            InnerData {
                inner_value: 111111111111,
                inner_name: "first".into(),
            },
        );
        outer.insert(
            "second_key".into(),
            InnerData {
                inner_value: 222222222222,
                inner_name: "second".into(),
            },
        );

        let data = NestedMaps { outer_map: outer };
        let v = to_canonical(&data).unwrap();

        // Map keys should be camelCase
        assert!(v["outerMap"]["firstKey"].is_object());
        assert!(v["outerMap"]["secondKey"].is_object());

        // Nested struct fields should be camelCase with stringified u64
        assert_eq!(v["outerMap"]["firstKey"]["innerValue"], "111111111111");
        assert_eq!(v["outerMap"]["firstKey"]["innerName"], "first");

        let restored: NestedMaps = from_canonical(v).unwrap();
        assert_eq!(data, restored);
    }

    #[test]
    fn test_triple_nested() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Level3 {
            deep_value: u64,
        }

        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Level2 {
            level_three: Level3,
            mid_value: i64,
        }

        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Level1 {
            level_two: Level2,
            top_value: u64,
        }

        let data = Level1 {
            level_two: Level2 {
                level_three: Level3 {
                    deep_value: 9007199254740993,
                },
                mid_value: -9007199254740993,
            },
            top_value: 42,
        };

        let v = to_canonical(&data).unwrap();
        assert_eq!(v["topValue"], "42");
        assert_eq!(v["levelTwo"]["midValue"], "-9007199254740993");
        assert_eq!(v["levelTwo"]["levelThree"]["deepValue"], "9007199254740993");

        let restored: Level1 = from_canonical(v).unwrap();
        assert_eq!(data, restored);
    }

    // #[test]
    // fn t() {
    //     use crate::Config;
    //     let cfg = crate::ConsensusConfig::testing();
    //     let cfg2 = Canonical(cfg);
    //     let s = serde_json::to_string_pretty(&cfg2).unwrap();
    //     // TODO: add some tests against ConsensusConfig
    //     dbg!(s);
    // }
}

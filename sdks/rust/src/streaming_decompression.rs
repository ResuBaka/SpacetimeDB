//! SDK-local experiment: owned BSATN decoding without a whole-message staging buffer.
//! No changes to the shared SATS borrowing decoder or wire format.

use crate::websocket::WsError;
use spacetimedb_client_api_messages::websocket::{common::*, v2::ServerMessage};
use spacetimedb_sats::{
    buffer::DecodeError,
    de::{self, Deserialize, Deserializer as _},
    i256, u256,
};
use std::io::{self, BufReader, Read};
use std::sync::Arc;

pub(crate) fn decode(raw: &[u8]) -> Result<ServerMessage, WsError> {
    match raw {
        [] => Err(WsError::EmptyMessage),
        [SERVER_MSG_COMPRESSION_TAG_NONE, bytes @ ..] => {
            spacetimedb_sats::bsatn::from_slice(bytes).map_err(|source| WsError::DeserializeMessage { source })
        }
        [SERVER_MSG_COMPRESSION_TAG_BROTLI, bytes @ ..] => {
            decode_reader(brotli::Decompressor::new(bytes, 4096), "brotli")
        }
        [SERVER_MSG_COMPRESSION_TAG_GZIP, bytes @ ..] => decode_reader(flate2::read::GzDecoder::new(bytes), "gzip"),
        [scheme, ..] => Err(WsError::UnknownCompressionScheme { scheme: *scheme }),
    }
}

pub(crate) fn decode_reader(reader: impl Read, scheme: &'static str) -> Result<ServerMessage, WsError> {
    let mut reader = BufReader::with_capacity(16 * 1024, reader);
    let mut decoder = Decoder {
        reader: &mut reader,
        io_error: None,
    };
    let result = ServerMessage::deserialize(&mut decoder);
    let read_error = decoder.io_error.take();
    // Like the baseline, ignore trailing BSATN bytes, but validate the entire first
    // compressed stream/member, even when deserialization failed. Codec errors
    // take precedence over BSATN errors, just as when decompressing first.
    let drained = io::copy(&mut reader, &mut io::sink());
    if let Some(source) = read_error.or_else(|| drained.err()) {
        return Err(WsError::Decompress {
            scheme,
            source: Arc::new(source),
        });
    }
    result.map_err(|source| WsError::DeserializeMessage { source })
}

struct Decoder<'a, R> {
    reader: &'a mut R,
    io_error: Option<io::Error>,
}

impl<R: Read> Decoder<'_, R> {
    fn exact(&mut self, out: &mut [u8]) -> Result<(), DecodeError> {
        let mut given = 0;
        while given < out.len() {
            match self.reader.read(&mut out[given..]) {
                Ok(0) => break,
                Ok(n) => given += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.io_error = Some(e);
                    break;
                }
            }
        }
        if given == out.len() {
            Ok(())
        } else {
            Err(DecodeError::BufferLength {
                for_type: "stream",
                expected: out.len(),
                given,
            })
        }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut bytes = [0; N];
        self.exact(&mut bytes)?;
        Ok(bytes)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = u32::from_le_bytes(self.array()?) as usize;
        let mut bytes = Vec::new();
        // Do not trust a length prefix enough to reserve gigabytes before seeing
        // data. Grow the final owned field incrementally, with fallible reserves.
        // This is not a new message-size limit.
        while bytes.len() < len {
            let start = bytes.len();
            let count = (len - start).min(64 * 1024);
            bytes
                .try_reserve(count)
                .map_err(|_| DecodeError::AllocationFailed(len))?;
            bytes.resize(start + count, 0);
            self.exact(&mut bytes[start..])?;
        }
        Ok(bytes)
    }
}

macro_rules! integers {
    ($($method:ident => $ty:ident),* $(,)?) => {$ (
        fn $method(self) -> Result<$ty, DecodeError> {
            Ok($ty::from_le_bytes(self.array()?))
        }
    )*};
}

impl<'de, R: Read> de::Deserializer<'de> for &mut Decoder<'_, R> {
    type Error = DecodeError;

    fn deserialize_product<V: de::ProductVisitor<'de>>(self, visitor: V) -> Result<V::Output, DecodeError> {
        visitor.visit_seq_product(self)
    }
    fn deserialize_sum<V: de::SumVisitor<'de>>(self, visitor: V) -> Result<V::Output, DecodeError> {
        visitor.visit_sum(self)
    }
    fn deserialize_bool(self) -> Result<bool, DecodeError> {
        match self.deserialize_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(DecodeError::InvalidBool(b)),
        }
    }
    integers! {
        deserialize_u8 => u8, deserialize_u16 => u16, deserialize_u32 => u32,
        deserialize_u64 => u64, deserialize_u128 => u128, deserialize_u256 => u256,
        deserialize_i8 => i8, deserialize_i16 => i16, deserialize_i32 => i32,
        deserialize_i64 => i64, deserialize_i128 => i128, deserialize_i256 => i256,
    }
    fn deserialize_f32(self) -> Result<f32, DecodeError> {
        self.deserialize_u32().map(f32::from_bits)
    }
    fn deserialize_f64(self) -> Result<f64, DecodeError> {
        self.deserialize_u64().map(f64::from_bits)
    }
    fn deserialize_str<V: de::SliceVisitor<'de, str>>(self, visitor: V) -> Result<V::Output, DecodeError> {
        let string = String::from_utf8(self.bytes()?).map_err(|_| DecodeError::InvalidUtf8)?;
        visitor.visit_owned(string)
    }
    fn deserialize_bytes<V: de::SliceVisitor<'de, [u8]>>(self, visitor: V) -> Result<V::Output, DecodeError> {
        visitor.visit_owned(self.bytes()?)
    }
    fn deserialize_array_seed<V: de::ArrayVisitor<'de, T::Output>, T: de::DeserializeSeed<'de> + Clone>(
        self,
        visitor: V,
        seed: T,
    ) -> Result<V::Output, DecodeError> {
        let remaining = self.deserialize_u32()? as usize;
        visitor.visit(Array {
            decoder: self,
            seed,
            remaining,
        })
    }
}

impl<'de, R: Read> de::SeqProductAccess<'de> for &mut Decoder<'_, R> {
    type Error = DecodeError;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Output>, DecodeError> {
        seed.deserialize(&mut **self).map(Some)
    }
}
impl<'a, 'b, 'de, R: Read> de::SumAccess<'de> for &'a mut Decoder<'b, R> {
    type Error = DecodeError;
    type Variant = Self;
    fn variant<V: de::VariantVisitor<'de>>(self, visitor: V) -> Result<(V::Output, Self), DecodeError> {
        let tag = self.deserialize_u8()?;
        visitor.visit_tag(tag).map(|variant| (variant, self))
    }
}
impl<'de, R: Read> de::VariantAccess<'de> for &mut Decoder<'_, R> {
    type Error = DecodeError;
    fn deserialize_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Output, DecodeError> {
        seed.deserialize(self)
    }
}
struct Array<'a, 'b, R, T> {
    decoder: &'a mut Decoder<'b, R>,
    seed: T,
    remaining: usize,
}
impl<'de, R: Read, T: de::DeserializeSeed<'de> + Clone> de::ArrayAccess<'de> for Array<'_, '_, R, T> {
    type Element = T::Output;
    type Error = DecodeError;
    fn next_element(&mut self) -> Result<Option<T::Output>, DecodeError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        self.remaining -= 1;
        self.seed.clone().deserialize(&mut *self.decoder).map(Some)
    }
    fn size_hint(&self) -> Option<usize> {
        // SATS array visitors cap speculative reservations at 4096 elements,
        // just as for the slice decoder. Later collection growth retains the
        // existing SATS allocation behavior (not universally fallible).
        Some(self.remaining.min(4096))
    }
}

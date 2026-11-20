use crate::buffer::{BufReader, DecodeError};
use crate::de::{self, Deserializer as _, SeqProductAccess, SumAccess, VariantAccess};
use crate::{i256, u256};

/// Deserializer from the BSATN data format.
pub struct Deserializer<'a, R> {
    // The input to deserialize.
    reader: &'a mut R,
}

impl<'a, 'de, R: BufReader<'de>> Deserializer<'a, R> {
    /// Returns a deserializer using the given `reader`.
    pub fn new(reader: &'a mut R) -> Self {
        Self { reader }
    }

    /// Reborrows the deserializer.
    #[inline]
    pub(crate) fn reborrow(&mut self) -> Deserializer<'_, R> {
        Deserializer { reader: self.reader }
    }

    /// Reads a length as a `u32` then converted to `usize`.
    pub(crate) fn deserialize_len(self) -> Result<usize, DecodeError> {
        Ok(self.deserialize_u32()? as usize)
    }

    /// Reads a slice of `len` elements.
    #[inline]
    #[doc(hidden)]
    pub fn get_slice(&mut self, len: usize) -> Result<&'de [u8], DecodeError> {
        self.reader.get_slice(len)
    }

    /// Read an exactly sized block for a derived fixed-width product.
    #[inline]
    #[doc(hidden)]
    pub fn get_array<const N: usize>(&mut self) -> Result<&'de [u8; N], DecodeError> {
        self.reader.get_array()
    }

    /// Probe a whole scalar run. On short input, leave the individual field
    /// readers to advance the cursor and report the usual field-specific error.
    #[inline]
    #[doc(hidden)]
    pub fn try_get_array<const N: usize>(&mut self) -> Option<&'de [u8; N]> {
        self.reader.get_array_chunk()
    }

    /// Decode the next field of a derived BSATN product.
    #[inline]
    #[doc(hidden)]
    pub fn field<T: de::Deserialize<'de>>(&mut self) -> Result<T, DecodeError> {
        T::deserialize_from_bsatn(self.reborrow())
    }

    /// Reads a byte slice from the `reader`.
    fn deserialize_bytes_inner(mut self) -> Result<&'de [u8], DecodeError> {
        let len = self.reborrow().deserialize_len()?;
        self.get_slice(len)
    }

    /// Check the complete span before using an untrusted element count for allocation.
    fn fixed_payload(mut self, width: usize) -> Result<(usize, &'de [u8]), DecodeError> {
        let len = self.reborrow().deserialize_len()?;
        let size = len.checked_mul(width).ok_or(DecodeError::AllocationFailed(len))?;
        Ok((len, self.get_slice(size)?))
    }
}

impl de::Error for DecodeError {
    fn custom(msg: impl std::fmt::Display) -> Self {
        Self::Other(msg.to_string())
    }

    fn unknown_variant_tag<'de, T: de::SumVisitor<'de>>(tag: u8, expected: &T) -> Self {
        let sum_name = expected.sum_name().map(|x| x.to_owned());
        Self::InvalidTag { tag, sum_name }
    }

    fn allocation_failed(size: usize) -> Self {
        Self::AllocationFailed(size)
    }
}

impl<'de, R: BufReader<'de>> de::Deserializer<'de> for Deserializer<'_, R> {
    type Error = DecodeError;

    fn deserialize_value<T: de::Deserialize<'de>>(self) -> Result<T, DecodeError> {
        T::deserialize_from_bsatn(self)
    }

    fn deserialize_product<V: de::ProductVisitor<'de>>(self, visitor: V) -> Result<V::Output, DecodeError> {
        visitor.visit_seq_product(self)
    }

    fn validate_product<V: de::ProductVisitor<'de>>(self, visitor: V) -> Result<(), Self::Error> {
        visitor.validate_seq_product(self)
    }

    fn deserialize_sum<V: de::SumVisitor<'de>>(self, visitor: V) -> Result<V::Output, DecodeError> {
        visitor.visit_sum(self)
    }

    fn validate_sum<V: de::SumVisitor<'de>>(self, visitor: V) -> Result<(), Self::Error> {
        visitor.validate_sum(self)
    }

    fn deserialize_bool(self) -> Result<bool, Self::Error> {
        let byte = self.reader.get_u8()?;
        match byte {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(DecodeError::InvalidBool(b)),
        }
    }
    fn deserialize_u8(self) -> Result<u8, DecodeError> {
        self.reader.get_u8()
    }
    fn deserialize_u16(self) -> Result<u16, DecodeError> {
        self.reader.get_u16()
    }
    fn deserialize_u32(self) -> Result<u32, DecodeError> {
        self.reader.get_u32()
    }
    fn deserialize_u64(self) -> Result<u64, DecodeError> {
        self.reader.get_u64()
    }
    fn deserialize_vec<T: de::Deserialize<'de>>(mut self) -> Result<Vec<T>, DecodeError> {
        let len = self.reborrow().deserialize_len()?;
        let cap = len.min(de::RESERVE_ARRAY_ELEMENTS);
        let mut values = Vec::new();
        values
            .try_reserve_exact(cap)
            .map_err(|_| DecodeError::AllocationFailed(cap))?;
        for _ in 0..len {
            values.push(self.field()?);
        }
        Ok(values)
    }
    #[inline(always)]
    fn deserialize_fixed_vec<T: de::Deserialize<'de>, const N: usize>(
        self,
        decode: impl Fn([u8; N]) -> T,
    ) -> Result<Vec<T>, DecodeError> {
        if N == 0 {
            return self.deserialize_array(de::BasicVecVisitor);
        }
        let (len, bytes) = self.fixed_payload(N)?;
        let chunks = bytes.as_chunks::<N>().0;
        let mut values = Vec::new();
        values
            .try_reserve_exact(len)
            .map_err(|_| DecodeError::AllocationFailed(len))?;
        values.extend(chunks.iter().copied().map(decode));
        Ok(values)
    }

    #[inline(always)]
    fn deserialize_pod_vec<T: de::Deserialize<'de> + bytemuck::Pod>(self) -> Result<Vec<T>, DecodeError> {
        if size_of::<T>() == 0 {
            return self.deserialize_array(de::BasicVecVisitor);
        }
        let (len, bytes) = self.fixed_payload(size_of::<T>())?;
        let mut values = bytemuck::allocation::try_zeroed_vec(len).map_err(|_| DecodeError::AllocationFailed(len))?;
        bytemuck::cast_slice_mut(&mut values).copy_from_slice(bytes);
        Ok(values)
    }
    fn deserialize_u128(self) -> Result<u128, DecodeError> {
        self.reader.get_u128()
    }
    fn deserialize_u256(self) -> Result<u256, DecodeError> {
        self.reader.get_u256()
    }
    fn deserialize_i8(self) -> Result<i8, DecodeError> {
        self.reader.get_i8()
    }
    fn deserialize_i16(self) -> Result<i16, DecodeError> {
        self.reader.get_i16()
    }
    fn deserialize_i32(self) -> Result<i32, DecodeError> {
        self.reader.get_i32()
    }
    fn deserialize_i64(self) -> Result<i64, DecodeError> {
        self.reader.get_i64()
    }
    fn deserialize_i128(self) -> Result<i128, DecodeError> {
        self.reader.get_i128()
    }
    fn deserialize_i256(self) -> Result<i256, DecodeError> {
        self.reader.get_i256()
    }
    fn deserialize_f32(self) -> Result<f32, Self::Error> {
        self.reader.get_u32().map(f32::from_bits)
    }
    fn deserialize_f64(self) -> Result<f64, Self::Error> {
        self.reader.get_u64().map(f64::from_bits)
    }

    fn deserialize_str<V: de::SliceVisitor<'de, str>>(self, visitor: V) -> Result<V::Output, Self::Error> {
        let slice = self.deserialize_bytes_inner()?;
        // Keep short strings on the std fast path. Long strings only need a pass/fail check.
        let slice = if slice.len() < 64 {
            core::str::from_utf8(slice)?
        } else {
            simdutf8::basic::from_utf8(slice).map_err(|_| DecodeError::InvalidUtf8)?
        };
        visitor.visit_borrowed(slice)
    }

    fn deserialize_bytes<V: de::SliceVisitor<'de, [u8]>>(self, visitor: V) -> Result<V::Output, Self::Error> {
        let slice = self.deserialize_bytes_inner()?;
        visitor.visit_borrowed(slice)
    }

    fn deserialize_array_seed<V: de::ArrayVisitor<'de, T::Output>, T: de::DeserializeSeed<'de> + Clone>(
        mut self,
        visitor: V,
        seed: T,
    ) -> Result<V::Output, Self::Error> {
        let len = self.reborrow().deserialize_len()?;
        let seeds = itertools::repeat_n(seed, len);
        visitor.visit(ArrayAccess { de: self, seeds })
    }

    fn validate_array_seed<V: de::ArrayVisitor<'de, T::Output>, T: de::DeserializeSeed<'de> + Clone>(
        mut self,
        visitor: V,
        seed: T,
    ) -> Result<(), Self::Error> {
        let len = self.reborrow().deserialize_len()?;
        let seeds = itertools::repeat_n(seed, len);
        visitor.validate(ArrayAccess { de: self, seeds })
    }
}

impl<'de, R: BufReader<'de>> SeqProductAccess<'de> for Deserializer<'_, R> {
    type Error = DecodeError;

    fn next_element_seed<T: de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Output>, DecodeError> {
        seed.deserialize(self.reborrow()).map(Some)
    }

    fn validate_next_element_seed<T: de::DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<()>, Self::Error> {
        seed.validate(self.reborrow()).map(Some)
    }
}

impl<'de, R: BufReader<'de>> SumAccess<'de> for Deserializer<'_, R> {
    type Error = DecodeError;
    type Variant = Self;

    fn variant<V: de::VariantVisitor<'de>>(self, visitor: V) -> Result<(V::Output, Self::Variant), Self::Error> {
        let tag = self.reader.get_u8()?;
        visitor.visit_tag(tag).map(|variant| (variant, self))
    }
}

impl<'de, R: BufReader<'de>> VariantAccess<'de> for Deserializer<'_, R> {
    type Error = DecodeError;
    fn deserialize_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Output, Self::Error> {
        seed.deserialize(self)
    }
    fn validate_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<(), Self::Error> {
        seed.validate(self)
    }
}

/// Deserializer for array elements.
pub struct ArrayAccess<'a, R, T> {
    de: Deserializer<'a, R>,
    seeds: itertools::RepeatN<T>,
}

impl<'de, R: BufReader<'de>, T: de::DeserializeSeed<'de> + Clone> de::ArrayAccess<'de> for ArrayAccess<'_, R, T> {
    type Element = T::Output;
    type Error = DecodeError;

    fn next_element(&mut self) -> Result<Option<T::Output>, Self::Error> {
        self.seeds
            .next()
            .map(|seed| seed.deserialize(self.de.reborrow()))
            .transpose()
    }

    fn validate_next_element(&mut self) -> Result<Option<()>, Self::Error> {
        self.seeds
            .next()
            .map(|seed| seed.validate(self.de.reborrow()))
            .transpose()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.seeds.len())
    }
}

//! The field types of payloads: little-endian integers, addresses, counted strings and bytes.

use std::net::Ipv4Addr;

use crate::Error;

/// Builds a payload field by field.
#[derive(Debug, Default)]
pub struct Writer {
    out: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.out
    }

    pub fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i32(&mut self, value: i32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    pub fn addr(&mut self, value: Ipv4Addr) {
        self.u32(u32::from(value));
    }

    /// Bytes without a count, for the last field of a payload.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    pub fn str(&mut self, value: &str) -> Result<(), Error> {
        let len = u16::try_from(value.len()).map_err(|_| Error::TooLong)?;
        self.u16(len);
        self.raw(value.as_bytes());
        Ok(())
    }

    pub fn bytes(&mut self, value: &[u8]) -> Result<(), Error> {
        let len = u32::try_from(value.len()).map_err(|_| Error::TooLong)?;
        self.u32(len);
        self.raw(value);
        Ok(())
    }

    pub fn pairs(&mut self, pairs: &[(String, String)]) -> Result<(), Error> {
        self.u16(u16::try_from(pairs.len()).map_err(|_| Error::TooLong)?);
        for (name, value) in pairs {
            self.str(name)?;
            self.str(value)?;
        }
        Ok(())
    }

    pub fn strs(&mut self, values: &[String]) -> Result<(), Error> {
        self.u16(u16::try_from(values.len()).map_err(|_| Error::TooLong)?);
        for value in values {
            self.str(value)?;
        }
        Ok(())
    }
}

/// Reads a payload field by field. Bytes after the last field read are ignored.
#[derive(Debug)]
pub struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(payload: &'a [u8]) -> Self {
        Self { rest: payload }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let (head, rest) = self.rest.split_at_checked(len).ok_or(Error::Truncated)?;
        self.rest = rest;
        Ok(head)
    }

    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64, Error> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    pub fn addr(&mut self) -> Result<Ipv4Addr, Error> {
        Ok(Ipv4Addr::from(self.u32()?))
    }

    /// How many bytes are not read yet.
    pub fn remaining(&self) -> usize {
        self.rest.len()
    }

    /// Everything not read yet.
    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.rest)
    }

    pub fn str(&mut self) -> Result<String, Error> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::NotText)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub fn pairs(&mut self) -> Result<Vec<(String, String)>, Error> {
        let count = self.u16()? as usize;
        let mut pairs = Vec::with_capacity(count.min(self.rest.len() / 4));
        for _ in 0..count {
            pairs.push((self.str()?, self.str()?));
        }
        Ok(pairs)
    }

    pub fn strs(&mut self) -> Result<Vec<String>, Error> {
        let count = self.u16()? as usize;
        let mut values = Vec::with_capacity(count.min(self.rest.len() / 2));
        for _ in 0..count {
            values.push(self.str()?);
        }
        Ok(values)
    }
}

//! Minimal protobuf wire-format codec for the Antigravity store.
//!
//! Only what the agy conversation DB uses: varints (wire type 0), 64-bit (1),
//! length-delimited (2) and 32-bit (5). Groups (3, 4) are rejected: agy never
//! writes them, and a byte string that "decodes" into a group is not a message.
//! Writers emit fields in the order they are added, which is how the field maps
//! in the research were laid out (ascending numbers).

/// Append `v` as a base-128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Read a varint at `*pos`, advancing it. At most 10 bytes; the tenth may
/// carry only the top bit of a u64.
pub fn get_varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v: u64 = 0;
    for i in 0..10 {
        let Some(&b) = buf.get(*pos) else {
            return Err("truncated varint".into());
        };
        *pos += 1;
        if i == 9 && b > 1 {
            return Err("varint overflows 64 bits".into());
        }
        v |= u64::from(b & 0x7f) << (7 * i);
        if b < 0x80 {
            return Ok(v);
        }
    }
    Err("varint longer than 10 bytes".into())
}

/// A message under construction.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }
    fn key(&mut self, field: u32, wire: u8) {
        put_varint(&mut self.buf, (u64::from(field) << 3) | u64::from(wire));
    }
    pub fn varint(&mut self, field: u32, v: u64) -> &mut Self {
        self.key(field, 0);
        put_varint(&mut self.buf, v);
        self
    }
    /// An int64 field (negative values take ten bytes, as protobuf specifies).
    pub fn int64(&mut self, field: u32, v: i64) -> &mut Self {
        self.varint(field, v as u64)
    }
    pub fn bytes(&mut self, field: u32, v: &[u8]) -> &mut Self {
        self.key(field, 2);
        put_varint(&mut self.buf, v.len() as u64);
        self.buf.extend_from_slice(v);
        self
    }
    pub fn string(&mut self, field: u32, v: &str) -> &mut Self {
        self.bytes(field, v.as_bytes())
    }
    pub fn message(&mut self, field: u32, m: &Writer) -> &mut Self {
        self.bytes(field, &m.buf)
    }
    pub fn finish(&self) -> Vec<u8> {
        self.buf.clone()
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// One decoded field: its number, value and the exact bytes it occupied
/// (key included), so a message can be rewritten without the fields it drops.
#[derive(Debug, Clone, Copy)]
pub struct Field<'a> {
    pub num: u32,
    pub value: Value<'a>,
    pub raw: &'a [u8],
}

/// Decode every top-level field of `buf`.
pub fn parse(buf: &[u8]) -> Result<Vec<Field<'_>>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let start = pos;
        let key = get_varint(buf, &mut pos)?;
        let num = key >> 3;
        if num == 0 || num > u64::from(u32::MAX >> 3) {
            return Err(format!("invalid field number {num}"));
        }
        let value = match key & 7 {
            0 => Value::Varint(get_varint(buf, &mut pos)?),
            1 => {
                let b = buf.get(pos..pos + 8).ok_or("truncated fixed64")?;
                pos += 8;
                Value::Fixed64(u64::from_le_bytes(b.try_into().unwrap()))
            }
            2 => {
                let len = get_varint(buf, &mut pos)?;
                let len = usize::try_from(len).map_err(|_| "length overflows")?;
                let end = pos.checked_add(len).ok_or("length overflows")?;
                let b = buf.get(pos..end).ok_or("truncated length-delimited field")?;
                pos = end;
                Value::Bytes(b)
            }
            5 => {
                let b = buf.get(pos..pos + 4).ok_or("truncated fixed32")?;
                pos += 4;
                Value::Fixed32(u32::from_le_bytes(b.try_into().unwrap()))
            }
            w => return Err(format!("unsupported wire type {w} in field {num}")),
        };
        out.push(Field { num: num as u32, value, raw: &buf[start..pos] });
    }
    Ok(out)
}

/// Read accessors over a decoded message. Missing or mistyped fields read as None.
#[derive(Debug, Clone, Default)]
pub struct Msg<'a> {
    pub fields: Vec<Field<'a>>,
}

impl<'a> Msg<'a> {
    pub fn decode(buf: &'a [u8]) -> Result<Self, String> {
        Ok(Msg { fields: parse(buf)? })
    }
    /// Decode, or an empty message if `buf` is not valid protobuf.
    pub fn lenient(buf: &'a [u8]) -> Self {
        Self::decode(buf).unwrap_or_default()
    }
    pub fn has(&self, num: u32) -> bool {
        self.fields.iter().any(|f| f.num == num)
    }
    pub fn varint(&self, num: u32) -> Option<u64> {
        self.fields.iter().rev().find_map(|f| match (f.num == num, f.value) {
            (true, Value::Varint(v)) => Some(v),
            _ => None,
        })
    }
    pub fn bytes(&self, num: u32) -> Option<&'a [u8]> {
        self.fields.iter().find_map(|f| match (f.num == num, f.value) {
            (true, Value::Bytes(b)) => Some(b),
            _ => None,
        })
    }
    pub fn all_bytes(&self, num: u32) -> Vec<&'a [u8]> {
        self.fields
            .iter()
            .filter_map(|f| match (f.num == num, f.value) {
                (true, Value::Bytes(b)) => Some(b),
                _ => None,
            })
            .collect()
    }
    /// A UTF-8 string field (lossy for invalid bytes).
    pub fn string(&self, num: u32) -> Option<String> {
        self.bytes(num).map(|b| String::from_utf8_lossy(b).into_owned())
    }
    /// A non-empty string field.
    pub fn text(&self, num: u32) -> Option<String> {
        self.string(num).filter(|s| !s.is_empty())
    }
    pub fn message(&self, num: u32) -> Option<Msg<'a>> {
        self.bytes(num).and_then(|b| Msg::decode(b).ok())
    }
}

/// google.protobuf.Timestamp from epoch milliseconds.
pub fn timestamp(ms: i64) -> Writer {
    let mut w = Writer::new();
    let secs = ms.div_euclid(1000);
    let nanos = ms.rem_euclid(1000) * 1_000_000;
    if secs != 0 {
        w.int64(1, secs);
    }
    if nanos != 0 {
        w.int64(2, nanos);
    }
    w
}

/// Epoch milliseconds of a google.protobuf.Timestamp message.
pub fn timestamp_ms(m: &Msg) -> Option<i64> {
    if !m.has(1) && !m.has(2) {
        return None;
    }
    let secs = m.varint(1).unwrap_or(0) as i64;
    let nanos = m.varint(2).unwrap_or(0) as i64;
    Some(secs * 1000 + nanos / 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(v: u64) -> Vec<u8> {
        let mut b = Vec::new();
        put_varint(&mut b, v);
        b
    }

    #[test]
    fn varints_round_trip_at_every_byte_boundary() {
        for (v, len) in [(0u64, 1), (127, 1), (128, 2), (16_383, 2), (16_384, 3), (u32::MAX as u64, 5), (u64::MAX, 10)] {
            let b = enc(v);
            assert_eq!(b.len(), len, "length of {v}");
            let mut p = 0;
            assert_eq!(get_varint(&b, &mut p).unwrap(), v);
            assert_eq!(p, len);
        }
        assert_eq!(enc(300), vec![0xac, 0x02], "the protobuf spec's own example");
    }

    #[test]
    fn malformed_varints_are_rejected_not_misread() {
        let mut p = 0;
        assert!(get_varint(&[0x80, 0x80], &mut p).is_err(), "truncated");
        let mut p = 0;
        assert!(get_varint(&[0xff; 11], &mut p).is_err(), "too long");
        let mut p = 0;
        let mut over = vec![0xff; 9];
        over.push(0x02);
        assert!(get_varint(&over, &mut p).is_err(), "overflows u64");
    }

    #[test]
    fn negative_int64_takes_ten_bytes_and_reads_back() {
        let mut w = Writer::new();
        w.int64(1, -1);
        let b = w.into_bytes();
        assert_eq!(b.len(), 11);
        let m = Msg::decode(&b).unwrap();
        assert_eq!(m.varint(1).unwrap() as i64, -1);
    }

    #[test]
    fn length_delimited_fields_and_lengths_past_127() {
        let long = "x".repeat(200);
        let mut inner = Writer::new();
        inner.string(1, "id").string(2, &long);
        let mut w = Writer::new();
        w.varint(1, 15).message(20, &inner).bytes(4, b"").string(3, "a").string(3, "b");
        let b = w.into_bytes();
        let m = Msg::decode(&b).unwrap();
        assert_eq!(m.varint(1), Some(15));
        let i = m.message(20).unwrap();
        assert_eq!(i.string(2).unwrap().len(), 200);
        assert_eq!(m.bytes(4), Some(&b""[..]), "an empty bytes field is present, not missing");
        assert_eq!(m.all_bytes(3), vec![&b"a"[..], &b"b"[..]], "repeated fields keep order");
        let raw: Vec<u8> = parse(&b).unwrap().iter().flat_map(|f| f.raw.to_vec()).collect();
        assert_eq!(raw, b, "raw spans reassemble the message byte for byte");
    }

    #[test]
    fn fixed_widths_are_skipped_and_truncation_is_an_error() {
        let b = [0x09, 1, 2, 3, 4, 5, 6, 7, 8, 0x15, 1, 2, 3, 4, 0x18, 7];
        let m = Msg::decode(&b).unwrap();
        assert_eq!(m.varint(3), Some(7));
        assert!(parse(&[0x0a, 0x05, b'a']).is_err(), "length past the end");
        assert!(parse(&[0x09, 1, 2]).is_err(), "short fixed64");
        assert!(parse(&[0x0b]).is_err(), "group start wire type");
        assert!(parse(&[0x00, 0x01]).is_err(), "field number zero");
    }

    #[test]
    fn timestamps_keep_milliseconds_including_before_the_epoch() {
        for ms in [0i64, 1, 999, 1_792_591_870_725, -1, -1500] {
            let b = timestamp(ms).into_bytes();
            let m = Msg::decode(&b).unwrap();
            assert_eq!(timestamp_ms(&m).unwrap_or(0), ms, "{ms}");
        }
    }
}

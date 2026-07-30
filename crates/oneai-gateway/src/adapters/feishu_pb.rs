//! Hand-rolled protobuf codec for the Feishu long-connection `Frame` + `Header`
//! messages.
//!
//! Feishu's WS long-connection uses a tiny custom protobuf schema (verified
//! against the official Go SDK `ws/pbbp2.pb.go`). Two messages only, so a
//! hand-rolled codec avoids pulling `prost` + a build-script `.proto` — fits
//! the supply-chain discipline. Wire format:
//!
//! - `Header { Key(1, bytes), Value(2, bytes) }`
//! - `Frame { SeqID(1, varint), LogID(2, varint), Service(3, varint),
//!           Method(4, varint), Headers(5, repeated Header submsg),
//!           PayloadEncoding(6, bytes), PayloadType(7, bytes),
//!           Payload(8, bytes), LogIDNew(9, bytes) }`

#![cfg(feature = "feishu")]

/// A protobuf message header key/value pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    pub key: String,
    pub value: String,
}

/// A Feishu long-connection WS frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<Header>,
    pub payload_encoding: String,
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub log_id_new: String,
}

// FrameType (Method) — control vs data.
pub const FRAME_TYPE_CONTROL: i32 = 0;
pub const FRAME_TYPE_DATA: i32 = 1;

// MessageType (Header "type" value).
pub const MESSAGE_TYPE_EVENT: &str = "event";
pub const MESSAGE_TYPE_PING: &str = "ping";
pub const MESSAGE_TYPE_PONG: &str = "pong";

// ─── varint ────────────────────────────────────────────────────────────────────

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn read_varint(buf: &mut &[u8]) -> Option<u64> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if buf.is_empty() {
            return None;
        }
        let b = buf[0];
        *buf = &buf[1..];
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None; // overflow / malformed
        }
    }
}

/// Field tag = (field_number << 3) | wire_type.
fn put_tag(out: &mut Vec<u8>, field: u32, wire: u32) {
    put_varint(out, ((field as u64) << 3) | (wire as u64 & 0x7));
}

fn length_delimited<'a>(buf: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = read_varint(buf)? as usize;
    if len > buf.len() {
        return None;
    }
    let (head, tail) = buf.split_at(len);
    *buf = tail;
    Some(head)
}

// ─── Header ────────────────────────────────────────────────────────────────────

fn encode_header(h: &Header) -> Vec<u8> {
    let mut out = Vec::new();
    // field 1, wire 2 (bytes): key
    put_tag(&mut out, 1, 2);
    put_varint(&mut out, h.key.len() as u64);
    out.extend_from_slice(h.key.as_bytes());
    // field 2, wire 2: value
    put_tag(&mut out, 2, 2);
    put_varint(&mut out, h.value.len() as u64);
    out.extend_from_slice(h.value.as_bytes());
    out
}

fn decode_header(mut buf: &[u8]) -> Option<Header> {
    let mut h = Header::default();
    while !buf.is_empty() {
        let tag = read_varint(&mut buf)?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 0x7) as u32;
        match (field, wire) {
            (1, 2) => h.key = String::from_utf8_lossy(length_delimited(&mut buf)?).into_owned(),
            (2, 2) => h.value = String::from_utf8_lossy(length_delimited(&mut buf)?).into_owned(),
            _ => skip_field(&mut buf, wire)?,
        }
    }
    Some(h)
}

// ─── Frame ───────────────────────────────────────────────────────────────────

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.payload.len());
        // field 1, varint: SeqID
        put_tag(&mut out, 1, 0);
        put_varint(&mut out, self.seq_id);
        // field 2, varint: LogID
        put_tag(&mut out, 2, 0);
        put_varint(&mut out, self.log_id);
        // field 3, varint: Service
        put_tag(&mut out, 3, 0);
        put_varint(&mut out, self.service as u64);
        // field 4, varint: Method
        put_tag(&mut out, 4, 0);
        put_varint(&mut out, self.method as u64);
        // field 5, repeated submsg: Headers
        for h in &self.headers {
            put_tag(&mut out, 5, 2);
            let enc = encode_header(h);
            put_varint(&mut out, enc.len() as u64);
            out.extend_from_slice(&enc);
        }
        // field 6, bytes: PayloadEncoding (opt — emit only if non-empty)
        if !self.payload_encoding.is_empty() {
            put_tag(&mut out, 6, 2);
            put_varint(&mut out, self.payload_encoding.len() as u64);
            out.extend_from_slice(self.payload_encoding.as_bytes());
        }
        // field 7, bytes: PayloadType (opt)
        if !self.payload_type.is_empty() {
            put_tag(&mut out, 7, 2);
            put_varint(&mut out, self.payload_type.len() as u64);
            out.extend_from_slice(self.payload_type.as_bytes());
        }
        // field 8, bytes: Payload (opt)
        if !self.payload.is_empty() {
            put_tag(&mut out, 8, 2);
            put_varint(&mut out, self.payload.len() as u64);
            out.extend_from_slice(&self.payload);
        }
        // field 9, bytes: LogIDNew (opt)
        if !self.log_id_new.is_empty() {
            put_tag(&mut out, 9, 2);
            put_varint(&mut out, self.log_id_new.len() as u64);
            out.extend_from_slice(self.log_id_new.as_bytes());
        }
        out
    }

    pub fn decode(mut buf: &[u8]) -> Option<Frame> {
        let mut f = Frame::default();
        while !buf.is_empty() {
            let tag = read_varint(&mut buf)?;
            let field = (tag >> 3) as u32;
            let wire = (tag & 0x7) as u32;
            match (field, wire) {
                (1, 0) => f.seq_id = read_varint(&mut buf)?,
                (2, 0) => f.log_id = read_varint(&mut buf)?,
                (3, 0) => f.service = read_varint(&mut buf)? as i32,
                (4, 0) => f.method = read_varint(&mut buf)? as i32,
                (5, 2) => {
                    if let Some(h) = decode_header(length_delimited(&mut buf)?) {
                        f.headers.push(h);
                    }
                }
                (6, 2) => {
                    f.payload_encoding =
                        String::from_utf8_lossy(length_delimited(&mut buf)?).into_owned()
                }
                (7, 2) => {
                    f.payload_type =
                        String::from_utf8_lossy(length_delimited(&mut buf)?).into_owned()
                }
                (8, 2) => f.payload = length_delimited(&mut buf)?.to_vec(),
                (9, 2) => {
                    f.log_id_new = String::from_utf8_lossy(length_delimited(&mut buf)?).into_owned()
                }
                _ => skip_field(&mut buf, wire)?,
            }
        }
        Some(f)
    }

    /// Find the first header value with the given key.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    /// Build a ping control frame (Method=control, Header type=ping).
    pub fn ping(service: i32) -> Frame {
        Frame {
            service,
            method: FRAME_TYPE_CONTROL,
            headers: vec![Header {
                key: "type".to_string(),
                value: MESSAGE_TYPE_PING.to_string(),
            }],
            ..Default::default()
        }
    }
}

/// Skip a protobuf field of the given wire type (for unknown fields).
fn skip_field(buf: &mut &[u8], wire: u32) -> Option<()> {
    match wire {
        0 => {
            read_varint(buf)?;
            Some(())
        }
        2 => {
            length_delimited(buf)?;
            Some(())
        }
        1 => {
            // 64-bit
            if buf.len() < 8 {
                return None;
            }
            *buf = &buf[8..];
            Some(())
        }
        5 => {
            // 32-bit
            if buf.len() < 4 {
                return None;
            }
            *buf = &buf[4..];
            Some(())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_minimal() {
        let f = Frame {
            seq_id: 7,
            log_id: 99,
            service: 42,
            method: FRAME_TYPE_DATA,
            payload: b"{\"hi\":1}".to_vec(),
            ..Default::default()
        };
        let bytes = f.encode();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn frame_roundtrip_with_headers() {
        let f = Frame {
            seq_id: 3,
            log_id: 4,
            service: 1,
            method: FRAME_TYPE_DATA,
            headers: vec![
                Header {
                    key: "type".into(),
                    value: "event".into(),
                },
                Header {
                    key: "message_id".into(),
                    value: "om_123".into(),
                },
            ],
            payload_encoding: "json".into(),
            payload: br#"{"a":"b"}"#.to_vec(),
            log_id_new: "log_x".into(),
            ..Default::default()
        };
        let bytes = f.encode();
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back, f);
        assert_eq!(back.header("type"), Some("event"));
        assert_eq!(back.header("message_id"), Some("om_123"));
        assert!(back.header("missing").is_none());
    }

    #[test]
    fn ping_frame_shape() {
        let f = Frame::ping(7);
        assert_eq!(f.method, FRAME_TYPE_CONTROL);
        assert_eq!(f.service, 7);
        assert_eq!(f.header("type"), Some("ping"));
        // roundtrips
        let back = Frame::decode(&f.encode()).unwrap();
        assert_eq!(back.header("type"), Some("ping"));
        assert_eq!(back.method, FRAME_TYPE_CONTROL);
    }

    #[test]
    fn decode_unknown_field_is_skipped() {
        // Synthesize a frame with a fake field 99 (varint) — decoder must skip it.
        let mut f = Frame {
            seq_id: 1,
            method: FRAME_TYPE_DATA,
            ..Default::default()
        };
        let mut bytes = f.encode();
        // append unknown varint field 99
        put_tag(&mut bytes, 99, 0);
        put_varint(&mut bytes, 12345);
        f.payload = vec![]; // unchanged
        let back = Frame::decode(&bytes).unwrap();
        assert_eq!(back.seq_id, 1);
        assert_eq!(back.method, FRAME_TYPE_DATA);
    }
}

//! MQTT 3.1.1 packets (OASIS mqtt-v3.1.1, section 3) for the desktop client
//! ([`crate::tcp`]) and the test broker ([`crate::broker`]). The firmware
//! uses ESP-IDF's esp-mqtt instead; with LTO none of this is linked there.
//!
//! Only what a Zigbee2MQTT-style bridge needs: CONNECT (will, credentials,
//! keepalive, clean session), CONNACK, PUBLISH at QoS 0/1, PUBACK,
//! SUBSCRIBE/SUBACK, UNSUBSCRIBE/UNSUBACK, PINGREQ/PINGRESP, DISCONNECT.
//! QoS 2 is refused.

/// A last will.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Will {
    /// Topic.
    pub topic: String,
    /// Payload.
    pub payload: Vec<u8>,
    /// QoS 0 or 1.
    pub qos: u8,
    /// Retained.
    pub retain: bool,
}

/// One control packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// CONNECT (3.1).
    Connect {
        /// Client identifier.
        client_id: String,
        /// Keepalive, s (0: off).
        keepalive: u16,
        /// Clean session.
        clean: bool,
        /// User name.
        username: Option<String>,
        /// Password.
        password: Option<Vec<u8>>,
        /// Last will.
        will: Option<Will>,
    },
    /// CONNACK (3.2): return code 0 = accepted.
    ConnAck {
        /// Session present.
        session_present: bool,
        /// Return code.
        code: u8,
    },
    /// PUBLISH (3.3).
    Publish {
        /// Topic name.
        topic: String,
        /// Payload.
        payload: Vec<u8>,
        /// QoS 0 or 1.
        qos: u8,
        /// Retain flag.
        retain: bool,
        /// DUP flag.
        dup: bool,
        /// Packet identifier (QoS 1).
        pid: u16,
    },
    /// PUBACK (3.4).
    PubAck(u16),
    /// SUBSCRIBE (3.8).
    Subscribe {
        /// Packet identifier.
        pid: u16,
        /// Topic filters with their maximum QoS.
        filters: Vec<(String, u8)>,
    },
    /// SUBACK (3.9).
    SubAck {
        /// Packet identifier.
        pid: u16,
        /// Granted QoS per filter (0x80 failure).
        codes: Vec<u8>,
    },
    /// UNSUBSCRIBE (3.10).
    Unsubscribe {
        /// Packet identifier.
        pid: u16,
        /// Topic filters.
        filters: Vec<String>,
    },
    /// UNSUBACK (3.11).
    UnsubAck(u16),
    /// PINGREQ (3.12).
    PingReq,
    /// PINGRESP (3.13).
    PingResp,
    /// DISCONNECT (3.14).
    Disconnect,
}

/// A malformed or unsupported packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MQTT: {}", self.0)
    }
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

fn put_len(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

fn frame(first: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(first);
    put_len(&mut out, body.len());
    out.extend(body);
    out
}

impl Packet {
    /// The packet on the wire.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Packet::Connect {
                client_id,
                keepalive,
                clean,
                username,
                password,
                will,
            } => {
                let mut b = Vec::new();
                put_str(&mut b, b"MQTT");
                b.push(4);
                let mut flags = 0u8;
                if *clean {
                    flags |= 0x02;
                }
                if let Some(w) = will {
                    flags |= 0x04 | (w.qos.min(1) << 3);
                    if w.retain {
                        flags |= 0x20;
                    }
                }
                if password.is_some() {
                    flags |= 0x40;
                }
                if username.is_some() {
                    flags |= 0x80;
                }
                b.push(flags);
                b.extend_from_slice(&keepalive.to_be_bytes());
                put_str(&mut b, client_id.as_bytes());
                if let Some(w) = will {
                    put_str(&mut b, w.topic.as_bytes());
                    put_str(&mut b, &w.payload);
                }
                if let Some(u) = username {
                    put_str(&mut b, u.as_bytes());
                }
                if let Some(p) = password {
                    put_str(&mut b, p);
                }
                frame(0x10, b)
            }
            Packet::ConnAck {
                session_present,
                code,
            } => frame(0x20, vec![u8::from(*session_present), *code]),
            Packet::Publish {
                topic,
                payload,
                qos,
                retain,
                dup,
                pid,
            } => {
                let mut b = Vec::with_capacity(topic.len() + payload.len() + 4);
                put_str(&mut b, topic.as_bytes());
                if *qos > 0 {
                    b.extend_from_slice(&pid.to_be_bytes());
                }
                b.extend_from_slice(payload);
                frame(
                    0x30 | (u8::from(*dup) << 3) | (qos.min(&1) << 1) | u8::from(*retain),
                    b,
                )
            }
            Packet::PubAck(pid) => frame(0x40, pid.to_be_bytes().to_vec()),
            Packet::Subscribe { pid, filters } => {
                let mut b = pid.to_be_bytes().to_vec();
                for (f, q) in filters {
                    put_str(&mut b, f.as_bytes());
                    b.push(*q);
                }
                frame(0x82, b)
            }
            Packet::SubAck { pid, codes } => {
                let mut b = pid.to_be_bytes().to_vec();
                b.extend_from_slice(codes);
                frame(0x90, b)
            }
            Packet::Unsubscribe { pid, filters } => {
                let mut b = pid.to_be_bytes().to_vec();
                for f in filters {
                    put_str(&mut b, f.as_bytes());
                }
                frame(0xA2, b)
            }
            Packet::UnsubAck(pid) => frame(0xB0, pid.to_be_bytes().to_vec()),
            Packet::PingReq => vec![0xC0, 0],
            Packet::PingResp => vec![0xD0, 0],
            Packet::Disconnect => vec![0xE0, 0],
        }
    }

    /// The fixed header at the front of `buf`: `(header bytes, remaining
    /// length)`, `Ok(None)` while incomplete.
    pub fn header(buf: &[u8]) -> Result<Option<(usize, usize)>, DecodeError> {
        let mut len = 0usize;
        let mut mul = 1usize;
        let mut i = 1;
        loop {
            let Some(&b) = buf.get(i) else {
                return Ok(None);
            };
            len += usize::from(b & 0x7F) * mul;
            i += 1;
            if b & 0x80 == 0 {
                return Ok(Some((i, len)));
            }
            mul *= 128;
            if i > 4 {
                return Err(DecodeError("remaining length over 4 bytes"));
            }
        }
    }

    /// One packet from the front of `buf`: `Ok(None)` while incomplete,
    /// else the packet and the bytes it took. Packets whose remaining
    /// length exceeds `max` are refused (memory bound).
    pub fn decode(buf: &[u8], max: usize) -> Result<Option<(Packet, usize)>, DecodeError> {
        let Some((i, len)) = Self::header(buf)? else {
            return Ok(None);
        };
        if len > max {
            return Err(DecodeError("packet too large"));
        }
        if buf.len() < i + len {
            return Ok(None);
        }
        let first = buf[0];
        let p = Packet::body(first, &buf[i..i + len])?;
        Ok(Some((p, i + len)))
    }

    fn body(first: u8, b: &[u8]) -> Result<Packet, DecodeError> {
        let mut r = Rd { b, i: 0 };
        Ok(match first >> 4 {
            1 => {
                if r.bytes()? != b"MQTT" || r.u8()? != 4 {
                    return Err(DecodeError("not MQTT 3.1.1"));
                }
                let flags = r.u8()?;
                let keepalive = r.u16()?;
                let client_id = r.string()?;
                let will = if flags & 0x04 != 0 {
                    Some(Will {
                        topic: r.string()?,
                        payload: r.bytes()?.to_vec(),
                        qos: (flags >> 3) & 3,
                        retain: flags & 0x20 != 0,
                    })
                } else {
                    None
                };
                let username = if flags & 0x80 != 0 {
                    Some(r.string()?)
                } else {
                    None
                };
                let password = if flags & 0x40 != 0 {
                    Some(r.bytes()?.to_vec())
                } else {
                    None
                };
                Packet::Connect {
                    client_id,
                    keepalive,
                    clean: flags & 0x02 != 0,
                    username,
                    password,
                    will,
                }
            }
            2 => Packet::ConnAck {
                session_present: r.u8()? & 1 != 0,
                code: r.u8()?,
            },
            3 => {
                let qos = (first >> 1) & 3;
                if qos > 1 {
                    return Err(DecodeError("QoS 2 is not supported"));
                }
                let topic = r.string()?;
                let pid = if qos > 0 { r.u16()? } else { 0 };
                Packet::Publish {
                    topic,
                    payload: b[r.i..].to_vec(),
                    qos,
                    retain: first & 1 != 0,
                    dup: first & 8 != 0,
                    pid,
                }
            }
            4 => Packet::PubAck(r.u16()?),
            8 => {
                let pid = r.u16()?;
                let mut filters = Vec::new();
                while r.i < b.len() {
                    filters.push((r.string()?, r.u8()?));
                }
                Packet::Subscribe { pid, filters }
            }
            9 => Packet::SubAck {
                pid: r.u16()?,
                codes: b[2.min(b.len())..].to_vec(),
            },
            10 => {
                let pid = r.u16()?;
                let mut filters = Vec::new();
                while r.i < b.len() {
                    filters.push(r.string()?);
                }
                Packet::Unsubscribe { pid, filters }
            }
            11 => Packet::UnsubAck(r.u16()?),
            12 => Packet::PingReq,
            13 => Packet::PingResp,
            14 => Packet::Disconnect,
            _ => return Err(DecodeError("unsupported packet type")),
        })
    }
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Rd<'a> {
    fn u8(&mut self) -> Result<u8, DecodeError> {
        let v = *self.b.get(self.i).ok_or(DecodeError("short packet"))?;
        self.i += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from(self.u8()?) << 8 | u16::from(self.u8()?))
    }
    fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = usize::from(self.u16()?);
        let s = self
            .b
            .get(self.i..self.i + n)
            .ok_or(DecodeError("short packet"))?;
        self.i += n;
        Ok(s)
    }
    fn string(&mut self) -> Result<String, DecodeError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| DecodeError("string is not UTF-8"))
    }
}

/// Whether topic `topic` matches filter `filter` (`+`, `#`; 4.7). Topics
/// starting with `$` do not match filters starting with a wildcard.
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && (filter.starts_with('+') || filter.starts_with('#')) {
        return false;
    }
    let mut f = filter.split('/');
    let mut t = topic.split('/');
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip() {
        let all = [
            Packet::Connect {
                client_id: "zrhub".into(),
                keepalive: 60,
                clean: true,
                username: Some("u".into()),
                password: Some(b"p".to_vec()),
                will: Some(Will {
                    topic: "z/bridge/state".into(),
                    payload: br#"{"state":"offline"}"#.to_vec(),
                    qos: 1,
                    retain: true,
                }),
            },
            Packet::ConnAck {
                session_present: false,
                code: 0,
            },
            Packet::Publish {
                topic: "a/b".into(),
                payload: vec![7; 300],
                qos: 1,
                retain: true,
                dup: false,
                pid: 9,
            },
            Packet::Publish {
                topic: "a".into(),
                payload: Vec::new(),
                qos: 0,
                retain: false,
                dup: false,
                pid: 0,
            },
            Packet::PubAck(9),
            Packet::Subscribe {
                pid: 2,
                filters: vec![("a/+/set".into(), 1), ("b/#".into(), 0)],
            },
            Packet::SubAck {
                pid: 2,
                codes: vec![1, 0],
            },
            Packet::Unsubscribe {
                pid: 3,
                filters: vec!["a/#".into()],
            },
            Packet::UnsubAck(3),
            Packet::PingReq,
            Packet::PingResp,
            Packet::Disconnect,
        ];
        for p in all {
            let w = p.encode();
            assert_eq!(
                Packet::decode(&w[..w.len() - 1], 1 << 20).unwrap(),
                None,
                "{p:?} incomplete"
            );
            let (q, n) = Packet::decode(&w, 1 << 20).unwrap().unwrap();
            assert_eq!((q, n), (p, w.len()));
        }
        // 300-byte payload: two length bytes.
        let w = Packet::Publish {
            topic: "t".into(),
            payload: vec![0; 300],
            qos: 0,
            retain: false,
            dup: false,
            pid: 0,
        }
        .encode();
        assert_eq!(&w[..3], &[0x30, 0xAF, 0x02]);
        assert!(Packet::decode(&w, 100).is_err());
    }

    #[test]
    fn wildcards() {
        assert!(topic_matches("zigbee2mqtt/+/set", "zigbee2mqtt/lamp/set"));
        assert!(!topic_matches("zigbee2mqtt/+/set", "zigbee2mqtt/a/b/set"));
        assert!(topic_matches("zigbee2mqtt/#", "zigbee2mqtt"));
        assert!(topic_matches("zigbee2mqtt/#", "zigbee2mqtt/bridge/state"));
        assert!(topic_matches("+/+", "a/b"));
        assert!(!topic_matches("#", "$SYS/x"));
        assert!(!topic_matches("a/b", "a/b/c"));
    }
}

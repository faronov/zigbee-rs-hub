//! Minimal DNS answers for the hub's setup (README "First-time setup without
//! a console"):
//!
//! - [`catch_all`]: the setup access point's DNS server: every `A` question
//!   is answered with the AP's own address (captive portal), other types
//!   with an empty answer;
//! - [`mdns`] / [`announce`]: an mDNS responder (RFC 6762) for a few
//!   `<name>.local` host names on the station address.
//!
//! Both work on caller buffers (no allocation); anything malformed or
//! unexpected gets no reply.

const TYPE_A: u16 = 1;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const HEADER: usize = 12;
/// Longest name kept (dotted, lowercase).
const MAX_NAME: usize = 64;

fn u16_at(msg: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*msg.get(at)?, *msg.get(at + 1)?]))
}

/// Decodes the name at `at` (with compression pointers) into dotted
/// lowercase in `out`; returns its length and the offset after the name in
/// the message.
fn read_name(msg: &[u8], mut at: usize, out: &mut [u8; MAX_NAME]) -> Option<(usize, usize)> {
    let mut len = 0;
    let mut next = None;
    for _ in 0..32 {
        let l = *msg.get(at)? as usize;
        match l & 0xC0 {
            0xC0 => {
                next.get_or_insert(at + 2);
                at = (u16_at(msg, at)? & 0x3FFF) as usize;
            }
            0 if l == 0 => return Some((len, next.unwrap_or(at + 1))),
            0 => {
                let label = msg.get(at + 1..at + 1 + l)?;
                let dot = usize::from(len > 0);
                if len + dot + l > MAX_NAME {
                    return None;
                }
                if dot == 1 {
                    out[len] = b'.';
                }
                out[len + dot..len + dot + l].copy_from_slice(label);
                out[len + dot..len + dot + l].make_ascii_lowercase();
                len += dot + l;
                at += 1 + l;
            }
            _ => return None,
        }
    }
    None
}

struct Writer<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Writer<'_> {
    fn put(&mut self, data: &[u8]) -> Option<()> {
        self.out.get_mut(self.len..self.len + data.len())?.copy_from_slice(data);
        self.len += data.len();
        Some(())
    }
    fn u16(&mut self, v: u16) -> Option<()> {
        self.put(&v.to_be_bytes())
    }
    fn name(&mut self, dotted: &str) -> Option<()> {
        for label in dotted.split('.').filter(|l| !l.is_empty()) {
            if label.len() > 63 {
                return None;
            }
            self.put(&[label.len() as u8])?;
            self.put(label.as_bytes())?;
        }
        self.put(&[0])
    }
    /// An `A` record: `name` is a compression pointer or written in full.
    fn a(&mut self, name: Result<u16, &str>, class: u16, ttl: u32, ip: [u8; 4]) -> Option<()> {
        match name {
            Ok(ptr) => self.u16(0xC000 | ptr)?,
            Err(dotted) => self.name(dotted)?,
        }
        self.u16(TYPE_A)?;
        self.u16(class)?;
        self.put(&ttl.to_be_bytes())?;
        self.u16(4)?;
        self.put(&ip)
    }
}

/// The setup AP's catch-all answer to a standard query: the first question
/// is echoed and, if it asks for `A`/`ANY` in class IN, answered with `ip`
/// (TTL 10 s); any other question gets an empty NOERROR answer.
pub fn catch_all(query: &[u8], ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    let flags = u16_at(query, 2)?;
    // Responses and opcodes other than QUERY get nothing.
    if flags & 0xF800 != 0 || u16_at(query, 4)? == 0 {
        return None;
    }
    let mut name = [0u8; MAX_NAME];
    let (_, end) = read_name(query, HEADER, &mut name)?;
    let qtype = u16_at(query, end)?;
    let qclass = u16_at(query, end + 2)?;
    let answer = (qtype == TYPE_A || qtype == TYPE_ANY) && qclass == CLASS_IN;
    let mut w = Writer { out, len: 0 };
    w.put(&query[..2])?;
    // QR, the query's RD, RA; NOERROR.
    w.u16(0x8080 | (flags & 0x0100))?;
    w.u16(1)?;
    w.u16(answer as u16)?;
    w.put(&[0; 4])?;
    w.put(query.get(HEADER..end + 4)?)?;
    if answer {
        w.a(Ok(HEADER as u16), CLASS_IN, 10, ip)?;
    }
    Some(w.len)
}

/// Whether `name` (dotted, any case) is one of `names`.
fn ours<'n>(name: &[u8], names: &[&'n str]) -> Option<&'n str> {
    names.iter().copied().find(|n| n.as_bytes().eq_ignore_ascii_case(name))
}

/// mDNS reply time to live (RFC 6762 10: 120 s for host records).
const MDNS_TTL: u32 = 120;

/// Answers an mDNS query whose questions ask for `A`/`ANY` of one of
/// `names` (`host.local`) with `ip`. `legacy` (the query came from a port
/// other than 5353, RFC 6762 6.7): a unicast reply with the query's ID and
/// question, without the cache-flush bit; otherwise a multicast response.
pub fn mdns(query: &[u8], names: &[&str], ip: [u8; 4], legacy: bool, out: &mut [u8]) -> Option<usize> {
    let flags = u16_at(query, 2)?;
    if flags & 0xF800 != 0 {
        return None;
    }
    let mut at = HEADER;
    let mut name = [0u8; MAX_NAME];
    for _ in 0..u16_at(query, 4)?.min(16) {
        let (len, end) = read_name(query, at, &mut name)?;
        let qtype = u16_at(query, end)?;
        // The top bit of the class is "unicast response requested".
        let qclass = u16_at(query, end + 2)? & 0x7FFF;
        if (qtype == TYPE_A || qtype == TYPE_ANY)
            && qclass == CLASS_IN
            && let Some(host) = ours(&name[..len], names)
        {
            let mut w = Writer { out, len: 0 };
            if legacy {
                w.put(&query[..2])?;
                w.u16(0x8400)?;
                w.u16(1)?;
                w.u16(1)?;
                w.put(&[0; 4])?;
                let qstart = w.len;
                w.name(host)?;
                w.u16(TYPE_A)?;
                w.u16(CLASS_IN)?;
                w.a(Ok(qstart as u16), CLASS_IN, 10, ip)?;
            } else {
                w.put(&[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0])?;
                w.a(Err(host), 0x8000 | CLASS_IN, MDNS_TTL, ip)?;
            }
            return Some(w.len);
        }
        at = end + 4;
    }
    None
}

/// An unsolicited mDNS response announcing all `names` at `ip` (RFC 6762
/// 8.3), sent when the station gets an address.
pub fn announce(names: &[&str], ip: [u8; 4], out: &mut [u8]) -> Option<usize> {
    let mut w = Writer { out, len: 0 };
    w.put(&[0, 0, 0x84, 0, 0, 0])?;
    w.u16(names.len() as u16)?;
    w.put(&[0; 4])?;
    for n in names {
        w.a(Err(n), 0x8000 | CLASS_IN, MDNS_TTL, ip)?;
    }
    Some(w.len)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// A query with one question (`name`, `qtype`, class IN or `class`).
    fn query(id: u16, name: &str, qtype: u16, class: u16, out: &mut [u8; 128]) -> usize {
        let mut w = Writer { out, len: 0 };
        w.u16(id).unwrap();
        w.u16(0x0100).unwrap();
        w.u16(1).unwrap();
        w.put(&[0; 6]).unwrap();
        w.name(name).unwrap();
        w.u16(qtype).unwrap();
        w.u16(class).unwrap();
        w.len
    }

    #[test]
    fn catch_all_answers_every_a_query_with_the_ap_address() {
        let mut q = [0u8; 128];
        let n = query(0xBEEF, "connectivitycheck.gstatic.com", TYPE_A, CLASS_IN, &mut q);
        let mut out = [0u8; 512];
        let len = catch_all(&q[..n], [192, 168, 71, 1], &mut out).unwrap();
        let r = &out[..len];
        assert_eq!(&r[..2], &[0xBE, 0xEF]);
        assert_eq!(u16_at(r, 2), Some(0x8180), "response, RD echoed, RA, NOERROR");
        assert_eq!((u16_at(r, 4), u16_at(r, 6)), (Some(1), Some(1)));
        assert_eq!(&r[HEADER..n], &q[HEADER..n], "question echoed");
        assert_eq!(&r[n..], &[0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 10, 0, 4, 192, 168, 71, 1]);
    }

    #[test]
    fn catch_all_leaves_other_types_unanswered_and_ignores_non_queries() {
        let mut q = [0u8; 128];
        let n = query(7, "example.com", 28, CLASS_IN, &mut q);
        let mut out = [0u8; 512];
        let len = catch_all(&q[..n], [10, 0, 0, 1], &mut out).unwrap();
        assert_eq!(u16_at(&out, 6), Some(0), "AAAA: no answer");
        assert_eq!(len, n);
        q[2] |= 0x80;
        assert_eq!(catch_all(&q[..n], [10, 0, 0, 1], &mut out), None, "a response");
        assert_eq!(catch_all(&q[..5], [10, 0, 0, 1], &mut out), None, "truncated");
    }

    #[test]
    fn names_with_pointer_loops_or_overlong_labels_are_rejected() {
        let mut name = [0u8; MAX_NAME];
        let looped = [0u8; HEADER].iter().copied().chain([0xC0, 12]).collect::<std::vec::Vec<_>>();
        assert_eq!(read_name(&looped, HEADER, &mut name), None);
        let mut long = std::vec![0u8; HEADER];
        long.push(63);
        long.extend([b'a'; 63]);
        long.push(10);
        long.extend([b'b'; 10]);
        long.push(0);
        assert_eq!(read_name(&long, HEADER, &mut name), None, "longer than MAX_NAME");
    }

    const NAMES: [&str; 2] = ["zigbee-rs-hub.local", "zigbee-rs-hub-ab12.local"];

    #[test]
    fn mdns_answers_its_names_case_insensitively() {
        let mut q = [0u8; 128];
        let n = query(0, "Zigbee-RS-Hub-AB12.local", TYPE_A, 0x8001, &mut q);
        let mut out = [0u8; 512];
        let len = mdns(&q[..n], &NAMES, [192, 168, 1, 50], false, &mut out).unwrap();
        let r = &out[..len];
        assert_eq!(&r[..12], &[0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let mut name = [0u8; MAX_NAME];
        let (nl, end) = read_name(r, HEADER, &mut name).unwrap();
        assert_eq!(&name[..nl], b"zigbee-rs-hub-ab12.local");
        assert_eq!(&r[end..], &[0, 1, 0x80, 1, 0, 0, 0, 120, 0, 4, 192, 168, 1, 50]);
        let n = query(0, "other.local", TYPE_A, CLASS_IN, &mut q);
        assert_eq!(mdns(&q[..n], &NAMES, [192, 168, 1, 50], false, &mut out), None);
        let n = query(0, "zigbee-rs-hub.local", 28, CLASS_IN, &mut q);
        assert_eq!(mdns(&q[..n], &NAMES, [192, 168, 1, 50], false, &mut out), None, "AAAA");
    }

    #[test]
    fn mdns_finds_a_compressed_second_question_and_answers_legacy_unicast() {
        // "_hap._tcp.local" PTR, then "zigbee-rs-hub" + pointer to "local".
        let mut q = std::vec![0x12, 0x34, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0];
        q.extend([4, b'_', b'h', b'a', b'p', 4, b'_', b't', b'c', b'p', 5]);
        let local = q.len() as u8;
        q.extend(b"local");
        q.extend([0, 0, 12, 0, 1]);
        q.push(13);
        q.extend(b"zigbee-rs-hub");
        q.extend([0xC0, local - 1, 0, 1, 0, 1]);
        let mut out = [0u8; 512];
        let len = mdns(&q, &NAMES, [10, 1, 2, 3], true, &mut out).unwrap();
        let r = &out[..len];
        assert_eq!(&r[..12], &[0x12, 0x34, 0x84, 0, 0, 1, 0, 1, 0, 0, 0, 0], "ID kept, question echoed");
        let mut name = [0u8; MAX_NAME];
        let (nl, end) = read_name(r, HEADER, &mut name).unwrap();
        assert_eq!(&name[..nl], b"zigbee-rs-hub.local");
        assert_eq!(&r[end..], &[0, 1, 0, 1, 0xC0, 12, 0, 1, 0, 1, 0, 0, 0, 10, 0, 4, 10, 1, 2, 3]);
    }

    #[test]
    fn announcement_lists_every_name() {
        let mut out = [0u8; 512];
        let len = announce(&NAMES, [1, 2, 3, 4], &mut out).unwrap();
        assert_eq!(u16_at(&out, 6), Some(2));
        assert_eq!(len, HEADER + (NAMES[0].len() + 2 + 14) + (NAMES[1].len() + 2 + 14));
    }
}

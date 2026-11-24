//! Network map: the coordinator's own tables (ZRHP `DIAG_GET_NEIGHBORS`,
//! `DIAG_GET_ROUTES`, `DIAG_GET_CHILDREN`, spec 7.4/7.8: the NCP does not
//! loop ZDO back to itself) and, breadth first over the routers found,
//! ZDO Mgmt_Lqi_req / Mgmt_Rtg_req over the air (R22 2.4.3.3.2/3).
//!
//! Runs on a job thread: every request blocks until its response or a
//! timeout ([`NetworkAdmin::zdo_request`]).

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use hub_coord::{CoordError, NetworkAdmin};
use hub_devices::hub::ApiError;
use hub_ncp::Hub as Ncp;

use crate::jw::Jw;

/// ZDO Mgmt_Lqi_req.
pub const MGMT_LQI_REQ: u16 = 0x0031;
/// ZDO Mgmt_Rtg_req.
pub const MGMT_RTG_REQ: u16 = 0x0032;

/// A device the application knows (for names and the routers to ask).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    /// IEEE (as in device records: `u64`, big-endian when printed).
    pub ieee: u64,
    /// Short address.
    pub nwk: Option<u16>,
    /// Device ID of the API.
    pub id: String,
    /// Display name.
    pub name: Option<String>,
    /// The node descriptor says router.
    pub router: bool,
}

/// Scan limits.
#[derive(Debug, Clone)]
pub struct ScanOpts {
    /// Also read routing tables.
    pub routes: bool,
    /// Routers asked at most.
    pub max_routers: usize,
    /// Wait for one response.
    pub timeout: Duration,
    /// Whole scan.
    pub budget: Duration,
    /// Stop early (the routers not asked yet are noted).
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Default for ScanOpts {
    fn default() -> Self {
        Self { routes: true, max_routers: 48, timeout: Duration::from_secs(4), budget: Duration::from_secs(150), cancel: None }
    }
}

/// One neighbour table entry (Mgmt_Lqi_rsp / `DIAG_GET_NEIGHBORS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeighborEntry {
    /// IEEE (wire order).
    pub ieee: [u8; 8],
    /// Short address.
    pub nwk: u16,
    /// 0 coordinator, 1 router, 2 end device, 3 unknown.
    pub device_type: u8,
    /// 0 parent, 1 child, 2 sibling, 3 none, 4 previous child.
    pub relationship: u8,
    /// Link quality.
    pub lqi: u8,
    /// Tree depth (Mgmt_Lqi only).
    pub depth: Option<u8>,
}

/// One routing table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteEntry {
    /// Destination.
    pub dst: u16,
    /// Next hop.
    pub next_hop: u16,
    /// 0 active, 1 discovery underway, 2 discovery failed, 3 inactive, 4 validation underway.
    pub status: u8,
}

/// Parse a Mgmt_Lqi_rsp (after the TSN): `(status, total, start, entries)`.
pub fn parse_lqi_rsp(zdp: &[u8]) -> Option<(u8, u8, u8, Vec<NeighborEntry>)> {
    let (&status, rest) = zdp.split_first()?;
    if status != 0 {
        return Some((status, 0, 0, Vec::new()));
    }
    let (&total, rest) = rest.split_first()?;
    let (&start, rest) = rest.split_first()?;
    let (&count, mut rest) = rest.split_first()?;
    let mut out = Vec::new();
    for _ in 0..count {
        if rest.len() < 22 {
            return None;
        }
        let (e, r) = rest.split_at(22);
        rest = r;
        out.push(NeighborEntry {
            ieee: e[8..16].try_into().unwrap(),
            nwk: u16::from_le_bytes([e[16], e[17]]),
            device_type: e[18] & 0x03,
            relationship: (e[18] >> 4) & 0x07,
            depth: Some(e[20]),
            lqi: e[21],
        });
    }
    Some((status, total, start, out))
}

/// Parse a Mgmt_Rtg_rsp (after the TSN): `(status, total, start, entries)`.
pub fn parse_rtg_rsp(zdp: &[u8]) -> Option<(u8, u8, u8, Vec<RouteEntry>)> {
    let (&status, rest) = zdp.split_first()?;
    if status != 0 {
        return Some((status, 0, 0, Vec::new()));
    }
    let (&total, rest) = rest.split_first()?;
    let (&start, rest) = rest.split_first()?;
    let (&count, mut rest) = rest.split_first()?;
    let mut out = Vec::new();
    for _ in 0..count {
        if rest.len() < 5 {
            return None;
        }
        let (e, r) = rest.split_at(5);
        rest = r;
        out.push(RouteEntry { dst: u16::from_le_bytes([e[0], e[1]]), status: e[2] & 0x07, next_hop: u16::from_le_bytes([e[3], e[4]]) });
    }
    Some((status, total, start, out))
}

fn be(ieee: &[u8; 8]) -> String {
    ieee.iter().rev().map(|b| format!("{b:02x}")).collect()
}

const TYPES: [&str; 4] = ["coordinator", "router", "end_device", "unknown"];
const RELATIONS: [&str; 8] = ["parent", "child", "sibling", "none", "previous_child", "unauthenticated_child", "unknown", "unknown"];
const ROUTE_STATUS: [&str; 8] = ["active", "discovery_underway", "discovery_failed", "inactive", "validation_underway", "reserved", "reserved", "reserved"];

struct Zdo<'a> {
    net: &'a dyn NetworkAdmin,
    ncp: &'a Ncp,
    tsn: u8,
    timeout: Duration,
}

impl Zdo<'_> {
    /// One ZDO request to `nwk` and its response's ZDP payload after the TSN.
    fn request(&mut self, nwk: u16, cluster: u16, args: &[u8]) -> Result<Vec<u8>, String> {
        self.tsn = self.tsn.wrapping_add(1);
        let mut zdp = vec![self.tsn];
        zdp.extend_from_slice(args);
        self.net.zdo_request(nwk, cluster, &zdp, self.timeout).map_err(|e| match e.meaning() {
            CoordError::NoAck(c) => {
                let legacy =
                    self.ncp.client().session().is_some_and(|s| hub_ncp::coord::legacy_zb_status(&s.negotiated.fw));
                let st = c.value as u8;
                match hub_ncp::coord::zb_status_text(st, legacy) {
                    Some(t) => format!("not delivered: {t} (zb status 0x{st:02X})"),
                    None => format!("not delivered (zb status 0x{st:02X})"),
                }
            }
            CoordError::Timeout => "no response".into(),
            _ => format!("send: {e}"),
        })
    }

    fn neighbors(&mut self, nwk: u16) -> Result<Vec<NeighborEntry>, String> {
        let mut out = Vec::new();
        for _ in 0..16 {
            let zdp = self.request(nwk, MGMT_LQI_REQ, &[out.len() as u8])?;
            let (status, total, _, page) = parse_lqi_rsp(&zdp).ok_or("malformed Mgmt_Lqi_rsp")?;
            if status != 0 {
                return Err(format!("Mgmt_Lqi_rsp: {}", status_text(status)));
            }
            let n = page.len();
            out.extend(page);
            if n == 0 || out.len() >= usize::from(total) {
                break;
            }
        }
        Ok(out)
    }

    fn routes(&mut self, nwk: u16) -> Result<Vec<RouteEntry>, String> {
        let mut out = Vec::new();
        for _ in 0..16 {
            let zdp = self.request(nwk, MGMT_RTG_REQ, &[out.len() as u8])?;
            let (status, total, _, page) = parse_rtg_rsp(&zdp).ok_or("malformed Mgmt_Rtg_rsp")?;
            if status != 0 {
                return Err(format!("Mgmt_Rtg_rsp: {}", status_text(status)));
            }
            let n = page.len();
            out.extend(page);
            if n == 0 || out.len() >= usize::from(total) {
                break;
            }
        }
        Ok(out)
    }
}

/// A ZDP status (Mgmt_Lqi_rsp, Mgmt_Rtg_rsp) with its name and meaning;
/// delivery failures are named by `hub_ncp::coord::zb_status_text`.
pub fn status_text(code: u8) -> String {
    let name = match code {
        0x80 => "INV_REQUESTTYPE",
        0x81 => "DEVICE_NOT_FOUND",
        0x84 => "NOT_SUPPORTED: the device does not implement this request",
        0x85 => "TIMEOUT",
        0x88 => "NO_ENTRY",
        0x8A => "INSUFFICIENT_SPACE",
        0x8B => "NOT_PERMITTED",
        0x8D => "NOT_AUTHORIZED",
        _ => "",
    };
    if name.is_empty() { format!("status 0x{code:02X}") } else { format!("0x{code:02X} {name}") }
}

/// A link as `source` reports it.
struct Link {
    source: u16,
    target: u16,
    lqi: Option<u8>,
    relationship: u8,
    depth: Option<u8>,
}

/// A routing-table entry of `source`.
struct RouteRow {
    source: u16,
    dst: u16,
    next_hop: u16,
    status: u8,
}

fn add_link(links: &mut Vec<Link>, from: u16, e: &NeighborEntry) {
    links.push(Link { source: from, target: e.nwk, lqi: Some(e.lqi), relationship: e.relationship, depth: e.depth });
}

/// Record what a neighbour entry says about a node; queue unscanned routers.
fn learn(nodes: &mut BTreeMap<u16, Node>, queue: &mut VecDeque<u16>, e: &NeighborEntry) {
    let n = nodes.entry(e.nwk).or_default();
    if e.ieee != [0; 8] && e.ieee != [0xFF; 8] {
        n.ieee = Some(e.ieee);
    }
    if e.device_type < 3 {
        n.device_type = Some(e.device_type);
    }
    if e.device_type == 1 && e.relationship != 4 && !n.scanned && !queue.contains(&e.nwk) && e.nwk != 0 {
        queue.push_back(e.nwk);
    }
}

#[derive(Default)]
struct Node {
    ieee: Option<[u8; 8]>,
    device_type: Option<u8>,
    scanned: bool,
    error: Option<String>,
}

/// Scan the network through `net`; `ncp` for the session (the
/// coordinator's IEEE, the NCP's Zigbee status numbering).
/// `progress(scanned, queued)` after every router. The map's JSON text
/// (structs while scanning, no JSON tree: RAM).
pub fn scan(net: &dyn NetworkAdmin, ncp: &Ncp, known: &[Known], opts: ScanOpts, now_s: f64, progress: &mut dyn FnMut(usize, usize)) -> Result<String, ApiError> {
    let started = Instant::now();
    let mut nodes: BTreeMap<u16, Node> = BTreeMap::new();
    let mut links: Vec<Link> = Vec::new();
    let mut routes: Vec<RouteRow> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let coord_ieee = ncp.client().session().and_then(|s| s.info.as_ref().map(|i| i.ieee_active));
    nodes.insert(0, Node { ieee: coord_ieee, device_type: Some(0), scanned: true, error: None });
    let mut queue: VecDeque<u16> = VecDeque::new();
    // The coordinator, from the NCP's tables.
    let tables = net.coordinator_tables(opts.routes);
    match tables.neighbors {
        Ok(list) => {
            for nb in list.iter().filter(|n| n.relationship != 4) {
                let e = NeighborEntry { ieee: nb.ieee, nwk: nb.nwk, device_type: nb.device_type, relationship: nb.relationship, lqi: nb.lqi, depth: None };
                learn(&mut nodes, &mut queue, &e);
                add_link(&mut links, 0, &e);
            }
        }
        Err(e) => notes.push(format!("coordinator neighbours: {e}")),
    }
    match tables.children {
        Ok(list) => {
            for c in list {
                let n = nodes.entry(c.nwk).or_default();
                n.ieee.get_or_insert(c.ieee);
                n.device_type.get_or_insert(if c.capability & 0x02 != 0 { 1 } else { 2 });
                if !links.iter().any(|l| l.source == 0 && l.target == c.nwk) {
                    links.push(Link { source: 0, target: c.nwk, lqi: None, relationship: 1, depth: None });
                }
            }
        }
        Err(e) => notes.push(format!("coordinator children: {e}")),
    }
    if let Some(r) = tables.routes {
        match r {
            Ok(list) => routes.extend(list.iter().map(|r| RouteRow { source: 0, dst: r.dst, next_hop: r.next_hop, status: r.status })),
            Err(e) => notes.push(format!("coordinator routes: {e}")),
        }
    }
    // Routers the application knows but the coordinator does not see.
    for k in known.iter().filter(|k| k.router) {
        if let Some(nwk) = k.nwk {
            let n = nodes.entry(nwk).or_default();
            n.ieee.get_or_insert(k.ieee.to_le_bytes());
            n.device_type.get_or_insert(1);
            if !queue.contains(&nwk) && nwk != 0 {
                queue.push_back(nwk);
            }
        }
    }

    let mut zdo = Zdo { net, ncp, tsn: 0xA0, timeout: opts.timeout };
    let mut asked = 0;
    while let Some(nwk) = queue.pop_front() {
        let cancelled = opts.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed));
        if cancelled || asked >= opts.max_routers || started.elapsed() > opts.budget {
            notes.push(format!("stopped after {asked} routers ({} not asked)", queue.len() + 1));
            break;
        }
        asked += 1;
        match zdo.neighbors(nwk) {
            Ok(list) => {
                for e in list.iter().filter(|e| e.relationship != 4) {
                    learn(&mut nodes, &mut queue, e);
                    add_link(&mut links, nwk, e);
                }
                nodes.entry(nwk).or_default().scanned = true;
            }
            Err(e) => nodes.entry(nwk).or_default().error = Some(e),
        }
        if opts.routes && nodes.get(&nwk).is_some_and(|n| n.scanned) {
            match zdo.routes(nwk) {
                Ok(list) => routes.extend(list.iter().map(|r| RouteRow { source: nwk, dst: r.dst, next_hop: r.next_hop, status: r.status })),
                // Many routers do not implement Mgmt_Rtg: a fact, not an error.
                Err(e) if e.starts_with("Mgmt_Rtg_rsp: 0x84") => notes.push(format!("0x{nwk:04X} does not report its routes (Mgmt_Rtg not supported)")),
                Err(e) => notes.push(format!("routes of 0x{nwk:04X}: {e}")),
            }
        }
        progress(asked, queue.len());
    }

    // Linear lookups: a collected map would pull a sort into the firmware.
    let by_ieee = |i: &[u8; 8]| known.iter().find(|k| k.ieee.to_le_bytes() == *i);
    let by_nwk = |n: u16| known.iter().find(|k| k.nwk == Some(n));
    let mut w = Jw::value(256 + 150 * nodes.len() + 90 * links.len() + 70 * routes.len())?;
    w.obj().key("time").num(now_s).key("duration_ms").num(started.elapsed().as_millis() as u64);
    w.key("nodes").arr();
    for (nwk, n) in &nodes {
        let k = n.ieee.as_ref().and_then(by_ieee).or_else(|| by_nwk(*nwk));
        w.obj().key("nwk").num(nwk);
        w.opt_str("ieee", n.ieee.as_ref().map(be).as_deref());
        w.key("type").str(TYPES[usize::from(n.device_type.unwrap_or(3).min(3))]);
        w.opt_str("device", k.map(|k| k.id.as_str()));
        w.opt_str("name", k.and_then(|k| k.name.as_deref()));
        w.key("scanned").num(n.scanned);
        w.opt_str("error", n.error.as_deref());
        w.end_obj();
    }
    w.end_arr().key("links").arr();
    for l in &links {
        w.obj().key("source").num(l.source).key("target").num(l.target).key("lqi");
        match l.lqi {
            Some(q) => w.num(q),
            None => w.null(),
        };
        w.key("relationship").str(RELATIONS[usize::from(l.relationship & 7)]).key("depth");
        match l.depth {
            Some(d) => w.num(d),
            None => w.null(),
        };
        w.end_obj();
    }
    w.end_arr().key("routes").arr();
    for r in &routes {
        w.obj().key("source").num(r.source).key("dst").num(r.dst).key("next_hop").num(r.next_hop);
        w.key("status").str(ROUTE_STATUS[usize::from(r.status & 7)]).end_obj();
    }
    w.end_arr().key("notes").arr();
    for n in &notes {
        w.str(n);
    }
    w.end_arr().end_obj();
    w.finish_value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_have_names() {
        assert_eq!(status_text(0x84), "0x84 NOT_SUPPORTED: the device does not implement this request");
        assert_eq!(status_text(0x42), "status 0x42");
    }

    #[test]
    fn mgmt_lqi_and_rtg_responses_parse() {
        let mut zdp = vec![0, 5, 2, 1];
        zdp.extend([0xDD; 8]);
        zdp.extend([1, 2, 3, 4, 5, 6, 7, 8]);
        zdp.extend(0x1234u16.to_le_bytes());
        zdp.extend([0x01 | 0x04 | 0x20, 0x02, 3, 177]);
        let (status, total, start, e) = parse_lqi_rsp(&zdp).unwrap();
        assert_eq!((status, total, start), (0, 5, 2));
        assert_eq!(e[0], NeighborEntry { ieee: [1, 2, 3, 4, 5, 6, 7, 8], nwk: 0x1234, device_type: 1, relationship: 2, lqi: 177, depth: Some(3) });
        assert!(parse_lqi_rsp(&zdp[..20]).is_none(), "truncated entry");
        assert_eq!(parse_lqi_rsp(&[0x84]).unwrap().0, 0x84, "NOT_SUPPORTED");

        let zdp = [0, 1, 0, 1, 0x34, 0x12, 0x10 | 0x02, 0x78, 0x56];
        let (_, total, _, r) = parse_rtg_rsp(&zdp).unwrap();
        assert_eq!(total, 1);
        assert_eq!(r[0], RouteEntry { dst: 0x1234, next_hop: 0x5678, status: 2 });
    }
}

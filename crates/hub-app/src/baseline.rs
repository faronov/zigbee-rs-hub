//! Golden files of the coordinator-boundary baselines
//! (`docs/coordinator-boundary.md` §4 and §7 PR A): the requests the hub
//! sends to the simulated NCP, the host store journal, the catalog areas
//! and API JSON, so that the boundary refactoring can show that none of
//! them changes.
//!
//! The files live in `tests/golden/baseline/`. `UPDATE_GOLDENS=1` rewrites
//! them; otherwise a difference fails with a summary of the changed lines.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value as J;
use zigbee_zrhp::codec::{Reader, Wire};
use zigbee_zrhp::msg::data::{ApsDataRequest, ApsDst};
use zigbee_zrhp::msg::ids;

/// The golden file `name`.
pub fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/baseline")
        .join(name)
}

/// `UPDATE_GOLDENS=1`: write the goldens instead of comparing.
pub fn updating() -> bool {
    std::env::var("UPDATE_GOLDENS").is_ok_and(|v| v == "1")
}

/// Compares `actual` with the golden file `name` (or rewrites it); `Err`
/// describes the difference.
pub fn check(name: &str, actual: &str) -> Result<(), String> {
    let p = path(name);
    if updating() {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, actual).unwrap();
        return Ok(());
    }
    match std::fs::read_to_string(&p) {
        Ok(want) if want == actual => Ok(()),
        Ok(want) => Err(diff(name, &want, actual)),
        Err(e) => Err(format!("golden {name}: {e} (UPDATE_GOLDENS=1 writes it)")),
    }
}

/// Fails with every difference of `results` at once.
pub fn assert_all(results: Vec<Result<(), String>>) {
    let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
    assert!(errors.is_empty(), "{}", errors.join("\n\n"));
}

/// The lines between the common head and tail of both texts.
pub fn diff(name: &str, want: &str, got: &str) -> String {
    const MAX: usize = 40;
    let (a, b): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (da, db) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    let mut s = format!(
        "golden {name} differs from line {}: {} lines in the golden, {} now (UPDATE_GOLDENS=1 rewrites it)\n",
        head + 1,
        da.len(),
        db.len()
    );
    for (sign, lines) in [('-', da), ('+', db)] {
        for (i, l) in lines.iter().enumerate().take(MAX) {
            s += &format!("{sign}{:>6} {l}\n", head + i + 1);
        }
        if lines.len() > MAX {
            s += &format!("{sign}       … {} more\n", lines.len() - MAX);
        }
    }
    s
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

/// A flash image, 32 bytes per line; runs of erased (`0xFF`) lines are one
/// line.
pub fn hexdump(bytes: &[u8]) -> String {
    const ROW: usize = 32;
    let mut s = String::new();
    let mut erased: Option<usize> = None;
    for (i, row) in bytes.chunks(ROW).enumerate() {
        let at = i * ROW;
        if row.iter().all(|b| *b == 0xFF) {
            erased.get_or_insert(at);
            continue;
        }
        if let Some(from) = erased.take() {
            s += &format!("{from:08x} ff × {}\n", at - from);
        }
        s += &format!("{at:08x} {}\n", hex(row));
    }
    if let Some(from) = erased {
        s += &format!("{from:08x} ff × {}\n", bytes.len() - from);
    }
    s
}

/// The image of a [`hexdump`].
pub fn unhexdump(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((at, rest)) = line.split_once(' ') else { continue };
        assert_eq!(usize::from_str_radix(at, 16).unwrap(), out.len(), "{line}");
        match rest.strip_prefix("ff × ") {
            Some(n) => out.resize(out.len() + n.parse::<usize>().unwrap(), 0xFF),
            None => out.extend(unhex(rest)),
        }
    }
    out
}

/// Replaces the values at `paths` (JSON pointers, `*` = every key or index)
/// with `"<volatile>"`; paths that do not exist are left alone.
pub fn normalize(v: &mut J, paths: &[&str]) {
    fn walk(v: &mut J, parts: &[&str]) {
        let Some((first, rest)) = parts.split_first() else {
            *v = J::String("<volatile>".into());
            return;
        };
        match (v, *first) {
            (J::Object(m), "*") => m.values_mut().for_each(|x| walk(x, rest)),
            (J::Array(a), "*") => a.iter_mut().for_each(|x| walk(x, rest)),
            (J::Object(m), k) => {
                if let Some(x) = m.get_mut(k) {
                    walk(x, rest);
                }
            }
            (J::Array(a), k) => {
                if let Some(x) = k.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                    walk(x, rest);
                }
            }
            _ => {}
        }
    }
    for p in paths {
        let parts: Vec<&str> = p.trim_start_matches('/').split('/').collect();
        walk(v, &parts);
    }
}

/// A golden JSON text: keys sorted, volatile fields replaced.
pub fn json(v: &J, volatile: &[&str]) -> String {
    let mut v = v.clone();
    normalize(&mut v, volatile);
    let mut s = serde_json::to_string_pretty(&v).unwrap();
    s.push('\n');
    s
}

/// One request the hub sent to the NCP.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Req {
    pub msg_id: u16,
    pub payload: Vec<u8>,
}

/// Requests of one scenario step, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    pub reqs: Vec<Req>,
}

/// Names of the message IDs, for reading the trace.
pub fn msg_name(id: u16) -> &'static str {
    match id {
        ids::SYS_NEGOTIATE => "SYS_NEGOTIATE",
        ids::SYS_GET_INFO => "SYS_GET_INFO",
        ids::SYS_RESET => "SYS_RESET",
        ids::SYS_GET_STATUS => "SYS_GET_STATUS",
        ids::SYS_SET_CONFIG => "SYS_SET_CONFIG",
        ids::NWK_INIT => "NWK_INIT",
        ids::NWK_FORM => "NWK_FORM",
        ids::NWK_LEAVE_RESET => "NWK_LEAVE_RESET",
        ids::NWK_GET_INFO => "NWK_GET_INFO",
        ids::NWK_PERMIT_JOIN => "NWK_PERMIT_JOIN",
        ids::NWK_ENERGY_SCAN => "NWK_ENERGY_SCAN",
        ids::NWK_CHANGE_CHANNEL => "NWK_CHANGE_CHANNEL",
        ids::NWK_SET_TX_POWER => "NWK_SET_TX_POWER",
        ids::NWK_ACTIVE_SCAN => "NWK_ACTIVE_SCAN",
        ids::NWK_ADD_ENDPOINT => "NWK_ADD_ENDPOINT",
        ids::NWK_GROUP_MEMBERSHIP => "NWK_GROUP_MEMBERSHIP",
        ids::NWK_REMOVE_DEVICE => "NWK_REMOVE_DEVICE",
        ids::APS_DATA_REQUEST => "APS_DATA_REQUEST",
        ids::SEC_GET_NETWORK_KEY => "SEC_GET_NETWORK_KEY",
        ids::SEC_GET_FRAME_COUNTERS => "SEC_GET_FRAME_COUNTERS",
        ids::TBL_SYNC => "TBL_SYNC",
        ids::TBL_READ => "TBL_READ",
        ids::BKP_EXPORT_BEGIN => "BKP_EXPORT_BEGIN",
        ids::BKP_EXPORT_READ => "BKP_EXPORT_READ",
        ids::BKP_EXPORT_END => "BKP_EXPORT_END",
        ids::BKP_RESTORE_BEGIN => "BKP_RESTORE_BEGIN",
        ids::BKP_RESTORE_PUT => "BKP_RESTORE_PUT",
        ids::BKP_RESTORE_COMMIT => "BKP_RESTORE_COMMIT",
        ids::BKP_RESTORE_ABORT => "BKP_RESTORE_ABORT",
        ids::DIAG_GET_COUNTERS => "DIAG_GET_COUNTERS",
        ids::DIAG_RESET_COUNTERS => "DIAG_RESET_COUNTERS",
        ids::DIAG_GET_NEIGHBORS => "DIAG_GET_NEIGHBORS",
        ids::DIAG_GET_ROUTES => "DIAG_GET_ROUTES",
        ids::DIAG_GET_CHILDREN => "DIAG_GET_CHILDREN",
        ids::FW_GET_SLOTS => "FW_GET_SLOTS",
        ids::FW_CONFIRM => "FW_CONFIRM",
        ids::LOG_CONFIG => "LOG_CONFIG",
        ids::GP_SINK_CONFIG => "GP_SINK_CONFIG",
        ids::GP_SINK_COMMISSIONING => "GP_SINK_COMMISSIONING",
        ids::GP_SINK_REMOVE => "GP_SINK_REMOVE",
        ids::GP_PAIRING_CONFIGURATION => "GP_PAIRING_CONFIGURATION",
        ids::GP_TRANSLATION_UPDATE => "GP_TRANSLATION_UPDATE",
        ids::GP_TRANSLATION_REQUEST => "GP_TRANSLATION_REQUEST",
        ids::GP_REMOTE_SINK_TABLE_REQUEST => "GP_REMOTE_SINK_TABLE_REQUEST",
        ids::GP_REMOTE_PROXY_TABLE_REQUEST => "GP_REMOTE_PROXY_TABLE_REQUEST",
        _ => "?",
    }
}

/// The trace file: `@ step` lines, then `msg_id name payload` (hex, `-`
/// when empty) per request.
pub fn render_trace(steps: &[Step]) -> String {
    let mut s = String::from(
        "# Requests the hub sent to the simulated NCP (SimState.trace), per scenario step.\n\
         # msg_id name payload-hex; compared exactly, or with baseline::relaxed.\n",
    );
    for st in steps {
        s += &format!("@ {}\n", st.name);
        for r in &st.reqs {
            let p = if r.payload.is_empty() {
                "-".to_string()
            } else {
                hex(&r.payload)
            };
            s += &format!("{:04x} {} {p}\n", r.msg_id, msg_name(r.msg_id));
        }
    }
    s
}

/// Parses [`render_trace`].
pub fn parse_trace(text: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
    {
        if let Some(name) = line.strip_prefix("@ ") {
            steps.push(Step {
                name: name.to_string(),
                reqs: Vec::new(),
            });
            continue;
        }
        let mut f = line.split(' ');
        let msg_id = u16::from_str_radix(f.next().unwrap(), 16).unwrap();
        let payload = match f.nth(1).unwrap() {
            "-" => Vec::new(),
            h => unhex(h),
        };
        steps
            .last_mut()
            .expect("a request before the first step")
            .reqs
            .push(Req { msg_id, payload });
    }
    steps
}

/// Control requests: the app thread waits for them today, so nothing it
/// sends later reaches the NCP before they complete (the boundary keeps
/// them as barriers).
pub fn is_barrier(r: &Req) -> bool {
    matches!(
        r.msg_id,
        ids::NWK_ADD_ENDPOINT
            | ids::NWK_PERMIT_JOIN
            | ids::NWK_GROUP_MEMBERSHIP
            | ids::NWK_REMOVE_DEVICE
            | ids::TBL_READ
            | ids::DIAG_GET_CHILDREN
            | ids::DIAG_GET_NEIGHBORS
    ) || (ids::GP_SINK_CONFIG..=ids::GP_REMOTE_PROXY_TABLE_REQUEST).contains(&r.msg_id)
}

/// Where a request goes: APS data by destination address, everything else
/// to the NCP itself.
pub fn destination(r: &Req) -> String {
    if r.msg_id != ids::APS_DATA_REQUEST {
        return "ncp".into();
    }
    match ApsDataRequest::read(&mut Reader::new(&r.payload)).map(|a| a.dst) {
        Ok(ApsDst::Nwk(n)) => format!("nwk {n:04x}"),
        Ok(ApsDst::Ieee(i)) => format!("ieee {}", hex(&i.0)),
        Ok(ApsDst::Group(g)) => format!("group {g:04x}"),
        Ok(ApsDst::Broadcast(b)) => format!("broadcast {b:04x}"),
        Err(_) => "undecodable".into(),
    }
}

fn line(r: &Req) -> String {
    format!(
        "{:04x} {} {}",
        r.msg_id,
        msg_name(r.msg_id),
        hex(&r.payload)
    )
}

fn multiset<'a>(reqs: impl IntoIterator<Item = &'a Req>) -> BTreeMap<&'a Req, i64> {
    let mut m = BTreeMap::new();
    for r in reqs {
        *m.entry(r).or_insert(0) += 1;
    }
    m
}

fn multiset_diff(
    what: &str,
    want: &BTreeMap<&Req, i64>,
    got: &BTreeMap<&Req, i64>,
    out: &mut Vec<String>,
) {
    for (r, n) in want {
        let m = got.get(r).copied().unwrap_or(0);
        if m < *n {
            out.push(format!("{what}: missing {}× {}", n - m, line(r)));
        }
    }
    for (r, m) in got {
        let n = want.get(r).copied().unwrap_or(0);
        if *m > n {
            out.push(format!("{what}: unexpected {}× {}", m - n, line(r)));
        }
    }
}

/// The comparison an asynchronous transport still has to pass: the same
/// requests (a multiset), in the same order per [`destination`], and the
/// same barriers ([`is_barrier`]) with the same requests between them.
/// Returns the differences.
pub fn relaxed(
    want: &[Req],
    got: &[Req],
    barrier: &dyn Fn(&Req) -> bool,
    dest: &dyn Fn(&Req) -> String,
) -> Vec<String> {
    let mut out = Vec::new();
    multiset_diff("multiset", &multiset(want), &multiset(got), &mut out);
    let by_dest = |v: &[Req]| {
        let mut m: BTreeMap<String, Vec<Req>> = BTreeMap::new();
        for r in v {
            m.entry(dest(r)).or_default().push(r.clone());
        }
        m
    };
    let (dw, dg) = (by_dest(want), by_dest(got));
    for k in dw.keys().chain(dg.keys().filter(|k| !dw.contains_key(*k))) {
        let (a, b) = (
            dw.get(k).map_or(&[][..], Vec::as_slice),
            dg.get(k).map_or(&[][..], Vec::as_slice),
        );
        if let Some(i) = (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)) {
            let show = |x: Option<&Req>| x.map_or("(none)".to_string(), line);
            out.push(format!(
                "order to {k}: request {i} was {}, now {}",
                show(a.get(i)),
                show(b.get(i))
            ));
        }
    }
    let segments = |v: &'_ [Req]| -> Vec<(Option<Req>, Vec<Req>)> {
        let mut segs = vec![(None, Vec::new())];
        for r in v {
            if barrier(r) {
                segs.last_mut().unwrap().0 = Some(r.clone());
                segs.push((None, Vec::new()));
            } else {
                segs.last_mut().unwrap().1.push(r.clone());
            }
        }
        segs
    };
    let (sw, sg) = (segments(want), segments(got));
    for i in 0..sw.len().max(sg.len()) {
        let (a, b) = (sw.get(i), sg.get(i));
        let name = |s: Option<&(Option<Req>, Vec<Req>)>| match s {
            Some((Some(r), _)) => line(r),
            Some((None, _)) => "(end)".into(),
            None => "(none)".into(),
        };
        if a.map(|s| &s.0) != b.map(|s| &s.0) {
            out.push(format!("barrier {i}: was {}, now {}", name(a), name(b)));
        }
        let what = format!("before barrier {i} ({})", name(a));
        let empty = Vec::new();
        multiset_diff(
            &what,
            &multiset(a.map_or(&empty, |s| &s.1)),
            &multiset(b.map_or(&empty, |s| &s.1)),
            &mut out,
        );
    }
    out
}

/// [`relaxed`] per step, with [`is_barrier`] and [`destination`].
pub fn relaxed_steps(want: &[Step], got: &[Step]) -> Vec<String> {
    let mut out = Vec::new();
    let names = |s: &[Step]| s.iter().map(|x| x.name.clone()).collect::<Vec<_>>();
    if names(want) != names(got) {
        out.push(format!(
            "steps were {:?}, now {:?}",
            names(want),
            names(got)
        ));
    }
    for (a, b) in want.iter().zip(got) {
        out.extend(
            relaxed(&a.reqs, &b.reqs, &is_barrier, &destination)
                .into_iter()
                .map(|e| format!("{}: {e}", a.name)),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aps(nwk: u16, tsn: u8) -> Req {
        let asdu = [0x00, tsn, 0x00];
        let body = ApsDataRequest {
            dst: ApsDst::Nwk(nwk),
            dst_ep: 1,
            src_ep: 1,
            profile: 0x0104,
            cluster: 6,
            tx_options: 0,
            radius: 0,
            non_member_radius: 0,
            priority: 1,
            relays: None,
            alias: None,
            asdu: &asdu,
        };
        Req {
            msg_id: ids::APS_DATA_REQUEST,
            payload: hub_ncp::driver::encode_body(&body).unwrap(),
        }
    }

    fn ctl(id: u16, b: u8) -> Req {
        Req {
            msg_id: id,
            payload: vec![b],
        }
    }

    fn check(want: &[Req], got: &[Req]) -> Vec<String> {
        relaxed(want, got, &is_barrier, &destination)
    }

    #[test]
    fn relaxed_allows_interleaving_of_destinations_only() {
        let (a1, a2, b1) = (aps(0x1111, 1), aps(0x1111, 2), aps(0x2222, 3));
        let pj = ctl(ids::NWK_PERMIT_JOIN, 60);
        let want = vec![
            a1.clone(),
            b1.clone(),
            a2.clone(),
            pj.clone(),
            aps(0x2222, 4),
        ];
        // Another destination's request moves: fine.
        assert_eq!(
            check(
                &want,
                &[
                    b1.clone(),
                    a1.clone(),
                    a2.clone(),
                    pj.clone(),
                    aps(0x2222, 4)
                ]
            ),
            Vec::<String>::new()
        );
        // Order to one destination changes.
        let e = check(
            &want,
            &[
                a2.clone(),
                b1.clone(),
                a1.clone(),
                pj.clone(),
                aps(0x2222, 4),
            ],
        );
        assert!(
            e.iter().any(|e| e.starts_with("order to nwk 1111")),
            "{e:?}"
        );
        // A request passes a barrier (same order per destination, same multiset).
        let e = check(
            &want,
            &[
                a1.clone(),
                b1.clone(),
                pj.clone(),
                a2.clone(),
                aps(0x2222, 4),
            ],
        );
        assert!(e.iter().any(|e| e.contains("before barrier 0")), "{e:?}");
        assert!(
            !e.iter()
                .any(|e| e.starts_with("multiset") || e.starts_with("order")),
            "{e:?}"
        );
        // A request more, a payload changed.
        let e = check(
            &want,
            &[
                a1.clone(),
                b1.clone(),
                a2.clone(),
                a2.clone(),
                pj,
                aps(0x2222, 5),
            ],
        );
        assert!(
            e.iter().any(|e| e.starts_with("multiset: unexpected 1×")),
            "{e:?}"
        );
        assert!(
            e.iter().any(|e| e.starts_with("multiset: missing 1×")),
            "{e:?}"
        );
    }

    #[test]
    fn trace_text_round_trips() {
        let steps = vec![
            Step {
                name: "boot".into(),
                reqs: vec![
                    ctl(ids::SYS_NEGOTIATE, 1),
                    Req {
                        msg_id: ids::NWK_GET_INFO,
                        payload: vec![],
                    },
                ],
            },
            Step {
                name: "empty".into(),
                reqs: vec![],
            },
            Step {
                name: "join".into(),
                reqs: vec![aps(0x1234, 9)],
            },
        ];
        assert_eq!(parse_trace(&render_trace(&steps)), steps);
    }

    #[test]
    fn normalize_and_hexdump() {
        let mut v = serde_json::json!({"a": {"t": 1, "k": 2}, "l": [{"t": 3}, {"t": 4, "u": 5}]});
        normalize(&mut v, &["/a/t", "/l/*/t", "/missing/x"]);
        assert_eq!(
            v,
            serde_json::json!({"a": {"t": "<volatile>", "k": 2}, "l": [{"t": "<volatile>"}, {"t": "<volatile>", "u": 5}]})
        );
        let mut img = vec![0xFF; 128];
        img[33] = 0;
        assert_eq!(
            hexdump(&img),
            format!(
                "00000000 ff × 32\n00000020 ff00{}\n00000040 ff × 64\n",
                "ff".repeat(30)
            )
        );
        assert_eq!(unhexdump(&hexdump(&img)), img);
    }
}

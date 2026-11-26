//! Vendor templates the hub implements (spec 17.4): hub code that a
//! definition turns on by name (`templates: [{name: …}]`, feature
//! `template.<name>`).
//!
//! `tuya_metering_zero_debounce` is zigbee-herdsman-converters'
//! `tuya.fz.TS011F_electrical_measurement` (Tuya TS011F plugs,
//! `TS011F_plug_1` and `TS011F_with_threshold`): these plugs report short
//! spurious zeros, so a zero power, current or voltage reading (Electrical
//! Measurement 0x050B, 0x0508, 0x0505) is held back for twice the
//! attribute's minimum reporting interval plus one second (5 s when it
//! has none) and published only if no other reading of it came meanwhile;
//! a held zero current also sets the power to 0 (the plug is slow to
//! report that). While the plug's switch is off, every Electrical
//! Measurement report sets the power to 0 at once.

use crate::device::{Device, Millis};
use crate::expr::Value;
use crate::model::{Binding, ReportSpec};
use crate::values::DecodeFlags;

/// Template name of the TS011F metering debounce.
pub const METERING_ZERO_DEBOUNCE: &str = "tuya_metering_zero_debounce";

const ELECTRICAL: u16 = 0x0B04;
const ACTIVE_POWER: u16 = 0x050B;
const RMS_CURRENT: u16 = 0x0508;
const RMS_VOLTAGE: u16 = 0x0505;

/// Decoded values of one dispatch: `(entity, part, value, flags)`.
pub(crate) type Decoded = Vec<(usize, Option<usize>, Value, DecodeFlags)>;

/// The `(endpoint, attribute)` of an entity whose own value is a ZCL
/// attribute of `cluster`.
fn zcl_of(d: &Device, i: usize, cluster: u16) -> Option<(u8, u16)> {
    let e = &d.entities[i];
    match &e.r.def.value.binding {
        Binding::Zcl(z) if z.cluster == cluster && z.mfr.is_none() && !e.orphaned => {
            Some((e.r.ep, z.attr))
        }
        _ => None,
    }
}

fn find(d: &Device, ep: u8, cluster: u16, attr: u16) -> Option<usize> {
    (0..d.entities.len()).find(|&i| zcl_of(d, i, cluster) == Some((ep, attr)))
}

/// A zero of the same kind as `v` (scaled values are floats).
fn zero_like(v: &Value) -> Value {
    match v {
        Value::Float(_) => Value::Float(0.0),
        _ => Value::Int(0),
    }
}

/// How long a zero of entity `i` is held: zhc's (minimum reporting
/// interval × 2 + 1) s.
fn hold_ms(d: &Device, i: usize) -> Millis {
    let e = &d.entities[i];
    let min = match crate::reporting::value_report(d, e.r.ep, &e.r.def.value) {
        Some(ReportSpec::Config { min, .. }) => u64::from(min),
        _ => 5,
    };
    (min * 2 + 1) * 1000
}

/// Applies the template to the values decoded from one report (or read):
/// zero readings are held back, other readings cancel a held zero, and
/// power is 0 while the switch of its endpoint is off.
pub(crate) fn zero_debounce(d: &mut Device, decoded: &mut Decoded, now: Millis) {
    let mut out: Decoded = Vec::with_capacity(decoded.len());
    let mut eps: Vec<u8> = Vec::new();
    for (i, p, v, f) in decoded.drain(..) {
        let m = zcl_of(d, i, ELECTRICAL)
            .filter(|(_, a)| p.is_none() && matches!(*a, ACTIVE_POWER | RMS_CURRENT | RMS_VOLTAGE));
        if let Some((ep, _)) = m {
            if !eps.contains(&ep) {
                eps.push(ep);
            }
            if v.as_f64() == Some(0.0) {
                let at = now + hold_ms(d, i);
                d.entities[i].state.held = Some((at, v));
                continue;
            }
            d.entities[i].state.held = None;
        }
        out.push((i, p, v, f));
    }
    for ep in eps {
        let off = find(d, ep, 0x0006, 0x0000)
            .is_some_and(|s| d.entities[s].state.value == Value::Bool(false));
        if let (true, Some(pw)) = (off, find(d, ep, ELECTRICAL, ACTIVE_POWER)) {
            let zero = match out.iter().position(|w| w.0 == pw && w.1.is_none()) {
                Some(k) => zero_like(&out.remove(k).2),
                None => d.entities[pw]
                    .state
                    .held
                    .take()
                    .map_or_else(|| zero_like(&d.entities[pw].state.value), |h| h.1),
            };
            d.entities[pw].state.held = None;
            out.push((pw, None, zero, DecodeFlags::default()));
        }
    }
    *decoded = out;
}

/// The held zeros that are due, as `(entity, value)`: a zero current also
/// zeroes the power of its endpoint.
pub(crate) fn due(d: &mut Device, now: Millis) -> Vec<(usize, Value)> {
    let mut out = Vec::new();
    for i in 0..d.entities.len() {
        let Some((_, v)) = d.entities[i].state.held.take_if(|h| now >= h.0) else {
            continue;
        };
        if let Some((ep, RMS_CURRENT)) = zcl_of(d, i, ELECTRICAL) {
            if let Some(pw) = find(d, ep, ELECTRICAL, ACTIVE_POWER) {
                d.entities[pw].state.held = None;
                out.retain(|(j, _)| *j != pw);
                let z = zero_like(&d.entities[pw].state.value);
                out.push((pw, z));
            }
        }
        out.retain(|(j, _)| *j != i);
        out.push((i, v));
    }
    out
}

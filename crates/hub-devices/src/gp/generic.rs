//! Generic GP tier (spec 13.4) and the GPD report decoders (0xA0–0xA3).

use crate::device::AttrCache;
use crate::expr::{NativeExpr, Value};
use crate::fingerprint::Fingerprint;
use crate::generic::{device_level, measurement};
use crate::model::*;
use crate::zcl::types::{CodecError, Reader, decode_value};
use crate::zcl::{TypedValue, ZclType, cluster as cl};

use super::GP_ENDPOINT;

/// Pseudo cluster under which the hub remembers GPD commands it has seen
/// (attribute = command ID) in the GP device's [`AttrCache`].
pub const SEEN_COMMANDS: u16 = 0xFFFF;

/// One decoded GP attribute record: cluster, attribute, manufacturer
/// code, value.
pub type GpRecord = (u16, u16, Option<u16>, TypedValue);

fn records(
    r: &mut Reader<'_>,
    cluster: Option<u16>,
    mfr: Option<u16>,
    out: &mut Vec<GpRecord>,
) -> Result<(), CodecError> {
    while r.remaining() > 0 {
        let c = match cluster {
            Some(c) => c,
            None => r.u16()?,
        };
        let attr = r.u16()?;
        let ty = ZclType(r.u8()?);
        let v = decode_value(ty, r)?;
        out.push((c, attr, mfr, TypedValue::new(ty, v)));
    }
    Ok(())
}

/// Decodes 0xA0 (attribute reporting), 0xA1 (manufacturer-specific),
/// 0xA2 (multi-cluster) and 0xA3 (manufacturer multi-cluster) payloads.
pub fn decode_report(cmd: u8, payload: &[u8]) -> Result<Vec<GpRecord>, CodecError> {
    let mut r = Reader::new(payload);
    let mut out = Vec::new();
    match cmd {
        0xA0 => {
            let c = r.u16()?;
            records(&mut r, Some(c), None, &mut out)?;
        }
        0xA1 => {
            let m = r.u16()?;
            let c = r.u16()?;
            records(&mut r, Some(c), Some(m), &mut out)?;
        }
        0xA2 => records(&mut r, None, None, &mut out)?,
        0xA3 => {
            let m = r.u16()?;
            records(&mut r, None, Some(m), &mut out)?;
        }
        _ => return Err(CodecError::Invalid),
    }
    Ok(out)
}

/// GPD DeviceIDs that are switches (no availability tracking, 4.6).
pub fn is_switch(device_id: u8) -> bool {
    matches!(device_id, 0x00..=0x07 | 0x10 | 0x20)
}

/// The event type of a GPD command (13.4).
pub fn command_event(cmd: u8) -> Option<String> {
    Some(match cmd {
        0x20 => "off".into(),
        0x21 => "on".into(),
        0x22 => "toggle".into(),
        0x23 => "release".into(),
        0x10..=0x17 => format!("recall_scene_{}", cmd - 0x10),
        0x18..=0x1F => format!("store_scene_{}", cmd - 0x18),
        0x30 | 0x35 => "brightness_move_up".into(),
        0x31 | 0x36 => "brightness_move_down".into(),
        0x32 | 0x37 => "brightness_step_up".into(),
        0x33 | 0x38 => "brightness_step_down".into(),
        0x34 => "brightness_stop".into(),
        0x40..=0x4B => [
            "hue_stop",
            "hue_move_up",
            "hue_move_down",
            "hue_step_up",
            "hue_step_down",
            "saturation_stop",
            "saturation_move_up",
            "saturation_move_down",
            "saturation_step_up",
            "saturation_step_down",
            "color_move",
            "color_step",
        ][usize::from(cmd - 0x40)]
        .into(),
        0x50 => "lock".into(),
        0x51 => "unlock".into(),
        0x60 => "press_1_of_1".into(),
        0x61 => "release_1_of_1".into(),
        0x62 => "press_1_of_2".into(),
        0x63 => "release_1_of_2".into(),
        0x64 => "press_2_of_2".into(),
        0x65 => "release_2_of_2".into(),
        0x66 => "short_press_1_of_1".into(),
        0x67 => "short_press_1_of_2".into(),
        0x68 => "short_press_2_of_2".into(),
        0x69 => "press".into(),
        0x6A => "release".into(),
        _ => return None,
    })
}

/// Typical commands of a DeviceID when the GPD sent no command list.
fn typical_commands(device_id: u8) -> Vec<u8> {
    match device_id {
        0x00 => vec![0x60, 0x61, 0x66],
        0x01 => vec![0x62, 0x63, 0x64, 0x65, 0x67, 0x68],
        0x02 => vec![0x20, 0x21, 0x22],
        0x03 => vec![0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38],
        0x07 => vec![0x69, 0x6A],
        0x10 => (0x40..=0x4B).collect(),
        0x20 => vec![0x50, 0x51],
        _ => Vec::new(),
    }
}

fn to_gp(mut e: EntityDef) -> EntityDef {
    for (_, v) in
        std::iter::once(("", &mut e.value)).chain(e.parts.iter_mut().map(|(n, v)| (n.as_str(), v)))
    {
        if let Binding::Zcl(z) = &v.binding {
            v.binding = Binding::Gp(GpBinding::Report {
                cluster: z.cluster,
                attr: z.attr,
                ty: z.ty,
                mfr: None,
            });
        }
        v.report = None;
        v.access = Some(Access {
            read: false,
            write: false,
            report: true,
        });
    }
    e.ep = Some(GP_ENDPOINT);
    e
}

/// Entities of the generic GP tier.
pub fn gp_generic_entities(fp: &Fingerprint, attrs: &AttrCache) -> Vec<EntityDef> {
    let Some(g) = &fp.gp else { return Vec::new() };
    let mut out: Vec<EntityDef> = Vec::new();
    // Commands → `action` event / per-contact events.
    let mut cmds: Vec<u8> = if g.commands.is_empty() {
        typical_commands(g.device_id)
    } else {
        g.commands.clone()
    };
    for &(ep, c, a, _) in attrs.map.keys() {
        if ep == GP_ENDPOINT && c == SEEN_COMMANDS && !cmds.contains(&(a as u8)) {
            cmds.push(a as u8);
        }
    }
    let vector = cmds.iter().any(|c| matches!(c, 0x69 | 0x6A));
    let contacts = g.switch_info.map_or(0, |(n, _)| n.min(8));
    if vector && contacts > 0 {
        for n in 0..contacts {
            let mut e = EntityDef::new(format!("button_{}", n + 1), Platform::Event);
            e.device_class = Some("button".into());
            e.event_types = vec!["press".into(), "release".into()];
            e.ep = Some(GP_ENDPOINT);
            // The runtime compares with the previous contact bitmap.
            e.capabilities.insert("contact_bit".into(), n.into());
            out.push(e);
        }
    }
    let mut types: Vec<String> = Vec::new();
    let mut triggers = Vec::new();
    for &c in &cmds {
        if (vector && contacts > 0 && matches!(c, 0x69 | 0x6A))
            || matches!(c, 0xA0..=0xA3 | 0xA8 | 0xB0..=0xBF | 0xE0..=0xFF)
        {
            continue;
        }
        let Some(t) = command_event(c) else { continue };
        if !types.contains(&t) {
            types.push(t.clone());
        }
        let mut attrs_e: Vec<(String, crate::expr::Expr)> = Vec::new();
        match c {
            0x30..=0x38 => {
                attrs_e.push((
                    "rate".into(),
                    NativeExpr::new("payload[0]", |c| {
                        c.payload
                            .and_then(|p| p.first())
                            .map_or(Value::Null, |b| Value::Int(i64::from(*b)))
                    }),
                ));
            }
            0x69 | 0x6A => {
                attrs_e.push((
                    "contacts".into(),
                    NativeExpr::new("payload[0]", |c| {
                        c.payload
                            .and_then(|p| p.first())
                            .map_or(Value::Null, |b| Value::Int(i64::from(*b)))
                    }),
                ));
            }
            _ => {}
        }
        attrs_e.push((
            "command".into(),
            NativeExpr::new("gpd_cmd", move |_| Value::Int(i64::from(c))),
        ));
        triggers.push(Trigger {
            source: TriggerSource::GpCmd(c),
            when: None,
            event: EventSel::Fixed(t),
            attrs: attrs_e,
        });
    }
    if !triggers.is_empty() {
        let mut e = EntityDef::new("action", Platform::Event);
        e.device_class = Some("button".into());
        e.event_types = types;
        e.triggers = triggers;
        e.ep = Some(GP_ENDPOINT);
        out.push(e);
    }
    // Reports: announced clusters up front, then every attribute seen.
    let mut pairs: Vec<(u16, u16)> = Vec::new();
    for &c in &g.server_clusters {
        let attrs_of: &[u16] = if c == cl::POWER_CONFIG {
            &[0x0021, 0x0020]
        } else {
            &[0x0000]
        };
        for &a in attrs_of {
            pairs.push((c, a));
        }
    }
    for &(ep, c, a, m) in attrs.map.keys() {
        if ep == GP_ENDPOINT && c != SEEN_COMMANDS && m.is_none() && !pairs.contains(&(c, a)) {
            pairs.push((c, a));
        }
    }
    for (c, a) in pairs {
        if let Some(e) = measurement(c, a) {
            if !out.iter().any(|x| x.key == e.key) {
                out.push(to_gp(e));
            }
        }
    }
    let mut dl = device_level(GP_ENDPOINT);
    for (key, f) in [
        ("frame_counter", DeviceField::FrameCounter),
        ("security_level", DeviceField::SecurityLevel),
    ] {
        let mut e = EntityDef::new(key, Platform::Sensor);
        e.category = Some(Category::Diagnostic);
        e.enabled = key == "security_level";
        e.ep = Some(GP_ENDPOINT);
        e.value.binding = Binding::DeviceField(f);
        e.value.access = Some(Access {
            read: true,
            write: false,
            report: true,
        });
        dl.push(e);
    }
    out.extend(dl);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zcl::ZclValue;

    #[test]
    fn report_decoders() {
        // zigbee-rs MultiClusterReport layout: cluster, attr, type, value.
        let p = [
            0x02, 0x04, 0x00, 0x00, 0x29, 0x5A, 0x08, 0x01, 0x00, 0x21, 0x00, 0x20, 0xC8,
        ];
        let r = decode_report(0xA2, &p).unwrap();
        assert_eq!(
            r[0],
            (
                0x0402,
                0x0000,
                None,
                TypedValue::new(ZclType::I16, ZclValue::Int(2138))
            )
        );
        assert_eq!(r[1].0, 0x0001);
        let p = [0x6B, 0x6D, 0x6C, 0xFC, 0x01, 0x00, 0x29, 0x10, 0x00];
        let r = decode_report(0xA1, &p).unwrap();
        assert_eq!(r[0].0, 0xFC6C);
        assert_eq!(r[0].2, Some(0x6D6B));
        assert!(decode_report(0xA2, &[0x02, 0x04, 0x00]).is_err());
        assert_eq!(command_event(0x43).as_deref(), Some("hue_step_up"));
        assert_eq!(command_event(0x12).as_deref(), Some("recall_scene_2"));
    }
}

//! Matching, resolution and robustness tests over the compiled examples
//! (crates/zdefc/tests/golden/*.cbor).

use std::fs;
use std::path::Path;
use zdef::expr::{Inputs, NoHost, Value, Vm};
use zdef::matching::{
    Endpoint, GpFingerprint, GpdId, Skipped, lookup_hashes, score_clause, select_best,
};
use zdef::model::*;
use zdef::resolve::{Layers, Target, resolve};
use zdef::{Fingerprint, HubFeatures, Identity, Source};

fn golden(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../zdefc/tests/golden")
            .join(format!("{name}.cbor")),
    )
    .unwrap()
}

fn def(name: &str) -> Definition {
    zdef::load_canonical(&golden(name), &HubFeatures::v1()).unwrap()
}

fn all_golden() -> Vec<(String, Vec<u8>)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../zdefc/tests/golden");
    let mut v: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "cbor"))
        .map(|p| {
            (
                p.file_stem().unwrap().to_string_lossy().into_owned(),
                fs::read(&p).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

fn zb(mfr: &str, model: &str) -> Fingerprint {
    Fingerprint {
        manufacturer_name: Some(mfr.into()),
        model_id: Some(model.into()),
        endpoints: vec![Endpoint {
            ep: 1,
            profile: 0x0104,
            device_id: 0x0302,
            in_clusters: vec![0x0000, 0x0402, 0xEF00],
            out_clusters: vec![0x0019],
        }],
        ..Default::default()
    }
}

fn gp(device_id: u8, mfr: Option<u16>, model: Option<u16>, src: u32) -> GpFingerprint {
    GpFingerprint {
        gpd_id: GpdId::SrcId(src),
        device_id,
        manufacturer_id: mfr,
        model_id: model,
        commands: vec![0xA2],
        server_clusters: vec![],
        client_clusters: vec![],
        switch_info: None,
        security_level: None,
    }
}

#[test]
fn every_golden_loads_and_is_canonical() {
    for (name, b) in all_golden() {
        let d =
            zdef::load_canonical(&b, &HubFeatures::v1()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(d.encode().unwrap(), b, "{name}");
        assert_eq!(zdef::ContentHash::of(&b), d.content_hash().unwrap());
    }
}

#[test]
fn tuya_match_with_packed_names() {
    let d = def("tuya-ts0601-trv-me167");
    let names = d.match_names();
    assert!(names.contains(&"_TZE200_bvu2wnxz") && names.contains(&"_TZE200_4utwoz2"));
    assert_eq!(
        d.match_score(Identity::Zigbee(&zb("_TZE200_bvu2wnxz", "TS0601"))),
        Some(500)
    );
    assert_eq!(
        d.match_score(Identity::Zigbee(&zb("_TZE200_bvu2wnxz\0\0 ", "TS0601"))),
        Some(500),
        "trailing NULs/spaces trimmed"
    );
    assert_eq!(
        d.match_score(Identity::Zigbee(&zb("_TZE200_xxxxxxxx", "TS0601"))),
        None
    );
    assert_eq!(
        d.match_score(Identity::Zigbee(&zb("_tze200_bvu2wnxz", "TS0601"))),
        None,
        "case-sensitive"
    );
    assert_eq!(
        d.display_identity(Some("_TZE284_o3x45p96")),
        (Some("AVATTO"), Some("TRV06_1"))
    );
    assert_eq!(
        d.display_identity(Some("_TZE200_ow09xlxm")),
        (Some("THALEOS"), Some("TRV06-AT"))
    );
    assert_eq!(
        d.display_identity(Some("_TZE200_d3z1ukqw\0")),
        (Some("Echos"), Some("Eco-4160"))
    );
    assert_eq!(
        d.display_identity(Some("other")),
        (Some("Tuya"), Some("TS0601_thermostat_3"))
    );
}

#[test]
fn scoring_table() {
    let exact = MatchClause {
        manufacturer: Some(NameMatch::Exact("M".into())),
        model: Some(NameMatch::Exact("X".into())),
        ..Default::default()
    };
    let prefix = MatchClause {
        manufacturer: Some(NameMatch::Prefix("_TZE2".into())),
        model: Some(NameMatch::Exact("X".into())),
        ..Default::default()
    };
    let model = MatchClause {
        model: Some(NameMatch::Exact("X".into())),
        ..Default::default()
    };
    let fp_only = MatchClause {
        endpoints: vec![(
            1,
            EpMatch {
                profile: Some(0x0104),
                device_id: Some(0x0302),
                in_clusters: vec![0x0402],
                out_clusters: vec![],
            },
        )],
        ..Default::default()
    };
    let with_ver = MatchClause {
        app_version: Some(VersionRange {
            min: Some(2),
            max: None,
        }),
        power: Some(Power::Mains),
        ..model.clone()
    };
    let f = |m: &str| {
        let mut x = zb(m, "X");
        x.app_version = Some(3);
        x.power_source = Some(1);
        x
    };
    assert_eq!(score_clause(&exact, Identity::Zigbee(&f("M"))), Some(500));
    assert_eq!(
        score_clause(&prefix, Identity::Zigbee(&f("_TZE204_aaaaaaaa"))),
        Some(400)
    );
    assert_eq!(
        score_clause(&prefix, Identity::Zigbee(&f("_TZ3000_aaaaaaaa"))),
        None
    );
    assert_eq!(score_clause(&model, Identity::Zigbee(&f("any"))), Some(300));
    assert_eq!(
        score_clause(&fp_only, Identity::Zigbee(&f("any"))),
        Some(200)
    );
    assert_eq!(
        score_clause(&with_ver, Identity::Zigbee(&f("any"))),
        Some(320)
    );
    let mut old = f("any");
    old.app_version = Some(1);
    assert_eq!(score_clause(&with_ver, Identity::Zigbee(&old)), None);
    assert_eq!(
        score_clause(&MatchClause::default(), Identity::Zigbee(&f("M"))),
        None,
        "empty clause"
    );
    assert_eq!(
        score_clause(&exact, Identity::Gp(&gp(2, None, None, 1))),
        None
    );
}

#[test]
fn gp_matching_with_masks_and_ranges() {
    let flower = def("gp-diyruz-flowerx");
    for bits in 0..4u16 {
        assert_eq!(
            flower.match_score(Identity::Gp(&gp(
                0x33,
                Some(0x6D6B),
                Some(0xF100 | bits),
                7
            ))),
            Some(600),
            "bits {bits} (spec 13.3: no extras for GP)"
        );
    }
    assert_eq!(
        flower.match_score(Identity::Gp(&gp(0x33, Some(0x6D6B), Some(0xF200), 7))),
        None
    );
    assert_eq!(
        flower.match_score(Identity::Gp(&gp(0x30, Some(0x6D6B), Some(0xF100), 7))),
        None
    );
    let ptm = def("gp-enocean-ptm216z");
    assert_eq!(
        ptm.match_score(Identity::Gp(&gp(0x07, None, None, 0x0150_1234))),
        Some(600)
    );
    assert_eq!(
        ptm.match_score(Identity::Gp(&gp(0x07, None, None, 0x0160_0000))),
        None
    );
    assert_eq!(
        ptm.match_score(Identity::Gp(&gp(0x02, None, None, 0x0150_1234))),
        None,
        "PTM 215ZE: same block, DeviceID 0x02"
    );
    let sensor = def("gp-zigbee-rs-sensor");
    assert_eq!(
        sensor.match_score(Identity::Gp(&gp(0x33, Some(0x6D6B), Some(0xF300), 1))),
        Some(600)
    );
    let h = lookup_hashes(Identity::Gp(&gp(0x33, Some(0x6D6B), Some(0xF300), 1)));
    let keys = zdef::matching::clause_keys(&sensor.match_[0]).unwrap();
    assert!(keys.iter().all(|(k, _)| h.contains(&k.hash())));
    let fk = zdef::matching::clause_keys(&flower.match_[0]).unwrap();
    assert!(fk.iter().all(|(k, _)| {
        lookup_hashes(Identity::Gp(&gp(0x33, Some(0x6D6B), Some(0xF103), 1))).contains(&k.hash())
    }));
}

#[test]
fn selection_tie_breaks_and_requires() {
    let mut a = def("innr-sp120");
    let mut b = a.clone();
    b.revision = Some(5);
    let fp = zb("innr", "SP 120");
    let s = select_best(
        &[(Source::Catalog, &a), (Source::Catalog, &b)],
        Identity::Zigbee(&fp),
        &HubFeatures::v1(),
        None,
    )
    .unwrap();
    assert_eq!((s.index, s.score), (1, 300), "newer revision");
    let s = select_best(
        &[(Source::Fetched, &a), (Source::Catalog, &b)],
        Identity::Zigbee(&fp),
        &HubFeatures::v1(),
        None,
    )
    .unwrap();
    assert_eq!(s.index, 0, "fetched over bank at equal score");
    a.priority = Some(1);
    let s = select_best(
        &[(Source::Catalog, &a), (Source::Fetched, &b)],
        Identity::Zigbee(&fp),
        &HubFeatures::v1(),
        None,
    )
    .unwrap();
    assert_eq!(s.index, 0, "priority first");
    let trv = def("tuya-ts0601-trv-me167");
    let mut skipped: Vec<Skipped> = Vec::new();
    let hub = HubFeatures::v1().without(zdef::Feature::Tuya.code() as u16);
    assert!(
        select_best(
            &[(Source::Catalog, &trv)],
            Identity::Zigbee(&zb("_TZE200_bvu2wnxz", "TS0601")),
            &hub,
            Some(&mut skipped)
        )
        .is_none()
    );
    assert_eq!(
        skipped,
        vec![Skipped {
            index: 0,
            missing_feature: 2
        }]
    );
    assert!(matches!(
        zdef::load(&golden("tuya-ts0601-trv-me167"), &hub),
        Err(zdef::LoadError::Requires(2))
    ));
}

fn generic_entity(key: &str, ep: u8, cluster: u16, attr: u16) -> Entity {
    Entity {
        key: Some(key.into()),
        platform: Some(Platform::Sensor),
        ep: Some(ep),
        zcl: Some(ZclBinding {
            cluster,
            attr,
            zcl_type: 0x21,
            mfr: None,
            ep: None,
        }),
        ..Default::default()
    }
}

fn layers<'a>(
    generic: &'a [Entity],
    catalog: Option<&'a Definition>,
    sd: Option<&'a Definition>,
    user: Option<&'a Definition>,
) -> Layers<'a> {
    Layers {
        generic,
        generic_consts: &[],
        catalog: catalog.map(|c| (Source::Catalog, c)),
        self_description: sd,
        user,
        overrides: None,
    }
}

#[test]
fn resolution_layers() {
    let innr = def("innr-sp120");
    let fp = Fingerprint {
        endpoints: vec![Endpoint {
            ep: 1,
            profile: 0x0104,
            device_id: 0x0051,
            in_clusters: vec![0x0006, 0x0B04, 0x0702],
            out_clusters: vec![],
        }],
        ..zb("innr", "SP 120")
    };
    let generic = vec![
        generic_entity("switch", 1, 6, 0),
        generic_entity("power_on_behavior", 1, 6, 0x4003),
        generic_entity("current", 1, 0x0B04, 0x0508),
        generic_entity("energy", 1, 0x0702, 0),
    ];
    let r = resolve(
        &layers(&generic, Some(&innr), None, None),
        Target::Zigbee(&fp),
    );
    let keys: Vec<&str> = r.entities.iter().map(|e| e.key()).collect();
    assert_eq!(
        keys,
        ["switch", "current", "energy"],
        "power_on_behavior excluded"
    );
    let cur = r.entities.iter().find(|e| e.key() == "current").unwrap();
    assert!(
        cur.patched && cur.entity.div == Some(Num::Int(1000)) && cur.entity.zcl.is_some(),
        "field-level patch keeps the binding"
    );
    assert_eq!(r.device.power, Some(Power::Mains));
    assert_eq!(r.vendor, Some("Innr"));

    let sd = def("zigbee-rs-selfdesc-thermometer");
    let fp2 = Fingerprint {
        endpoints: vec![Endpoint {
            ep: 1,
            profile: 0x0104,
            device_id: 0x0302,
            in_clusters: vec![0x0402, 0x0405, 0xFC6B, 0xFC6C],
            out_clusters: vec![],
        }],
        ..zb("zigbee-rs", "zrs-thermo-1")
    };
    let mut cat = Definition::new(Kind::Definition);
    cat.id = Some("cat/x".into());
    cat.entities.push(Entity {
        key: Some("led_mode".into()),
        platform: Some(Platform::Switch),
        zcl: Some(ZclBinding {
            cluster: 0xFC6C,
            attr: 3,
            zcl_type: 0x10,
            mfr: None,
            ep: None,
        }),
        ..Default::default()
    });
    let g = vec![generic_entity("temperature", 1, 0x0402, 0)];
    let r = resolve(
        &layers(&g, Some(&cat), Some(&sd), None),
        Target::Zigbee(&fp2),
    );
    let led = r.entities.iter().find(|e| e.key() == "led_mode").unwrap();
    assert_eq!(
        (led.source, led.entity.platform),
        (Source::SelfDescription, Some(Platform::Select)),
        "self-description over catalog (D11)"
    );
    let t = r
        .entities
        .iter()
        .find(|e| e.key() == "temperature")
        .unwrap();
    assert_eq!(
        t.entity.report,
        Some(Report::Config {
            min: 30,
            max: 600,
            change: Some(Num::Int(10))
        }),
        "generic patch from the schema"
    );
    cat.self_description = Some(SelfDescription::Under);
    let r = resolve(
        &layers(&g, Some(&cat), Some(&sd), None),
        Target::Zigbee(&fp2),
    );
    assert_eq!(
        r.entities
            .iter()
            .find(|e| e.key() == "led_mode")
            .unwrap()
            .source,
        Source::Catalog,
        "catalog demotes the schema"
    );
    cat.self_description = Some(SelfDescription::Ignore);
    let r = resolve(
        &layers(&g, Some(&cat), Some(&sd), None),
        Target::Zigbee(&fp2),
    );
    assert!(!r.entities.iter().any(|e| e.key() == "report_interval"));
    let mut user = Definition::new(Kind::Definition);
    user.id = Some("user/x".into());
    let r = resolve(
        &layers(&g, Some(&cat), Some(&sd), Some(&user)),
        Target::Zigbee(&fp2),
    );
    assert_eq!(
        r.entities
            .iter()
            .find(|e| e.key() == "led_mode")
            .unwrap()
            .source,
        Source::SelfDescription,
        "user replaces the catalog (and its demotion)"
    );
    assert!(r.layers.iter().all(|l| l.source != Source::Catalog));
}

#[test]
fn present_if_on_gp_model_bits() {
    let flower = def("gp-diyruz-flowerx");
    for (bits, want) in [(0u16, 5usize), (1, 7), (2, 5), (3, 8)] {
        let g = gp(0x33, Some(0x6D6B), Some(0xF100 | bits), 1);
        let r = resolve(&layers(&[], Some(&flower), None, None), Target::Gp(&g));
        assert_eq!(r.entities.len(), want, "model bits {bits}");
        assert!(r.entities.iter().all(|e| e.ep == 242));
    }
}

#[test]
fn expressions_of_examples_evaluate() {
    let flower = def("gp-diyruz-flowerx");
    let ill = flower.entity("illuminance").unwrap().from.as_ref().unwrap();
    let mut vm = Vm::new();
    assert_eq!(
        vm.eval(
            ill.code(),
            &flower.consts,
            &Inputs::x(Value::Int(30001)),
            &NoHost
        )
        .unwrap(),
        Value::Int(1000)
    );
    let rwl = def("philips-rwl022-dimmer");
    let t = &rwl.entity("action").unwrap().more().triggers[0];
    let p = [2u8, 0, 0, 0, 1, 0, 9, 0];
    assert_eq!(
        vm.eval(t.event.code(), &rwl.consts, &Inputs::payload(&p), &NoHost)
            .unwrap(),
        Value::Str("up_hold")
    );
    assert!(
        rwl.entity("action")
            .unwrap()
            .more()
            .event_types
            .iter()
            .any(|e| e == "up_hold")
    );
    let hold = &t.attrs[0];
    assert_eq!(hold.0, "hold_time");
    assert_eq!(
        vm.eval(hold.1.code(), &rwl.consts, &Inputs::payload(&p), &NoHost)
            .unwrap(),
        Value::Int(9)
    );
    assert!(
        vm.eval(t.event.code(), &rwl.consts, &Inputs::payload(&[2]), &NoHost)
            .is_err(),
        "short payload: error, i.e. null"
    );
    let ptm = def("gp-enocean-ptm216z");
    let w = ptm.entity("rocker_a_top").unwrap().more().triggers[0]
        .when
        .clone()
        .unwrap();
    let i1 = Inputs {
        x: Value::Null,
        payload: Some(&[0x01]),
        last: Some("release"),
    };
    assert_eq!(
        vm.eval(w.code(), &ptm.consts, &i1, &NoHost).unwrap(),
        Value::Bool(true)
    );
    let i2 = Inputs {
        x: Value::Null,
        payload: Some(&[0x01]),
        last: Some("press"),
    };
    assert_eq!(
        vm.eval(w.code(), &ptm.consts, &i2, &NoHost).unwrap(),
        Value::Bool(false)
    );
}

/// xorshift64*, deterministic.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

#[test]
fn fuzz_decoder_mutations() {
    let mut rng = Rng(0x9e3779b97f4a7c15);
    let inputs = all_golden();
    for _ in 0..20000 {
        let (_, base) = &inputs[rng.below(inputs.len())];
        let mut b = base.clone();
        for _ in 0..1 + rng.below(4) {
            match rng.below(4) {
                0 => {
                    let i = rng.below(b.len());
                    b[i] = rng.next() as u8;
                }
                1 => {
                    let i = rng.below(b.len());
                    b[i] ^= 1 << rng.below(8);
                }
                2 => {
                    let n = rng.below(b.len());
                    b.truncate(n);
                }
                _ => {
                    let i = rng.below(b.len() + 1);
                    b.insert(i, rng.next() as u8);
                }
            }
            if b.is_empty() {
                break;
            }
        }
        if let Ok(d) = zdef::decode(&b) {
            let _ = d.validate();
            let _ = d.encode();
        }
        let _ = zdef::load(&b, &HubFeatures::v1());
        let _ = zdef::bank::Bank::parse(&b);
        let _ = zdef::sign::parse_cose_sign1(&b);
    }
    for _ in 0..5000 {
        let n = rng.below(300);
        let b: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        let _ = zdef::decode(&b);
        let _ = zdef::cbor::Reader::validate(&b);
    }
}

#[test]
fn fuzz_verifier_and_vm() {
    let mut rng = Rng(42);
    let consts = vec![
        Const::Str("k".into()),
        Const::Float(0.5),
        Const::Map(vec![(Scalar::Int(1), Scalar::Str("a".into()))]),
        Const::Str("u8 u16le".into()),
    ];
    let mut vm = Vm::new();
    let ops: Vec<u8> = (0u8..=255)
        .filter(|o| zdef::expr::op::name(*o).is_some())
        .collect();
    let mut verified = 0;
    for _ in 0..200000 {
        let n = 1 + rng.below(40);
        let mut code = Vec::with_capacity(n);
        for _ in 0..n {
            if rng.below(3) == 0 {
                code.push(rng.next() as u8);
            } else {
                code.push(ops[rng.below(ops.len())]);
            }
        }
        let ctx = zdef::expr::Context::new(zdef::expr::Site::Event).with_x(zdef::expr::Ty::ANY);
        let ok = zdef::expr::verify(&code, &consts, &ctx).is_ok();
        verified += ok as usize;
        let r = vm.eval(
            &code,
            &consts,
            &Inputs {
                x: Value::Int(3),
                payload: Some(&[1, 2, 3, 4]),
                last: Some("p"),
            },
            &NoHost,
        );
        if ok {
            assert!(
                !matches!(
                    r,
                    Err(zdef::expr::EvalError::Malformed
                        | zdef::expr::EvalError::StackOverflow
                        | zdef::expr::EvalError::StackUnderflow
                        | zdef::expr::EvalError::BadJump)
                ),
                "verified program failed structurally: {code:02x?} {r:?}"
            );
        }
    }
    assert!(
        verified > 100,
        "the generator produces valid programs ({verified})"
    );
}

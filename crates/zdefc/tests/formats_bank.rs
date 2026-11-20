//! Source formats, schema, diagnostics and bank tests.

use std::fs;
use std::path::{Path, PathBuf};
use zdefc::tree::{Node, Value};
use zdefc::{Format, Options, compile_str};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn example(name: &str) -> String {
    fs::read_to_string(
        root()
            .join("../../definitions/examples")
            .join(format!("{name}.yaml")),
    )
    .unwrap()
}

fn compile(src: &str, f: Format) -> Vec<u8> {
    compile_str(src, f, &Options::default())
        .unwrap_or_else(|e| panic!("{}", zdefc::format_diags("src", &e)))
        .cbor
}

fn errors(src: &str, f: Format) -> String {
    match compile_str(src, f, &Options::default()) {
        Ok(_) => panic!("expected an error for:\n{src}"),
        Err(e) => zdefc::format_diags("t", &e),
    }
}

#[test]
fn json5_and_jsonc_are_equivalent_to_yaml() {
    let j5 = fs::read_to_string(root().join("tests/fixtures/gp-zigbee-rs-switch.json5")).unwrap();
    assert_eq!(
        compile(&j5, Format::Json5),
        compile(&example("gp-zigbee-rs-switch"), Format::Yaml)
    );
    let jc = fs::read_to_string(root().join("tests/fixtures/philips-sml001-motion.jsonc")).unwrap();
    assert_eq!(
        compile(&jc, Format::Jsonc),
        compile(&example("philips-sml001-motion"), Format::Yaml)
    );
    // strict JSON rejects the JSONC comments and trailing commas
    assert!(compile_str(&jc, Format::Json, &Options::default()).is_err());
}

#[test]
fn key_order_in_source_does_not_change_the_output() {
    let a = "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\nentities:\n  - key: action\n    platform: event\n    triggers:\n      - {cmd: {cluster: 6, id: 1}, attrs: {b: '\"q\"', a: '\"p\"'}, event: '\"on\"'}\n    event_types: ['on']\n";
    let b = "match: [{model: M}]\nmodel: M\nentities:\n  - event_types: ['on']\n    triggers:\n      - {event: '\"on\"', attrs: {a: '\"p\"', b: '\"q\"'}, cmd: {id: 1, cluster: 6}}\n    platform: event\n    key: action\nvendor: V\nrevision: 1\nid: t/x\nzdef: 1\n";
    assert_eq!(compile(a, Format::Yaml), compile(b, Format::Yaml));
}

#[test]
fn strict_yaml_and_schema_diagnostics() {
    let base = "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\n";
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: s, platform: switch, zcl: {{cluster: 6, attr: 0, type: bool}}, value_on: on}}\n"
        ),
        Format::Yaml,
    );
    assert!(e.contains("t:8:82:") && e.contains("ambiguous"), "{e}");
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: t, platform: sensr, zcl: {{cluster: 6, attr: 0, type: bool}}}}\n"
        ),
        Format::Yaml,
    );
    assert!(
        e.contains("/entities/0/platform") && e.contains("not one of"),
        "{e}"
    );
    let e = errors(&format!("{base}colour: red\n"), Format::Yaml);
    assert!(e.contains("unknown field \"colour\""), "{e}");
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: t, platform: sensor, unit: degC, zcl: {{cluster: 6, attr: 0, type: u8}}}}\n"
        ),
        Format::Yaml,
    );
    assert!(e.contains("/entities/0/unit"), "{e}");
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: t, platform: sensor, zcl: {{cluster: 6, attr: 0, type: u8}}, from: 'x +'}}\n"
        ),
        Format::Yaml,
    );
    assert!(
        e.contains("/entities/0/from") && e.contains("expression"),
        "{e}"
    );
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: t, platform: sensor, zcl: {{cluster: 6, attr: 0, type: u8}}, from: 'last'}}\n"
        ),
        Format::Yaml,
    );
    assert!(e.contains("not available in `from`"), "{e}");
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: t, platform: sensor, zcl: {{cluster: 6, attr: 0, type: str}}, from: 'x * 2'}}\n"
        ),
        Format::Yaml,
    );
    assert!(e.contains("type error"), "{e}");
    let e = errors(
        &format!("{base}white_label: [{{vendor: W, manufacturer: [nope]}}]\n"),
        Format::Yaml,
    );
    assert!(e.contains("not a manufacturer name"), "{e}");
    let e = errors(
        "zdef_fragment: 1\nid: x/y\nentities: [{key: a, platform: sensor, tuya: [1, value]}]\n",
        Format::Yaml,
    );
    assert!(e.contains("not allowed"), "{e}");
    let e = errors(
        "zdef_fragment: 1\nentities: [{key: a, platform: sensor, tuya: [1, value]}]\n",
        Format::Yaml,
    );
    assert!(e.contains("only zcl bindings"), "{e}");
    let e = errors(
        &format!(
            "{base}entities:\n  - {{key: a, platform: sensor, zcl: {{cluster: 6, attr: 0, type: u8}}}}\n  - {{key: a, platform: sensor, zcl: {{cluster: 6, attr: 1, type: u8}}}}\n"
        ),
        Format::Yaml,
    );
    assert!(e.contains("duplicate entity key"), "{e}");
}

#[test]
fn requires_is_computed_and_merged() {
    let c = compile_str(
        &example("tuya-ts0601-trv-me167"),
        Format::Yaml,
        &Options::default(),
    )
    .unwrap();
    assert_eq!(c.def.requires, vec![1, 2, 5]);
    let c = compile_str(
        &example("aqara-rtcgq14lm-motion-p1"),
        Format::Yaml,
        &Options::default(),
    )
    .unwrap();
    assert_eq!(c.def.requires, vec![1, 4]);
    let src = "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\nrequires: [wasm]\n";
    assert_eq!(
        compile_str(src, Format::Yaml, &Options::default())
            .unwrap()
            .def
            .requires,
        vec![60]
    );
}

fn to_json(n: &Node) -> serde_json::Value {
    match &n.v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => (*b).into(),
        Value::Int(i) => (*i).into(),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap(),
        Value::Str(s) => s.clone().into(),
        Value::Seq(v) => v.iter().map(to_json).collect(),
        Value::Map(m) => m
            .iter()
            .map(|(k, _, v)| (k.as_json(), to_json(v)))
            .collect::<serde_json::Map<_, _>>()
            .into(),
    }
}

#[test]
fn published_schema_is_current_and_standard() {
    let published = fs::read_to_string(root().join("../../schema/zdef-v1.schema.json")).unwrap();
    assert_eq!(
        published,
        zdefc::schema::schema_json(),
        "regenerate with `zdefc schema > schema/zdef-v1.schema.json`"
    );
    let schema: serde_json::Value = serde_json::from_str(&published).unwrap();
    let v = jsonschema::draft202012::new(&schema).expect("valid JSON Schema 2020-12");
    let dir = root().join("../../definitions/examples");
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        let tree = zdefc::parse_source(&fs::read_to_string(&p).unwrap(), Format::Yaml).unwrap();
        let errs: Vec<String> = v
            .iter_errors(&to_json(&tree))
            .map(|e| e.to_string())
            .collect();
        assert!(errs.is_empty(), "{}: {errs:?}", p.display());
        assert!(zdefc::schema::validator().validate(&tree).is_empty());
    }
    // both validators reject the same broken documents
    for bad in [
        "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\n",
        "zdef: 1\nzdef_fragment: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\n",
        "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\nentities: [{key: a, platform: sensor, tuya: [1, nope]}]\n",
        "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\nentities: [{key: a, platform: event, triggers: [{event: x}]}]\n",
        "zdef: 1\nid: T/X\nrevision: 1\nvendor: V\nmodel: M\nmatch: [{model: M}]\n",
    ] {
        let tree = zdefc::parse_source(bad, Format::Yaml).unwrap();
        assert!(!v.is_valid(&to_json(&tree)), "jsonschema accepts {bad}");
        assert!(
            !zdefc::schema::validator().validate(&tree).is_empty(),
            "zdefc accepts {bad}"
        );
    }
}

#[test]
fn bank_build_lookup_tamper() {
    use zdef::bank::{BLOCK_MAX, Bank, MinizInflate};
    use zdef::matching::{Identity, lookup_hashes};
    let mut defs = Vec::new();
    for e in fs::read_dir(root().join("../../definitions/examples")).unwrap() {
        let p = e.unwrap().path();
        let c = compile_str(
            &fs::read_to_string(&p).unwrap(),
            Format::Yaml,
            &Options::default(),
        )
        .unwrap();
        if !c.def.is_fragment() {
            defs.push((p.display().to_string(), c.cbor));
        }
    }
    let key = zdefc::bank::SigningKey::from_seed([7; 32]);
    let params = zdefc::bank::BankParams {
        generation: 3,
        catalog_date: 20261120,
        catalog_serial: 1,
        max_size: 256 * 1024,
    };
    let built = zdefc::bank::build_bank(&defs, &params, &key).unwrap();
    let keys = [zdef::sign::TrustedKey::new(key.public())];
    let bank = Bank::parse(&built.bytes).unwrap();
    bank.verify(&keys).unwrap();
    assert_eq!(bank.header.definitions as usize, defs.len());
    let mut inf = MinizInflate::new();
    let strings = bank.load_strings(&mut inf).unwrap();
    let mut buf = [0u8; BLOCK_MAX];
    // Zigbee lookup by names
    let fp = zdef::Fingerprint {
        manufacturer_name: Some("_TZE284_o3x45p96".into()),
        model_id: Some("TS0601".into()),
        ..Default::default()
    };
    let found: Vec<String> = lookup_hashes(Identity::Zigbee(&fp))
        .into_iter()
        .flat_map(|h| bank.lookup(h).collect::<Vec<_>>())
        .map(|e| bank.load(&mut inf, &strings, &e, &mut buf).unwrap())
        .filter(|d| d.match_score(Identity::Zigbee(&fp)).is_some())
        .map(|d| d.id.unwrap())
        .collect();
    assert_eq!(found, ["tuya/ts0601-thermostat-3"]);
    // GP lookup through a masked model ID
    let g = zdef::GpFingerprint {
        gpd_id: zdef::matching::GpdId::SrcId(9),
        device_id: 0x30,
        manufacturer_id: Some(0x6D6B),
        model_id: Some(0xF202),
        commands: vec![],
        server_clusters: vec![],
        client_clusters: vec![],
    };
    let found: Vec<String> = lookup_hashes(Identity::Gp(&g))
        .into_iter()
        .flat_map(|h| bank.lookup(h).collect::<Vec<_>>())
        .map(|e| bank.load(&mut inf, &strings, &e, &mut buf).unwrap())
        .filter(|d| d.match_score(Identity::Gp(&g)).is_some())
        .map(|d| d.id.unwrap())
        .collect();
    assert_eq!(found, ["efektalab/bgt1-gp"]);
    // tampering: body byte, header byte, wrong key
    let mut t = built.bytes.clone();
    let n = t.len();
    t[n - 10] ^= 1;
    assert_eq!(
        Bank::parse(&t).unwrap().verify(&keys),
        Err(zdef::bank::BankError::BodyHash)
    );
    let mut t = built.bytes.clone();
    t[8] ^= 1; // generation
    assert!(matches!(
        Bank::parse(&t).unwrap().verify(&keys),
        Err(zdef::bank::BankError::Signature(
            zdef::sign::SignError::BadSignature
        ))
    ));
    let other = [zdef::sign::TrustedKey::new(
        zdefc::bank::SigningKey::from_seed([8; 32]).public(),
    )];
    assert!(matches!(
        bank.verify(&other),
        Err(zdef::bank::BankError::Signature(
            zdef::sign::SignError::UnknownKey(_)
        ))
    ));
    // manifest and shards
    let m = zdefc::bank::build_manifest(
        &params,
        &[("bank-4m.zdb".into(), &built.bytes)],
        &built.shards,
        vec![1],
        &key,
    );
    let man = zdef::sign::Manifest::verify(&m, &keys).unwrap();
    assert_eq!(man.banks[0].sha256, zdef::hash::sha256(&built.bytes));
    assert_eq!(man.shards.len(), 256);
    for (h, s) in &built.shards {
        assert_eq!(man.shards[*h as usize], Some(zdef::hash::sha256(s)));
        for rec in zdef::bank::decode_shard(s).unwrap() {
            assert_eq!(zdef::bank::shard_of(rec.key_hash), *h);
            zdef::load_canonical(&rec.definition, &zdef::HubFeatures::v1()).unwrap();
        }
    }
    let mut bad = m.clone();
    let k = bad.len() - 70;
    bad[k] ^= 1;
    assert!(zdef::sign::Manifest::verify(&bad, &keys).is_err());
    // size limit
    let small = zdefc::bank::BankParams {
        max_size: 1000,
        ..params
    };
    assert!(
        zdefc::bank::build_bank(&defs, &small, &key)
            .err()
            .unwrap()
            .contains("above")
    );
}

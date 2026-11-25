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

#[test]
fn white_labels_by_fingerprint() {
    let shown = |src: &str, fp: &zdef::Fingerprint| {
        let c = compile_str(src, Format::Yaml, &Options::default()).unwrap();
        let again = zdefc::decompile(&c.cbor, Format::Yaml).unwrap();
        assert_eq!(compile(&again, Format::Yaml), c.cbor, "{again}");
        let d = zdef::load_canonical(&c.cbor, &zdef::HubFeatures::v1()).unwrap();
        d.display_identity(fp).1.unwrap().to_string()
    };
    let paulmann = "zdef: 1\nid: paulmann/291.46\nrevision: 1\nvendor: Paulmann\nmodel: '291.46'\nmatch:\n  - {model: RGBWW, manufacturer: Paulmann Licht GmbH, sw_build: PIIC5800}\n  - {model: RGBWW, manufacturer: Paulmann Licht GmbH, sw_build: PIIC51104}\nwhite_label:\n  - {vendor: Paulmann, model: '291.52', manufacturer: [Paulmann Licht GmbH], model_id: [RGBWW], sw_build: [PIIC5800]}\n  - {vendor: Paulmann, model: '291.50', model_id: [RGBWW], sw_build: [PIIC51104]}\n";
    let sw = |v: Option<&str>| zdef::Fingerprint {
        manufacturer_name: Some("Paulmann Licht GmbH".into()),
        model_id: Some("RGBWW".into()),
        sw_build_id: v.map(Into::into),
        ..Default::default()
    };
    assert_eq!(shown(paulmann, &sw(Some("PIIC5800"))), "291.52");
    assert_eq!(shown(paulmann, &sw(Some("PIIC51104\0"))), "291.50");
    assert_eq!(shown(paulmann, &sw(Some("PIIC5809"))), "291.46");
    assert_eq!(shown(paulmann, &sw(None)), "291.46");

    let other = "zdef: 1\nid: t/x\nrevision: 1\nvendor: V\nmodel: X\nmatch: [{model: M}]\nwhite_label:\n  - {vendor: V, model: HW, hw_version: [9, 13]}\n  - vendor: V\n    model: EP\n    logical_type: end_device\n    endpoints: {1: {profile: 0x0104, device_id: 0x0402, in: [0, 3, 0x0500, 1], out: [3]}}\n";
    let ep = |ep: u8, in_clusters: &[u16]| zdef::matching::Endpoint {
        ep,
        profile: 0x0104,
        device_id: 0x0402,
        in_clusters: in_clusters.to_vec(),
        out_clusters: vec![3],
    };
    let mut fp = zdef::Fingerprint {
        model_id: Some("M".into()),
        hw_version: Some(13),
        ..Default::default()
    };
    assert_eq!(shown(other, &fp), "HW");
    fp.hw_version = Some(10);
    assert_eq!(shown(other, &fp), "X");
    fp.logical_type = Some(2);
    fp.endpoints = vec![ep(1, &[0, 1, 3, 0x0500])];
    assert_eq!(shown(other, &fp), "EP", "cluster order does not count");
    fp.logical_type = Some(1);
    assert_eq!(shown(other, &fp), "X", "router");
    fp.logical_type = Some(2);
    fp.endpoints = vec![ep(1, &[0, 1, 3, 0x0500, 0x0020])];
    assert_eq!(shown(other, &fp), "X", "another cluster");
    fp.endpoints = vec![ep(1, &[0, 1, 3, 0x0500]), ep(2, &[0])];
    assert_eq!(shown(other, &fp), "X", "another endpoint");

    // Leading spaces count in white labels (zhc compares them exactly).
    let legrand = "zdef: 1\nid: legrand/412015\nrevision: 1\nvendor: Legrand\nmodel: '412015'\nmatch: [{model: Smart shedder module}]\nwhite_label: [{vendor: Legrand, model: '412172', model_id: [' Smart shedder module']}]\n";
    let model = |m: &str| zdef::Fingerprint {
        model_id: Some(m.into()),
        ..Default::default()
    };
    assert_eq!(shown(legrand, &model(" Smart shedder module\0")), "412172");
    assert_eq!(shown(legrand, &model("Smart shedder module")), "412015");
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
            defs.push(zdefc::bank::Item::new(&p.display().to_string(), c.cbor).unwrap());
        }
    }
    let defs: Vec<&zdefc::bank::Item> = defs.iter().collect();
    let key = zdefc::bank::SigningKey::from_seed([7; 32]);
    let params = zdefc::bank::BankParams {
        generation: 3,
        catalog_date: 20261120,
        catalog_serial: 1,
        max_size: 256 * 1024,
        ..Default::default()
    };
    let unsigned = zdefc::bank::build_bank(&defs, &params).unwrap();
    assert_eq!(Bank::parse(&unsigned.bytes).unwrap().header.sig_len, 0);
    let signed = zdefc::bank::sign_bank(&unsigned.bytes, &key).unwrap();
    assert_eq!(signed.len(), unsigned.report.total);
    // re-signing replaces the signature
    assert_eq!(zdefc::bank::sign_bank(&signed, &key).unwrap(), signed);
    let keys = [zdef::sign::TrustedKey::new(key.public())];
    let bank = Bank::parse(&signed).unwrap();
    bank.verify(&keys).unwrap();
    assert_eq!(bank.header.definitions as usize, defs.len());
    let mut inf = MinizInflate::new();
    let strings = bank.strings(&mut inf).unwrap();
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
        switch_info: None,
        security_level: None,
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
    let mut t = signed.clone();
    let n = t.len();
    t[n - 10] ^= 1;
    assert_eq!(
        Bank::parse(&t).unwrap().verify(&keys),
        Err(zdef::bank::BankError::BodyHash)
    );
    let mut t = signed.clone();
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
    let (shards, content) = zdefc::bank::build_shards(&defs).unwrap();
    let man0 = zdefc::bank::manifest(
        &params,
        &[zdefc::bank::BankFile {
            name: "bank-4m.zdb".into(),
            bytes: &signed,
            definitions: defs.len() as u32,
            generation: 3,
        }],
        &shards,
        vec![1],
        Some(content),
        vec![
            ("commit".into(), "abc".into()),
            ("zhc".into(), "25.1.0".into()),
        ],
    );
    let m = key.cose_sign1(&man0.encode(), false);
    let man = zdef::sign::Manifest::verify(&m, &keys).unwrap();
    assert_eq!(man, man0);
    assert_eq!(man.banks[0].sha256, zdef::hash::sha256(&signed));
    assert_eq!(man.shards.len(), 256);
    // The hub's summary: no shard hashes in RAM, each read in place.
    let (sum, payload) = zdef::sign::Manifest::verify_summary(&m, &keys).unwrap();
    assert!(sum.shards.is_empty());
    assert_eq!(
        zdef::sign::Manifest {
            shards: man.shards.clone(),
            ..sum
        },
        man
    );
    for (i, h) in man.shards.iter().enumerate() {
        assert_eq!(zdef::sign::manifest_shard(payload, i).unwrap(), *h);
    }
    assert_eq!(zdef::sign::manifest_shard(payload, 256).unwrap(), None);
    let mut recs = 0;
    for (h, s) in &shards {
        assert_eq!(man.shards[*h as usize], Some(zdef::hash::sha256(s)));
        for rec in zdef::bank::decode_shard(s).unwrap() {
            assert!(
                rec.key_hashes
                    .iter()
                    .all(|k| zdef::bank::shard_of(*k) == *h)
            );
            assert!(rec.key_hashes.windows(2).all(|w| w[0] < w[1]));
            zdef::load_canonical(&rec.definition, &zdef::HubFeatures::v1()).unwrap();
            recs += 1;
        }
        assert_eq!(
            zdef::bank::encode_shard(&zdef::bank::decode_shard(s).unwrap()),
            *s
        );
    }
    assert!(recs >= defs.len());
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
        zdefc::bank::build_bank(&defs, &small)
            .err()
            .unwrap()
            .contains("above")
    );
}

/// The committed catalog public key (keys/zdef-catalog-ed25519.pub.pem) is
/// the one the user confirmed; its key ID is what manifests carry.
#[test]
fn committed_catalog_public_key() {
    let pem = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../keys/zdef-catalog-ed25519.pub.pem"),
    )
    .unwrap();
    let pk = zdefc::keys::parse_public(&pem).unwrap();
    assert_eq!(
        zdefc::hex(&pk),
        "ff931767df347a7bccd14b69602d4eb7841ef8503f771248b717f4554f63c6a2"
    );
    assert_eq!(zdefc::hex(&zdef::sign::key_id(&pk)), "66f64ac1cbfc9e27");
    assert_eq!(zdefc::keys::public_pem(&pk), pem);
}

/// `catalog build` from a plan, `sign`, `verify`: banks hold the largest
/// priority prefix that fits, shards hold everything.
#[test]
fn catalog_plan_sign_verify() {
    let dir = std::env::temp_dir().join(format!("zdefc-catalog-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let ex = root().join("../../definitions/examples");
    let mut files: Vec<String> = fs::read_dir(&ex)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| !p.to_string_lossy().contains("selfdesc"))
        .map(|p| p.display().to_string())
        .collect();
    files.sort();
    let q = |v: &[String]| {
        v.iter()
            .map(|f| format!("{f:?}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let plan = format!(
        r#"{{"date": 20261121, "serial": 2, "requires": ["expr-v1"], "source": {{"commit": "abc"}},
            "definitions": [{}],
            "banks": [{{"name": "bank-small.zdb", "max_size": 2500, "candidates": [{}]}},
                      {{"name": "bank-big.zdb", "max_size": 262144, "candidates": [{}]}}]}}"#,
        q(&files),
        q(&files),
        q(&files)
    );
    let plan = zdefc::catalog::Plan::parse(&plan, std::path::Path::new("/")).unwrap();
    assert_eq!(plan.params.generation, 2026112102);
    let o = zdefc::catalog::build(&plan, &dir, None).unwrap();
    assert!(!o.signed && !dir.join("manifest.cose").exists());
    let small = &o.banks[0];
    assert!(small.size <= 2500 && !small.included.is_empty() && !small.no_room.is_empty());
    assert_eq!(o.banks[1].included.len(), files.len());
    assert!(
        fs::read_to_string(dir.join("catalog.json"))
            .unwrap()
            .contains("\"no_room\"")
    );
    let key = zdefc::bank::SigningKey::from_seed([9; 32]);
    zdefc::catalog::sign(&dir, &key).unwrap();
    let m = zdefc::catalog::verify(&dir, key.public()).unwrap();
    assert_eq!((m.catalog_date, m.catalog_serial), (20261121, 2));
    assert_eq!(m.banks[1].definitions as usize, files.len());
    assert!(
        zdefc::catalog::verify(&dir, zdefc::bank::SigningKey::from_seed([1; 32]).public()).is_err()
    );
    // a changed shard is caught
    let shard = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().contains("shard-"))
        .unwrap();
    let mut b = fs::read(&shard).unwrap();
    let n = b.len();
    b[n - 1] ^= 1;
    fs::write(&shard, b).unwrap();
    assert!(zdefc::catalog::verify(&dir, key.public()).is_err());
    let _ = fs::remove_dir_all(&dir);
}

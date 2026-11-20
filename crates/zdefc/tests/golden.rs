//! Golden tests over definitions/examples.

use std::fs;
use std::path::{Path, PathBuf};
use zdefc::{Format, Options, compile_str};

fn examples() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../definitions/examples");
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "yaml"))
        .collect();
    v.sort();
    v
}

fn stem(p: &Path) -> String {
    p.file_stem().unwrap().to_string_lossy().into_owned()
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn compile(p: &Path, opts: &Options) -> zdefc::Compiled {
    let src = fs::read_to_string(p).unwrap();
    compile_str(&src, Format::Yaml, opts).unwrap_or_else(|e| {
        panic!(
            "{}:\n{}",
            p.display(),
            zdefc::format_diags(&p.display().to_string(), &e)
        )
    })
}

/// Files where the prototype output is known to differ (see docs/zdef.md, "Prototype differences").
const PROTO_DIVERGENT: &[&str] = &["philips-rwl022-dimmer"];

#[test]
fn byte_exact_with_prototype() {
    for p in examples() {
        let name = stem(&p);
        let ours = compile(
            &p,
            &Options {
                emit_requires: false,
                strip_description: false,
            },
        );
        let proto = fs::read(golden_dir().join("proto").join(format!("{name}.cbor"))).unwrap();
        if PROTO_DIVERGENT.contains(&name.as_str()) {
            assert_ne!(
                ours.cbor, proto,
                "{name} now equals the prototype: remove it from PROTO_DIVERGENT"
            );
            // The prototype output still decodes and carries the same model up to the constant order.
            let d = zdef::decode(&proto).unwrap();
            assert_eq!(d.entities.len(), ours.def.entities.len());
        } else {
            assert_eq!(
                zdefc::hex(&ours.cbor),
                zdefc::hex(&proto),
                "{name} differs from the prototype"
            );
        }
    }
}

#[test]
fn golden_cbor() {
    let update = std::env::var("ZDEF_UPDATE_GOLDEN").is_ok();
    for p in examples() {
        let name = stem(&p);
        let c = compile(&p, &Options::default());
        let f = golden_dir().join(format!("{name}.cbor"));
        if update {
            fs::write(&f, &c.cbor).unwrap();
        }
        let want = fs::read(&f)
            .unwrap_or_else(|_| panic!("missing {} (run with ZDEF_UPDATE_GOLDEN=1)", f.display()));
        assert_eq!(zdefc::hex(&c.cbor), zdefc::hex(&want), "{name}");
        zdef::load(&c.cbor, &zdef::HubFeatures::v1()).unwrap();
    }
}

#[test]
fn yaml_cbor_round_trip() {
    for p in examples() {
        for fmt in [Format::Yaml, Format::Json, Format::Json5] {
            let c1 = compile(&p, &Options::default());
            let text = zdefc::decompile(&c1.cbor, fmt)
                .unwrap_or_else(|e| panic!("{}: {e:?}", p.display()));
            let c2 = compile_str(&text, fmt, &Options::default()).unwrap_or_else(|e| {
                panic!(
                    "{} {fmt:?}:\n{}\n{text}",
                    p.display(),
                    zdefc::format_diags("decompiled", &e)
                )
            });
            assert_eq!(
                zdefc::hex(&c1.cbor),
                zdefc::hex(&c2.cbor),
                "{} via {fmt:?}:\n{text}",
                p.display()
            );
            let again = zdefc::decompile(&c2.cbor, fmt).unwrap();
            assert_eq!(text, again, "decompile is stable");
        }
    }
}

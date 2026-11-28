//! Vendor template coverage of a definitions tree (spec 17.4): which of
//! the generated definitions that use the family templates `tuya` and
//! `lumi` this firmware runs, and what the others lack. Needs the
//! definitions repository:
//!
//! ```sh
//! ZDEF_DEFS=../zigbee-rs-definitions/definitions/generated \
//!   cargo test -p hub-devices --test template_coverage -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use hub_devices::model::LayerSource;
use hub_devices::resolve::{HUB_FEATURES, missing_features};

fn yaml_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            yaml_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "yaml") {
            out.push(p);
        }
    }
}

#[test]
#[ignore = "needs ZDEF_DEFS (the definitions repository)"]
fn family_template_coverage() {
    let Ok(dir) = std::env::var("ZDEF_DEFS") else {
        eprintln!("ZDEF_DEFS not set");
        return;
    };
    let mut files = Vec::new();
    yaml_files(std::path::Path::new(&dir), &mut files);
    files.sort();
    // family -> (complete runnable, partial runnable, refused)
    let mut count: BTreeMap<&str, [usize; 3]> = BTreeMap::new();
    let mut lacks: BTreeMap<String, usize> = BTreeMap::new();
    for p in &files {
        let src = std::fs::read_to_string(p).unwrap();
        let Ok(c) = zdefc::compile_str(&src, zdefc::Format::Yaml, &Default::default()) else {
            continue;
        };
        let fams: Vec<&str> = c
            .def
            .templates
            .iter()
            .map(|t| t.name.as_str())
            .filter(|n| matches!(*n, "tuya" | "lumi"))
            .collect();
        if fams.is_empty() {
            continue;
        }
        let zd = zdef::load(&c.cbor, &zdef::HubFeatures(u128::MAX)).unwrap();
        let d = hub_devices::zdef_adapter::adapt(zd, LayerSource::Catalog, None).unwrap();
        let miss = missing_features(&d, HUB_FEATURES);
        for f in fams {
            let n = count.entry(f).or_default();
            n[if !miss.is_empty() {
                2
            } else if d.partial() {
                1
            } else {
                0
            }] += 1;
        }
        if !miss.is_empty() && std::env::var("ZDEF_SHOW").is_ok() {
            println!("{}: {}", d.id, miss.join(", "));
        }
        for m in miss {
            *lacks.entry(m).or_default() += 1;
        }
    }
    for (f, [full, partial, refused]) in &count {
        println!("template.{f}: {full} complete + {partial} partial run, {refused} refused");
    }
    let mut top: Vec<_> = lacks.into_iter().collect();
    top.sort_by_key(|b| std::cmp::Reverse(b.1));
    for (m, n) in top.iter().take(40) {
        println!("{n:5}  {m}");
    }
}

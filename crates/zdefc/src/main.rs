//! `zdefc` command line.

#![allow(clippy::collapsible_if)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use zdefc::bank::{self, BankParams, SigningKey};
use zdefc::{Format, Options};

const USAGE: &str = "zdefc - zdef device definition compiler

usage:
  zdefc compile <file> [-o out.cbor] [--format yaml|json|jsonc|json5] [--catalog] [--no-requires]
  zdefc check <file>...                 validate, print size, hash and requires
  zdefc decompile <file.cbor> [--format yaml|json|json5] [-o out]
  zdefc hash <file>                     content hash (source is compiled first)
  zdefc expr '<expression>' [--site from|to|reset_after|present_if|when|event|attrs|payload]
                                        bytecode (hex), constants and disassembly
  zdefc measure <dir>                   size table of every definition in <dir>
  zdefc schema                          print the JSON Schema
  zdefc bank build -o <dir> [--key <file>] [--date YYYYMMDD] [--serial n] [--generation n]
                   [--max-size bytes] [--name bank.zdb] <file|dir>...
  zdefc bank keygen -o <file>           new Ed25519 signing key (keep it out of git)
  zdefc bank pubkey [--key <file>]      public key and key ID
  zdefc bank verify <bank.zdb> --pubkey <hex>
  zdefc bank list <bank.zdb>

The signing key is read from --key or the ZDEF_SIGNING_KEY environment variable
(64 hex digits, the Ed25519 seed).
";

struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
}

const FLAGS: &[&str] = &["--catalog", "--no-requires", "--json"];

fn parse_args(v: &[String]) -> Args {
    let mut a = Args {
        pos: Vec::new(),
        opts: Vec::new(),
    };
    let mut i = 0;
    while i < v.len() {
        let s = &v[i];
        if s.starts_with('-') && s.len() > 1 && !s.starts_with("--=") {
            if FLAGS.contains(&s.as_str()) {
                a.opts.push((s.clone(), None));
            } else {
                a.opts.push((s.clone(), v.get(i + 1).cloned()));
                i += 1;
            }
        } else {
            a.pos.push(s.clone());
        }
        i += 1;
    }
    a
}

impl Args {
    fn get(&self, k: &str) -> Option<&str> {
        self.opts
            .iter()
            .find(|o| o.0 == k)
            .and_then(|o| o.1.as_deref())
    }
    fn has(&self, k: &str) -> bool {
        self.opts.iter().any(|o| o.0 == k)
    }
}

fn die(m: impl std::fmt::Display) -> ExitCode {
    eprintln!("zdefc: {m}");
    ExitCode::from(1)
}

fn read(p: &str) -> Result<Vec<u8>, String> {
    fs::read(p).map_err(|e| format!("{p}: {e}"))
}

fn is_cbor(p: &str) -> bool {
    p.ends_with(".cbor") || p.ends_with(".zdef")
}

fn compile_file(p: &str, fmt: Option<Format>, opts: &Options) -> Result<zdefc::Compiled, String> {
    let b = read(p)?;
    let src = String::from_utf8(b).map_err(|_| format!("{p}: not UTF-8"))?;
    zdefc::compile_str(&src, fmt.unwrap_or(Format::from_path(p)), opts)
        .map_err(|e| zdefc::format_diags(p, &e).trim_end().to_string())
}

fn load_key(a: &Args) -> Result<SigningKey, String> {
    let text = match a.get("--key") {
        Some(f) => fs::read_to_string(f).map_err(|e| format!("{f}: {e}"))?,
        None => std::env::var("ZDEF_SIGNING_KEY")
            .map_err(|_| "no signing key: use --key <file> or ZDEF_SIGNING_KEY")?,
    };
    SigningKey::parse(&text)
}

fn sources(inputs: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for i in inputs {
        let p = Path::new(i);
        if p.is_dir() {
            let mut v: Vec<PathBuf> = fs::read_dir(p)
                .map_err(|e| format!("{i}: {e}"))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().is_some_and(|e| {
                        ["yaml", "yml", "json", "jsonc", "json5", "cbor"]
                            .contains(&e.to_str().unwrap_or(""))
                    })
                })
                .collect();
            v.sort();
            out.extend(v);
        } else {
            out.push(p.to_path_buf());
        }
    }
    Ok(out)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first().cloned() else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let a = parse_args(&argv[1..]);
    let fmt = a
        .get("--format")
        .map(|f| Format::from_name(f).ok_or(f.to_string()));
    let fmt = match fmt.transpose() {
        Ok(f) => f,
        Err(f) => return die(format!("unknown format {f}")),
    };
    let res: Result<(), String> = (|| match cmd.as_str() {
        "compile" => {
            let f = a.pos.first().ok_or("compile: missing file")?;
            let opts = Options {
                emit_requires: !a.has("--no-requires"),
                strip_description: a.has("--catalog"),
            };
            let c = compile_file(f, fmt, &opts)?;
            match a.get("-o") {
                Some(o) => fs::write(o, &c.cbor).map_err(|e| format!("{o}: {e}"))?,
                None => std::io::stdout()
                    .write_all(&c.cbor)
                    .map_err(|e| e.to_string())?,
            }
            eprintln!("{f}: {} B, sha256 {:#}", c.cbor.len(), c.hash);
            Ok(())
        }
        "check" => {
            if a.pos.is_empty() {
                return Err("check: missing file".into());
            }
            let mut bad = 0;
            for f in sources(&a.pos)? {
                let f = f.to_string_lossy().to_string();
                let r = if is_cbor(&f) {
                    read(&f).and_then(|b| {
                        zdef::load(&b, &zdef::HubFeatures::v1())
                            .map(|d| (b, d))
                            .map_err(|e| format!("{f}: {e}"))
                    })
                } else {
                    compile_file(&f, fmt, &Options::default()).map(|c| (c.cbor, c.def))
                };
                match r {
                    Ok((b, d)) => {
                        let req: Vec<&str> = d
                            .requires
                            .iter()
                            .map(|x| {
                                zdef::Feature::from_code(*x as u64)
                                    .map(|f| f.as_str())
                                    .unwrap_or("?")
                            })
                            .collect();
                        println!(
                            "{f}: ok, {} B, sha256 {:#}, requires [{}]",
                            b.len(),
                            zdef::ContentHash::of(&b),
                            req.join(", ")
                        );
                    }
                    Err(e) => {
                        bad += 1;
                        eprintln!("{e}");
                    }
                }
            }
            if bad > 0 {
                Err(format!("{bad} file(s) failed"))
            } else {
                Ok(())
            }
        }
        "decompile" => {
            let f = a.pos.first().ok_or("decompile: missing file")?;
            let text = zdefc::decompile(&read(f)?, fmt.unwrap_or(Format::Yaml))
                .map_err(|e| format!("{f}: {}", e.join("; ")))?;
            match a.get("-o") {
                Some(o) => fs::write(o, text).map_err(|e| format!("{o}: {e}"))?,
                None => print!("{text}"),
            }
            Ok(())
        }
        "hash" => {
            let f = a.pos.first().ok_or("hash: missing file")?;
            let b = if is_cbor(f) {
                read(f)?
            } else {
                compile_file(f, fmt, &Options::default())?.cbor
            };
            println!("{}", zdef::ContentHash::of(&b));
            Ok(())
        }
        "expr" => {
            let src = a.pos.first().ok_or("expr: missing expression")?;
            let site = match a.get("--site").unwrap_or("from") {
                "from" => zdef::expr::Site::From,
                "to" => zdef::expr::Site::To,
                "reset_after" => zdef::expr::Site::ResetAfter,
                "present_if" => zdef::expr::Site::PresentIf,
                "when" => zdef::expr::Site::When,
                "event" => zdef::expr::Site::Event,
                "attrs" => zdef::expr::Site::Attr,
                "payload" => zdef::expr::Site::PressPayload,
                s => return Err(format!("unknown site {s}")),
            };
            let ctx = zdef::expr::Context::new(site)
                .with_payload(true)
                .with_gpd(true);
            let mut pool = Vec::new();
            let code = zdefc::expr::compile(src, &mut pool, &ctx).map_err(|e| e.to_string())?;
            let hex: Vec<String> = code.iter().map(|b| format!("{b:02x}")).collect();
            println!("{} {} consts: {:?}", code.len(), hex.join(" "), pool);
            println!("{}", zdefc::expr::disasm(&code));
            Ok(())
        }
        "measure" => {
            let dir = a.pos.first().ok_or("measure: missing directory")?;
            println!(
                "| File | Entities | CBOR | Catalog form | Deflate | Longest expression | SHA-256 (8 B) |"
            );
            println!("|---|---:|---:|---:|---:|---:|---|");
            let mut rows = Vec::new();
            let mut all = Vec::new();
            for f in sources(std::slice::from_ref(dir))? {
                let f = f.to_string_lossy().to_string();
                if is_cbor(&f) {
                    continue;
                }
                let c = compile_file(&f, fmt, &Options::default())?;
                let cat = compile_file(
                    &f,
                    fmt,
                    &Options {
                        strip_description: true,
                        ..Options::default()
                    },
                )?;
                let ents = c.def.entities.len()
                    + c.def
                        .entities
                        .iter()
                        .map(|e| e.parts.len().saturating_sub(1))
                        .sum::<usize>();
                all.extend_from_slice(&cat.cbor);
                let name = Path::new(&f)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                rows.push((
                    name,
                    ents,
                    c.cbor.len(),
                    cat.cbor.len(),
                    bank::deflate(&cat.cbor).len(),
                    c.longest_expr,
                    format!("{:#}", c.hash),
                ));
            }
            rows.sort_by_key(|r| r.2);
            for r in &rows {
                println!(
                    "| `{}` | {} | {} | {} | {} | {} | `{}` |",
                    r.0,
                    r.1,
                    r.2,
                    r.3,
                    r.4,
                    if r.5 == 0 {
                        "—".to_string()
                    } else {
                        r.5.to_string()
                    },
                    r.6
                );
            }
            let d = bank::deflate(&all).len();
            println!(
                "\nall catalog forms in one block: {} B raw, {} B deflated ({} B per definition)",
                all.len(),
                d,
                d / rows.len().max(1)
            );
            Ok(())
        }
        "schema" => {
            print!("{}", zdefc::schema::schema_json());
            Ok(())
        }
        "bank" => bank_cmd(&a),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        c => Err(format!("unknown command {c}; see zdefc --help")),
    })();
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => die(e),
    }
}

fn bank_cmd(a: &Args) -> Result<(), String> {
    let sub = a
        .pos
        .first()
        .map(|s| s.as_str())
        .ok_or("bank: missing subcommand")?;
    match sub {
        "keygen" => {
            let out = a.get("-o").ok_or("bank keygen: -o <file> required")?;
            if Path::new(out).exists() {
                return Err(format!("{out} exists; not overwriting a key"));
            }
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(|e| format!("random: {e}"))?;
            let k = SigningKey::from_seed(seed);
            let text = format!(
                "# zdef Ed25519 signing key (seed). Secret: never commit.\n# public {} kid {}\n{}\n",
                zdefc::hex(&k.public()),
                zdefc::hex(&k.kid()),
                zdefc::hex(&seed)
            );
            let mut o = fs::OpenOptions::new();
            o.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            o.open(out)
                .and_then(|mut f| f.write_all(text.as_bytes()))
                .map_err(|e| format!("{out}: {e}"))?;
            println!(
                "public key {}\nkid {}",
                zdefc::hex(&k.public()),
                zdefc::hex(&k.kid())
            );
            Ok(())
        }
        "pubkey" => {
            let k = load_key(a)?;
            println!(
                "public key {}\nkid {}",
                zdefc::hex(&k.public()),
                zdefc::hex(&k.kid())
            );
            Ok(())
        }
        "build" => {
            let out = PathBuf::from(a.get("-o").ok_or("bank build: -o <dir> required")?);
            let key = load_key(a)?;
            let num = |k: &str, d: u64| -> Result<u64, String> {
                a.get(k)
                    .map(|v| v.parse::<u64>().map_err(|_| format!("{k}: not a number")))
                    .unwrap_or(Ok(d))
            };
            let params = BankParams {
                generation: num("--generation", 1)? as u32,
                catalog_date: num("--date", 20261120)? as u32,
                catalog_serial: num("--serial", 0)? as u16,
                max_size: num("--max-size", 256 * 1024)? as usize,
            };
            let mut defs = Vec::new();
            for f in sources(&a.pos[1..])? {
                let f = f.to_string_lossy().to_string();
                let cbor = if is_cbor(&f) {
                    read(&f)?
                } else {
                    compile_file(&f, None, &Options::default())?.cbor
                };
                if zdef::decode(&cbor).is_ok_and(|d| d.is_fragment()) {
                    eprintln!("{f}: self-description fragment, not banked");
                    continue;
                }
                defs.push((f, cbor));
            }
            if defs.is_empty() {
                return Err("bank build: no inputs".into());
            }
            let built = bank::build_bank(&defs, &params, &key)?;
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            let name = a.get("--name").unwrap_or("bank.zdb").to_string();
            fs::write(out.join(&name), &built.bytes).map_err(|e| e.to_string())?;
            for (h, s) in &built.shards {
                fs::write(out.join(format!("shard-{h:02x}.cbor")), s).map_err(|e| e.to_string())?;
            }
            let manifest = bank::build_manifest(
                &params,
                &[(name.clone(), &built.bytes)],
                &built.shards,
                Vec::new(),
                &key,
            );
            fs::write(out.join("manifest.cose"), manifest).map_err(|e| e.to_string())?;
            let r = &built.report;
            println!(
                "{name}: {} B ({} definitions, {} index keys, {} blocks, {} shared strings)\n  strings {} B, index {} B, blocks {} B (catalog form {} B raw), shards {}",
                r.total,
                r.definitions,
                r.keys,
                r.blocks,
                r.strings,
                r.strings_bytes,
                r.index_bytes,
                r.blocks_bytes,
                r.catalog_form_bytes,
                built.shards.len()
            );
            Ok(())
        }
        "verify" | "list" => {
            let f = a.pos.get(1).ok_or("bank: missing bank file")?;
            let bytes = read(f)?;
            let bank = zdef::bank::Bank::parse(&bytes).map_err(|e| format!("{f}: {e:?}"))?;
            if sub == "verify" {
                let pk = a
                    .get("--pubkey")
                    .ok_or("bank verify: --pubkey <hex> required")?;
                let pk: [u8; 32] = bank::unhex(pk)
                    .and_then(|b| b.try_into().ok())
                    .ok_or("--pubkey: 64 hex digits")?;
                bank.verify(&[zdef::sign::TrustedKey::new(pk)])
                    .map_err(|e| format!("{f}: {e:?}"))?;
                println!(
                    "{f}: signature and body hash ok; {}",
                    zdef::bank::describe(&bank.header)
                );
                return Ok(());
            }
            println!("{}", zdef::bank::describe(&bank.header));
            let mut inf = zdef::bank::MinizInflate::new();
            let st = bank.load_strings(&mut inf).map_err(|e| format!("{e:?}"))?;
            let mut buf = [0u8; zdef::bank::BLOCK_MAX];
            for b in 0..bank.header.blocks {
                let n = bank
                    .inflate_block(&mut inf, b, &mut buf)
                    .map_err(|e| format!("{e:?}"))?;
                let slots = zdef::bank::block_slots(&buf[..n]).map_err(|e| format!("{e:?}"))?;
                for s in 0..slots {
                    let item =
                        zdef::bank::block_slot(&buf[..n], s as u8).map_err(|e| format!("{e:?}"))?;
                    let d = zdef::codec::decode_with(
                        item,
                        &zdef::codec::DecodeOptions { strings: Some(&st) },
                    )
                    .map_err(|e| format!("{e}"))?;
                    println!(
                        "block {b} slot {s}: {} rev {} ({} B catalog form)",
                        d.id.unwrap_or_default(),
                        d.revision.unwrap_or(0),
                        item.len()
                    );
                }
            }
            Ok(())
        }
        s => Err(format!("unknown bank subcommand {s}")),
    }
}

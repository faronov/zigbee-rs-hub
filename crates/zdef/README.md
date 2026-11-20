# zdef

Device definitions for zigbee-rs-hub: the data model, its canonical CBOR,
the expression VM, matching, layer resolution, catalog banks and
signatures. Specification: [`docs/device-support.md`](../../docs/device-support.md)
(sections 3, 6–9, 12, 13). `no_std` + `alloc`; the `std` feature only adds
`std::error::Error` impls.

The compiler (`crates/zdefc`) builds on this crate; the hub entity engine
consumes it. The API below is the contract between them; changes are made
deliberately and listed in this file's history.

## API overview

### Loading a definition

```rust
use zdef::{load, HubFeatures, Definition};

let def: Definition = zdef::load(&cbor, &HubFeatures::v1())?; // what the hub runs per load
// = codec::decode (strict deterministic CBOR: shortest heads, definite
//   lengths, sorted unique keys, known keys outside 64..127)
//   + validate() (fields, bindings, fragment rules, every expression verified)
//   + requires check against the hub's features
let def = zdef::load_canonical(&cbor, &hub)?;                  // + re-encode equality (uploads, zdefc)
let hash = zdef::ContentHash::of(&cbor);           // SHA-256, spec 8.6; `{:#}` = 8-byte short form
let bytes = def.encode()?;                         // canonical standalone CBOR
```

Lower-level: `codec::decode` (no re-encode check), `codec::decode_with`
(catalog form with a bank string table), `validate::validate`,
`validate::compute_requires`, the `CborDecode` / `CborEncode` traits.

### Model (`zdef::model`)

`Definition { kind: Definition|Fragment, major, id, revision, vendor, model,
description, match_, white_label, requires, device, generic, endpoints,
tuya, configure, entities, gp, ignore, consts, source, mfr, priority,
self_description, templates, x_wasm, ext }`. Closed sets are enums from
`zdef::enums` (`Power`, `Mode`, `SelfDescription`, `TimeSync`, `TuyaSend`,
`LogicalType`, `BatteryCurve`, `CodecName`, `ScheduleVariant`,
`TemplateName`), encoded as integers.

`Entity` keeps the fields most entities use inline (`key`, `platform`,
`device_class`/`unit` as `TableRef` into `enums::DEVICE_CLASSES`/`UNITS`,
`state_class`, `category`, `ep`, `precision`, `access`, `enabled`, the
bindings `zcl`/`tuya`/`gp`, `from`, `mul`, `div`, `offset`, `invalid`, `min`,
`max`, `step`, `report`, `map`) and the rest out of line in
`more: Option<Box<EntityMore>>` (`name`, `struct_`, `to`,
`value_on`/`value_off`, `mode`, `reset_after`, `parts`, `event_types`,
`triggers`, `press`, `codec`, `present_if`, `attrs`, `hub`, `invert`, `ext`;
used by ~7 % of catalog entities). Read with `e.more()` (an empty static
block when absent), write with `e.more_mut()`, drop an empty block with
`e.compact()`.
Strings are resolved: packed Tuya names, white-label indices and the
constant references of `event_types`/`attrs` are plain strings in the model.
`parts` keys are `keys::SUB` part keys (`entity.part("target_temperature")`).

Expressions are `Expr(Vec<u8>)` bytecode whose constants live in
`Definition::consts`; `Entity::for_each_expr` visits them with their
`expr::Site`.

### Expressions (`zdef::expr`)

```rust
let ctx = zdef::validate::context_for(entity, Site::From, is_gp);
let ty = zdef::expr::verify(expr.code(), &def.consts, &ctx)?;   // done by validate()

let mut vm = zdef::expr::Vm::new();                             // 512 B arena; reuse it
let v = vm.eval_or_null(expr.code(), &def.consts, &Inputs::x(Value::Int(raw)), &host);
// or vm.eval(..) -> Result<Value, EvalError>; an Err means state `null` + error counter (spec 7.1)
```

`Host<'a>` supplies `state.<key>`, `opt.<key>`, `gpd.*`, `fp.*`, `now()`,
`tz_offset()` and `battery_percent(mV)`; every method has a `Null`/`None`
default. `Inputs { x, payload, last }`. Values: `Null, Bool, Int(i64),
Float(f64), Str(&str), Bytes(&[u8])`.

Limits: program ≤ 255 B, stack 16, 8 locals, strings ≤ 64 B, forward jumps
only (≤ `len` steps). The VM bounds-checks everything, so unverified bytes
return `EvalError::Malformed` instead of panicking.

### Matching (`zdef::matching`)

```rust
let fp = Fingerprint { manufacturer_name, model_id, mfr_code, endpoints, .. };
let score = def.match_score(Identity::Zigbee(&fp));             // Option<u16>, spec 3.3 table
let best = select_best(&[(Source::Catalog, &a), (Source::Fetched, &b)],
                       Identity::Zigbee(&fp), &HubFeatures::v1(), Some(&mut skipped));
let hashes = lookup_hashes(Identity::Gp(&gpfp));                // bank index lookups
```

Names compare exactly after trimming trailing NULs/spaces; `{prefix}` and
`{any: [...]}` (Tuya `_TZE…` lists) are supported. GP clauses match DeviceID,
Manufacturer ID, Model ID with mask, SrcID range, IEEE prefix, command and
cluster subsets. `Fingerprint::hash16()` is the spec 3.2 fingerprint hash.

### Resolution (`zdef::resolve`)

```rust
let res = resolve(&Layers { generic: &tier_a_entities, generic_consts: &[],
                            catalog: Some((Source::Catalog, &cat)),
                            self_description: Some(&schema), user: None,
                            overrides: Some(&ov) },
                  Target::Zigbee(&fp));
for e in &res.entities { /* e.ep, e.entity, e.source, e.consts, e.patched */ }
```

Precedence: user > self-description > catalog > generic; a user definition
replaces the catalog one; `self_description: under|ignore` demotes or drops
layer 3. Merge by `(endpoint, key)`; `generic.exclude/disable/only_endpoints/
patch`; device fields merge field-wise; `present_if` is evaluated against
the fingerprint or GP identity; default endpoints follow spec 6.3 (GP: 242).

### Banks and signatures (`zdef::bank`, `zdef::sign`)

```rust
let bank = Bank::parse(&bytes)?;
bank.verify(&[TrustedKey::new(PUBLIC_KEY)])?;          // body SHA-256 + COSE_Sign1 Ed25519
let mut inf = MinizInflate::new();                     // feature `miniz`, or a ROM-tinfl `Inflate` impl
// without `soft-sha256`: bank.verify_with::<HwSha>(..), TrustedKey::new_with::<HwSha>(..)
let strings = bank.load_strings(&mut inf)?;
let mut buf = [0u8; BLOCK_MAX];
for h in lookup_hashes(id) {
    for e in bank.lookup(h) { let def = bank.load(&mut inf, &strings, &e, &mut buf)?; /* re-check + score */ }
}
let manifest = Manifest::verify(&manifest_cose, &keys)?;
```

Shard records carry `key_hashes` (one record per definition and shard);
`Manifest` adds `content` (digest of all definition content hashes) and
`source`, `ManifestAsset` adds `definitions` and `generation`; both derive
`Default`.

### Features

`default = ["bank", "sign", "miniz", "soft-sha256"]`. The hub build can use
`default-features = false, features = ["bank", "sign"]` and supply SHA-256
(`hash::Sha256`, e.g. the S3 accelerator or mbedTLS) and inflate
(`bank::Inflate`, e.g. ROM `tinfl`) itself; the `*_with::<H>` functions take
the SHA-256 implementation. `std` adds `std::error::Error` impls.

## Sizes

See [`docs/zdef.md`](../../docs/zdef.md): code size per entry point on
xtensa-esp32s3 (`tools/zdef-size/measure.sh`) and RAM per loaded definition.

## API history

- 2026-11-20: first version (`2344439`).
- 2026-11-20: `load` no longer re-encodes (smaller hub image);
  `load_canonical` added with the old behaviour. Decoder internals only
  otherwise.
- 2026-11-20 (`zdef-size`): v1 freeze. Closed string sets are integer
  enums; `Entity` rare fields moved to `EntityMore` (`e.more()`,
  `e.more_mut()`); `ValidationError { path: ErrPath, message: &'static str,
  verify, feature }` with `Display` (no `format!` in the crate); `Codec`
  schedule parameters (`TuyaScheduleDay { day, transitions, variant }`,
  `TuyaScheduleWeek(ScheduleWeek)`), `StructBinding.codec: CodecName`;
  `TuyaQuery { configure, announce, interval_s }`; `Battery::Curve(BatteryCurve)`;
  `ConfigureOp::Cmd { payload }`; `MatchClause { logical_type, ext }`;
  `Definition.templates`; `GpFingerprint { switch_info, security_level }`
  (`GpFingerprint::new`); GP scores without extras; `IndexKey::Manufacturer`;
  `ShardRecord.key_hashes`; manifest `content`/`source` and bank
  `definitions`/`generation`; features `bank`, `sign`, `miniz`,
  `soft-sha256` with `Sha256`/`Inflate` traits; `num::format_number`
  (`str()` without `core::fmt`); `LoadError::Requires` names the feature.

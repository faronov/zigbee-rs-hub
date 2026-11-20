# zdefc-proto

Python prototype of `zdefc`, the definition compiler (YAML → CBOR) specified
in [`docs/device-support.md`](../../docs/device-support.md). It was used to
check the format against real definitions and to measure encoded sizes.
The production compiler is [`crates/zdefc`](../../crates/zdefc) (Rust, CLI
and WebAssembly), sharing [`crates/zdef`](../../crates/zdef) with the hub;
its golden tests compare against this prototype's output, which is
committed under `crates/zdefc/tests/golden/proto/` (byte-identical for 12 of
the 13 examples; the differences are listed in
[`docs/zdef.md`](../../docs/zdef.md)).

## Use

Python 3 with `PyYAML` and `cbor2` (`pip install pyyaml cbor2`).

```sh
python3 tools/zdefc-proto/zdefc.py measure definitions/examples   # size table (spec appendix 20)
python3 tools/zdefc-proto/zdefc.py compile definitions/examples/innr-sp120.yaml > innr.cbor
python3 tools/zdefc-proto/zdefc.py expr 'x > 130536 ? 0 : x - 65536'
# 18 01 03 d0 f7 0f 34 3d 04 02 00 3c 06 01 03 80 80 08 11 consts: []
```

## What it implements

| Spec | Implemented |
|---|---|
| 7.2 grammar | all operators with Rust precedence, `?:`, `??`, byte indexing, calls, `lookup` with a map literal and default |
| 7.5 bytecode | the opcode table, LEB128 / zig-zag immediates, forward jumps, one constant pool per definition, 255-byte limit |
| 8.1 CBOR | core deterministic encoding (`cbor2` canonical), integer keys, integral numbers as integers, the shorter of IEEE float and tag-4 decimal fraction |
| 8.2 key tables | prototype tables (top level, entity, sub-maps) |
| 8.3 enumerations | platform, device class, unit, state class, category, access, ZCL type, Tuya DP type (subsets of the HA tables) |
| 8.4 strings | packed Tuya manufacturer names, white-label name indices |

## What it leaves out

- `let`, static type checking, and the bytecode verifier (7.5);
- the full HA device-class and unit tables and their pairing rules (4.4);
- `requires` computation (8.5) and schema validation against the CDDL
  (`schema/zdef-v1.cddl` will live in `faronov/zigbee-rs-definitions`);
- catalog form with shared-string references and bank building (8.4, 9.1);
- error messages with YAML line numbers.

Its key tables are not normative: the v1 tables are `crates/zdef/src/keys.rs`.

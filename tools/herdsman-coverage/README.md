# herdsman-coverage

Measures how much of
[zigbee-herdsman-converters](https://github.com/Koenkk/zigbee-herdsman-converters)
(zhc, MIT) the hub's declarative definition format can express, and how large
the translated catalog would be. The method and the results are in
[`docs/device-support.md`](../../docs/device-support.md), section 17; the bank
budget derived from it is section 16.3.

| File | What |
|---|---|
| `measure.cjs` | Loads a zhc build with instrumentation, classifies every definition into tiers A–D, writes `stats.json`, `results.json`, `proxy.json`, prints a Markdown summary |
| `proxy_size.py` | Encodes `proxy.json` like a bank (integer keys, shared string table, 4 KiB deflate blocks) and prints the size per tier |

## Run

Node 22 or newer (zhc 26.x requires `^22.2`), Python 3 with `cbor2`.

```sh
mkdir -p /tmp/zhc && cd /tmp/zhc
npm init -y >/dev/null
npm install zigbee-herdsman-converters@26.130.0      # the version measured in the spec
node <hub>/tools/herdsman-coverage/measure.cjs \
     node_modules/zigbee-herdsman-converters/dist /tmp/zhc/out
pip install cbor2
python3 <hub>/tools/herdsman-coverage/proxy_size.py /tmp/zhc/out
```

A run takes about 10 s. Results for 26.130.0 (2026-11-19):

| Tier | Definitions | Share |
|---|---:|---:|
| A generic declarative | 1944 | 41.7 % |
| B Tuya declarative | 625 | 13.4 % |
| A + B | 2569 | 55.1 % |
| C vendor template | 1219 | 26.1 % |
| D custom JS | 876 | 18.8 % |

## How it classifies

1. Before any device file loads, every exported function of `lib/modernExtend`
   (`m.*`), of the vendor libraries (`tuya.modernExtend.*`, `philips.m.*`,
   `lumi.lumiModernExtend.*`, `ikea.*`, …) and of `tuya.valueConverter` /
   `tuya.valueConverterBasic` is wrapped. The wrapper tags what the call
   returns (the extend's `fromZigbee`, `toZigbee`, `configure`, `onEvent`
   items, or the value converter) with the function name, keeps the call
   arguments for `proxy.json`, and flags the call if an argument is code (a
   function or an inline `{from: fn}` converter).
2. Exported converter objects (`converters/fromZigbee`, `converters/toZigbee`,
   `lib/tuya` `fz`/`tz`, `lib/legacy`, vendor libraries) are indexed by
   identity.
3. Every item of every definition is then one of: generic extend, Tuya
   extend/converter, vendor extend/converter, named standard converter, or
   custom (inline). Inline `configure` functions are replayed against a
   recording mock device (bind, configureReporting, read, write, command;
   `utils.sleep` stubbed, 300 ms limit) and count as declarative when they
   complete.
4. Tier: D if anything is custom, else C if anything is vendor-specific, else
   B if anything is Tuya-specific, else A.

## Limits

- A named standard converter counts as translatable; the hub must implement
  it as generic behaviour. `stats.json` → `abWithTopStd` shows how many A/B
  definitions the most-used 25/50/100/150 cover.
- The mock replay does not notice `configure` code that branches on device
  fields the mock does not set.
- `exposes` given as a function (106 definitions) is reported
  (`dynamicExposes`) but does not change the tier on its own.
- `proxy.json` is the generator *input*, not its output; its encoded size
  approximates the translated definitions (spec 17.3).

The script only reads zhc; nothing from zhc is copied into this repository.

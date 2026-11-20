#!/usr/bin/env python3
"""zdefc-proto: prototype of the zdef definition compiler (YAML -> CBOR).

Implements enough of docs/device-support.md to measure encoded sizes and to
show what the real compiler (`zdefc`, Rust, CLI + WebAssembly) will emit:

- section 7: expression language -> stack bytecode (Rust operator precedence,
  forward-only jumps, one constant pool per definition);
- section 8.1: core deterministic CBOR, integer map keys, integers for
  integral numbers, shortest of IEEE float and tag-4 decimal fraction;
- section 8.3: enumerations (platform, device class, unit, state class,
  category, ZCL type, Tuya DP type);
- section 8.4: packed Tuya manufacturer names, white-label name indices.

Not implemented (the Rust compiler will): `let`, static type checking, the
bytecode verifier, the full HA device-class/unit tables and their pairing
rules, `requires` computation, catalog form with shared-string references,
CDDL schema validation. The key tables below are the prototype's; the
authoritative ones will be `schema/zdef-v1.cddl` in faronov/zigbee-rs-definitions.

usage:
  zdefc.py measure <dir>            size table of every <dir>/*.yaml (Markdown)
  zdefc.py compile <file.yaml>      CBOR to stdout (binary)
  zdefc.py expr '<expression>'      bytecode of one expression (hex)
needs: PyYAML, cbor2
"""
import glob
import hashlib
import json
import re
import sys
import zlib

import cbor2
import yaml

# ---------------------------------------------------------------- enumerations (8.3)
PLATFORMS = ['sensor', 'binary_sensor', 'number', 'select', 'switch', 'button', 'text', 'light', 'cover',
             'climate', 'lock', 'fan', 'event', 'siren', 'valve', 'update']
UNITS = ['°C', '°F', 'K', '%', 'hPa', 'kPa', 'Pa', 'lx', 'ppm', 'ppb', 'µg/m³', 'W', 'kW', 'Wh', 'kWh', 'V', 'mV', 'A',
         'mA', 'Hz', 'VA', 'var', 's', 'min', 'h', 'd', 'ms', 'm', 'cm', 'mm', 'km', 'm³', 'L', 'm³/h', 'L/min', 'dB',
         'dBm', 'mired', '°']
DEVICE_CLASSES = ['temperature', 'humidity', 'pressure', 'illuminance', 'battery', 'voltage', 'current', 'power',
                  'energy', 'power_factor', 'frequency', 'carbon_dioxide', 'carbon_monoxide', 'pm25', 'pm10', 'pm1',
                  'volatile_organic_compounds', 'moisture', 'occupancy', 'motion', 'door', 'window', 'opening',
                  'smoke', 'gas', 'problem', 'tamper', 'vibration', 'enum', 'duration', 'distance', 'signal_strength',
                  'button', 'restart', 'identify', 'update', 'outlet', 'switch', 'connectivity', 'plug', 'running',
                  'safety', 'sound', 'lock', 'cold', 'heat', 'light', 'presence', 'water', 'blind', 'curtain',
                  'shade', 'shutter', 'garage', 'gate', 'awning', 'damper', 'doorbell', 'water_heater',
                  'atmospheric_pressure']
STATE_CLASSES = ['measurement', 'total', 'total_increasing', 'measurement_angle']
CATEGORY = {'config': 1, 'diagnostic': 2}
ZCL_TYPES = {'bool': 0x10, 'map8': 0x18, 'map16': 0x19, 'u8': 0x20, 'u16': 0x21, 'u24': 0x22, 'u32': 0x23,
             'u48': 0x25, 'i8': 0x28, 'i16': 0x29, 'i24': 0x2A, 'i32': 0x2B, 'enum8': 0x30, 'enum16': 0x31,
             'f32': 0x39, 'ostr': 0x41, 'str': 0x42}
TUYA_TYPES = {'raw': 0, 'bool': 1, 'value': 2, 'string': 3, 'enum': 4, 'bitmap': 5}
ACCESS = {'r': 1, 'w': 2, 'p': 4}
# Closed string sets, encoded as their index (8.3).
CLOSED = {
    'power': ['mains', 'battery', 'energy_harvesting'],
    'mode': ['auto', 'box', 'slider', 'text', 'password'],
    'self_description': ['ignore', 'under', 'over'],
    'send': ['dataRequest', 'sendData'],
    'logical_type': ['coordinator', 'router', 'end_device'],
    'battery': ['3V_2100', '3V_1500_2800', 'CR2032'],
}
TIME_SYNC = {'off': 0, 1970: 1, '1970': 1, 2000: 2, '2000': 2}
CODECS = ['zcl_tagged', 'tuya_schedule_day', 'tuya_schedule_week', 'bcd_time', 'hhmm', 'ascii', 'utf16be_hex',
          'bitmap_flags']
SCHEDULE_VARIANTS = ['multi_dp', 'single_dp', 'trv602z', 'trv603wz']
CODEC_PARAMS = {'day': 45, 'transitions': 46, 'min': 60, 'max': 61, 'variant': 150, 'scale': 159}

# ---------------------------------------------------------------- key tables (8.2)
TOP = ['zdef', 'id', 'revision', 'vendor', 'model', 'description', 'match', 'white_label', 'requires', 'device',
       'generic', 'endpoints', 'tuya', 'configure', 'entities', 'gp', 'ignore', 'consts', 'source', 'mfr',
       'zdef_fragment', 'templates', 'priority', 'self_description']
ENT = ['key', 'platform', 'device_class', 'unit', 'state_class', 'category', 'name', 'ep', 'zcl', 'tuya', 'struct',
       'gp', 'from', 'to', 'mul', 'div', 'offset', 'precision', 'invalid', 'map', 'value_on', 'value_off', 'min',
       'max', 'step', 'mode', 'access', 'report', 'enabled', 'reset_after', 'parts', 'event_types', 'triggers',
       'press', 'codec', 'present_if', 'attrs', 'hub']
SUB = ['cluster', 'attr', 'type', 'mfr', 'ep', 'id', 'attrs', 'clusters', 'value', 'codec', 'tag', 'cmd', 'report',
       'gp_cmd', 'when', 'event', 'write', 'any', 'prefix', 'manufacturer', 'model', 'device_id', 'manufacturer_id',
       'model_id', 'src_id', 'mask', 'gp', 'bind', 'read', 'power', 'sleepy', 'battery', 'voltage_to_percent',
       'checkin_s', 'exclude', 'patch', 'disable', 'only_endpoints', 'time_sync', 'magic_packet', 'query',
       'security_min', 'report_interval_s', 'vendor',
       # 44+: codec parameters, then composite part names
       'tuya_schedule_day', 'day', 'transitions',
       'hvac_mode', 'hvac_action', 'target_temperature', 'current_temperature']
TOPK = {k: i for i, k in enumerate(TOP)}
ENTK = {k: i for i, k in enumerate(ENT)}
SUBK = {k: i for i, k in enumerate(SUB)}

# ---------------------------------------------------------------- bytecode (7.5)
OP = dict(X=0x01, I8=0x02, INT=0x03, CONST=0x04, NULL=0x05, TRUE=0x06, FALSE=0x07,
          STATE=0x0A, OPT=0x0B, PAYLOAD=0x0C, LAST=0x0D, GPD=0x0E,
          ADD=0x10, SUB=0x11, MUL=0x12, DIV=0x13, IDIV=0x14, MOD=0x15, NEG=0x16,
          AND=0x20, OR=0x21, XOR=0x22, BNOT=0x23, SHL=0x24, SHR=0x25,
          EQ=0x30, NE=0x31, LT=0x32, LE=0x33, GT=0x34, GE=0x35, LNOT=0x36, LAND=0x37, LOR=0x38,
          JMP=0x3C, JZ=0x3D, JNN=0x3E, POP=0x3F, INDEX=0x40, LOOKUP=0x48, CALL=0x70)
FUNCS = ['bit', 'bits', 'sext', 'bcd', 'len', 'u8', 'i8', 'u16le', 'u16be', 'i16le', 'i16be', 'u24le', 'u24be',
         'i24le', 'i24be', 'u32le', 'u32be', 'i32le', 'i32be', 'f32le', 'f32be', 'ascii', 'hex', 'min', 'max',
         'clamp', 'abs', 'round', 'floor', 'ceil', 'int', 'float', 'pow10', 'log10', 'str', 'isnull', 'pack', 'now',
         'tz_offset', 'battery_percent']
GPD_FIELDS = ['device_id', 'manufacturer_id', 'model_id', 'src_id']
# Binary operators by precedence, lowest first (Rust order: comparisons below bit operators).
BIN = [('??',), ('||',), ('&&',), ('==', '!=', '<', '<=', '>', '>='), ('|',), ('^',), ('&',), ('<<', '>>'),
       ('+', '-'), ('*', '/', '//', '%')]
BINOP = {'+': 'ADD', '-': 'SUB', '*': 'MUL', '/': 'DIV', '//': 'IDIV', '%': 'MOD', '&': 'AND', '|': 'OR',
         '^': 'XOR', '<<': 'SHL', '>>': 'SHR', '==': 'EQ', '!=': 'NE', '<': 'LT', '<=': 'LE', '>': 'GT', '>=': 'GE',
         '&&': 'LAND', '||': 'LOR'}
TOKEN = re.compile(r'\s*(?:(0x[0-9a-fA-F]+|\d+\.\d*|\d+)|("(?:[^"\\]|\\.)*"|\'[^\']*\')'
                   r'|([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)?)'
                   r'|(\?\?|==|!=|<=|>=|&&|\|\||<<|>>|//|[-+*/%&|^~!<>?:()\[\],{}]))')
MAX_PROGRAM = 255


def tokenize(src):
    out, pos, src = [], 0, src.strip()
    while pos < len(src):
        m = TOKEN.match(src, pos)
        if not m or m.end() == pos:
            raise SyntaxError(f'bad token at {src[pos:]!r}')
        pos = m.end()
        num, string, ident, op = m.groups()
        if num is not None:
            out.append(('num', float(num) if '.' in num else int(num, 0)))
        elif string is not None:
            out.append(('str', string[1:-1]))
        elif ident is not None:
            out.append(('id', ident))
        else:
            out.append(('op', op))
    out.append(('eof', None))
    return out


def leb128(n):
    out = bytearray()
    while True:
        b, n = n & 0x7F, n >> 7
        out.append(b | 0x80 if n else b)
        if not n:
            return bytes(out)


class ExprCompiler:
    """Recursive-descent compiler; one instance per definition (shared constant pool)."""

    def __init__(self):
        self.consts = []
        self.longest = 0

    def const(self, value):
        key = json.dumps(value, sort_keys=True, default=str)
        for i, c in enumerate(self.consts):
            if json.dumps(c, sort_keys=True, default=str) == key:
                return i
        self.consts.append(value)
        return len(self.consts) - 1

    def compile(self, src):
        self.toks, self.i, self.code = tokenize(str(src)), 0, bytearray()
        self.expr()
        if self.toks[self.i][0] != 'eof':
            raise SyntaxError(f'trailing {self.toks[self.i]!r} in {src!r}')
        if len(self.code) > MAX_PROGRAM:
            raise ValueError(f'expression longer than {MAX_PROGRAM} bytes: {src!r}')
        self.longest = max(self.longest, len(self.code))
        return bytes(self.code)

    # -- token helpers
    def peek(self):
        return self.toks[self.i]

    def take(self, value=None):
        tok = self.toks[self.i]
        if value is not None and tok[1] != value:
            raise SyntaxError(f'expected {value!r}, got {tok!r}')
        self.i += 1
        return tok

    # -- emit helpers
    def emit(self, op, *imm):
        self.code.append(OP[op])
        for x in imm:
            self.code += x

    def jump(self, op):
        self.emit(op)
        self.code.append(0)
        return len(self.code) - 1

    def patch(self, at):
        off = len(self.code) - at - 1
        if off > 255:
            raise ValueError('jump too long')
        self.code[at] = off

    # -- grammar (7.2)
    def expr(self):
        self.binary(0)
        if self.peek() == ('op', '?'):
            self.take()
            jz = self.jump('JZ')
            self.expr()
            end = self.jump('JMP')
            self.patch(jz)
            self.take(':')
            self.expr()
            self.patch(end)

    def binary(self, level):
        if level == len(BIN):
            return self.unary()
        self.binary(level + 1)
        while self.peek()[0] == 'op' and self.peek()[1] in BIN[level]:
            op = self.take()[1]
            if op == '??':
                skip = self.jump('JNN')
                self.emit('POP')
                self.binary(level + 1)
                self.patch(skip)
            else:
                self.binary(level + 1)
                self.emit(BINOP[op])
            if level == 3:      # comparisons do not chain
                break

    def unary(self):
        for sym, op in (('-', 'NEG'), ('!', 'LNOT'), ('~', 'BNOT')):
            if self.peek() == ('op', sym):
                self.take()
                self.unary()
                self.emit(op)
                return
        self.primary()
        while self.peek() == ('op', '['):
            self.take()
            self.expr()
            self.take(']')
            self.emit('INDEX')

    def primary(self):
        kind, v = self.take()
        if kind == 'num':
            if isinstance(v, float):
                self.emit('CONST', leb128(self.const(v)))
            elif -128 <= v < 128:
                self.emit('I8', bytes([v & 0xFF]))
            else:
                self.emit('INT', leb128((v << 1) ^ (v >> 63)))
        elif kind == 'str':
            self.emit('CONST', leb128(self.const(v)))
        elif kind == 'id':
            self.name(v)
        elif (kind, v) == ('op', '('):
            self.expr()
            self.take(')')
        else:
            raise SyntaxError(f'unexpected {(kind, v)!r}')

    def name(self, v):
        simple = {'x': 'X', 'payload': 'PAYLOAD', 'last': 'LAST', 'true': 'TRUE', 'false': 'FALSE', 'null': 'NULL'}
        if v in simple:
            self.emit(simple[v])
        elif v.startswith('state.'):
            self.emit('STATE', leb128(self.const(v[6:])))
        elif v.startswith('opt.'):
            self.emit('OPT', leb128(self.const(v[4:])))
        elif v.startswith('gpd.'):
            self.emit('GPD', bytes([GPD_FIELDS.index(v[4:])]))
        elif self.peek() == ('op', '('):
            self.call(v)
        else:
            raise SyntaxError(f'unknown identifier {v}')

    def call(self, fn):
        self.take('(')
        if fn == 'lookup':
            self.expr()
            self.take(',')
            self.emit('LOOKUP', leb128(self.const(self.map_literal())))
            if self.peek() == ('op', ','):      # default value
                self.take()
                skip = self.jump('JNN')
                self.emit('POP')
                self.expr()
                self.patch(skip)
            self.take(')')
            return
        argc = 0
        if self.peek() != ('op', ')'):
            self.expr()
            argc = 1
            while self.peek() == ('op', ','):
                self.take()
                self.expr()
                argc += 1
        self.take(')')
        self.emit('CALL', bytes([FUNCS.index(fn)]), bytes([argc]))

    def map_literal(self):
        self.take('{')
        pairs = []
        while self.peek() != ('op', '}'):
            k = self.take()[1]
            self.take(':')
            pairs.append([k, self.take()[1]])
            if self.peek() == ('op', ','):
                self.take()
        self.take('}')
        return pairs


# ---------------------------------------------------------------- CBOR encoding (8)
EXPR_FIELDS = {'from', 'to', 'when', 'event', 'reset_after', 'present_if'}
TUYA_PREFIX = ['_TZE200_', '_TZE204_', '_TZE284_', '_TZ3000_', '_TZ3210_', '_TZE202_', '_TYZB01_', '_TZ3290_']
BASE36 = '0123456789abcdefghijklmnopqrstuvwxyz'


def pack_tuya_name(s):
    """8.4: `_TZxxxx_` + 8 base-36 chars -> 7 bytes (prefix index + 48-bit value)."""
    if len(s) == 16 and s[:8] in TUYA_PREFIX and all(c in BASE36 for c in s[8:]):
        n = 0
        for c in s[8:]:
            n = n * 36 + BASE36.index(c)
        return bytes([TUYA_PREFIX.index(s[:8])]) + n.to_bytes(6, 'big')
    return s


def number(v):
    """8.1: integral -> int; else the shorter of shortest exact float and tag-4 decimal fraction."""
    if isinstance(v, float):
        if v == int(v) and abs(v) < 2 ** 53:
            return int(v)
        for e in range(1, 10):
            m = round(v * 10 ** e)
            if abs(m / 10 ** e - v) < 1e-12:
                frac = cbor2.CBORTag(4, [-e, m])
                return frac if len(cbor2.dumps(frac)) < len(cbor2.dumps(v, canonical=True)) else v
    return v


def normalize(v):
    if isinstance(v, float):
        return number(v)
    if isinstance(v, dict):
        return {k: normalize(x) for k, x in v.items()}
    if isinstance(v, list):
        return [normalize(x) for x in v]
    return v


class Encoder:
    def __init__(self, doc):
        self.comp = ExprCompiler()
        self.match_names = [n for m in doc.get('match', []) if isinstance(m.get('manufacturer'), dict)
                            for n in m['manufacturer'].get('any', [])]

    def matcher(self, v):
        if isinstance(v, dict) and 'any' in v:
            return [pack_tuya_name(x) for x in v['any']]
        if isinstance(v, dict) and 'prefix' in v:
            return {SUBK['prefix']: v['prefix']}
        return v

    @staticmethod
    def codec(v):
        if isinstance(v, str):
            return CODECS.index(v)
        (name, params), = v.items()
        if name == 'bitmap_flags':
            return {CODECS.index(name): params}
        out = {CODEC_PARAMS[k]: (SCHEDULE_VARIANTS.index(x) if k == 'variant' else x) for k, x in params.items()}
        if name == 'tuya_schedule_day' and out.get(150) == 0:
            del out[150]                                                # multi_dp is the default
        return {CODECS.index(name): out}

    def sub(self, v):
        if isinstance(v, list):
            return [self.sub(x) for x in v]
        if not isinstance(v, dict):
            return v
        out = {}
        for k, x in v.items():
            if k in CLOSED and isinstance(x, str):
                out[SUBK.get(k, 156)] = CLOSED[k].index(x)
            elif k == 'time_sync':
                out[SUBK[k]] = TIME_SYNC[x]
            elif k == 'send':
                out[142] = CLOSED[k].index(x)
            elif k == 'query':
                items = x if isinstance(x, list) else [x]
                mask = ('configure' in items) | ('announce' in items) << 1
                if mask:
                    out[SUBK[k]] = mask
                for it in items:
                    if isinstance(it, dict):
                        out[146] = it['interval_s']
            elif k == 'codec':
                out[SUBK[k]] = self.codec(x)
            elif k in EXPR_FIELDS:
                out[SUBK.get(k, ENTK.get(k))] = self.comp.compile(x)
            elif k == 'attrs' and isinstance(x, dict):
                out[SUBK[k]] = {self.comp.const(a): self.comp.compile(e) for a, e in x.items()}
            elif k == 'type' and isinstance(x, str):
                out[SUBK[k]] = ZCL_TYPES[x]
            elif k == 'manufacturer' and isinstance(x, list):          # white label -> indices
                out[SUBK[k]] = [self.match_names.index(n) for n in x]
            elif k in ('manufacturer', 'model'):
                out[SUBK[k]] = self.matcher(x)
            elif k in ENTK and k not in SUBK:
                out[ENTK[k]] = self.value(k, x)
            else:
                out[SUBK[k]] = self.sub(x)
        return out

    def value(self, k, v):
        if k == 'platform':
            return PLATFORMS.index(v)
        if k == 'device_class':
            return DEVICE_CLASSES.index(v)
        if k == 'unit':
            return UNITS.index(v)
        if k == 'state_class':
            return STATE_CLASSES.index(v)
        if k == 'category':
            return CATEGORY[v]
        if k == 'access':
            return sum(ACCESS[c] for c in v)
        if k == 'tuya':
            return [v[0], TUYA_TYPES[v[1]]]
        if k in EXPR_FIELDS:
            return self.comp.compile(v)
        if k == 'map':
            return [x for pair in v.items() for x in pair]
        if k == 'mode':
            return CLOSED[k].index(v)
        if k == 'codec':
            return self.codec(v)
        if k == 'parts':
            return {SUBK[p]: self.entity(x) for p, x in v.items()}
        if k == 'attrs':
            return {self.comp.const(a): self.comp.compile(e) for a, e in v.items()}
        if k == 'event_types':
            return [self.comp.const(s) for s in v]
        if isinstance(v, (dict, list)):
            return self.sub(v)
        return v

    def entity(self, e):
        return {ENTK[k]: self.value(k, v) for k, v in e.items()}

    def encode(self, doc):
        out = {}
        for k, v in doc.items():
            if k == 'entities':
                out[TOPK[k]] = [self.entity(e) for e in v]
            elif k == 'generic':
                out[TOPK[k]] = {SUBK[gk]: ({pk: self.entity(pv) for pk, pv in gv.items()} if gk == 'patch' else gv)
                                for gk, gv in v.items()}
            elif k == 'endpoints':
                out[TOPK[k]] = v
            elif k == 'self_description':
                out[TOPK[k]] = CLOSED[k].index(v)
            elif isinstance(v, (dict, list)):
                out[TOPK[k]] = self.sub(v)
            else:
                out[TOPK[k]] = v
        if self.comp.consts:
            out[TOPK['consts']] = self.comp.consts
        return cbor2.dumps(normalize(out), canonical=True)


def compile_doc(doc):
    enc = Encoder(doc)
    return enc.encode(doc), enc.comp.longest


def measure(directory):
    rows, blocks = [], []
    for path in sorted(glob.glob(f'{directory}/*.yaml')):
        src = open(path, encoding='utf-8').read()
        doc = yaml.safe_load(src)
        cbor, longest = compile_doc(doc)
        catalog_doc = {k: v for k, v in doc.items() if k != 'description'}
        catalog, _ = compile_doc(catalog_doc)
        blocks.append(catalog)
        entities = len(doc.get('entities', [])) + sum(len(e['parts']) - 1 for e in doc.get('entities', []) if 'parts' in e)
        yaml_len = len(re.sub(r'(?m)^\s*#.*\n', '', src).encode())
        rows.append((path.rsplit('/', 1)[-1], entities, yaml_len, len(cbor), len(catalog),
                     len(zlib.compress(catalog, 9)) - 6, longest or '—', hashlib.sha256(cbor).hexdigest()[:16]))
    print('| File | Entities | YAML (no comments) | CBOR | Catalog form | Deflate | Longest expression | SHA-256 (8 B) |')
    print('|---|---:|---:|---:|---:|---:|---:|---|')
    for r in sorted(rows, key=lambda r: r[3]):
        print('| `%s` | %d | %d | %d | %d | %d | %s | `%s` |' % r)
    together = b''.join(blocks)
    print(f'\nall catalog forms in one block: {len(together)} B raw, {len(zlib.compress(together, 9)) - 6} B deflated '
          f'({(len(zlib.compress(together, 9)) - 6) // len(blocks)} B per definition)')


def main(argv):
    if len(argv) != 3 or argv[1] not in ('measure', 'compile', 'expr'):
        sys.exit(__doc__)
    cmd, arg = argv[1], argv[2]
    if cmd == 'measure':
        measure(arg)
    elif cmd == 'compile':
        sys.stdout.buffer.write(compile_doc(yaml.safe_load(open(arg, encoding='utf-8')))[0])
    else:
        comp = ExprCompiler()
        code = comp.compile(arg)
        print(len(code), code.hex(' '), 'consts:', comp.consts)


if __name__ == '__main__':
    main(sys.argv)

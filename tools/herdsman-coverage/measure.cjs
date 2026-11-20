#!/usr/bin/env node
// Coverage measurement of zigbee-herdsman-converters (zhc) definitions.
//
// Classifies every zhc definition into the translation tiers of
// docs/device-support.md section 17:
//   A  generic declarative   only generic modernExtend calls (m.*) with literal
//                            arguments, named standard fz/tz converters, and
//                            configure functions that a recording mock replays
//   B  Tuya declarative      A plus Tuya extends, named Tuya converters, and DP
//                            tables whose value converters are named or built
//                            by valueConverterBasic / parametrised named helpers
//   C  vendor template       uses vendor-library extends or converters
//                            (philips.m.light, lumi.*, ikea.*, ...), nothing custom
//   D  custom JS             any inline converter, onEvent, inline value
//                            converter, function argument, or configure that the
//                            mock could not replay
//
// Usage (see README.md next to this file):
//   node measure.cjs <path to zigbee-herdsman-converters/dist> <output dir>
//
// Outputs in <output dir>: stats.json (aggregates), results.json (per
// definition), proxy.json (per-definition generator input for proxy_size.py),
// and a Markdown summary on stdout.
'use strict';

const fs = require('fs');
const path = require('path');

const [, , DIST, OUT] = process.argv;
if (!DIST || !OUT) {
  console.error('usage: node measure.cjs <zhc dist dir> <output dir>');
  process.exit(2);
}
fs.mkdirSync(OUT, { recursive: true });
const log = (...a) => process.stderr.write(a.join(' ') + '\n');

// ---------------------------------------------------------------------------
// Identity maps
// ---------------------------------------------------------------------------
const origin = new Map();   // fz/tz/configure/onEvent item or value converter -> "<fn name>[!fn]"
const named = new Map();    // exported converter object -> "fz.x" / "tz.x" / "<lib>.<path>"
const vcNamed = new Map();  // tuya.valueConverter.<name> object -> name
const callArgs = new Map(); // extend / converter object -> [fn name, args] (for proxy.json)

const isFz = (o) => o && typeof o === 'object' && 'cluster' in o && typeof o.convert === 'function';
const isTz = (o) => o && typeof o === 'object' && Array.isArray(o.key) && (o.convertSet || o.convertGet);
const fnSrc = (f) => Function.prototype.toString.call(f);
const srcLen = (x) => {
  if (!x) return 0;
  if (typeof x === 'function') return fnSrc(x).length;
  return Object.values(x).reduce((n, v) => n + (typeof v === 'function' ? fnSrc(v).length : 0), 0);
};

// Index exported converter objects by identity (depth <= 2).
function nameConverters(obj, prefix, depth = 0) {
  if (!obj || typeof obj !== 'object' || depth > 2) return;
  for (const k of Object.keys(obj)) {
    let v;
    try { v = obj[k]; } catch { continue; }
    if (isFz(v) || isTz(v)) { if (!named.has(v)) named.set(v, prefix + k); }
    else if (v && typeof v === 'object' && !Array.isArray(v)) nameConverters(v, prefix + k + '.', depth + 1);
  }
}

// True if a call passes code: a function, or an inline value converter
// ({from: fn}) that is neither named nor built by a known helper.
function hasFunctionArg(args) {
  const isCode = (v) => typeof v === 'function'
    || (v && typeof v === 'object' && !Array.isArray(v) && typeof v.from === 'function' && !vcNamed.has(v) && !origin.has(v));
  for (const a of args) {
    if (isCode(a)) return true;
    if (a && typeof a === 'object' && !named.has(a) && !a.isModernExtend) {
      for (const v of Object.values(a)) {
        if (isCode(v)) return true;
        if (Array.isArray(v) && v.some(isCode)) return true;
      }
    }
  }
  return false;
}

// Tag everything a modernExtend returns with the name of the function that built it.
function tagExtend(name, me) {
  if (!me || typeof me !== 'object') return;
  if (Array.isArray(me)) { me.forEach((x) => tagExtend(name, x)); return; }
  if (!me.isModernExtend) return;
  for (const k of ['fromZigbee', 'toZigbee', 'configure', 'exposes', 'onEvent']) {
    for (const it of [].concat(me[k] || [])) if (it && (typeof it === 'object' || typeof it === 'function')) origin.set(it, name);
  }
  origin.set(me, name);
}
const tagValue = (name, r) => { if (r && typeof r === 'object') origin.set(r, name); };

// Replace every exported function of `obj` with a wrapper that tags its result.
// The outermost call wins because it tags last.
function wrapExports(obj, prefix, tagger = tagExtend) {
  if (!obj || typeof obj !== 'object') return;
  for (const k of Object.keys(obj)) {
    const f = obj[k];
    if (typeof f !== 'function' || /^[A-Z]/.test(k) || /^class /.test(fnSrc(f))) continue;
    const w = function (...a) {
      const r = f.apply(this, a);
      tagger(prefix + k + (hasFunctionArg(a) ? '!fn' : ''), r);
      if (r && typeof r === 'object') callArgs.set(r, [prefix + k, a]);
      return r;
    };
    try { obj[k] = w; } catch { /* read-only export */ }
    if (obj[k] !== w) {
      try { Object.defineProperty(obj, k, { value: w, writable: true, configurable: true, enumerable: true }); } catch { /* keep */ }
    }
  }
}

// ---------------------------------------------------------------------------
// Instrument zhc before any device file loads
// ---------------------------------------------------------------------------
nameConverters(require(path.join(DIST, 'converters/fromZigbee')), 'fz.');
nameConverters(require(path.join(DIST, 'converters/toZigbee')), 'tz.');
const libs = {};
for (const f of fs.readdirSync(path.join(DIST, 'lib')).filter((f) => f.endsWith('.js'))) {
  try { libs[f.replace(/\.js$/, '')] = require(path.join(DIST, 'lib', f)); } catch { /* not loadable alone */ }
}
for (const [n, mod] of Object.entries(libs)) nameConverters(mod, n + '.');

const tuya = libs.tuya;
for (const [k, v] of Object.entries(tuya.valueConverter)) if (v && typeof v === 'object') vcNamed.set(v, k);
wrapExports(tuya.valueConverter, 'vcf.', tagValue);       // parametrised named converters
wrapExports(tuya.valueConverterBasic, 'vcb.', tagValue);  // lookup, divideBy, scale, ...
wrapExports(libs.modernExtend, 'm.');
for (const [n, mod] of Object.entries(libs)) {
  if (n === 'modernExtend') continue;
  if (mod.modernExtend) wrapExports(mod.modernExtend, n + '.me.');
  wrapExports(mod, n + '.');
  for (const [k, v] of Object.entries(mod)) {
    if (v && typeof v === 'object' && !Array.isArray(v) && !['modernExtend', 'valueConverter', 'valueConverterBasic'].includes(k)) {
      wrapExports(v, `${n}.${k}.`);                       // philips.m.*, lumi.lumiModernExtend.*, ...
    }
  }
}
libs.utils.sleep = async () => {};                       // configure functions sleep between steps
const quiet = () => {};                                   // zhc logs to stdout; keep stdout for the summary
libs.logger.setLogger({ debug: quiet, info: quiet, warning: quiet, error: quiet });

const defs = [];
for (const f of fs.readdirSync(path.join(DIST, 'devices')).filter((f) => f.endsWith('.js'))) {
  for (const d of require(path.join(DIST, 'devices', f)).definitions || []) defs.push({ file: f.replace(/\.js$/, ''), d });
}

// ---------------------------------------------------------------------------
// Recording mock device for configure functions
// ---------------------------------------------------------------------------
function mockDevice(d, ops) {
  const eps = new Map();
  const fp0 = Array.isArray(d.fingerprint) ? d.fingerprint[0] : undefined;
  const dev = {
    ieeeAddr: '0x0000000000000000', networkAddress: 1,
    modelID: (d.zigbeeModel && d.zigbeeModel[0]) || (fp0 && fp0.modelID) || 'model',
    manufacturerName: (fp0 && fp0.manufacturerName) || 'manufacturer',
    powerSource: 'Battery', type: 'EndDevice', softwareBuildID: '1', dateCode: '1',
    applicationVersion: 1, stackVersion: 1, hardwareVersion: 1, zclVersion: 1, manufacturerID: 1,
    meta: {}, customClusters: {}, interviewCompleted: true,
    getEndpoint: (id) => mkEp(id), save: () => ops.push('save'), addCustomCluster: () => ops.push('customCluster'),
  };
  function mkEp(id) {
    if (eps.has(id)) return eps.get(id);
    const rec = (op) => async () => { ops.push(op); return {}; };
    const ep = {
      ID: id, deviceIeeeAddress: dev.ieeeAddr, clusters: {}, inputClusters: [], outputClusters: [], binds: [], configuredReportings: [],
      bind: rec('bind'), unbind: rec('unbind'), configureReporting: rec('configureReporting'), read: rec('read'), write: rec('write'),
      command: rec('command'), writeStructured: rec('writeStructured'), defaultResponse: rec('defaultResponse'),
      addToGroup: rec('group'), removeFromGroup: rec('group'),
      saveClusterAttributeKeyValue: () => ops.push('meta'), getClusterAttributeValue: () => undefined,
      supportsInputCluster: () => true, supportsOutputCluster: () => true, getInputClusters: () => [], getOutputClusters: () => [],
      getDevice: () => dev, save: () => {},
    };
    eps.set(id, ep);
    return ep;
  }
  Object.defineProperty(dev, 'endpoints', { get: () => { if (!eps.size) mkEp(1); return [...eps.values()]; } });
  return dev;
}

const probe = { tried: 0, ok: 0, errors: {} };
async function replayConfigure(d, fn) {
  const ops = [];
  const def = { ...d, endpoint: d.endpoint || (() => ({ default: 1 })) };
  probe.tried++;
  try {
    await Promise.race([
      fn(mockDevice(d, ops), { ID: 1, deviceIeeeAddress: '0x00000000000000c0' }, def),
      new Promise((_, reject) => setTimeout(() => reject(new Error('timeout')), 300)),
    ]);
    probe.ok++;
    return true;
  } catch (e) {
    const k = String((e && e.message) || e).slice(0, 80).replace(/\d+/g, 'N');
    probe.errors[k] = (probe.errors[k] || 0) + 1;
    return false;
  }
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------
function classify(it) {
  if (named.has(it)) {
    const n = named.get(it);
    if (n.startsWith('fz.') || n.startsWith('tz.')) return ['std', n];
    if (n.startsWith('tuya.')) return ['tuyaNamed', n];
    if (n.startsWith('legacy.')) return ['legacy', n];
    return ['vendorNamed', n];
  }
  if (origin.has(it)) {
    const o = origin.get(it);
    if (o.startsWith('m.')) return ['m', o];
    if (o.startsWith('vcb.') || o.startsWith('vcf.')) return ['tuyaVC', o];
    if (o.startsWith('tuya.')) return ['tuyaME', o];
    return ['vendorME', o];
  }
  return ['custom', ''];
}

const inc = (o, k, n = 1) => { o[k] = (o[k] || 0) + n; };
const sanitize = (v, depth = 0) => {
  if (depth > 6) return null;
  if (v == null || ['number', 'boolean', 'string'].includes(typeof v)) return v;
  if (typeof v === 'function') return null;
  if (Array.isArray(v)) return v.map((x) => sanitize(x, depth + 1));
  if (vcNamed.has(v)) return ['vc', vcNamed.get(v)];
  if (callArgs.has(v)) { const [n, a] = callArgs.get(v); return [n, sanitize(a, depth + 1)]; }
  if (named.has(v)) return ['n', named.get(v)];
  const o = {};
  for (const [k, x] of Object.entries(v)) if (typeof x !== 'function') o[k] = sanitize(x, depth + 1);
  return o;
};

(async () => {
  const S = { total: defs.length, tiers: {}, cats: {}, reasons: {}, stdUse: {}, mUse: {}, tuyaVC: {}, vendorUse: {},
    ts0601: { total: 0, tiers: {} }, tuyaDp: { total: 0, tiers: {} }, whiteLabels: 0, zigbeeModels: 0, fingerprints: 0, dynamicExposes: 0 };
  const results = [];
  const proxy = [];
  for (const { file, d } of defs) {
    const cats = new Set();
    const reasons = [];
    const std = new Set(); const mfn = new Set(); const vendor = new Set();
    let customSrc = 0; let nExposes = 0;
    const items = [];
    for (const e of [].concat(d.extend || [])) {
      if (!e) continue;
      for (const k of ['fromZigbee', 'toZigbee', 'configure', 'onEvent']) for (const it of [].concat(e[k] || [])) items.push([it, k]);
      for (const ex of [].concat(e.exposes || [])) {
        if (typeof ex === 'function' && !origin.has(ex)) cats.add('dynamicExposes');
        else if (ex && typeof ex === 'object') nExposes++;
      }
    }
    for (const k of ['fromZigbee', 'toZigbee', 'configure', 'onEvent']) for (const it of [].concat(d[k] || [])) items.push([it, k]);
    if (Array.isArray(d.exposes)) nExposes += d.exposes.length;
    else if (typeof d.exposes === 'function') cats.add('dynamicExposes');

    for (const [it, kind] of items) {
      if (it == null) continue;   // e.g. m.numeric without reporting yields configure: [undefined]
      let [c, n] = classify(it);
      if (n.endsWith('!fn')) { c = 'custom'; reasons.push('fnArg'); }
      else if (c === 'custom' && kind === 'configure' && typeof it === 'function' && await replayConfigure(d, it)) c = 'configureReplayed';
      cats.add(c);
      if (c === 'std') { inc(S.stdUse, n); std.add(n); }
      if (c === 'm') { inc(S.mUse, n); mfn.add(n); }
      if (['vendorME', 'vendorNamed', 'legacy'].includes(c)) { inc(S.vendorUse, n); vendor.add(n); }
      if (c === 'custom') { if (!n.endsWith('!fn')) reasons.push(kind); customSrc += srcLen(it); }
    }
    const dps = d.meta && d.meta.tuyaDatapoints;
    if (dps) {
      cats.add('tuyaDP');
      for (const dp of dps) {
        const conv = dp[2];
        if (!conv) continue;
        if (vcNamed.has(conv)) inc(S.tuyaVC, vcNamed.get(conv));
        else if (origin.has(conv)) inc(S.tuyaVC, origin.get(conv));
        else { cats.add('custom'); reasons.push('tuyaVC'); customSrc += srcLen(conv); }
      }
    }
    let tier;
    if (cats.has('custom')) tier = 'D';
    else if (cats.has('vendorME') || cats.has('vendorNamed') || cats.has('legacy')) tier = 'C';
    else if (cats.has('tuyaME') || cats.has('tuyaNamed') || cats.has('tuyaVC') || cats.has('tuyaDP')) tier = 'B';
    else tier = 'A';

    for (const c of cats) inc(S.cats, c);
    for (const r of new Set(reasons)) inc(S.reasons, r);
    inc(S.tiers, tier);
    if (cats.has('dynamicExposes')) S.dynamicExposes++;
    const ts0601 = (d.zigbeeModel || []).some((z) => /^TS0601/.test(z))
      || (Array.isArray(d.fingerprint) && d.fingerprint.some((f) => /^TS0601/.test(f.modelID || '')));
    if (ts0601) { S.ts0601.total++; inc(S.ts0601.tiers, tier); }
    if (dps) { S.tuyaDp.total++; inc(S.tuyaDp.tiers, tier); }
    S.whiteLabels += (d.whiteLabel || []).length;
    S.zigbeeModels += (d.zigbeeModel || []).length;
    S.fingerprints += Array.isArray(d.fingerprint) ? d.fingerprint.length : 0;

    results.push({ file, vendor: d.vendor, model: d.model, tier, cats: [...cats], reasons, std: [...std], m: [...mfn],
      vendorItems: [...vendor], customSrc, nExposes, nDp: (dps || []).length });
    proxy.push({ tier, file, zm: d.zigbeeModel, fp: Array.isArray(d.fingerprint) ? d.fingerprint.map((f) => [f.modelID, f.manufacturerName]) : null,
      vendor: d.vendor, model: d.model, ds: d.description,
      x: (d.extend || []).map((e) => (callArgs.has(e) ? sanitize(e) : null)),
      fz: (d.fromZigbee || []).map((c) => named.get(c) || null), tz: (d.toZigbee || []).map((c) => named.get(c) || null),
      ex: Array.isArray(d.exposes) ? d.exposes.map((e) => ({ n: e.name, t: e.type, a: e.access, u: e.unit, mi: e.value_min,
        ma: e.value_max, v: e.values, c: e.category, f: e.features ? e.features.map((f) => f.name) : undefined })) : null,
      dp: dps ? sanitize(dps) : null });
  }

  // ---- derived figures ----------------------------------------------------
  const sortDesc = (o) => Object.entries(o).sort((a, b) => b[1] - a[1]);
  const stdRank = new Map(sortDesc(S.stdUse).map(([n], i) => [n, i]));
  const AB = results.filter((r) => r.tier === 'A' || r.tier === 'B');
  S.abWithTopStd = Object.fromEntries([25, 50, 100, 150].map((N) => [N, AB.filter((r) => r.std.every((n) => stdRank.get(n) < N)).length]));
  // Extends that carry device-specific data: a definition using only other
  // generic extends needs nothing but naming in the hub's generic tier.
  const specific = new Set(['m.numeric', 'm.enumLookup', 'm.binary', 'm.deviceAddCustomCluster', 'm.text', 'm.actionEnumLookup',
    'm.quirkAddEndpointCluster', 'm.quirkCheckinInterval', 'm.forcePowerSource', 'm.forceDeviceType', 'm.customTimeResponse',
    'm.writeTimeDaily', 'm.poll', 'm.ignoreClusterReport', 'm.deviceEndpoints', 'm.skipDefaultResponse',
    'm.customLocalTemperatureCalibrationRange', 'm.iasArmCommandDefaultResponse', 'm.iasGetPanelStatusResponse']);
  S.pureGeneric = results.filter((r) => r.tier === 'A' && r.m.every((n) => !specific.has(n)) && !r.cats.includes('configureReplayed')).length;
  const C = results.filter((r) => r.tier === 'C');
  const cUse = {};
  C.forEach((r) => r.vendorItems.forEach((v) => inc(cUse, v)));
  const vRank = new Map(sortDesc(cUse).map(([n], i) => [n, i]));
  S.cWithTopVendor = Object.fromEntries([10, 20, 40, 80].map((N) => [N, C.filter((r) => r.vendorItems.every((v) => vRank.get(v) < N)).length]));
  S.cVendorDistinct = vRank.size;
  S.cVendorTop = sortDesc(cUse).slice(0, 15);
  const D = results.filter((r) => r.tier === 'D');
  S.dSmallCustom = Object.fromEntries([400, 800].map((L) => [L, D.filter((r) => r.customSrc <= L).length]));
  const vcTotal = Object.values(S.tuyaVC).reduce((a, b) => a + b, 0);
  let acc = 0;
  S.tuyaVcCoverage = { uses: vcTotal, distinct: Object.keys(S.tuyaVC).length };
  sortDesc(S.tuyaVC).forEach(([, n], i) => { acc += n; if ([5, 10, 20, 40].includes(i + 1)) S.tuyaVcCoverage['top' + (i + 1)] = +(100 * acc / vcTotal).toFixed(1); });
  S.stdDistinct = stdRank.size;
  S.mDistinct = Object.keys(S.mUse).length;
  S.configureReplay = { tried: probe.tried, ok: probe.ok, errors: sortDesc(probe.errors).slice(0, 15) };
  const mean = (xs) => +(xs.reduce((a, b) => a + b, 0) / (xs.length || 1)).toFixed(1);
  S.meanExposesAB = mean(AB.map((r) => r.nExposes));
  S.meanDpPerTable = mean(results.filter((r) => r.nDp).map((r) => r.nDp));

  fs.writeFileSync(path.join(OUT, 'stats.json'), JSON.stringify(S, null, 1));
  fs.writeFileSync(path.join(OUT, 'results.json'), JSON.stringify(results));
  fs.writeFileSync(path.join(OUT, 'proxy.json'), JSON.stringify(proxy));

  const t = S.tiers;
  const pct = (n) => (100 * n / S.total).toFixed(1) + ' %';
  const row = (name, n) => `| ${name} | ${n} | ${pct(n)} |`;
  const out = [
    '| Tier | Definitions | Share |', '|---|---:|---:|',
    row('A generic declarative', t.A || 0), row('B Tuya declarative', t.B || 0),
    row('A + B', (t.A || 0) + (t.B || 0)), row('C vendor template', t.C || 0), row('D custom JS', t.D || 0), '',
    `- definitions ${S.total}, white labels ${S.whiteLabels}, zigbeeModel strings ${S.zigbeeModels}, fingerprint entries ${S.fingerprints}`,
    `- pure generic (naming only): ${S.pureGeneric}; configure functions replayed: ${probe.ok}/${probe.tried}`,
    `- A+B covered by the top-N standard converters: ${JSON.stringify(S.abWithTopStd)} (${S.stdDistinct} distinct)`,
    `- Tuya DP tables ${S.tuyaDp.total}: ${JSON.stringify(S.tuyaDp.tiers)}; TS0601 ${S.ts0601.total}: ${JSON.stringify(S.ts0601.tiers)}`,
    `- Tuya value converters: ${JSON.stringify(S.tuyaVcCoverage)}`,
    `- C unblocked by the top-N vendor items: ${JSON.stringify(S.cWithTopVendor)} (${S.cVendorDistinct} distinct)`,
    `- D reasons: ${JSON.stringify(S.reasons)}; D with <= N chars of custom JS: ${JSON.stringify(S.dSmallCustom)}`,
  ];
  process.stdout.write(out.join('\n') + '\n');
  log(`wrote ${OUT}/stats.json, results.json, proxy.json`);
})();

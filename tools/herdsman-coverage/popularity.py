#!/usr/bin/env python3
"""Popularity of zhc definitions from GitHub issue mentions (spec 17.2, D17).

For the 4 MB bank the catalog builder needs a popularity order (decision D12).
zigbee-herdsman-converters has no public usage statistics, so this script
counts, per definition, how many GitHub issues and pull requests of the
Zigbee2MQTT and ZHA projects mention one of its identifiers.

Method:
1. Identifiers per definition (from measure.cjs's proxy.json): the model,
   every `zigbeeModel` string, every fingerprint model ID and manufacturer
   name. Dropped: identifiers shorter than 4 characters; identifiers without
   a digit, `.` or `_` (zhc has `zigbeeModel` strings such as "Switch",
   "motion", "Remote" that would match ordinary words); and identifiers shared
   by two or more definitions (TS0601, TS0001, …), which cannot tell them
   apart.
2. Issue texts (title + body, first 20 000 characters) of the repositories
   below are fetched once through the GitHub REST API and cached as JSONL;
   later runs fetch only issues updated since the cache.
3. An issue mentions an identifier if the identifier appears as a whole word,
   case-insensitively. Word characters: letters, digits, `_ . + -`;
   identifiers with spaces are matched as phrases.
4. Score = number of distinct issues (over all repositories) mentioning at
   least one identifier of the definition.
5. Coordinator/adapter definitions (USB/network adapters with router
   firmware: SLZB-06M, Sonoff dongles, "Silabs series 2 router", ...) get score
   0 and are listed separately: Zigbee2MQTT's issue template asks every
   reporter for the adapter, so they top the raw counts (SLZB-06M: 1 083).
   The rule (`is_adapter`) is the same as `ADAPTER_*` in
   faronov/zigbee-rs-definitions generator/src/translate.ts.

usage: python3 popularity.py <out dir of measure.cjs> [--max-pages N]
needs: the GitHub CLI `gh`, logged in (it makes the API calls; ~500 requests
       for a first full run, a few for later incremental runs)
output: <out>/popularity.json ([{file, vendor, model, tier, score, adapter}], sorted),
        Markdown summary on stdout
"""
import json
import os
import re
import subprocess
import sys

REPOS = ['Koenkk/zigbee2mqtt', 'Koenkk/zigbee-herdsman-converters', 'zigpy/zha-device-handlers',
         'zigpy/zha']
BODY_LIMIT = 20000
WORD = re.compile(r'[A-Za-z0-9_.+-]+')


ADAPTER_DESCRIPTION = re.compile(r'\b(coordinator|with router firmware|adapter with router|usb (stick|dongle|adapter)|zigbee (usb )?(stick|dongle)|'
                                 r'dongle (plus|max|lite)|zigbee-ethernet|ethernet router|bridge router)\b', re.I)
ADAPTER_MODEL = re.compile(r'^(SLZB-|ZBDongle|Dongle-|tubeszb|CC2530\.ROUTER|CC2531|EFR32MG21\.Router|Silabs series 2 router|ZREL$|SkyConnect|'
                           r'ZBT-1$|ConBee|RaspBee|UZG-|XZG)', re.I)


def is_adapter(d):
    """Coordinator / NCP / adapter hardware (not a router product such as a repeater)."""
    if ADAPTER_DESCRIPTION.search(d.get('ds') or ''):
        return True
    return any(ADAPTER_MODEL.search(str(x)) for x in [d.get('model') or ''] + list(d.get('zm') or []))


JQ = '.[] | {n: .number, u: .updated_at, t: ((.title // "") + "\\n" + ((.body // "")[:%d]))}' % BODY_LIMIT


def gh_lines(path, paginate):
    """Run `gh api` (it carries the user's GitHub login) and yield one JSON object per issue."""
    cmd = ['gh', 'api', path, '--jq', JQ] + (['--paginate'] if paginate else [])
    out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout
    return [json.loads(line) for line in out.splitlines() if line.strip()]


def fetch(repo, cache, max_pages):
    """Fetch issues and PRs of `repo` (only those updated since the cache) into the JSONL cache."""
    known, since = {}, None
    if os.path.exists(cache):
        for line in open(cache, encoding='utf-8'):
            it = json.loads(line)
            known[it['n']] = it
            since = max(since or it['u'], it['u'])
    path = f'repos/{repo}/issues?state=all&per_page=100&sort=updated&direction=asc'
    if since:
        path += f'&since={since}'
    if max_pages is None:
        items = gh_lines(path, paginate=True)
    else:                             # quick test: first pages only
        items = [it for p in range(1, max_pages + 1) for it in gh_lines(f'{path}&page={p}', paginate=False)]
    for it in items:
        known[it['n']] = it
    print(f'{repo}: {len(known)} issues and pull requests', file=sys.stderr)
    with open(cache, 'w', encoding='utf-8') as f:
        for it in known.values():
            f.write(json.dumps(it) + '\n')
    return [it['t'] for it in known.values()]


def identifiers(defs):
    owner = {}
    for i, d in enumerate(defs):
        ids = {d.get('model') or ''}
        ids.update(d.get('zm') or [])
        for model_id, mfr in d.get('fp') or []:
            ids.update(x for x in (model_id, mfr) if x)
        ids = {x.strip().lower() for x in ids if isinstance(x, str) and len(x.strip()) >= 4
               and re.search(r'[0-9._]', x)}    # model-like only: "Switch", "motion", "Hive" are words
        for x in ids:
            owner.setdefault(x, set()).add(i)
    return {x: next(iter(o)) for x, o in owner.items() if len(o) == 1}


def main(argv):
    if len(argv) < 2:
        sys.exit(__doc__)
    out = argv[1]
    max_pages = int(argv[argv.index('--max-pages') + 1]) if '--max-pages' in argv else None
    defs = json.load(open(f'{out}/proxy.json', encoding='utf-8'))
    unique = identifiers(defs)
    words = {x: i for x, i in unique.items() if WORD.fullmatch(x)}
    phrases = {}                      # first word -> [(phrase, definition)]
    for x, i in unique.items():
        if x not in words and WORD.findall(x):
            phrases.setdefault(WORD.findall(x)[0].strip('.-+'), []).append((x, i))

    texts = []
    for repo in REPOS:
        texts += fetch(repo, f"{out}/issues-{repo.replace('/', '_')}.jsonl", max_pages)

    score = [0] * len(defs)
    for text in texts:
        low = text.lower()
        seen = set()
        for w in WORD.findall(low):
            w = w.strip('.-+')
            if w in words:
                seen.add(words[w])
            for phrase, i in phrases.get(w, ()):
                if re.search(r'(?<![a-z0-9_.+-])' + re.escape(phrase) + r'(?![a-z0-9_+-])', low):
                    seen.add(i)
        for i in seen:
            score[i] += 1

    adapters = {i for i, d in enumerate(defs) if is_adapter(d)}
    ranked = sorted(({'file': d['file'], 'vendor': d['vendor'], 'model': d['model'], 'tier': d['tier'],
                      'score': 0 if i in adapters else s, 'raw': s, 'adapter': i in adapters}
                     for i, (d, s) in enumerate(zip(defs, score))), key=lambda r: -r['score'])
    json.dump(ranked, open(f'{out}/popularity.json', 'w', encoding='utf-8'), indent=0)
    ab = [r for r in ranked if r['tier'] in 'AB']
    print(f'issues scanned: {len(texts)}; definitions with a unique identifier: '
          f'{len(set(unique.values()))} of {len(defs)}; mentioned at least once: {sum(1 for s in score if s)}')
    print(f'fully translated (A+B) mentioned at least once: {sum(1 for r in ab if r["score"])} of {len(ab)}')
    excl = sorted((r for r in ranked if r['adapter']), key=lambda r: -r['raw'])
    print(f'adapters excluded from the ranking: {len(excl)} ('
          + ', '.join(f"{r['vendor']} {r['model']} {r['raw']}" for r in excl[:5]) + ')\n')
    print('| Rank | Vendor | Model | Tier | Issues |')
    print('|---:|---|---|---|---:|')
    for n, r in enumerate(ranked[:20], 1):
        print(f"| {n} | {r['vendor']} | {r['model']} | {r['tier']} | {r['score']} |")


if __name__ == '__main__':
    main(sys.argv)

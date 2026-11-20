import subprocess,sys,re,collections
nm,elf=sys.argv[1],sys.argv[2]
out=subprocess.run([nm,'-S','--size-sort','-C','--radix=d',elf],capture_output=True,text=True).stdout
g=collections.Counter()
for l in out.splitlines():
    p=l.split(None,3)
    if len(p)<4 or p[2].lower() not in 'tr': continue
    name=p[3]; size=int(p[1])
    m=re.match(r'^(?:<)?(?:_?)([a-z_][a-z0-9_]*)::(?:([a-z_][a-z0-9_]*)::)?',name)
    if m:
        k=m.group(1)
        if k=='zdef' and m.group(2): k='zdef::'+m.group(2)
    else: k=name[:40]
    if name.startswith('<'):
        mm=re.match(r'<([a-z_0-9]+)::(?:([a-z_0-9]+)::)?',name)
        if mm: k=mm.group(1)+('::'+mm.group(2) if mm.group(1)=='zdef' and mm.group(2) else '')
    g[k]+=size
for k,v in g.most_common(25): print(f'{v:8d} {k}')
print(sum(g.values()),'total t/r')

#!/usr/bin/env python3
# parse.py <arm-dir>    e.g. python3 bench/parse.py bench/results/v1/skill
# Parse token/cost/turn metrics from a run's pi JSON logs. stdlib only.
import json, sys, os

def parse(path):
    calls=0; inp=out=cr=cw=cost=0; tools={}; tbytes=0; errs=0; last_ctx=0; resbytes={}
    for l in open(path):
        l=l.strip()
        if not l.startswith('{'): continue
        d=json.loads(l)
        t=d.get('type')
        if t=='turn_end':
            m=d['message']
            if m['role']=='assistant':
                u=m.get('usage') or {}
                calls+=1
                inp+=u.get('input',0); out+=u.get('output',0)
                cr+=u.get('cacheRead',0); cw+=u.get('cacheWrite',0)
                c=u.get('cost')
                cost+=c.get('total',0) if isinstance(c,dict) else 0
                last_ctx=u.get('input',0)+u.get('cacheRead',0)+u.get('cacheWrite',0)
                for x in m['content']:
                    if x.get('type')=='toolCall': tools[x['name']]=tools.get(x['name'],0)+1
        elif t=='tool_execution_end':
            r=d.get('result') or {}
            n=sum(len(c.get('text','')) for c in r.get('content',[]) if isinstance(c,dict))
            tbytes+=n
            resbytes[d['toolName']]=resbytes.get(d['toolName'],0)+n
            if d.get('isError'): errs+=1
    return dict(calls=calls,input=inp,output=out,cacheRead=cr,cacheWrite=cw,
                total=inp+out+cr+cw,last_ctx=last_ctx,cost=round(cost,5),
                tools=tools,result_chars=tbytes,result_chars_by_tool=resbytes,errors=errs)

def main():
    arm=sys.argv[1].rstrip('/')
    tim={}
    tp=os.path.join(arm,'timing.txt')
    if os.path.exists(tp):
        for line in open(tp):
            w,ms,rc=line.split()
            tim[int(w)]=(int(ms),int(rc))
    res={}
    for w in (1,2,3):
        p=os.path.join(arm,f'w{w}.json')
        if not os.path.exists(p): continue
        r=parse(p)
        r['wall_ms'],r['rc']=tim.get(w,(None,None))
        md=os.path.join(arm,f'w{w}.md')
        r['md_bytes']=os.path.getsize(md) if os.path.exists(md) else 0
        res[w]=r
    agg={}
    for k in ('input','output','cacheRead','cacheWrite','total','cost','calls','result_chars','errors','md_bytes'):
        agg[k]=sum(r.get(k) or 0 for r in res.values())
    agg['wall_s']=round(sum((r.get('wall_ms') or 0) for r in res.values())/1000,1)
    agg['total_cost_usd']=round(agg.pop('cost'),4)
    print(json.dumps(dict(arm=arm,waves=res,agg=agg),indent=1))

if __name__ == '__main__':
    main()

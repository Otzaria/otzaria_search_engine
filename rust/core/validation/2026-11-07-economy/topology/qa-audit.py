#!/usr/bin/env python3
"""Independent read-only audit of the two frozen diagnostic artifacts.

Reads archives, raw records, context and stdout; never imports or executes their
controllers, executable files or source. Writes only the requested QA JSON.
"""
import argparse
from collections import Counter
import hashlib
import json
import math
from pathlib import Path, PurePosixPath
import re
import statistics
import stat
import zipfile

S = 'smt_siblings'
D = 'distinct_physical_cores'
REV = {'B':'8ef29fd592793fbc9d5af0299fdd7a8607789bac', 'H':'4761356749815d283ede41ef956c64b3dbffe048', 'C':'7b1d1329591c0d019c9835b21c28450c32b2d5fc'}
BIN = {'B':'a58b7ec047d3b39d0f4417571d652920d14ea7f8370030aa6bddd349dc506e5a', 'H':'974a92cc60324467d26ebad61c7d21ca0e7512087201eaa037ff357d42151337'}
CONFIG = {
    'topology': {'run':42431565386, 'commit':'1c1630d0b0034ddcfaf724fca92643764f44a9a7', 'ref':'refs/heads/diag/shared-core-economy-topology-20261107', 'controller':'topology-controller.py', 'controller_sha256':'2b62a452e1484af74dd141ab58e6bc67df0696caa092c40958665ffa1705a688', 'count':64},
    'candidate': {'run':42431839752, 'commit':'b4fa35781b5b07f8bb25bf851c55c95b64e7a464', 'ref':'refs/heads/diag/shared-core-render-producer-20261108', 'controller':'render-producer-controller.py', 'controller_sha256':'55137bcde2d6831e41873134c4bbd9be808ed6b29d38a30f7b290e816aa61c43', 'count':72},
}
METRICS = ['search_during_p50_ms','search_during_p95_ms','search_during_p99_ms','search_idle_p95_ms','search_samples','add_ms','total_ms','docs_per_sec','peak_rss_kib','rss_before_kib','peak_buffered_bytes','spilled_bytes','peak_reserved_bytes','peak_active_preparations']

def sha(data): return hashlib.sha256(data).hexdigest()
def median(values): return statistics.median(values) if values else None
def ratio(a,b): return a/b if a is not None and b not in (None,0) else None
def cpu_list(text):
    out=set()
    for part in text.split(','):
        low, sep, high=part.partition('-')
        out.update(range(int(low),int(high)+1) if sep else [int(low)])
    return out

def independent_schedule(kind, affinities, hashes):
    out=[]
    counts=Counter()
    if kind=='topology':
        blocks=[('R1','real','BHHB',None,S),('CB1','control','BHHB','B',D),('R2','real','HBBH',None,S),('CH1','control','HBBH','H',D),('R3','real','HBBH',None,D),('CB2','control','HBBH','B',S),('R4','real','BHHB',None,D),('CH2','control','BHHB','H',S)]
        for index,(block,mode,order,actual,first) in enumerate(blocks):
            for precedence,case in enumerate((first,D if first==S else S),1):
                for pos,label in enumerate(order,1):
                    actual_label=actual or label
                    out.append({'run':len(out)+1,'case_run':index*4+pos,'block':block,'kind':mode,'order':order,'position':pos,'pseudo_label':label,'actual_binary':actual_label,'topology_case':case,'topology_precedence':precedence,'affinity':affinities[case],'source_commit':REV[actual_label],'binary_sha256':hashes[actual_label]})
    else:
        for index,(order,first) in enumerate(zip(['BHC','CHB','BCH','HCB','HBC','CBH'],[S,D,D,S,D,S]),1):
            other=D if first==S else S
            for mode,seq in [('real',(first,other)),('control',(other,first))]:
                for precedence,case in enumerate(seq,1):
                    for pos,label in enumerate(order,1):
                        actual=label if mode=='real' else 'C'
                        counts[case]+=1
                        out.append({'run':len(out)+1,'case_run':counts[case],'block':('R' if mode=='real' else 'CC')+str(index),'permutation':index,'kind':mode,'order':order,'position':pos,'C_position':order.index('C')+1,'pseudo_label':label,'actual_binary':actual,'topology_case':case,'topology_precedence':precedence,'affinity':affinities[case],'source_commit':REV[actual],'binary_sha256':hashes.get(actual)})
    return out

def pressure(snapshot, category):
    text=snapshot.get('pressure_'+category)
    if text is None: return None
    match=re.search(r'^some .*\btotal=(\d+)',text,re.M)
    return int(match[1]) if match else None

def summarize(rows):
    medians={key:median([r['sample'][key] for r in rows]) for key in METRICS}
    process={key:median([r['process'][key] for r in rows]) for key in ['process_wall_seconds','whole_process_cpu_user_seconds','whole_process_cpu_system_seconds']}
    process['whole_process_cpu_seconds']=median([r['process']['whole_process_cpu_user_seconds']+r['process']['whole_process_cpu_system_seconds'] for r in rows])
    for category in ['cpu','io','memory']:
        values=[]
        for r in rows:
            before=pressure(r['process']['before'],category); after=pressure(r['process']['after'],category)
            if before is not None and after is not None: values.append(after-before)
        process[category+'_psi_some_delta_us']=median(values)
    return {'processes':len(rows),'fixture_medians':medians,'process_context_medians':process,'p95_values':[r['sample']['search_during_p95_ms'] for r in rows],'run_numbers':[r['run'] for r in rows]}

def main():
    p=argparse.ArgumentParser()
    p.add_argument('--kind',choices=CONFIG,required=True)
    p.add_argument('--archive',type=Path,required=True)
    p.add_argument('--archive-sha256',required=True)
    p.add_argument('--data',type=Path,required=True)
    p.add_argument('--out',type=Path,required=True)
    args=p.parse_args(); cfg=CONFIG[args.kind]
    archive_sha=sha(args.archive.read_bytes()); assert archive_sha==args.archive_sha256
    with zipfile.ZipFile(args.archive) as z:
        names=z.namelist(); assert len(names)==len(set(names))
        manifest=z.read('SHA256SUMS').decode(); hashes={}
        for line in manifest.splitlines():
            value,name=line.split('  ',1)
            assert re.fullmatch('[a-f0-9]{64}',value)
            path=PurePosixPath(name); assert not path.is_absolute() and '..' not in path.parts and name not in hashes
            hashes[name]=value
        assert set(names)==set(hashes)|{'SHA256SUMS'}
        for name in names:
            assert not stat.S_ISLNK(z.getinfo(name).external_attr>>16)
            content=z.read(name)
            if name!='SHA256SUMS': assert sha(content)==hashes[name],name
            local=args.data/name
            assert local.is_file() and not local.is_symlink() and local.read_bytes()==content,name
    read=lambda name:json.loads((args.data/name).read_text())
    env=read('environment.json'); status=read('status.json')
    assert env['controller_commit']==cfg['commit']
    assert env['controller_sha256']==cfg['controller_sha256']==hashes[cfg['controller']]
    for key,value in {'GITHUB_RUN_ID':str(cfg['run']),'GITHUB_RUN_ATTEMPT':'1','GITHUB_SHA':cfg['commit'],'GITHUB_REF':cfg['ref'],'RUNNER_OS':'Linux','RUNNER_ARCH':'X64'}.items(): assert env['github'][key]==value,(key,env['github'].get(key))
    assert env['source_artifact']['id']==13594355530
    assert env['source_artifact']['zip_sha256']=='6f31738c00b39e519e84b5aa9ef076509423a4c64965ee581349f54734b8bd48'
    assert hashes['source-artifact/controller.py']=='d82706e96fc4c1d213870d33e6bc83204ec8d6597da128795554c3226b31850e'
    assert hashes['source-artifact/layout_metadata.rs']=='c85a383fa627537380b6d94c9c0fb60d402f3b0bdd4beb9e48215d3afa8e49e6'
    assert hashes['source-artifact/indexing_throughput.rs']=='58b15ac4a0736284bae336b48f6d334bc1de8bcfd3ac52689ab0417ec264ebca'
    for label in 'BH': assert hashes[f'binaries/{label}-indexing-throughput']==BIN[label]==env['binary_sha256'][label]
    assert env['runtime_environment']=={'CARGO_BUILD_JOBS':'1','OTZARIA_BENCH_LINES':'12000','OTZARIA_BENCH_ECONOMY':'1'}
    assert env['unset_runtime_environment']==['OTZARIA_BENCH_NOTES']
    affinities=env['measurement_affinities']; topology=env['topology']; allowed=set(env['allowed_affinity'])
    assert set(affinities)=={S,D}
    assert all(len(v)==len(set(v))==2 and set(v)<=allowed for v in affinities.values())
    assert affinities[S][0]==affinities[D][0]
    a,b=affinities[S]; anchor,c=affinities[D]; assert a==anchor and len({a,b,c})==3
    identity=lambda cpu:(topology[str(cpu)]['physical_package_id'],topology[str(cpu)]['core_id'])
    siblings=lambda cpu:cpu_list(topology[str(cpu)]['thread_siblings_list'])
    assert identity(a)==identity(b) and b in siblings(a) and a in siblings(b)
    assert identity(a)[0]==identity(c)[0] and identity(a)[1]!=identity(c)[1]
    assert c not in siblings(a) and a not in siblings(c)
    binary_hashes=env['binary_sha256']
    if args.kind=='topology': assert env['rust_builds']==status['rust_builds']==0
    else:
        assert status['candidate_build_invocations']==env['candidate_build_invocations']==1
        identity_expected={'commit':REV['C'],'parent':REV['H'],'tree':'b5bd824ac930f64a21cd5b3365bd69e9fd1f9c17','path':'rust/src/api/search_engine.rs','source_git_blob':'7566ac88fbb32f176e4f6ef18d0b64b903136d97','source_sha256':'94d670184b239bdc68b7b00290eff4c731ac244283bb50c10605b70fb87d4b99'}
        assert env['candidate']==identity_expected
        source=read('candidate-source.json'); assert source['candidate']==identity_expected
        assert source['changed_paths_from_H']==[identity_expected['path']]
        assert source['commit']==REV['C'] and source['tree']==identity_expected['tree'] and source['tracked_source_clean']
        assert hashes['candidate-search_engine.rs']==identity_expected['source_sha256']
        assert env['rustc']==env['original_environment']['rustc'] and env['cargo']==env['original_environment']['cargo']
        build=read('build-logs/C/provenance.json'); assert build==env['C']
        assert build['commit']==REV['C'] and build['tree']==identity_expected['tree']
        assert build['files_sha256']==env['original_environment']['H']['files_sha256']
        assert build['layout']==env['original_environment']['H']['layout']
        assert hashes['binaries/C-indexing-throughput']==binary_hashes['C']==build['binary_sha256']
        command=read('build-logs/C/command.json')
        assert command[:3]==['cargo','test','--manifest-path'] and command[4:]==['--locked','--release','--test','indexing_throughput','--example','__otzaria_economy_layout','--no-run','--message-format=json']
        assert read('build-logs/C/dependencies-before.json')==read('build-logs/C/dependencies-with-layout-target.json')
        assert not (args.data/'build-logs/B').exists() and not (args.data/'build-logs/H').exists()
    planned=independent_schedule(args.kind,affinities,binary_hashes)
    assert len(planned)==cfg['count']
    predeclared=read('predeclared-schedule.json')
    if args.kind=='topology': assert predeclared==planned
    else:
        assert predeclared==independent_schedule(args.kind,affinities,BIN)
        assert read('resolved-schedule.json')==planned
    records=[json.loads(line) for line in (args.data/'samples.jsonl').read_text().splitlines()]
    assert len(records)==status['completed_processes']
    assert status['state']=='complete' and len(records)==cfg['count'],status
    assert all(r['sample']['stamps']==env['expected_stamps'] for r in records)
    for r,expected in zip(records,planned):
        entry={k:v for k,v in r.items() if k not in ('sample','process')}; assert entry==expected
        stem=f"{r['run']:02d}-{r['block']}-{r['position']}"
        context=read('runs/'+stem+'.context.json'); assert r['process']==context and context['schedule']==entry
        cmd=context['command']; assert cmd[:3]==['taskset','-c',','.join(map(str,r['affinity']))]
        assert PurePosixPath(cmd[3]).name==r['actual_binary']+'-indexing-throughput'
        assert cmd[4:]==['measure_search_during_indexing','--ignored','--exact','--nocapture','--test-threads=1'] and context['returncode']==0
        log=(args.data/'runs'/f'{stem}.stdout.log').read_text()
        markers=[line.partition('OTZARIA_RESPONSIVENESS ')[2] for line in log.splitlines() if 'OTZARIA_RESPONSIVENESS ' in line]
        assert len(markers)==1
        raw=json.loads(markers[0]); sample=r['sample']
        extra={key:int(re.search(r'\b'+key+r': (\d+)',raw['diagnostics']).group(1)) for key in ['active_preparations','peak_active_preparations','reserved_bytes','peak_reserved_bytes']}
        assert raw|extra==sample
        assert sample['economy'] is True and sample['notes'] is False and sample['cpus']==2 and sample['indexed_docs']==96000
        assert isinstance(sample['search_samples'],int) and sample['search_samples']>0
        for key in ['search_during_p50_ms','search_during_p95_ms','search_during_p99_ms','search_idle_p95_ms','add_ms','total_ms','docs_per_sec']:
            assert math.isfinite(sample[key]) and sample[key]>0
        assert sample['search_during_p50_ms']<=sample['search_during_p95_ms']<=sample['search_during_p99_ms']
        assert sample['total_ms']>=sample['add_ms']
        assert math.isclose(sample['docs_per_sec']*sample['total_ms']/1000,96000,rel_tol=1e-9)
        for key in ['process_wall_seconds','whole_process_cpu_user_seconds','whole_process_cpu_system_seconds']: assert context[key]>=0
        assert context['after']['monotonic_ns']>context['before']['monotonic_ns']
    labels='BH' if args.kind=='topology' else 'BHC'
    contrasts=[('H','B')] if args.kind=='topology' else [('H','B'),('C','H'),('C','B')]
    result={'kind':args.kind,'run_id':cfg['run'],'controller_commit':cfg['commit'],'archive_sha256':archive_sha,'archive_files':len(names),'manifest_hashes_verified':len(hashes),'status':status,'completed_processes':len(records),'raw_record_context_stdout_agreement':True,'binary_sha256':binary_hashes,'affinities':affinities,'selected_cpu_topology':{str(cpu):topology[str(cpu)] for cpu in sorted({a,b,c})},'by_topology':{},'qa_scope':'read-only archive/source/raw-data inspection; no builds or fixture executions'}
    for case in [S,D]:
        selected=[r for r in records if r['topology_case']==case]
        real=[r for r in selected if r['kind']=='real']; controls=[r for r in selected if r['kind']=='control']
        summaries={label:summarize([r for r in real if r['actual_binary']==label]) for label in labels}
        blocks=[]
        for block in dict.fromkeys(r['block'] for r in selected):
            group=[r for r in selected if r['block']==block]
            grouped={label:summarize([r for r in group if r['pseudo_label']==label]) for label in labels}
            values={f'{num}_over_{den}':{key:ratio(grouped[num]['fixture_medians'][key],grouped[den]['fixture_medians'][key]) for key in METRICS} for num,den in contrasts}
            blocks.append({'block':block,'kind':group[0]['kind'],'order':group[0]['order'],'topology_precedence':group[0]['topology_precedence'],'actual_binaries':sorted({r['actual_binary'] for r in group}),'pseudo_label_summaries':grouped,'ratios':values})
        result['by_topology'][case]={'real':summaries,'real_ratios':{f'{num}_over_{den}':{key:ratio(summaries[num]['fixture_medians'][key],summaries[den]['fixture_medians'][key]) for key in METRICS} for num,den in contrasts},'blocks':blocks,'control_processes':len(controls),'control_by_pseudo_label':{label:summarize([r for r in controls if r['pseudo_label']==label]) for label in labels}}
    result['distinct_over_sibling_ratio_of_ratios']={f'{num}_over_{den}':{key:ratio(result['by_topology'][D]['real_ratios'][f'{num}_over_{den}'][key],result['by_topology'][S]['real_ratios'][f'{num}_over_{den}'][key]) for key in METRICS} for num,den in contrasts}
    args.out.parent.mkdir(parents=True,exist_ok=True); args.out.write_text(json.dumps(result,indent=2,sort_keys=True)+'\n')
    compact={'kind':args.kind,'records':len(records),'manifest_hashes_verified':len(hashes),'qa_json':str(args.out),'qa_sha256':sha(args.out.read_bytes()),'affinities':affinities,'p95':{case:{label:result['by_topology'][case]['real'][label]['fixture_medians']['search_during_p95_ms'] for label in labels} for case in [S,D]}}
    print(json.dumps(compact,indent=2))

if __name__=='__main__': main()

#!/usr/bin/env python3
"""Owned HTTP history-gap/full-load versus bounded snapshot reconciliation A/B/A.

Only fresh /tmp output and a closed /tmp source snapshot are accepted. Each run
uses an APFS clone and its own loopback service/token. The source never opens
writable; App/live index/watch/old resource sampler are never accessed.
Force >16384 committed revisions on one probe, then a same-snapshot two-ID
rename/reuse plus monotonic insert. Measure the real /search refresh separately
from setup, SQL writes, oracle comparisons, status requests and shutdown.
No cache-cold or unique allocator attribution claim. Full six-field ordered
code300 equality uses baseline full reload as oracle and SQL probes are checked.
"""
import argparse
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import secrets
import sqlite3
import sys
import time
from urllib.parse import urlencode

import check_service_round2 as shared

EXT = 'round10refresh'
ROOT = '/__MacEvery_Round10_Refresh__'
RESULT_FIELDS = {'path','basename','kind','size','mtime','score'}


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, separators=(',',':'), sort_keys=True).encode()).hexdigest()


def measure(sampler, label, operation):
    sampler.phase = label
    before = sampler.capture();started = time.monotonic()
    result = operation()
    after = sampler.capture();wall = time.monotonic()-started
    samples = [s for s in sampler.samples if s['phase']==label and 'cpu_ns' in s]
    shared.require('cpu_ns' in before and 'cpu_ns' in after, 'Native CPU/VM capture failed')
    sampler.phase = 'idle'
    return result, {'wall_seconds':wall,'CPU_seconds':(after['cpu_ns']-before['cpu_ns'])/1e9,
        'RSS_before_bytes':before['rss_bytes'],'RSS_after_bytes':after['rss_bytes'],
        'footprint_before_bytes':before['physical_footprint_bytes'],'footprint_after_bytes':after['physical_footprint_bytes'],
        'RSS_sampled_peak_bytes':max(s['rss_bytes'] for s in samples),
        'footprint_sampled_peak_bytes':max(s['physical_footprint_bytes'] for s in samples),
        'samples':len(samples)}


def prepare(database):
    with closing(sqlite3.connect(database)) as db:
        shared.require(db.execute('SELECT 1 FROM files WHERE ext_lower=? LIMIT 1',(EXT,)).fetchone() is None, 'Probe collision')
        for number,name in enumerate(['code-alpha.round10refresh','code-beta.round10refresh']):
            path=ROOT+'/'+name
            db.execute('INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
                       (path,path.lower(),name,name.lower(),EXT,'file',number+1,100,90,1,number+1,1700000000000000000))
        db.commit();db.execute('PRAGMA wal_checkpoint(TRUNCATE)')


def gap(database):
    with closing(sqlite3.connect(database)) as db:
        db.execute('BEGIN IMMEDIATE')
        ids=db.execute('SELECT id,path FROM files WHERE ext_lower=? ORDER BY id',(EXT,)).fetchall()
        shared.require(len(ids)==2, 'Expected two initial probes')
        for _ in range(20000):db.execute('UPDATE files SET size=size+1 WHERE id=?',(ids[0][0],))
        db.execute('DELETE FROM files WHERE id=?',(ids[1][0],))
        name='code-reused.round10refresh';path=ROOT+'/'+name
        db.execute('INSERT INTO files(id,path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)',
            (ids[1][0],path,path.lower(),name,name.lower(),EXT,'file',55,200,190,1,99,1700000000000000001))
        name='code-new.round10refresh';path=ROOT+'/'+name
        db.execute('INSERT INTO files(path,path_lower,basename,basename_lower,ext_lower,kind,size,mtime,ctime,dev,inode,indexed_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)',
            (path,path.lower(),name,name.lower(),EXT,'file',77,300,290,1,100,1700000000000000002))
        db.commit()
        rev,floor=db.execute('SELECT revision,journal_floor FROM macevery_content_state WHERE singleton=1').fetchone()
    return {'revision':rev,'journal_floor':floor,'forced_revision_operations':20003}


def valid_rows(response):
    value=shared.response_json(response)
    shared.require(isinstance(value,list) and len(value)<=300, 'Invalid result count')
    shared.require(all(isinstance(row,dict) and set(row)==RESULT_FIELDS and type(row['score']) is int for row in value), 'Six-field result contract changed')
    return value


def query_gate(response, expected_count=None):
    # Socket per-I/O timeout is not an end-to-end successful-query budget.
    shared.require(response.get('wall_seconds',float('inf'))<=15, 'Completed query exceeded actual15second budget')
    value=valid_rows(response)
    if expected_count is not None:shared.require(len(value)==expected_count,'Complete query recall count mismatch')
    return value


def one(binary, mother, folder, args, metrics):
    folder.mkdir();database=folder/'index.sqlite'
    copied=shared.clone_or_backup(mother,database,args.startup_timeout)
    shared.require(copied['method'].startswith('APFS_clonefile'),'Same-layout APFS clone required')
    shared.write_json(folder/'copy.local.json',copied)
    port,token=shared.free_port(),secrets.token_hex(24)
    service=shared.OwnedService([str(binary),'serve','--addr',f'127.0.0.1:{port}','--backend','memory'],
        {**os.environ,'MACEVERY_DB':str(database),'MACEVERY_AUTH_TOKEN':token,
         'MACEVERY_EXIT_WITH_PARENT':'1','MACEVERY_MAX_ACTIVE_SEARCHES':'1'},folder)
    sampler=None
    result={'status':'FAIL','phases':{}}
    try:
        result['binary']=shared.identity(binary,True)
        sampler=shared.Sampler(service.process.pid,args.sample_interval,metrics)
        deadline=time.monotonic()+args.startup_timeout
        while True:
            shared.require(service.reap() is None,'Owned service exited during startup')
            status=shared.request(port,'/status',token,min(2,max(.01,deadline-time.monotonic())))
            if status.get('status')==200 and status.get('json',{}).get('ready'):break
            shared.require(time.monotonic()<deadline,'Owned service startup timed out')
            time.sleep(.05)
        before=shared.response_json(status)
        # Equal resident-query history before the refresh; not a cold-cache run.
        warm=shared.request(port,'/search?'+urlencode({'q':'code','limit':300}),token,args.query_timeout)
        shared.write_json(folder/'warm-code.response.local.json',warm)
        query_gate(warm,300)
        changed=gap(database)
        shared.require(changed['journal_floor']>before['content_revision'],'Fixture did not cross retained history')
        shared.require(changed['revision']-before['content_revision']==20003,'Forced revision delta mismatch')
        result['changed']=changed;result['status_before']=before
        response,phase=measure(sampler,'history_gap_refresh_code',lambda:shared.request(port,
            '/search?'+urlencode({'q':'code','limit':300}),token,args.query_timeout))
        result['phases']['history_gap_refresh_code']=phase
        shared.write_json(folder/'code.response.local.json',response)
        rows=query_gate(response,300)
        result['code_count']=len(rows);result['code_ordered_sha256']=digest(rows)
        probe_response=shared.request(port,'/search?'+urlencode({'q':'ext:'+EXT,'limit':300}),token,args.query_timeout)
        shared.write_json(folder/'probes.response.local.json',probe_response)
        probes=query_gate(probe_response,3)
        with closing(shared.readonly(database)) as db:
            expected={r[0]:r[1:] for r in db.execute('SELECT path,basename,kind,size,mtime FROM files WHERE ext_lower=?',(EXT,))}
        shared.require(len(probes)==len(expected)==3,'Probe recall mismatch')
        shared.require({row['path'] for row in probes}==set(expected),'Probe SQL path set mismatch')
        for row in probes:shared.require(tuple(row[k] for k in ['basename','kind','size','mtime'])==expected[row['path']],'Probe metadata mismatch')
        result['probe_ordered_sha256']=digest(probes)
        result['status_after']=shared.response_json(shared.request(port,'/status',token,args.query_timeout))
        shared.require(result['status_after']['content_revision']==changed['revision'] and not result['status_after']['stale'],'Published stale/partial target')
        for number in range(3):
            response,phase=measure(sampler,f'steady_code_{number}',lambda:shared.request(port,
                '/search?'+urlencode({'q':'code','limit':300}),token,args.query_timeout))
            result['phases'][f'steady_code_{number}']=phase
            shared.write_json(folder/f'steady-code-{number}.response.local.json',response)
            shared.require(query_gate(response,300)==rows,'Repeated code results changed')
        sampler.phase='idle_tail';time.sleep(1);result['idle_tail_endpoint']=sampler.capture()
        result['status']='PASS'
    except BaseException as error:
        result['failure']={'type':type(error).__name__,'message':str(error)}
        raise
    finally:
        try:
            if sampler is not None:sampler.close()
        finally:
            try:
                result['cleanup']=service.close(port)
            finally:
                shared.write_json(folder/'samples.local.json',sampler.samples if sampler is not None else [])
                shared.write_json(folder/'result.local.json',result)
        shared.require(result['cleanup']['process_group_gone'] and result['cleanup']['port_closed'],'Owned cleanup failed; do not continue')
    return result


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['baseline','candidate','snapshot','output']:p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--blocks',type=int,default=2)
    p.add_argument('--query-timeout',type=float,default=15)
    p.add_argument('--startup-timeout',type=float,default=180)
    p.add_argument('--sample-interval',type=float,default=.02)
    args=p.parse_args()
    shared.require(sys.platform=='darwin' and 1<=args.blocks<=3 and 0<args.query_timeout<=15
        and .005<=args.sample_interval<=.1 and 5<=args.startup_timeout<=300,'Measurement bounds exceeded')
    source=shared.require_tmp(args.snapshot.resolve(strict=True));output=shared.require_tmp(args.output)
    shared.require(not output.exists() and not shared.nonempty_wal(source),'Fresh output/closed source required')
    output.mkdir(mode=0o700,parents=True)
    report={'status':'FAIL','runs':[],'controller_sha256':shared.sha256(Path(__file__)),
        'source':shared.db_identity(source),'source_sha256':shared.sha256(source),
        'notes':['Only owned loopback services/clones; no App/watch/live DB.',
            'Baseline full reload is ordered six-field oracle; SQL verifies all probe metadata.',
            '20003 revision operations affect only three final IDs; history_gap is forced after load.',
            'Refresh-query includes HTTP/encoding; SQL/oracle/status/idle/cleanup outside timed phase.',
            'A/B/A warm same input, not cache-cold or full physical-write audit. FP/RSS discrete peaks can miss short peaks.']}
    try:
        mother=output/'mother.sqlite';shared.clone_or_backup(source,mother,args.startup_timeout);prepare(mother)
        mother_sha=shared.sha256(mother);metrics=shared.ProcessMetrics();oracle=None
        for block in range(args.blocks):
            for position,variant in enumerate(['baseline','candidate','baseline']):
                run=one(getattr(args,variant).resolve(strict=True),mother,output/f'b{block}-{position}-{variant}',args,metrics)
                run.update(block=block,position=position,variant=variant)
                report['runs'].append(run)
                value=(run['code_count'],run['code_ordered_sha256'],run['probe_ordered_sha256'])
                if oracle is None:oracle=value
                shared.require(value==oracle,'Full ordered code300/probe differs from baseline full load')
                before,after=run['status_before'],run['status_after']
                if variant=='candidate':shared.require(after['reload_count']==before['reload_count'],'Candidate still full-loaded after bounded history gap')
                else:shared.require(after['reload_count']==before['reload_count']+1,'Baseline did not full-load')
                shared.write_json(output/'summary.local.json',report)
                print(f'b{block}-{position}-{variant}: PASS',flush=True)
        shared.require(shared.sha256(mother)==mother_sha,'Mother changed')
        shared.require(shared.db_identity(source)==report['source'] and shared.sha256(source)==report['source_sha256'],'Frozen source changed')
        report.update(status='PASS',all_owned_cleaned=True,source_unchanged=True,full_ordered_code_and_probes_equal=True)
    except BaseException as error:
        report['failure']={'type':type(error).__name__,'message':str(error)}
        raise
    finally:shared.write_json(output/'summary.local.json',report)
    return 0


if __name__=='__main__':
    raise SystemExit(main())

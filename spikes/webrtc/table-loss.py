#!/usr/bin/env python3
"""Tabulates the runs of batch-loss.sh: results/<browser>-loss<p>-<config>.json."""
import json, re, sys
from results_io import find, load as load_report

def load():
    rows = []
    for path in find('results/*-loss[0-9][0-9]-*.json'):
        m = re.match(r'results/(\w+)-loss(\d\d)-(.+)\.json', path)
        if not m or 'FAILED' in path: continue
        r = load_report(path)
        rows.append((m.group(1), int(m.group(2)), m.group(3), r))
    return rows

def late_sends(r):
    """How often the endpoint itself sent a frame more than 45 ms after the one before: the machine was busy."""
    log = r['server']['tx']['log']
    return sum(1 for a, b in zip(log, log[1:]) if b[2] - a[2] > 45)

def disturbed(r):
    rx = r['server']['rx']
    return late_sends(r) > 8 or rx.get('frames_the_browser_never_sent', 0) > 0.01 * (rx['frames_in_span'] or 1)

order = ['plain', 'fec', 'red', 'red12', 'red-fec', 'nack', 'red-nack']
rows = sorted(load(), key=lambda x: (x[0], x[1], order.index(x[2]) if x[2] in order else 99))

print('## Uplink: browser to endpoint (the endpoint drops, then counts what it could still have)\n')
print('"Dropped": frames whose packet the endpoint threw away. "Deep FEC": the LBRR inside a redundant block, which is the frame two back. "Skipped by the browser": holes in its RTP times that are not our drops. A run is marked "disturbed" when the browser skipped more than 1% of its frames or the endpoint itself sent more than 8 frames over 45 ms after the one before: the machine was busy with something else and the run says little.\n')
print('| Browser | Loss set | Run | Frames | Skipped by the browser | Dropped | Had by RED | by FEC (LBRR of the next packet) | by either | by either or deep FEC | Left | Frames with LBRR | Resent after NACK | NACK wait p50 / p90 ms | Left with resends alone within 60 / 100 / 200 ms / ever | Left with everything and a resend within 60 / 100 / 200 ms | Mean payload bytes |')
print('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|')
pc = lambda a, n: f"{a} ({100*a/n:.2f}%)"
for b, p, cfg, r in rows:
    if cfg.startswith('out-'): continue
    rx = r['server']['rx']
    n = rx['frames_in_span'] or 1
    miss = rx['frames_missing_when_due']
    w = rx['resend_wait_ms']
    deep = rx.get('recoverable_by_red_or_fec_or_the_lbrr_inside_a_redundant_block')
    left = rx.get('left_after_all_three')
    lr = rx.get('left_with_resends_alone_within_ms') or {}
    la = rx.get('left_after_all_three_and_a_resend_within_ms') or {}
    nack = rx['resent_and_arrived'] > 0
    gaps = rx.get('frames_the_browser_never_sent', 0)
    mark = ' (disturbed)' if disturbed(r) else ''
    print(f"| {b} | {p}% | {cfg}{mark} | {n} | {gaps} | {pc(miss, n)} | {rx['recoverable_by_red']} | {rx['recoverable_by_fec']} | {rx['recoverable_by_red_or_fec']} | {deep} | {pc(left, n) if left is not None else '-'} | {100*rx['frames_with_lbrr']/n:.0f}% | {rx['resent_and_arrived']} of {rx['dropped_on_purpose']} | {w['p50']:.0f} / {w['p90']:.0f} | {' / '.join(pc(lr[k], n) for k in ['60','100','200','any']) if nack and lr else '-'} | {' / '.join(pc(la[k], n) for k in ['60','100','200']) if nack and la else '-'} | {rx['payload_bytes']['mean']:.0f} |")

print('\n## Downlink: endpoint to browser (the endpoint drops, the browser counts what it had to make up)\n')
print('| Browser | Loss set | Run | Late sends by the endpoint | Packets sent | Dropped | Browser: lost | Concealed samples | of all samples | FEC packets used | Packets discarded | Mean jitter-buffer delay ms | Mean payload bytes sent |')
print('|---|---|---|---|---|---|---|---|---|---|---|---|---|')
for b, p, cfg, r in rows:
    if 'nack' in cfg or cfg.startswith('out-'): continue
    tx = r['server']['tx']
    st = [x for x in r['stats'] if x.get('inbound')]
    if not st: continue
    i = st[-1]['inbound']
    tot = i.get('totalSamplesReceived') or 1
    jb = 1000 * (i.get('jitterBufferDelay') or 0) / max(1, i.get('jitterBufferEmittedCount') or 1)
    gaps = r['server']['rx'].get('frames_the_browser_never_sent', 0)
    cfg = cfg + (' (disturbed)' if disturbed(r) else '')
    print(f"| {b} | {p}% | {cfg} | {late_sends(r)} | {tx['packets']} | {tx['dropped_on_purpose']} | {i.get('packetsLost')} | {i.get('concealedSamples')} | {100*(i.get('concealedSamples') or 0)/tot:.2f}% | {i.get('fecPacketsReceived')} (discarded {i.get('fecPacketsDiscarded')}) | {i.get('packetsDiscarded')} | {jb:.0f} | {tx['payload_bytes_mean']:.0f} |")

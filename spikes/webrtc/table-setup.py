#!/usr/bin/env python3
"""Tabulates the set-up runs of batch-misc.sh: when each side could first do what,
in ms after the page set the answer (both clocks are the machine's wall clock)."""
import json, sys
from results_io import load

runs = ['basic-1', 'basic-2', 'basic-3', 'dtls13', 'dtls-auto', 'dc-inband', 'dc-negotiated', 'tcp-only-candidate', 'tcp-udp-ignored',
        'rtt100-dc-inband', 'rtt100-dc-negotiated', 'rtt100-dtls13-dc-negotiated', 'rtt100-dtls-auto-dc-negotiated', 'rtt100-tcp']
print('| Browser | Run | DTLS | SRTP profile | First check in | ClientHello in | Our first flight out | Endpoint connected | Page connected | Endpoint: channel open | Page: channel open | Page: first message of ours | Endpoint: first message of the page | CPU of the handshake µs | Costliest datagram µs | DTLS datagrams in / out until connected |')
print('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|')
for b in ['chrome', 'firefox']:
    for name in runs:
        try: r = load(f'results/{b}-{name}.json')
        except OSError: continue
        s = r['server']; m = r['marks']; t0 = m['answer_set']; c = s['created_wall_ms']
        page = lambda k: f"{m[k] - t0:.0f}" if k in m else '-'
        srv = lambda ms: f"{c + ms - t0:.0f}" if ms is not None else '-'
        tr = s['trace']
        first = lambda pred: next((t[0] for t in tr if pred(t)), None)
        ev = lambda what: next((e['t_ms'] for e in s['events'] if e['what'] == what), None)
        conn = s.get('connected_after_ms')
        dtls_in = sum(1 for t in tr if t[2] == 'dtls' and t[1] == 'in' and (conn is None or t[0] <= conn + 0.5))
        dtls_out = sum(1 for t in tr if t[2] == 'dtls' and t[1] == 'out' and (conn is None or t[0] <= conn + 0.5))
        tp = r['stats'][-1].get('transport') or {}
        cost = (s['cost']['packet_in_whole'].get('dtls') or {})
        print(f"| {b} | {name} | {s.get('dtls')} | {tp.get('srtpCipher')} | {srv(first(lambda t: t[2] == 'stun'))} | {srv(first(lambda t: t[2] == 'dtls' and t[1] == 'in'))} | {srv(first(lambda t: t[2] == 'dtls' and t[1] == 'out'))} | {srv(conn)} | {page('connected')} | {srv(ev('channel-open'))} | {page('dc_open')} | {page('dc_first_message')} | {srv(ev('channel-data'))} | {s['handshake_cpu_us']:.0f} | {cost.get('max_us', 0):.0f} | {dtls_in} / {dtls_out} |")

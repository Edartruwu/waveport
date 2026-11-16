#!/usr/bin/env python3
"""Reads reports in results/ and says what each run showed.

  ./analyze.py results/chrome-basic-1.json [more.json ...]   (packed reports are found under the same name)

Playback: the endpoint logs when it sent each 20 ms frame and with which RTP
time; the page logs, every 10 ms, which 100 ms symbol its audio graph was
rendering (`heard`) and what the receiver's synchronisation source said
(`ssrc`), and every 100 ms the receiver's statistics.
"""
import json, sys
from results_io import load

def pct(v, p):
    v = sorted(v)
    return v[min(len(v) - 1, int((len(v) - 1) * p))] if v else None

def summary(v, unit='ms'):
    if not v: return 'none'
    return f"n={len(v)} p5={pct(v,.05):.1f} p50={pct(v,.5):.1f} p95={pct(v,.95):.1f} max={max(v):.1f} {unit}"

def playback(r):
    s = r.get('server') or {}
    log = (s.get('tx') or {}).get('log') or []
    if not log: return {}
    ts0 = s['tx']['ts0']
    sent = {ts: wall for _seq, ts, wall, dropped in log if not dropped}  # rtp ts -> wall sent
    first_ts = log[0][1]
    out = {}
    # 1. The synchronisation source: at page time w it says rtpTimestamp R was played.
    #    Lag = w - (when the endpoint sent R).
    lag = []
    for row in r['ssrc']:
        if len(row) < 2 or row[1] is None: continue
        w, R = row[0], row[1]
        if R in sent: lag.append(w - sent[R])
    out['ssrc_lag_ms'] = lag
    # 2. The audio graph: symbol k is the 100 ms starting at sample k*4800 of the
    #    endpoint's signal, i.e. RTP time ts0 + k*4800 (mod 256 symbols).
    #    For each first block of a new symbol heard at page time h, compare with
    #    when the endpoint sent the frame that starts that symbol.
    heard = [(h[0], h[1]) for h in r['heard'] if isinstance(h[1], int)]
    edges = []
    prev = None
    for at, sym in heard:
        if sym >= 0 and prev is not None and prev >= 0 and sym == (prev + 1) % 256:
            edges.append((at, sym))
        prev = sym
    graph_lag = []
    cycle = 256 * 4800
    for at, sym in edges:
        # the candidate RTP time of this symbol's start nearest to what was being sent at `at`
        base = ts0 + sym * 4800
        best = None
        for n in range(0, 40):
            ts = (base + n * cycle) & 0xffffffff
            if ts in sent and sent[ts] <= at + 5:
                best = ts
        if best is not None: graph_lag.append((at, at - sent[best], best))
    out['graph_lag_ms'] = [g[1] for g in graph_lag]
    # 3. How well the source tracks the graph: at each symbol edge heard at page
    #    time h (RTP time T rendered), what RTP time did the source report then?
    ssrc = [(row[0], row[1]) for row in r['ssrc'] if len(row) > 1 and row[1] is not None]
    err = []
    j = 0
    for at, _lag, T in graph_lag:
        while j + 1 < len(ssrc) and ssrc[j + 1][0] <= at: j += 1
        if ssrc and ssrc[j][0] <= at and at - ssrc[j][0] < 30:
            err.append((ssrc[j][1] - T) / 48.0)  # ms the source is ahead of what is rendered
    out['ssrc_ahead_of_graph_ms'] = err
    # 4. Counters: emitted samples against what was really sent in the same time.
    stats = [x for x in r['stats'] if x.get('inbound')]
    if len(stats) > 3:
        a, b = stats[2], stats[-1]
        d = lambda k: (b['inbound'].get(k, 0) or 0) - (a['inbound'].get(k, 0) or 0)
        wall = (b['at'] - a['at'])
        out['counters'] = {
            'window_ms': round(wall, 1),
            'emitted_ms': round(d('jitterBufferEmittedCount') / 48.0, 1),
            'received_ms': round(d('totalSamplesReceived') / 48.0, 1),
            'concealed_ms': round(d('concealedSamples') / 48.0, 1),
            'inserted_ms': round(d('insertedSamplesForDeceleration') / 48.0, 1),
            'removed_ms': round(d('removedSamplesForAcceleration') / 48.0, 1),
            'packets': d('packetsReceived'),
            'mean_jitter_buffer_delay_ms': round(1000 * d('jitterBufferDelay') / max(1, d('jitterBufferEmittedCount')), 1),
            'fields_present': sorted(b['inbound'].keys()),
        }
    # 5. After the endpoint stops: how long the page goes on hearing the signal.
    stops = [x for x in (s['tx'].get('stops') or []) if x.get('cmd') == 'stop']
    if stops:
        t = stops[0]['last_sent_wall_ms']
        last_ts = stops[0]['last_ts']
        after = [h for h in r['heard'] if isinstance(h[1], int) and h[0] > t]
        tail = [h[0] - t for h in after if h[1] >= 0 and h[0] - t < 1400]
        energetic = [h[0] - t for h in after if isinstance(h[2], (int, float)) and h[2] > 1e-5 and h[0] - t < 1400]
        ss = [(row[0] - t, row[1]) for row in r['ssrc'] if len(row) > 1 and row[1] is not None and 0 < row[0] - t < 1400]
        reached = next((w for w, R in ss if R >= last_ts), None)
        out['stop'] = {
            'signal_heard_for_ms_after_last_packet_sent': round(max(tail), 1) if tail else 0,
            'sound_heard_for_ms_after_last_packet_sent': round(max(energetic), 1) if energetic else 0,
            'source_reached_last_rtp_time_after_ms': None if reached is None else round(reached, 1),
            'source_final_minus_last_sent_ms': round((ss[-1][1] - last_ts) / 48.0, 1) if ss else None,
        }
    return out

def main():
    for path in sys.argv[1:]:
        r = load(path)
        s = r.get('server') or {}
        m = r['marks']; t0 = m.get('start', 0)
        print(f"\n== {path}  ({r['browser']}, {r['query']})")
        if r.get('error'): print('  ERROR', r['error'])
        print('  marks ms:', {k: round(v - t0, 1) for k, v in sorted(m.items(), key=lambda kv: kv[1])})
        print('  server: connected after', s.get('connected_after_ms'), 'ms;', s.get('dtls'), '; handshake cpu', round(s.get('handshake_cpu_us') or 0), 'us; packets in', s.get('transport_packets_in'))
        log_tx = (s.get('tx') or {}).get('log') or []
        late = sum(1 for a, b in zip(log_tx, log_tx[1:]) if b[2] - a[2] > 45)
        print('  negotiated:', s.get('negotiated'), '; endpoint late sends (>45 ms after the one before):', late, '(DISTURBED: the machine was busy)' if late > 8 else '')
        tr = (r['stats'][-1].get('transport') if r['stats'] else None)
        print('  browser transport:', tr)
        st_last = r['stats'][-1] if r['stats'] else {}
        if st_last.get('pairIds') and st_last.get('cands'):
            print('  pair:', [st_last['cands'].get(i) for i in st_last['pairIds']])
        rx = s.get('rx') or {}
        keys = ['packets', 'padding_only_packets', 'expected_by_seq', 'payload_types', 'red_packets', 'red_shapes', 'opus_modes', 'dropped_on_purpose', 'dropped_again', 'resent_and_arrived', 'resend_wait_ms', 'frames_in_span', 'frames_missing_when_due', 'recoverable_by_red', 'recoverable_by_fec', 'recoverable_by_red_or_fec', 'frames_with_lbrr', 'signal', 'payload_bytes']
        print('  rx:', {k: rx.get(k) for k in keys})
        if rx.get('tele'): print('  tele in:', [(t['event'], t['end'], t['duration'], t['pt']) for t in rx['tele']])
        if (rx.get('frame_mode') or {}).get('frames'): print('  frame mode:', rx['frame_mode'])
        tx = s.get('tx') or {}
        print('  tx:', {k: tx.get(k) for k in ['packets', 'payload_bytes_mean', 'dropped_on_purpose']})
        cost = s.get('cost') or {}
        for k in ['packet_in_whole']:
            for kind, v in (cost.get(k) or {}).items():
                print(f"  cost {k} {kind}: n={v.get('n')} p50={v.get('p50_us')} p99={v.get('p99_us')} max={v.get('max_us')} us")
        v = cost.get('tick_with_a_frame_written') or {}
        print(f"  cost packet out (tick with a frame): n={v.get('n')} p50={v.get('p50_us')} p99={v.get('p99_us')} max={v.get('max_us')} us")
        inbound = st_last.get('inbound') or {}
        print('  browser inbound (end):', {k: inbound.get(k) for k in ['packetsReceived', 'packetsLost', 'concealedSamples', 'concealmentEvents', 'totalSamplesReceived', 'jitterBufferEmittedCount', 'fecPacketsReceived', 'fecPacketsDiscarded', 'nackCount', 'packetsDiscarded', 'retransmittedPacketsReceived'] if k in inbound})
        outb = st_last.get('outbound') or {}
        print('  browser outbound (end):', outb, st_last.get('remoteInbound'))
        if st_last.get('playout'): print('  media-playout:', st_last['playout'])
        p = playback(r)
        if p:
            print('  playback: source lag (page time - time the endpoint sent that RTP time):', summary(p['ssrc_lag_ms']))
            print('  playback: graph lag (page heard a symbol begin - time the endpoint sent it):', summary(p['graph_lag_ms']))
            print('  playback: source ahead of what the graph renders:', summary(p['ssrc_ahead_of_graph_ms']))
            if p.get('counters'):
                c = dict(p['counters']); fp = c.pop('fields_present'); print('  playback counters:', c); print('  inbound fields:', fp)
            if p.get('stop'): print('  after stop:', p['stop'])
        if r.get('notes'): print('  notes:', r['notes'][:12])
        dc = {k: round(m[k] - t0, 1) for k in ('dc_open', 'dc_first_message', 'dc_echo') if k in m}
        if dc: print('  data channel ms:', dc, [e for e in (s.get('events') or []) if e['what'].startswith('channel')][:3])
        if r.get('whip'): print('  whip:', r['whip'])
        states = [(round(x[0] - t0), x[2], x[3]) for x in r['states']]
        print('  states:', states[-6:])

if __name__ == '__main__':
    main()

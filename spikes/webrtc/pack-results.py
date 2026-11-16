#!/usr/bin/env python3
"""Packs the reports in results/: each `<name>.json` becomes a compact `<name>.json.gz`.
A run's report is 0.3 to 2 MB as the endpoint writes it; packed, a twentieth of that."""
import glob, gzip, json, os

before = after = 0
for path in sorted(glob.glob('results/*.json')):
    if os.path.basename(path).startswith(('bench', 'pipecat-http')):
        continue
    with open(path) as f:
        data = json.load(f)
    before += os.path.getsize(path)
    with gzip.open(path + '.gz', 'wt', compresslevel=9) as f:
        json.dump(data, f, separators=(',', ':'))
    after += os.path.getsize(path + '.gz')
    os.remove(path)
print(f'packed {before / 1e6:.1f} MB into {after / 1e6:.1f} MB')

"""Reads a report of a run, whether packed (`.json.gz`, by pack-results.py) or as the endpoint wrote it (`.json`)."""
import glob, gzip, json, os


def load(path):
    if not os.path.exists(path) and os.path.exists(path + '.gz'):
        path += '.gz'
    if path.endswith('.gz'):
        with gzip.open(path, 'rt') as f:
            return json.load(f)
    with open(path) as f:
        return json.load(f)


def find(pattern):
    """Paths matching a `.json` pattern, packed or not, named without the `.gz`."""
    names = set(glob.glob(pattern)) | {p[:-3] for p in glob.glob(pattern + '.gz')}
    return sorted(names)

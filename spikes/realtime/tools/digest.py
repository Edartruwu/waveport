#!/usr/bin/env python3
"""Reads a recording and prints it short: one line for each message, with
runs of the same kind of message folded into one.

    python3 -I tools/digest.py recordings/openai/detector_on.jsonl [--align] [--notes] [--usage]

--align   for each piece of transcript the provider sent with its audio:
          the time, the letters so far and the audio received so far
--notes   only what the runner noted
--usage   add up the tokens of every response in the files given
"""

import json
import sys

RATE = {"openai": 24000, "gemini": 24000}


def kind(event):
    if "type" in event:
        return event["type"]
    keys = [k for k in event if k not in ("usageMetadata",)]
    return "+".join(keys) if keys else "(empty)"


def detail(event):
    out = []
    for key in ("event_id", "item_id", "response_id", "audio_start_ms", "audio_end_ms", "transcript", "text"):
        if key in event and not isinstance(event[key], (dict, list)):
            out.append(f"{key}={event[key]!r}")
    if "delta" in event and isinstance(event["delta"], str):
        out.append(f"delta={event['delta']!r}")
    if "error" in event and event["error"]:
        e = event["error"]
        out.append(f"ERROR type={e.get('type')} code={e.get('code')} param={e.get('param')} for={e.get('event_id')} message={e.get('message')!r}")
    if "item" in event and isinstance(event["item"], dict):
        i = event["item"]
        out.append(f"item={i.get('id')} {i.get('type')}/{i.get('role', i.get('name', ''))} status={i.get('status')}")
    if "response" in event and isinstance(event["response"], dict):
        r = event["response"]
        out.append(f"response={r.get('id')} status={r.get('status')} conversation={r.get('conversation_id')}")
    return " ".join(out)


def lines(path):
    with open(path, encoding="utf-8") as f:
        for raw in f:
            raw = raw.strip()
            if raw:
                yield json.loads(raw)


def digest(path, notes_only):
    folded = None  # [kind, dir, first t, last t, count, bytes]

    def flush():
        nonlocal folded
        if folded:
            k, d, t0, t1, n, b = folded
            print(f"{t0:>7}..{t1:<7} {d:<4} {k} x{n} ({b} bytes of audio)" if b else f"{t0:>7}..{t1:<7} {d:<4} {k} x{n}")
        folded = None

    for line in lines(path):
        t, d, event = line["t"], line["dir"], line["event"]
        if d == "note":
            flush()
            print(f"{t:>7}          note {json.dumps(event, ensure_ascii=False)}")
            continue
        if notes_only:
            continue
        k = kind(event)
        foldable = "delta" in k or k == "input_audio_buffer.append" or k.startswith("realtimeInput") or "modelTurn" in json.dumps(event)[:80]
        size = 0
        for key in ("audio", "delta"):
            if isinstance(event.get(key), dict):
                size = event[key].get("bytes", 0)
        if foldable:
            if folded and folded[0] == k and folded[1] == d:
                folded[3], folded[4], folded[5] = t, folded[4] + 1, folded[5] + size
            else:
                flush()
                folded = [k, d, t, t, 1, size]
            continue
        flush()
        print(f"{t:>7}          {d:<4} {k} {detail(event)}")
    flush()


def align(path):
    provider = "gemini" if "/gemini/" in path else "openai"
    rate = RATE[provider]
    letters = {}
    first_audio_got = {}
    print("time_ms  item  letters_so_far  audio_received_ms  text")
    for line in lines(path):
        event = line["event"]
        if line["dir"] != "in":
            continue
        k = kind(event)
        item = event.get("item_id", "")
        if k == "response.output_audio.delta" and item not in first_audio_got:
            first_audio_got[item] = line["got"] - event["delta"]["bytes"] // 2
        if k == "response.output_audio_transcript.delta":
            letters[item] = letters.get(item, 0) + len(event["delta"])
            base = first_audio_got.get(item, line["got"])
            print(f"{line['t']:>7}  {item[-6:]}  {letters[item]:>5}  {(line['got'] - base) * 1000 // rate:>7}  {event['delta']!r}")
        if k == "response.output_audio.done":
            base = first_audio_got.get(item, line["got"])
            print(f"{line['t']:>7}  {item[-6:]}  audio done: {(line['got'] - base) * 1000 // rate} ms of audio in all")
        if k == "response.output_audio_transcript.done":
            print(f"{line['t']:>7}  {item[-6:]}  transcript done: {event.get('transcript')!r}")


def usage(paths):
    total = {}

    def add(prefix, value):
        if isinstance(value, dict):
            for key, inner in value.items():
                add(f"{prefix}.{key}" if prefix else key, inner)
        elif isinstance(value, (int, float)):
            total[prefix] = total.get(prefix, 0) + value

    responses = 0
    transcriptions = {}
    for path in paths:
        for line in lines(path):
            event = line["event"]
            if line["dir"] != "in":
                continue
            if event.get("type") == "response.done":
                responses += 1
                add("", event["response"].get("usage") or {})
            if event.get("type") == "conversation.item.input_audio_transcription.completed":
                for key, value in (event.get("usage") or {}).items():
                    if isinstance(value, (int, float)):
                        transcriptions[key] = transcriptions.get(key, 0) + value
    print(f"{responses} responses")
    for key in sorted(total):
        print(f"  {key}: {total[key]}")
    print(f"input transcription: {transcriptions}")


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = {a for a in sys.argv[1:] if a.startswith("--")}
    if "--usage" in flags:
        usage(args)
        return
    for path in args:
        print(f"== {path}")
        if "--align" in flags:
            align(path)
        else:
            digest(path, "--notes" in flags)


if __name__ == "__main__":
    main()

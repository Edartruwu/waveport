#!/bin/sh
# Makes the speech this spike sends. A machine speaks it: the free
# synthesiser Flite with its voice `slt`, through ffmpeg, as the speech of
# `waveport-testkit` was made. Nobody was recorded.
#
# Run from `spikes/realtime/`: sh audio/make.sh
set -eu
cd "$(dirname "$0")"
say() {
    name=$1
    text=$2
    # 24 kHz, what OpenAI Realtime takes as `audio/pcm`.
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "flite=text='$text':voice=slt" \
        -ar 24000 -ac 1 -c:a pcm_s16le -bitexact -map_metadata -1 "$name.24k.wav"
    # 16 kHz, what the product listens at and Gemini Live takes.
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "flite=text='$text':voice=slt" \
        -ar 16000 -ac 1 -c:a pcm_s16le -bitexact -map_metadata -1 "$name.16k.wav"
    # G.711 mu-law at 8 kHz, without a header, as a carrier sends it.
    ffmpeg -hide_banner -loglevel error -y -f lavfi -i "flite=text='$text':voice=slt" \
        -ar 8000 -ac 1 -f mulaw "$name.8k.ulaw"
}
say time  "What time is it right now?"
say count "Please count slowly from one to thirty."
say story "I would like to tell you a long story about a small boat that sailed across a very wide and quiet lake."
say sum   "Never mind. What is two plus two?"
say hello "Hello there."

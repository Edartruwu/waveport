#!/usr/bin/env bash
# Everything but the loss runs, one browser at a time so that times mean something.
cd "$(dirname "$0")"
for b in chrome firefox; do
  for i in 1 2 3; do ./run.sh $b "exp=basic-$i&dur=6"; done
  # DTLS versions
  ./run.sh $b 'exp=dtls13&dtls=13&dur=5'
  ./run.sh $b 'exp=dtls-auto&dtls=0&dur=5'
  # Data channel: opened in band, and agreed beforehand
  ./run.sh $b 'exp=dc-inband&dc=inband&dur=5'
  ./run.sh $b 'exp=dc-negotiated&dc=negotiated&dcid=4&dur=5'
  # The same with 50 ms each way, to count round trips
  ./run.sh $b 'exp=rtt100-dc-inband&delay=50&dc=inband&dur=6'
  ./run.sh $b 'exp=rtt100-dc-negotiated&delay=50&dc=negotiated&dcid=4&dur=6'
  ./run.sh $b 'exp=rtt100-dtls13-dc-negotiated&delay=50&dtls=13&dc=negotiated&dcid=4&dur=6'
  ./run.sh $b 'exp=rtt100-dtls-auto-dc-negotiated&delay=50&dtls=0&dc=negotiated&dcid=4&dur=6'
  ./run.sh $b 'exp=rtt100-tcp&delay=50&tcp=1&udp=0&dc=negotiated&dcid=4&dur=6'
  # DTMF both ways
  ./run.sh $b 'exp=dtmf&tele=1&dtmf=159%23&server_dtmf=42%23&dur=9'
  ./run.sh $b 'exp=dtmf-48k-only&tele=48&dtmf=159%23&server_dtmf=42%23&dur=9'
  ./run.sh $b 'exp=dtmf-8k-only&tele=8&dtmf=159%23&server_dtmf=42%23&dur=9'
  ./run.sh $b 'exp=dtmf-8k-only-audio-clock&tele=8&ts48=1&dtmf=159%23&server_dtmf=42%23&dur=9'
  # ICE restart
  ./run.sh $b 'exp=ice-restart&restart_at=3000&dur=8'
  # What is still played after the endpoint stops
  for i in 1 2 3; do ./run.sh $b "exp=stop-$i&stop_at=3000&dur=7"; done
  ./run.sh $b 'exp=stop-red&red=1&stop_at=3000&dur=7'
  # Frame mode against RTP mode
  ./run.sh $b 'exp=frame-mode-clean&frame_mode=1&dur=8'
  ./run.sh $b 'exp=frame-mode-loss10&frame_mode=1&loss_in=0.10&dur=15'
  ./run.sh $b 'exp=frame-mode-loss10-red&frame_mode=1&red=1&loss_in=0.10&dur=15'
  # Signalling in the shape of WHIP
  ./run.sh $b 'exp=whip&whip=1&dur=5'
  # ICE over TCP
  ./run.sh $b 'exp=tcp-only-candidate&tcp=1&udp=0&dur=8'
  ./run.sh $b 'exp=tcp-udp-ignored&tcp=1&drop_udp=1&dur=10'
done
CHROME_FLAGS='--webrtc-ip-handling-policy=disable_non_proxied_udp' ./run.sh chrome 'exp=tcp-chrome-udp-disabled&tcp=1&dur=8'

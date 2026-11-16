#!/usr/bin/env bash
# Loss in both directions at 5, 10 and 20 %, speech both ways, 30 s a run.
# Uplink (browser -> endpoint) is judged by the endpoint's accounting,
# downlink by the browser's own concealment counters.
cd "$(dirname "$0")"
d=${DUR:-30}
# ONLY='chrome-loss(05|10)-red$' runs only what matches; SERIAL=1 runs one loss level at a time.
one() { # browser name query
  if [ -n "${ONLY:-}" ] && ! [[ "$1-$2" =~ $ONLY ]]; then return 0; fi
  ./run.sh "$1" "exp=$2&src=speech&dur=$d&seed=11&$3"
}
[ -n "${NO_ZERO:-}" ] || ( one chrome "loss00-plain" "loss_in=0&loss_out=0"; one chrome "loss00-red" "red=1"; one chrome "loss00-nack" "nack=1&munge_nack=1"; one firefox "loss00-plain" "loss_in=0"; one firefox "loss00-nack" "nack=1&munge_nack=1" )
for p in 05 10 20; do
  l="loss_in=0.$p&loss_out=0.$p"
  f=$((10#$p))
  [ -n "${FIREFOX_ONLY:-}" ] || ( one chrome  "loss$p-plain"        "$l"
    one chrome  "loss$p-fec"          "$l&fec=$f"
    one chrome  "loss$p-red"          "$l&red=1"
    one chrome  "loss$p-red12"        "$l&red=1&red_dist=1,2"
    one chrome  "loss$p-red-fec"      "$l&red=1&fec=$f"
    one chrome  "loss$p-nack"         "$l&nack=1&munge_nack=1"
    one chrome  "loss$p-red-nack"     "$l&red=1&nack=1&munge_nack=1" ) &
  [ -n "${CHROME_ONLY:-}" ] || ( one firefox "loss$p-plain"        "$l"
    one firefox "loss$p-fec"          "$l&fec=$f"
    one firefox "loss$p-nack"         "$l&nack=1&munge_nack=1" ) &
  [ -z "${SERIAL:-}" ] || wait
done
wait

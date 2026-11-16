#!/usr/bin/env bash
# Runs one experiment in a headless browser and waits for its report.
#   ./run.sh chrome 'exp=basic&dur=8'
#   ./run.sh firefox 'exp=red&red=1&dur=10'
#   CHROME_FLAGS='--webrtc-ip-handling-policy=disable_non_proxied_udp' ./run.sh chrome 'exp=tcp&tcp=1'
# The endpoint must be running: cargo run --release -p spike-endpoint -- serve
set -u
browser=$1; query=$2
here=$(cd "$(dirname "$0")" && pwd)
port=${HTTP_PORT:-8787}
page=${PAGE:-test.html}
exp=$(printf '%s' "$query" | tr '&' '\n' | sed -n 's/^exp=//p')
dur=$(printf '%s' "$query" | tr '&' '\n' | sed -n 's/^dur=//p'); dur=${dur:-8}
out="$here/results/$browser-$exp.json"
rm -f "$out" "$here/results/$browser-$exp-FAILED.json"
# The Firefox here is a snap, which cannot read hidden directories or /tmp.
profile="$here/target/profiles/$browser-$$"
rm -rf "$profile"; mkdir -p "$profile"
url="http://127.0.0.1:$port/$page?$query"
case $browser in
  chrome)
    google-chrome --headless=new --no-first-run --no-default-browser-check --disable-gpu \
      --user-data-dir="$profile" --mute-audio --autoplay-policy=no-user-gesture-required \
      --use-fake-device-for-media-stream --use-fake-ui-for-media-stream \
      --disable-background-timer-throttling --disable-renderer-backgrounding \
      --disable-backgrounding-occluded-windows ${CHROME_FLAGS:-} "$url" >"$profile/log.txt" 2>&1 &
    ;;
  firefox)
    cat > "$profile/user.js" <<PREFS
user_pref("media.navigator.streams.fake", true);
user_pref("media.navigator.permission.disabled", true);
user_pref("media.autoplay.default", 0);
user_pref("media.autoplay.blocking_policy", 0);
user_pref("media.autoplay.block-webaudio", false);
user_pref("media.volume_scale", "0.0");
user_pref("privacy.reduceTimerPrecision", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("app.update.enabled", false);
${FIREFOX_PREFS:-}
PREFS
    firefox --headless --no-remote --profile "$profile" "$url" >"$profile/log.txt" 2>&1 &
    ;;
  *) echo "unknown browser $browser" >&2; exit 2 ;;
esac
pid=$!
deadline=$(( $(date +%s) + dur + ${EXTRA_WAIT:-25} ))
status=timeout
while [ "$(date +%s)" -lt "$deadline" ]; do
  if [ -f "$out" ]; then status=ok; break; fi
  if [ -f "$here/results/$browser-$exp-FAILED.json" ]; then status=failed; break; fi
  sleep 0.5
done
kill "$pid" 2>/dev/null; sleep 0.5; pkill -f "$profile" 2>/dev/null
[ "$status" = ok ] && rm -rf "$profile"
echo "$browser $exp: $status"
[ "$status" = ok ]

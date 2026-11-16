// The stock Pipecat client, unchanged, against the stub. Bundled by esbuild
// into ../web/pipecat/bundle.js. What happened goes to /report.
import { PipecatClient, RTVIEvent, RTVI_PROTOCOL_VERSION } from '@pipecat-ai/client-js';
import { SmallWebRTCTransport } from '@pipecat-ai/small-webrtc-transport';

const q = new URLSearchParams(location.search);
const wall = () => performance.timeOrigin + performance.now();
const browser = navigator.userAgent.includes('Firefox') ? 'firefox' : 'chrome';
const report = { exp: q.get('exp') || 'pipecat', browser, ua: navigator.userAgent, query: location.search, rtvi: RTVI_PROTOCOL_VERSION, marks: {}, states: [], callbacks: [], heard: [], stats: [], ssrc: [], notes: [] };
const t0 = wall();
const note = (name, ...args) => { report.callbacks.push([Math.round(wall() - t0), name, JSON.parse(JSON.stringify(args, (k, v) => (v instanceof MediaStreamTrack ? `track:${v.kind}` : v)))]); };

async function main() {
  await fetch('/pipecat-reset');
  const ctx = new AudioContext({ sampleRate: 48000 });
  await ctx.resume();
  await ctx.audioWorklet.addModule('/detector.js');

  const callbacks = {};
  for (const name of ['onConnected', 'onDisconnected', 'onError', 'onTransportStateChanged', 'onBotStarted', 'onBotConnected', 'onBotReady', 'onBotDisconnected', 'onServerMessage', 'onMessageError', 'onUserStartedSpeaking', 'onUserStoppedSpeaking', 'onBotStartedSpeaking', 'onBotStoppedSpeaking', 'onUserTranscript', 'onBotOutput', 'onBotTranscript', 'onBotLlmText', 'onBotTtsText', 'onTrackStopped', 'onDeviceError', 'onUnsupportedFeature', 'onMediaStateChanged']) {
    callbacks[name] = (...args) => note(name, ...args);
  }
  callbacks.onTrackStarted = (track, participant) => {
    note('onTrackStarted', track.kind, participant);
    if (track.kind !== 'audio' || (participant && participant.local)) return;
    const stream = new MediaStream([track]);
    const el = document.getElementById('out');
    el.srcObject = stream; el.play().catch((e) => report.notes.push('play: ' + e));
    const node = new AudioWorkletNode(ctx, 'detector');
    ctx.createMediaStreamSource(stream).connect(node);
    const quiet = ctx.createGain(); quiet.gain.value = 0; node.connect(quiet); quiet.connect(ctx.destination);
    node.port.onmessage = (m) => { const now = wall(); for (const [, a, b] of m.data) report.heard.push([Math.round(now - t0), a, typeof b === 'number' ? Number(b.toExponential(2)) : b]); };
  };

  const transport = new SmallWebRTCTransport({ waitForICEGathering: q.get('wait') === '1' });
  const client = new PipecatClient({ transport, enableMic: true, enableCam: false, callbacks });
  window.client = client;
  const pass = new URLSearchParams(location.search); pass.delete('exp'); pass.delete('dur'); pass.delete('start');
  let ready;
  try {
    if (q.get('start') === '1') ready = await client.startBotAndConnect({ endpoint: '/start', requestData: { transport: 'webrtc' } });
    else ready = await client.connect({ webrtcUrl: '/api/offer?' + pass });
    report.marks.ready = Math.round(wall() - t0);
    report.ready = ready;
  } catch (e) {
    report.error = String(e && e.stack || e);
  }
  report.state_after_connect = client.state;

  const pc = transport.pc;
  const dur = Number(q.get('dur') || 8);
  const stats = setInterval(async () => {
    if (!pc) return;
    const entry = { at: Math.round(wall() - t0) };
    (await pc.getStats()).forEach((s) => {
      if (s.type === 'inbound-rtp') entry['inbound-' + s.kind] = { packetsReceived: s.packetsReceived, packetsLost: s.packetsLost, concealedSamples: s.concealedSamples, totalSamplesReceived: s.totalSamplesReceived, jitterBufferEmittedCount: s.jitterBufferEmittedCount, packetsDiscarded: s.packetsDiscarded };
      if (s.type === 'outbound-rtp') entry['outbound-' + s.kind] = { packetsSent: s.packetsSent, bytesSent: s.bytesSent };
      if (s.type === 'transport') entry.transport = { dtlsState: s.dtlsState, tlsVersion: s.tlsVersion, srtpCipher: s.srtpCipher };
      if (s.type === 'codec') { entry.codecs = entry.codecs || []; entry.codecs.push([s.payloadType, s.mimeType, s.sdpFmtpLine]); }
    });
    report.stats.push(entry);
  }, 1000);
  if (q.get('text')) setTimeout(() => { try { client.sendText ? client.sendText(q.get('text')) : report.notes.push('no sendText'); } catch (e) { report.notes.push('sendText: ' + e); } }, 2000);
  await new Promise((r) => setTimeout(r, dur * 1000));
  clearInterval(stats);
  if (pc) {
    report.transceivers = pc.getTransceivers().map((t) => ({ mid: t.mid, kind: t.receiver.track && t.receiver.track.kind, direction: t.direction, currentDirection: t.currentDirection, stopped: t.stopped }));
    report.offer = pc.localDescription && pc.localDescription.sdp;
    report.answer = pc.remoteDescription && pc.remoteDescription.sdp;
    report.dc = transport.dc && { label: transport.dc.label, id: transport.dc.id, readyState: transport.dc.readyState, ordered: transport.dc.ordered };
  }
  report.state_at_end = client.state;
  report.pc_id = transport.pc_id;
  const sid = (transport.pc_id || '').split('#')[1] || 0;
  await fetch(`/report?name=${encodeURIComponent(browser + '-' + report.exp)}&id=${sid}`, { method: 'POST', body: JSON.stringify(report) });
  try { await client.disconnect(); } catch (e) { report.notes.push('disconnect: ' + e); }
  document.title = 'done';
}
main().catch(async (e) => {
  report.error = String(e && e.stack || e);
  await fetch(`/report?name=${encodeURIComponent(browser + '-' + report.exp + '-FAILED')}`, { method: 'POST', body: JSON.stringify(report) });
  document.title = 'failed';
});

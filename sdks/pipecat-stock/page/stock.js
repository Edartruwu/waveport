// The stock client of Pipecat's on a page, as an application of it has
// it: `@pipecat-ai/client-js` with the transport its server is reached
// by, `@pipecat-ai/small-webrtc-transport`, both as they are published
// and with nothing of Waveport's between them and the daemon.
//
// The page does nothing by itself. Whoever drives it (`test/run.js`)
// calls `window.stock.begin`, and then the client as an application
// would. What the client tells its application is kept in
// `window.stock.told`, in the order it was told.

import { PipecatClient, RTVI_PROTOCOL_VERSION } from "@pipecat-ai/client-js";
import { SmallWebRTCTransport, WavMediaManager } from "@pipecat-ai/small-webrtc-transport";

/** What an application can be told of, and is kept here. */
const CALLBACKS = [
  "onConnected",
  "onDisconnected",
  "onError",
  "onMessageError",
  "onTransportStateChanged",
  "onBotStarted",
  "onBotConnected",
  "onBotReady",
  "onBotDisconnected",
  "onServerMessage",
  "onUnsupportedFeature",
  "onUserStartedSpeaking",
  "onUserStoppedSpeaking",
  "onUserTranscript",
  "onUserInput",
  "onBotStartedSpeaking",
  "onBotStoppedSpeaking",
  "onBotInterrupted",
  "onBotLlmStarted",
  "onBotLlmStopped",
  "onBotLlmText",
  "onBotTtsStarted",
  "onBotTtsStopped",
  "onBotTtsText",
  "onBotOutput",
  "onLLMFunctionCallStarted",
  "onLLMFunctionCallInProgress",
  "onLLMFunctionCallStopped",
  "onDeviceError",
];

/** What was told, as something that can be read from outside the page. */
const plain = (told) => JSON.parse(JSON.stringify(told, (_, value) => (value instanceof Error ? String(value) : value)) ?? "null");

window.stock = {
  /** The version of RTVI the client speaks. */
  speaks: RTVI_PROTOCOL_VERSION,
  /** What the client told its application: the callback, and what it was called with. */
  told: [],
  client: undefined,
  transport: undefined,

  /**
   * Makes a client, as an application does.
   *
   * @param {{ mic: boolean }} how Whether it opens a microphone.
   */
  begin(how) {
    const told = [];
    this.told = told;
    const callbacks = {};
    for (const name of CALLBACKS) {
      callbacks[name] = (...args) => told.push([name, ...plain(args)]);
    }
    callbacks.onTrackStarted = (track, participant) => {
      told.push(["onTrackStarted", track.kind, participant?.local === true]);
      if (track.kind === "audio" && participant?.local !== true) {
        // An application plays what the agent says.
        const out = document.getElementById("out");
        out.srcObject = new MediaStream([track]);
        out.play().catch((refused) => told.push(["play", String(refused)]));
      }
    };
    // The transport opens devices with one of two things it comes
    // with. The one it takes when it is told nothing fetches a program
    // from its maker's servers as it starts, and tells a third party
    // when that fails. The other asks nobody anything, and is the one
    // it is given here: from the offer on, everything is the same.
    this.transport = new SmallWebRTCTransport({ mediaManager: new WavMediaManager() });
    this.client = new PipecatClient({
      transport: this.transport,
      enableMic: how.mic,
      enableCam: false,
      callbacks,
    });
  },
};

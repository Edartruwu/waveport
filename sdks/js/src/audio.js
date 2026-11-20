// @ts-check
/**
 * The voice of a client in a browser: what the agent says is played,
 * and what the microphone hears is sent.
 *
 * `Voice.format` says how this browser can write audio: as Opus at
 * 48 kHz, where it codes and decodes Opus itself (WebCodecs), and as
 * plain samples otherwise. `Voice.open` makes a voice that writes it
 * so: it asks for the microphone and is ready to play. `attach` puts
 * the voice on the line of a client that said hello with that format.
 * From then on it plays what the client is sent on the audio thread
 * (`worklet.js`), where what was played is counted by the sample and
 * said to the session, and it sends what the microphone hears, 20 ms at
 * a time, silence too.
 *
 * The voice is opened before the client connects, and attached the
 * moment the client is welcomed: an agent that greets speaks as soon
 * as its far end is on the line, and what a voice was not there for is
 * neither played nor said to have been. `call` (`index.js`) does it in
 * that order.
 *
 * A browser lets a page play sound once its user has done something on
 * it, so `Voice.open` is called from a click or a key.
 *
 * The microphone is asked for with the echo of what is played taken
 * out of it (`echoCancellation: true`), which leaves how much is taken
 * out to the browser. Chrome takes out everything the page plays, what
 * is played here too. `"all"` in `constraints` asks for that by name
 * where a browser knows the word, and `settings` says what was given.
 *
 * @module
 */

import { HEARS, PLAYS } from "./worklet.js";

/**
 * @typedef {import("./client.js").Client} Client
 * @typedef {import("./protocol.js").AudioFormat} AudioFormat
 * @typedef {import("./worklet.js").Played} Played
 * @typedef {import("./worklet.js").ToPlayout} ToPlayout
 */

/**
 * @typedef {object} VoiceOptions
 * @property {MediaStream} [microphone] What is heard, in place of the
 *   microphone the browser would be asked for.
 * @property {boolean} [owned] Whether a microphone that was given is
 *   the voice's to let go of when it stops.
 * @property {MediaTrackConstraints} [constraints] What is asked of the
 *   microphone, beside and in place of one channel with the echo of
 *   what is played, the noise around and the level of the voice seen
 *   to.
 * @property {string | URL} [worklet] Where `worklet.js` is, if not
 *   beside this file.
 * @property {number} [bitrate] How many bits a second of Opus are
 *   sent. 32000 if left out.
 * @property {(error: Error) => void} [onerror] Told what went wrong
 *   with the audio once it was started.
 */

/** How long a frame of audio is: 20 ms, in microseconds. */
const FRAME_US = 20_000;

/** The rate Opus is coded and decoded at here. A decoder gives it whatever it is asked. */
const OPUS_RATE = 48_000;

/**
 * How Opus is asked for, at `rate`.
 *
 * @param {number} rate
 */
const opus = (rate) => ({ codec: "opus", sampleRate: rate, numberOfChannels: 1 });

/**
 * Asks the browser for the microphone: one channel, with the echo of
 * what is played, the noise around and the level of the voice seen to,
 * unless `constraints` say otherwise.
 *
 * @param {MediaTrackConstraints} [constraints]
 * @returns {Promise<MediaStream>}
 */
export function microphone(constraints) {
  return navigator.mediaDevices.getUserMedia({
    audio: {
      channelCount: 1,
      echoCancellation: true,
      noiseSuppression: true,
      autoGainControl: true,
      ...constraints,
    },
  });
}

/** The voice of a client. */
export class Voice {
  /** The client whose line the voice is on. @type {Client | undefined} */
  #client;

  /** How the voice writes audio. @type {AudioFormat} */
  #format;

  /** @type {AudioContext} */
  #context;

  /** @type {AudioWorkletNode} */
  #plays;

  /** @type {MediaStream} */
  #microphone;

  /** Whether the microphone is the voice's own to let go of. */
  #owns;

  /** @type {AudioDecoder | undefined} */
  #decoder;

  /** @type {AudioEncoder | undefined} */
  #encoder;

  /** The run the audio that arrives is of. @type {number | undefined} */
  #run;

  /** The runs that began and are not over. @type {Set<number>} */
  #runs = new Set();

  /** The run of each packet that is being decoded, in order. @type {number[]} */
  #decoding = [];

  /** The runs that were sent whole while some of them was being decoded. @type {Set<number>} */
  #ending = new Set();

  /** When the next frame that is sent was heard, in microseconds. */
  #heardAt = 0;

  /** When the next packet that is decoded begins, in microseconds. */
  #saidAt = 0;

  #muted = false;

  #stopped = false;

  /** How many frames the audio thread was last told the output takes. */
  #latency = -1;

  /** @type {(() => void)[]} */
  #listening = [];

  /** @type {(error: Error) => void} */
  #failed;

  /**
   * @param {AudioFormat} format
   * @param {AudioContext} context
   * @param {AudioWorkletNode} plays
   * @param {AudioWorkletNode} hears
   * @param {MediaStream} microphone
   * @param {boolean} owns
   * @param {VoiceOptions} options
   */
  constructor(format, context, plays, hears, microphone, owns, options) {
    this.#format = format;
    this.#context = context;
    this.#plays = plays;
    this.#microphone = microphone;
    this.#owns = owns;
    this.#failed = options.onerror ?? (() => {});
    if (format.codec === "opus") {
      this.#decoder = new AudioDecoder({
        output: (audio) => this.#decoded(audio),
        error: (error) => this.#failed(error),
      });
      this.#decoder.configure(opus(format.rate));
      this.#encoder = new AudioEncoder({
        output: (packet) => {
          const bytes = new Uint8Array(packet.byteLength);
          packet.copyTo(bytes);
          this.#client?.send(bytes);
        },
        error: (error) => this.#failed(error),
      });
      // A packet every 20 ms, silence too: the session hears at the
      // pace it is sent. That it is a voice is said for the browsers
      // that know the words, and left out by the ones that do not.
      const voiced = { frameDuration: FRAME_US, usedtx: false, application: "voip", signal: "voice" };
      this.#encoder.configure({
        ...opus(format.rate),
        bitrate: options.bitrate ?? 32_000,
        opus: /** @type {OpusEncoderConfig} */ (voiced),
      });
    }
    plays.port.onmessage = (event) => {
      for (const { run, samples, last } of /** @type {Played[]} */ (event.data)) {
        this.#client?.played(run, samples);
        if (last) {
          this.#runs.delete(run);
        }
      }
      this.#measure();
    };
    hears.port.onmessage = (event) => this.#heard(/** @type {Float32Array} */ (event.data));
  }

  /**
   * How this browser writes audio at `rate` samples a second: as Opus
   * if it codes and decodes Opus itself, and as plain samples if not.
   *
   * @param {number} [rate]
   * @returns {Promise<AudioFormat>}
   */
  static async format(rate = OPUS_RATE) {
    const coded = typeof AudioEncoder === "function" && typeof AudioDecoder === "function";
    if (coded && rate === OPUS_RATE) {
      try {
        const [codes, decodes] = await Promise.all([
          AudioEncoder.isConfigSupported({ ...opus(rate), bitrate: 32_000 }),
          AudioDecoder.isConfigSupported(opus(rate)),
        ]);
        if (codes.supported === true && decodes.supported === true) {
          return { codec: "opus", rate };
        }
      } catch {
        // A browser that cannot say whether it can, cannot.
      }
    }
    return { codec: "pcm16", rate };
  }

  /**
   * Makes a voice that writes audio as `format` says, which is what
   * `Voice.format` answered and what the client says hello with: asks
   * for the microphone, and is ready to play. It is on no line until
   * it is attached to a client, and what the microphone hears until
   * then goes nowhere.
   *
   * @param {AudioFormat} format
   * @param {VoiceOptions} [options]
   * @returns {Promise<Voice>}
   * @throws {Error} The browser does not play at the rate asked for, or
   *   its user did not allow the microphone.
   */
  static async open(format, options = {}) {
    const context = new AudioContext({ sampleRate: format.rate, latencyHint: "interactive" });
    try {
      if (context.sampleRate !== format.rate) {
        throw new Error(`this browser does not play at ${format.rate} samples a second`);
      }
      const worklet = options.worklet ?? new URL("./worklet.js", import.meta.url);
      await context.audioWorklet.addModule(worklet);
      const owns = options.microphone === undefined || options.owned === true;
      const heard = options.microphone ?? (await microphone(options.constraints));
      const plays = new AudioWorkletNode(context, PLAYS, {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [1],
      });
      plays.connect(context.destination);
      const hears = new AudioWorkletNode(context, HEARS, {
        numberOfInputs: 1,
        numberOfOutputs: 1,
        channelCount: 1,
        channelCountMode: "explicit",
        outputChannelCount: [1],
        processorOptions: { frame: format.rate / 50 },
      });
      context.createMediaStreamSource(heard).connect(hears);
      // What hears is asked for what it has only while something
      // listens to it: its silence goes to the output.
      const silent = context.createGain();
      silent.gain.value = 0;
      hears.connect(silent).connect(context.destination);
      await context.resume();
      return new Voice(format, context, plays, hears, heard, owns, options);
    } catch (error) {
      await context.close();
      throw error;
    }
  }

  /**
   * Puts the voice on the line of `client`: what the client is sent
   * from here on is played, and what the microphone hears is sent.
   * Called as soon as the client is welcomed, before anything else is
   * waited for.
   *
   * @param {Client} client
   * @throws {Error} The voice is on a line already or was stopped, the
   *   client is on a line of text, or its line is written otherwise
   *   than the voice writes audio.
   */
  attach(client) {
    if (this.#client !== undefined || this.#stopped) {
      throw new Error("the voice is on a line already, or was stopped");
    }
    const line = client.audio;
    if (line === undefined) {
      throw new Error("the client is on a line of text");
    }
    if (line.codec !== this.#format.codec || line.rate !== this.#format.rate) {
      throw new Error("the line is not written as the voice writes audio");
    }
    this.#client = client;
    this.#listening = [
      client.on("mark", (mark) => this.#marked(mark.run, mark.end === true)),
      client.on("audio", (audio) => this.#sent(audio)),
      client.on("clear", (clear) => this.#discard(clear.run)),
      // A session cuts what it was saying where it was heard when its
      // line drops: what the page still holds of it is not played.
      client.on("away", () => this.#discardAll()),
      client.on("closed", () => this.stop()),
    ];
  }

  /** How the voice writes audio. */
  get format() {
    return this.#format;
  }

  /**
   * What the microphone was given of what it was asked for: whether
   * the echo of what is played is taken out of it, and how much of it
   * (`echoCancellation`), among the rest.
   *
   * @returns {MediaTrackSettings}
   */
  get settings() {
    return this.#microphone.getAudioTracks()[0]?.getSettings() ?? {};
  }

  /** Whether silence is sent in place of what the microphone hears. */
  get muted() {
    return this.#muted;
  }

  set muted(muted) {
    this.#muted = muted;
  }

  /**
   * Stops playing and hearing, and lets go of the microphone if the
   * voice asked for it. The client goes on.
   */
  stop() {
    if (this.#stopped) {
      return;
    }
    this.#stopped = true;
    for (const stop of this.#listening.splice(0)) {
      stop();
    }
    if (this.#owns) {
      for (const track of this.#microphone.getTracks()) {
        track.stop();
      }
    }
    for (const codec of [this.#decoder, this.#encoder]) {
      if (codec !== undefined && codec.state !== "closed") {
        codec.close();
      }
    }
    void this.#context.close();
  }

  /**
   * Tells the audio thread how long the output takes to sound what it
   * is given, which is not the same all the time. Of what the browser
   * says of it the longest is taken: what was not heard is not to be
   * said to have been played.
   */
  #measure() {
    const context = this.#context;
    const stamp = context.getOutputTimestamp().contextTime ?? 0;
    const behind = stamp > 0 ? context.currentTime - stamp : 0;
    const latency = Math.max(behind, context.outputLatency || 0, context.baseLatency || 0);
    const frames = Math.round(latency * context.sampleRate);
    if (frames !== this.#latency) {
      this.#latency = frames;
      this.#tell({ type: "latency", frames });
    }
  }

  /** @param {ToPlayout} told */
  #tell(told) {
    if (told.type === "audio") {
      this.#plays.port.postMessage(told, [told.samples.buffer]);
    } else {
      this.#plays.port.postMessage(told);
    }
  }

  /**
   * A run begins, or has been sent whole.
   *
   * @param {number} run
   * @param {boolean} whole
   */
  #marked(run, whole) {
    if (!whole) {
      this.#run = run;
      this.#runs.add(run);
      this.#measure();
      this.#tell({ type: "begin", run });
    } else if (this.#decoding.includes(run)) {
      // The end of a run follows the last of its audio, which is still
      // being decoded.
      this.#ending.add(run);
    } else {
      this.#tell({ type: "end", run });
    }
  }

  /**
   * Audio of the run that was marked last.
   *
   * @param {ArrayBuffer} audio
   */
  #sent(audio) {
    const run = this.#run;
    if (run === undefined || !this.#runs.has(run) || this.#stopped) {
      return;
    }
    if (this.#decoder === undefined) {
      const samples = new Int16Array(audio, 0, audio.byteLength >> 1);
      const heard = new Float32Array(samples.length);
      for (let n = 0; n < samples.length; n += 1) {
        heard[n] = /** @type {number} */ (samples[n]) / 32_768;
      }
      this.#tell({ type: "audio", run, samples: heard });
      return;
    }
    if (this.#decoder.state !== "configured") {
      return;
    }
    this.#decoding.push(run);
    this.#decoder.decode(
      new EncodedAudioChunk({ type: "key", timestamp: this.#saidAt, data: audio }),
    );
    this.#saidAt += FRAME_US;
  }

  /**
   * A packet has been decoded.
   *
   * @param {AudioData} audio
   */
  #decoded(audio) {
    const run = this.#decoding.shift();
    const samples = new Float32Array(audio.numberOfFrames);
    audio.copyTo(samples, { planeIndex: 0, format: "f32-planar" });
    audio.close();
    if (run === undefined) {
      return;
    }
    this.#tell({ type: "audio", run, samples });
    if (this.#ending.has(run) && !this.#decoding.includes(run)) {
      this.#ending.delete(run);
      this.#tell({ type: "end", run });
    }
  }

  /**
   * What is held of a run and was not played is discarded.
   *
   * @param {number} run
   */
  #discard(run) {
    this.#ending.delete(run);
    this.#runs.delete(run);
    if (this.#run === run) {
      this.#run = undefined;
    }
    this.#tell({ type: "clear", run });
  }

  #discardAll() {
    for (const run of [...this.#runs]) {
      this.#discard(run);
    }
  }

  /**
   * The microphone heard a frame.
   *
   * @param {Float32Array} frame
   */
  #heard(frame) {
    const client = this.#client;
    if (this.#stopped || client === undefined) {
      return;
    }
    if (this.#muted) {
      frame.fill(0);
    }
    if (this.#encoder === undefined) {
      const samples = new Int16Array(frame.length);
      for (let n = 0; n < frame.length; n += 1) {
        const sample = Math.max(-1, Math.min(1, /** @type {number} */ (frame[n])));
        samples[n] = Math.round(sample * 32_767);
      }
      client.send(samples);
      return;
    }
    if (this.#encoder.state !== "configured") {
      return;
    }
    const heard = new AudioData({
      format: "f32-planar",
      sampleRate: this.#context.sampleRate,
      numberOfFrames: frame.length,
      numberOfChannels: 1,
      timestamp: this.#heardAt,
      data: /** @type {Float32Array<ArrayBuffer>} */ (frame),
    });
    this.#heardAt += FRAME_US;
    this.#encoder.encode(heard);
    heard.close();
  }
}

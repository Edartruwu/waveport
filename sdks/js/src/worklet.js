// @ts-check
/**
 * What runs on the audio thread of a page: what the agent says is
 * played here, and what the microphone hears is cut into frames here.
 *
 * A session takes a response as heard up to what its client says it
 * played, so what is said to have been played is counted here, where it
 * is played, and by the sample: `Playout` counts a sample once it has
 * been given to the output and the output has had the time it takes to
 * sound (`latency`), and never counts what was discarded or what was
 * not there to play.
 *
 * The file is a module of its own with nothing imported, which is how a
 * page gives it to `audioWorklet.addModule`. The two classes that do
 * the counting and the cutting have nothing of the audio thread in
 * them, and are tested without one.
 *
 * @module
 */

/**
 * What the audio thread is told.
 *
 * @typedef {{ type: "begin", run: number }
 *   | { type: "audio", run: number, samples: Float32Array }
 *   | { type: "end", run: number }
 *   | { type: "clear", run: number }
 *   | { type: "latency", frames: number }} ToPlayout
 */

/**
 * What the audio thread says of a run: how many of its samples have
 * been played, and whether that is the last that will be said of it.
 *
 * @typedef {{ run: number, samples: number, last: boolean }} Played
 */

/**
 * A run of audio as it is played.
 *
 * @typedef {object} Run
 * @property {Float32Array[]} waiting What was sent and is not given to
 *   the output yet.
 * @property {number} at How much of the first of those was given.
 * @property {number} given How many samples were given to the output.
 * @property {number} heard How many of those have had time to sound.
 * @property {number} said How many were said to have been played.
 * @property {boolean} whole Whether the run has been sent to its end.
 */

/** How often what was played is said: every 100 ms at 48 kHz. */
const SAY_EVERY = 4800;

/**
 * Plays runs of audio, one after another in the order they began, and
 * counts what was played of each.
 */
export class Playout {
  /** The runs that are not over, by their numbers, oldest first. @type {Map<number, Run>} */
  #runs = new Map();

  /**
   * What was given to the output and has not had time to sound: how
   * many samples of which run, and the frame from which they have.
   *
   * @type {{ run: number, samples: number, due: number }[]}
   */
  #sounding = [];

  /** How many frames the output takes to sound what it is given. */
  #latency = 0;

  /** The frame at which what was played was last said. */
  #saidAt = 0;

  /**
   * How many frames the output takes to sound what it is given.
   *
   * @param {number} frames
   */
  set latency(frames) {
    this.#latency = Number.isFinite(frames) && frames > 0 ? Math.round(frames) : 0;
  }

  /**
   * A run begins: the audio that follows is its.
   *
   * @param {number} run
   */
  begin(run) {
    if (!this.#runs.has(run)) {
      this.#runs.set(run, { waiting: [], at: 0, given: 0, heard: 0, said: -1, whole: false });
    }
  }

  /**
   * More of a run. Audio of a run that did not begin, or that is over,
   * is not played.
   *
   * @param {number} run
   * @param {Float32Array} samples
   */
  push(run, samples) {
    const playing = this.#runs.get(run);
    if (playing !== undefined && !playing.whole && samples.length > 0) {
      playing.waiting.push(samples);
    }
  }

  /**
   * A run has been sent to its end.
   *
   * @param {number} run
   */
  end(run) {
    const playing = this.#runs.get(run);
    if (playing !== undefined) {
      playing.whole = true;
    }
  }

  /**
   * Discards what is held of a run and was not given to the output.
   * Returns what was played of it, which is the last that is said of
   * it: what the output was given, it sounds.
   *
   * @param {number} run
   * @returns {Played | undefined} Nothing, if there is no such run.
   */
  clear(run) {
    const playing = this.#runs.get(run);
    if (playing === undefined) {
      return undefined;
    }
    this.#runs.delete(run);
    this.#sounding = this.#sounding.filter((given) => given.run !== run);
    return { run, samples: playing.given, last: true };
  }

  /**
   * Fills `out` with what is to sound next, from the frame `frame` of
   * the output. Where there is nothing to play, there is silence.
   *
   * @param {Float32Array} out
   * @param {number} frame
   */
  render(out, frame) {
    let filled = 0;
    for (const [run, playing] of this.#runs) {
      const before = filled;
      while (filled < out.length) {
        const next = playing.waiting[0];
        if (next === undefined) {
          break;
        }
        const takes = Math.min(next.length - playing.at, out.length - filled);
        out.set(next.subarray(playing.at, playing.at + takes), filled);
        filled += takes;
        playing.at += takes;
        if (playing.at === next.length) {
          playing.waiting.shift();
          playing.at = 0;
        }
      }
      const given = filled - before;
      if (given > 0) {
        playing.given += given;
        this.#sounding.push({ run, samples: given, due: frame + filled + this.#latency });
      }
      // A run that is not played to its end yet is not overtaken by the
      // one after it: what is missing of it is silence, for now.
      if (!playing.whole || playing.waiting.length > 0) {
        break;
      }
    }
    out.fill(0, filled);
  }

  /**
   * What there is to say at the frame `frame` of what was played: of
   * each run a few times a second as it is played, and once more when
   * it has been played to its end.
   *
   * @param {number} frame
   * @returns {Played[]}
   */
  played(frame) {
    while (this.#sounding.length > 0) {
      const first = /** @type {{ run: number, samples: number, due: number }} */ (this.#sounding[0]);
      if (first.due > frame) {
        break;
      }
      this.#sounding.shift();
      const playing = this.#runs.get(first.run);
      if (playing !== undefined) {
        playing.heard += first.samples;
      }
    }
    const due = frame - this.#saidAt >= SAY_EVERY;
    /** @type {Played[]} */
    const said = [];
    for (const [run, playing] of this.#runs) {
      const over = playing.whole && playing.waiting.length === 0 && playing.heard === playing.given;
      if (over) {
        this.#runs.delete(run);
        said.push({ run, samples: playing.heard, last: true });
      } else if (due && playing.heard !== playing.said && playing.heard > 0) {
        playing.said = playing.heard;
        said.push({ run, samples: playing.heard, last: false });
      }
    }
    if (due) {
      this.#saidAt = frame;
    }
    return said;
  }
}

/**
 * Cuts what a microphone hears, which comes in pieces of any length,
 * into frames of one length.
 */
export class Framer {
  /** @type {Float32Array} */
  #frame;

  #held = 0;

  /** @param {number} length How many samples a frame holds. */
  constructor(length) {
    this.#frame = new Float32Array(length);
  }

  /**
   * Takes the next piece. Returns the frames it completes, each a
   * buffer of its own.
   *
   * @param {Float32Array} piece
   * @returns {Float32Array[]}
   */
  push(piece) {
    /** @type {Float32Array[]} */
    const frames = [];
    let from = 0;
    while (from < piece.length) {
      const takes = Math.min(piece.length - from, this.#frame.length - this.#held);
      this.#frame.set(piece.subarray(from, from + takes), this.#held);
      this.#held += takes;
      from += takes;
      if (this.#held === this.#frame.length) {
        frames.push(this.#frame.slice());
        this.#held = 0;
      }
    }
    return frames;
  }
}

/** The name the processor that plays is registered under. */
export const PLAYS = "waveport-plays";

/** The name the processor that hears is registered under. */
export const HEARS = "waveport-hears";

/**
 * What the scope of an audio thread has, where this file runs in one.
 *
 * @type {{
 *   AudioWorkletProcessor?: new () => { port: MessagePort },
 *   registerProcessor?: (name: string, processor: unknown) => void,
 *   currentFrame?: number,
 * }}
 */
const scope = /** @type {any} */ (globalThis);

if (scope.AudioWorkletProcessor !== undefined && scope.registerProcessor !== undefined) {
  const Processor = scope.AudioWorkletProcessor;

  /** Plays what the agent says, and says what was played. */
  class Plays extends Processor {
    #playout = new Playout();

    constructor() {
      super();
      this.port.onmessage = (event) => {
        const told = /** @type {ToPlayout} */ (event.data);
        switch (told.type) {
          case "begin":
            this.#playout.begin(told.run);
            break;
          case "audio":
            this.#playout.push(told.run, told.samples);
            break;
          case "end":
            this.#playout.end(told.run);
            break;
          case "clear": {
            const played = this.#playout.clear(told.run);
            if (played !== undefined) {
              this.port.postMessage([played]);
            }
            break;
          }
          case "latency":
            this.#playout.latency = told.frames;
            break;
        }
      };
    }

    /**
     * @param {Float32Array[][]} _inputs
     * @param {Float32Array[][]} outputs
     */
    process(_inputs, outputs) {
      const out = outputs[0]?.[0];
      if (out === undefined) {
        return true;
      }
      const frame = scope.currentFrame ?? 0;
      this.#playout.render(out, frame);
      // Both ears hear the one channel there is.
      for (const other of outputs[0]?.slice(1) ?? []) {
        other.set(out);
      }
      const played = this.#playout.played(frame + out.length);
      if (played.length > 0) {
        this.port.postMessage(played);
      }
      return true;
    }
  }

  /** Cuts what the microphone hears into frames of 20 ms. */
  class Hears extends Processor {
    /** @type {Framer | undefined} */
    #framer;

    /** @param {{ processorOptions?: { frame?: number } }} [options] */
    constructor(options) {
      super();
      this.#framer = new Framer(options?.processorOptions?.frame ?? 960);
    }

    /** @param {Float32Array[][]} inputs */
    process(inputs) {
      const heard = inputs[0]?.[0];
      if (heard !== undefined && this.#framer !== undefined) {
        for (const frame of this.#framer.push(heard)) {
          this.port.postMessage(frame, [frame.buffer]);
        }
      }
      return true;
    }
  }

  scope.registerProcessor(PLAYS, Plays);
  scope.registerProcessor(HEARS, Hears);
}

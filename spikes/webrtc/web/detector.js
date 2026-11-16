// Reads the known signal back from what the browser plays: every 10 ms the
// pair of tones (the number of the 100 ms it is), and every 40 ms whether a
// DTMF pair is there. Runs in the audio thread; `currentFrame` is the frame of
// the context's own clock at which the block ended.
const LO = [...Array(16)].map((_, i) => 800 + 200 * i);
const HI = [...Array(16)].map((_, i) => 4400 + 200 * i);
const DTMF_LO = [697, 770, 852, 941];
const DTMF_HI = [1209, 1336, 1477, 1633];
const KEYS = ['123A', '456B', '789C', '*0#D'];

function goertzel(buf, n, freq, rate) {
  const c = 2 * Math.cos(2 * Math.PI * freq / rate);
  let s1 = 0, s2 = 0;
  for (let i = 0; i < n; i++) { const s0 = buf[i] + c * s1 - s2; s2 = s1; s1 = s0; }
  return s1 * s1 + s2 * s2 - c * s1 * s2;
}

class Detector extends AudioWorkletProcessor {
  constructor() {
    super();
    this.block = new Float32Array(Math.round(sampleRate / 100));
    this.n = 0;
    this.long = new Float32Array(Math.round(sampleRate / 25));
    this.m = 0;
    this.out = [];
  }
  process(inputs) {
    const ch = inputs[0] && inputs[0][0];
    const len = ch ? ch.length : 128;
    for (let i = 0; i < len; i++) {
      const s = ch ? ch[i] : 0;
      this.block[this.n++] = s;
      this.long[this.m++] = s;
      if (this.n === this.block.length) {
        this.n = 0;
        let energy = 0;
        for (let k = 0; k < this.block.length; k++) energy += this.block[k] * this.block[k];
        energy /= this.block.length;
        let sym = -1;
        if (energy > 1e-5) {
          let bl = 0, bh = 0, tl = 0, th = 0, il = 0, ih = 0;
          for (let d = 0; d < 16; d++) {
            const l = goertzel(this.block, this.block.length, LO[d], sampleRate);
            const h = goertzel(this.block, this.block.length, HI[d], sampleRate);
            tl += l; th += h;
            if (l > bl) { bl = l; il = d; }
            if (h > bh) { bh = h; ih = d; }
          }
          if (bl > 0.6 * tl && bh > 0.6 * th) sym = ih * 16 + il;
        }
        this.out.push([currentFrame + i + 1, sym, energy]);
      }
      if (this.m === this.long.length) {
        this.m = 0;
        let energy = 0;
        for (let k = 0; k < this.long.length; k++) energy += this.long[k] * this.long[k];
        if (energy / this.long.length > 1e-5) {
          const n = this.long.length;
          const lo = DTMF_LO.map(f => goertzel(this.long, n, f, sampleRate));
          const hi = DTMF_HI.map(f => goertzel(this.long, n, f, sampleRate));
          const il = lo.indexOf(Math.max(...lo)), ih = hi.indexOf(Math.max(...hi));
          // Parseval: a pure pair of tones puts about energy * n^2 / 2 into its two bins.
          const share = (lo[il] + hi[ih]) / (energy * n / 2);
          if (share > 0.7 && lo[il] > 0.15 * hi[ih] && hi[ih] > 0.15 * lo[il]) {
            this.out.push([currentFrame + i + 1, 'dtmf', KEYS[il][ih]]);
          }
        }
      }
    }
    if (this.out.length >= 5) { this.port.postMessage(this.out); this.out = []; }
    return true;
  }
}
registerProcessor('detector', Detector);

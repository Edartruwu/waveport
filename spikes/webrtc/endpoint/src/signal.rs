//! The known signal: every 100 ms a new pair of tones that says which 100 ms
//! it is (a number 0..256, so 25.6 s before it repeats), and its detector.
//! The same numbers are in `web/signal.js`.

use std::f64::consts::TAU;

pub const RATE: usize = 48_000;
pub const FRAME: usize = 960; // 20 ms
pub const SYMBOL: usize = 4_800; // 100 ms
pub const BLOCK: usize = 480; // 10 ms, what the detector looks at

/// Low digit of the symbol: 800, 1000, .. 3800 Hz. High digit: 4400 .. 7400 Hz.
pub fn freqs(symbol: u32) -> (f64, f64) {
    let lo = symbol % 16;
    let hi = (symbol / 16) % 16;
    (800.0 + 200.0 * f64::from(lo), 4400.0 + 200.0 * f64::from(hi))
}

/// Fills `out` with the signal starting at absolute sample `at`.
pub fn tones(at: u64, out: &mut [i16]) {
    for (i, o) in out.iter_mut().enumerate() {
        let n = at + i as u64;
        let symbol = (n / SYMBOL as u64) as u32 % 256;
        let within = (n % SYMBOL as u64) as usize;
        let (f1, f2) = freqs(symbol);
        // 5 ms raised-cosine edges so that a change of tone does not click.
        let edge = 240usize;
        let gain = if within < edge {
            0.5 - 0.5 * (std::f64::consts::PI * within as f64 / edge as f64).cos()
        } else if within >= SYMBOL - edge {
            0.5 - 0.5 * (std::f64::consts::PI * (SYMBOL - within) as f64 / edge as f64).cos()
        } else {
            1.0
        };
        let t = within as f64 / RATE as f64;
        let v = 0.25 * (TAU * f1 * t).sin() + 0.25 * (TAU * f2 * t).sin();
        *o = (v * gain * 32767.0) as i16;
    }
}

fn goertzel(block: &[f32], freq: f64) -> f64 {
    let w = TAU * freq / RATE as f64;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in block {
        let s0 = f64::from(x) + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - c * s1 * s2
}

/// What one 10 ms block holds: the symbol if both digits stand out, and its energy.
pub fn detect(block: &[f32]) -> (Option<u32>, f64) {
    let energy: f64 = block.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>() / block.len() as f64;
    if energy < 1e-5 {
        return (None, energy);
    }
    let mut best = [(0u32, 0.0f64); 2];
    let mut total = [0.0f64; 2];
    for d in 0..16u32 {
        let lo = goertzel(block, 800.0 + 200.0 * f64::from(d));
        let hi = goertzel(block, 4400.0 + 200.0 * f64::from(d));
        total[0] += lo;
        total[1] += hi;
        if lo > best[0].1 {
            best[0] = (d, lo);
        }
        if hi > best[1].1 {
            best[1] = (d, hi);
        }
    }
    if best[0].1 > 0.6 * total[0] && best[1].1 > 0.6 * total[1] {
        (Some(best[1].0 * 16 + best[0].0), energy)
    } else {
        (None, energy)
    }
}

/// Reads a 16-bit mono WAV and brings it to 48 kHz by linear interpolation.
pub fn load_speech(path: &str) -> Vec<i16> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Some(data_at) = bytes.windows(4).position(|w| w == b"data") else {
        return Vec::new();
    };
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize;
    let pcm: Vec<i16> =
        bytes[data_at + 8..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
    if rate == RATE || pcm.is_empty() {
        return pcm;
    }
    let n = pcm.len() * RATE / rate;
    (0..n)
        .map(|i| {
            let x = i as f64 * rate as f64 / RATE as f64;
            let a = x.floor() as usize;
            let b = (a + 1).min(pcm.len() - 1);
            let f = x - a as f64;
            (f64::from(pcm[a]) * (1.0 - f) + f64::from(pcm[b]) * f) as i16
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_symbol_is_read_back() {
        for symbol in 0..256u32 {
            let mut pcm = vec![0i16; SYMBOL];
            tones(u64::from(symbol) * SYMBOL as u64, &mut pcm);
            // The middle 10 ms blocks, away from the edges.
            for b in 2..8 {
                let block: Vec<f32> =
                    pcm[b * BLOCK..(b + 1) * BLOCK].iter().map(|s| f32::from(*s) / 32768.0).collect();
                assert_eq!(detect(&block).0, Some(symbol));
            }
        }
    }
}

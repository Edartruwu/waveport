//! What was sent of each response, and where in the stream: so that a
//! far end that says where in the stream it is can be told apart into
//! how much of which response was played.
//!
//! A browser knows nothing of responses. What it can say is the RTP
//! time of the audio it played last, or how much audio has come out of
//! its buffer since the call began. Both are places in one line: all
//! the audio that was sent, one response after the other.

use std::collections::VecDeque;

use waveport_domain::ids::OpTag;

/// How many responses are remembered.
const RESPONSES: usize = 64;

/// How many stretches of one response are remembered. A response is one
/// stretch unless its audio ran out on the way and went on later.
const STRETCHES: usize = 16;

/// How far back in the stream a time is still told from one ahead: a
/// quarter of the range RTP time has, which is more than six hours.
const FAR_BACK: u32 = 1 << 30;

/// Audio of one response that was sent without a break.
#[derive(Clone, Copy, Debug)]
struct Stretch {
    /// The RTP time it begins at.
    time: u32,
    /// How many samples it holds.
    samples: u32,
    /// How many samples were sent before it, of every response.
    before: u64,
}

/// What was sent of one response.
#[derive(Debug)]
struct Response {
    tag: OpTag,
    stretches: VecDeque<Stretch>,
    /// How much of it the session was last told was played.
    told: u32,
}

/// What was sent on a call, the newest last.
#[derive(Debug, Default)]
pub(crate) struct Sent {
    responses: VecDeque<Response>,
    /// How many samples were sent in all.
    total: u64,
    /// Where the audio that was sent last ends.
    ends: Option<u32>,
    /// The place the far end said last.
    place: Option<u64>,
}

impl Sent {
    /// Audio of the response `tag` was sent: `samples` of it from the
    /// RTP time `time` on.
    pub(crate) fn add(&mut self, tag: OpTag, time: u32, samples: u32) {
        if self.responses.back().is_none_or(|last| last.tag != tag) {
            if self.responses.len() >= RESPONSES {
                self.responses.pop_front();
            }
            self.responses.push_back(Response {
                tag,
                stretches: VecDeque::new(),
                told: 0,
            });
        }
        let follows = self.ends == Some(time);
        if let Some(response) = self.responses.back_mut() {
            match response.stretches.back_mut() {
                Some(last) if follows => last.samples = last.samples.saturating_add(samples),
                _ => {
                    if response.stretches.len() >= STRETCHES {
                        response.stretches.pop_front();
                    }
                    response.stretches.push_back(Stretch {
                        time,
                        samples,
                        before: self.total,
                    });
                }
            }
        }
        self.total += u64::from(samples);
        let ends = time.wrapping_add(samples);
        self.ends = Some(ends);
        // RTP time wraps round: what is too far back to be told from
        // what is ahead is forgotten.
        let old = |response: &Response| {
            response
                .stretches
                .back()
                .is_none_or(|last| ends.wrapping_sub(last.time) >= FAR_BACK)
        };
        while self.responses.len() > 1 && self.responses.front().is_some_and(old) {
            self.responses.pop_front();
        }
    }

    /// Where in all that was sent the RTP time `time` is: how many
    /// samples were sent before it. `None` for a time before anything
    /// that is remembered.
    pub(crate) fn before(&self, time: u32) -> Option<u64> {
        let ends = self.ends?;
        // No more can have been played than was sent.
        if time.wrapping_sub(ends) < FAR_BACK {
            return Some(self.total);
        }
        let stretches = self.responses.iter().rev();
        let mut stretches = stretches.flat_map(|response| response.stretches.iter().rev());
        stretches.find_map(|stretch| {
            let into = time.wrapping_sub(stretch.time);
            // A time between two stretches is one at which nothing was
            // sent: the stretch before it was played, and no more.
            (into < FAR_BACK).then(|| stretch.before + u64::from(into.min(stretch.samples)))
        })
    }

    /// The far end has played the first `place` samples of all that was
    /// sent. `told` is given each response of which that says something
    /// new, with how many of its samples were played.
    pub(crate) fn played(&mut self, place: u64, mut told: impl FnMut(OpTag, u32)) {
        let place = place.min(self.total);
        if self.place == Some(place) {
            return;
        }
        self.place = Some(place);
        for response in &mut self.responses {
            let played = response.stretches.iter().map(|stretch| {
                let into = place.saturating_sub(stretch.before);
                u32::try_from(into).map_or(stretch.samples, |into| into.min(stretch.samples))
            });
            let played = played.fold(0_u32, u32::saturating_add);
            if played != response.told {
                response.told = played;
                told(response.tag, played);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use waveport_domain::ids::{Epoch, GenId, OpTag};

    use super::Sent;

    const fn tag(generation: u32) -> OpTag {
        OpTag {
            generation: GenId(generation),
            epoch: Epoch(1),
        }
    }

    fn told(sent: &mut Sent, place: u64) -> Vec<(u32, u32)> {
        let mut said = Vec::new();
        sent.played(place, |tag, played| said.push((tag.generation.0, played)));
        said
    }

    #[test]
    fn a_time_in_the_stream_is_a_place_in_what_was_sent() {
        let mut sent = Sent::default();
        assert_eq!(sent.before(5), None);
        // Three frames of one response, a pause, and two of the next.
        for frame in 0..3 {
            sent.add(tag(1), 10_000 + frame * 960, 960);
        }
        sent.add(tag(2), 50_000, 960);
        sent.add(tag(2), 50_960, 960);
        assert_eq!(sent.before(10_000), Some(0));
        assert_eq!(sent.before(10_001), Some(1));
        assert_eq!(sent.before(12_879), Some(2_879));
        // The end of one, the pause after it, and the beginning of the
        // next are one place.
        for time in [12_880, 30_000, 50_000] {
            assert_eq!(sent.before(time), Some(2_880), "{time}");
        }
        assert_eq!(sent.before(50_001), Some(2_881));
        assert_eq!(sent.before(51_919), Some(4_799));
        // No more was played than was sent, and what was before all of
        // it is no place.
        for time in [51_920, 51_921, 51_920 + (1 << 30) - 1] {
            assert_eq!(sent.before(time), Some(4_800), "{time}");
        }
        for time in [9_999, 0, 51_920 + (1 << 30)] {
            assert_eq!(sent.before(time), None, "{time}");
        }
    }

    #[test]
    fn a_place_says_how_much_of_each_response_was_played() {
        let mut sent = Sent::default();
        for frame in 0..3 {
            sent.add(tag(1), 10_000 + frame * 960, 960);
        }
        sent.add(tag(2), 50_000, 960);
        assert_eq!(told(&mut sent, 0), []);
        assert_eq!(told(&mut sent, 960), [(1, 960)]);
        // Said again, it says nothing new.
        assert_eq!(told(&mut sent, 960), []);
        // Into the second: the first was played to its end.
        assert_eq!(told(&mut sent, 3_000), [(1, 2_880), (2, 120)]);
        assert_eq!(told(&mut sent, 3_100), [(2, 220)]);
        // A far end may say less than it said before.
        assert_eq!(told(&mut sent, 1_920), [(1, 1_920), (2, 0)]);
        // More than there is, is all there is; and what is sent later
        // is not played yet.
        assert_eq!(told(&mut sent, u64::MAX), [(1, 2_880), (2, 960)]);
        sent.add(tag(2), 50_960, 960);
        assert_eq!(told(&mut sent, 3_840), []);
        assert_eq!(told(&mut sent, 4_800), [(2, 1_920)]);
    }

    #[test]
    fn a_response_that_went_on_after_a_break_is_one_response() {
        let mut sent = Sent::default();
        sent.add(tag(1), 0, 960);
        // Its audio ran out, and went on 100 ms later.
        sent.add(tag(1), 960 + 4_800, 960);
        assert_eq!(sent.before(960 + 2_000), Some(960));
        assert_eq!(sent.before(960 + 4_800 + 10), Some(970));
        assert_eq!(told(&mut sent, 970), [(1, 970)]);
        // RTP time that wraps round is still one line.
        let mut sent = Sent::default();
        sent.add(tag(1), u32::MAX - 959, 960);
        sent.add(tag(1), 0, 960);
        assert_eq!(sent.before(u32::MAX - 959), Some(0));
        assert_eq!(sent.before(u32::MAX), Some(959));
        assert_eq!(sent.before(0), Some(960));
        assert_eq!(sent.before(500), Some(1_460));
    }

    #[test]
    fn a_response_of_any_length_is_played_from_its_first_frame() {
        // Five seconds without a break: as many frames as that is are
        // one stretch of the stream, and not one each.
        let mut sent = Sent::default();
        for n in 0..250_u32 {
            sent.add(tag(1), 1_000 + n * 960, 960);
        }
        assert_eq!(sent.before(1_000 + 960), Some(960));
        assert_eq!(sent.before(1_000 + 100 * 960), Some(100 * 960));
        assert_eq!(told(&mut sent, 960), [(1, 960)]);
        assert_eq!(told(&mut sent, 250 * 960), [(1, 250 * 960)]);
    }

    #[test]
    fn only_the_last_responses_are_remembered() {
        let mut sent = Sent::default();
        for generation in 0..100_u32 {
            sent.add(tag(generation), generation * 2_000, 960);
        }
        assert_eq!(sent.responses.len(), 64);
        // The oldest that is remembered is the 37th, and what was
        // before it is no place.
        assert_eq!(sent.before(36 * 2_000), Some(36 * 960));
        assert_eq!(sent.before(36 * 2_000 - 1), None);
        // A response that is far back in time is forgotten, however
        // few there are.
        let mut sent = Sent::default();
        sent.add(tag(1), 0, 960);
        sent.add(tag(2), 1 << 30, 960);
        assert_eq!(sent.responses.len(), 1);
        assert_eq!(sent.before(0), None);
        // One response of many breaks keeps the last of them.
        let mut sent = Sent::default();
        for stretch in 0..40_u32 {
            sent.add(tag(1), stretch * 5_000, 960);
        }
        assert_eq!(sent.responses.front().map(|r| r.stretches.len()), Some(16));
    }
}

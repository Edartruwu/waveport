//! What both ends of a call are made of: `str0m`, set up as this crate
//! uses it.

use core::time::Duration;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use str0m::config::{CryptoProvider, DtlsVersion};
use str0m::format::{Codec, CodecConfig, FormatParams};
use str0m::media::Frequency;
use str0m::{Rtc, RtcConfig};

use crate::crypto::{self, Certificate};

/// The payload types this end gives the keys of a telephone where it is
/// the one that names them. An end that answers takes those of the
/// offer.
const KEYS_AT_48: u8 = 110;
const KEYS_AT_8: u8 = 126;

/// How often `str0m` says what the far end reports of the audio it was
/// sent.
const REPORTS: Duration = Duration::from_secs(1);

/// The clocks the keys of a telephone are offered or answered with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyClocks {
    /// The clock of the audio.
    pub(crate) at_48: bool,
    /// The clock of a telephone line.
    pub(crate) at_8: bool,
}

/// What one end of a call is to be able to do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is something an end does or does not, and none follows from another"
)]
pub(crate) struct Wanted {
    /// Redundant audio (RFC 2198).
    pub(crate) red: bool,
    /// Asking for audio that was lost to be sent again.
    pub(crate) nack: bool,
    /// The keys of a telephone as events (RFC 4733).
    pub(crate) keys: KeyClocks,
    /// Whether this end only answers the checks of ICE, as a server
    /// with an address of its own does.
    pub(crate) lite: bool,
    /// Whether DTLS 1.3 is spoken with a far end that has it.
    pub(crate) dtls13: bool,
}

fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(crypto::provider())))
}

/// One end of a call that has not begun.
pub(crate) fn rtc(certificate: &Certificate, wanted: Wanted, now: Instant) -> Rtc {
    let mut config = RtcConfig::new()
        .set_crypto_provider(provider())
        .set_dtls_cert(certificate.clone().into_dtls())
        .set_ice_lite(wanted.lite)
        // Every packet as it came, with its number and its time: the
        // session has the one buffer that puts audio in order.
        .set_rtp_mode(true)
        .set_stats_interval(Some(REPORTS))
        .set_dtls_version(if wanted.dtls13 {
            DtlsVersion::Auto
        } else {
            DtlsVersion::Dtls12
        });

    // Audio, and nothing else: Opus, with redundancy folded on to it,
    // and the keys.
    let mut codecs = CodecConfig::empty();
    codecs.enable_opus(true, wanted.red);
    let keys = [
        (wanted.keys.at_48, KEYS_AT_48, Frequency::FORTY_EIGHT_KHZ),
        (wanted.keys.at_8, KEYS_AT_8, Frequency::EIGHT_KHZ),
    ];
    for (_, kind, clock) in keys.into_iter().filter(|(wanted, ..)| *wanted) {
        let events = FormatParams::parse_line("0-16");
        codecs.add_config(kind.into(), None, Codec::Tele, clock, None, events);
    }
    if wanted.nack {
        let mut kinds = codecs.params().to_vec();
        for kind in &mut kinds {
            if kind.spec().codec == Codec::Opus {
                kind.set_fb_nack(true);
            }
        }
        codecs = CodecConfig::new_from_payload_params(kinds);
    }
    // The frame before, and no further back.
    codecs.set_red_distances(&[1]);
    *config.codec_config() = codecs;
    config.build(now)
}

/// What an offer asks for, as far as an answer depends on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Offered {
    /// A line of audio.
    pub(crate) audio: bool,
    /// Redundant audio.
    pub(crate) red: bool,
    /// The keys of a telephone, and on which clocks.
    pub(crate) keys: KeyClocks,
    /// A data channel.
    pub(crate) channel: bool,
}

impl Offered {
    /// Reads an offer for what it names. What it means is `str0m`'s to
    /// read: this only looks for names.
    pub(crate) fn read(sdp: &str) -> Self {
        let mut offered = Self::default();
        for line in sdp.lines().map(str::trim) {
            if line.starts_with("m=audio ") {
                offered.audio = true;
            } else if line.starts_with("m=application ") {
                offered.channel = true;
            } else if let Some(map) = line.strip_prefix("a=rtpmap:") {
                // `<payload type> <name>/<clock>[/<channels>]`
                let Some((_, coding)) = map.split_once(' ') else {
                    continue;
                };
                let mut parts = coding.trim().split('/');
                let name = parts.next().unwrap_or_default();
                let clock = parts.next().unwrap_or_default();
                if name.eq_ignore_ascii_case("red") && clock == "48000" {
                    offered.red = true;
                } else if name.eq_ignore_ascii_case("telephone-event") {
                    offered.keys.at_48 |= clock == "48000";
                    offered.keys.at_8 |= clock == "8000";
                }
            }
        }
        offered
    }

    /// What an answer to it is made with.
    pub(crate) const fn answered(self, dtls13: bool) -> Wanted {
        Wanted {
            red: self.red,
            // A far end that sends no redundant audio is asked to send
            // again what was lost. Not both: while it is asked, the
            // reports it is sent show no loss, and it never protects
            // its audio by itself.
            nack: !self.red,
            // Offered both clocks, a browser sends on the telephone's
            // and counts as if it were the audio's.
            keys: KeyClocks {
                at_48: self.keys.at_48,
                at_8: self.keys.at_8 && !self.keys.at_48,
            },
            lite: true,
            dtls13,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyClocks, Offered};

    const CHROME: &str = "v=0\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        a=rtpmap:63 red/48000/2\r\n\
        a=rtpmap:9 G722/8000\r\n\
        a=rtpmap:110 telephone-event/48000\r\n\
        a=rtpmap:126 telephone-event/8000\r\n\
        m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";

    const FIREFOX: &str = "v=0\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 109 9 0 8 101\n\
        a=rtpmap:109 opus/48000/2\n\
        a=rtpmap:101 telephone-event/8000\n";

    #[test]
    fn an_offer_is_read_for_what_it_names() {
        let chrome = Offered::read(CHROME);
        let both = KeyClocks {
            at_48: true,
            at_8: true,
        };
        assert_eq!(
            chrome,
            Offered {
                audio: true,
                red: true,
                keys: both,
                channel: true
            }
        );
        let firefox = Offered::read(FIREFOX);
        let telephone = KeyClocks {
            at_48: false,
            at_8: true,
        };
        assert_eq!(
            firefox,
            Offered {
                audio: true,
                red: false,
                keys: telephone,
                channel: false
            }
        );
        // The redundancy of video is not that of audio, and a line that
        // is cut short names nothing.
        let other = Offered::read("m=video 9 x 96\na=rtpmap:96 red/90000\na=rtpmap:97\n");
        assert_eq!(other, Offered::default());
    }

    #[test]
    fn an_answer_takes_one_repair_and_one_clock_for_keys() {
        let chrome = Offered::read(CHROME).answered(false);
        assert!(chrome.red && !chrome.nack && chrome.lite && !chrome.dtls13);
        assert_eq!(
            chrome.keys,
            KeyClocks {
                at_48: true,
                at_8: false
            }
        );
        let firefox = Offered::read(FIREFOX).answered(true);
        assert!(!firefox.red && firefox.nack && firefox.dtls13);
        assert_eq!(
            firefox.keys,
            KeyClocks {
                at_48: false,
                at_8: true
            }
        );
        assert_eq!(
            Offered::default().answered(false).keys,
            KeyClocks::default()
        );
    }
}

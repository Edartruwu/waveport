//! The part of a description that is about the checks of ICE, by
//! itself: the name and the password they are made with, and where an
//! end is reached (RFC 8840).
//!
//! A far end that is somewhere else than it was begins its checks anew.
//! Where a call is set up as WHIP sets one up (RFC 9725, section 4.3),
//! it does not offer again for that: it sends this part of its new
//! offer where it made the first, and is answered with this part of
//! the answer. Whoever takes a fragment puts it into the description
//! the fragment is a part of, and has the offer or the answer that was
//! not sent.
//!
//! Nothing here checks a description. What is made of a fragment is
//! read by whoever answers offers, as any offer is.

use core::fmt::Write as _;

const UFRAG: &str = "a=ice-ufrag:";
const PWD: &str = "a=ice-pwd:";
const CANDIDATE: &str = "a=candidate:";
const NO_MORE: &str = "a=end-of-candidates";
const MID: &str = "a=mid:";
const MEDIA: &str = "m=";

/// How long a name of checks may be, and how short (RFC 8839, section
/// 5.4).
const UFRAG_LENGTHS: core::ops::RangeInclusive<usize> = 4..=256;

/// And a password.
const PWD_LENGTHS: core::ops::RangeInclusive<usize> = 22..=256;

/// How many places an end may say it is reached at in one fragment.
/// A browser on a machine with many networks says a few dozen.
const PLACES: usize = 64;

/// The lines of `text`, whichever way they end.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|line| !line.is_empty())
}

/// Whether `value` is made of what the names and passwords of checks
/// are made of, and is as long as one of them is.
fn is_ice(value: &str, lengths: &core::ops::RangeInclusive<usize>) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || c == '+' || c == '/';
    lengths.contains(&value.len()) && value.chars().all(plain)
}

/// The name and the password that `text` gives its checks: those of a
/// description, or of a fragment. `None` where it gives none, or gives
/// what can be neither.
#[must_use]
pub fn credentials(text: &str) -> Option<(&str, &str)> {
    let ufrag = lines(text).find_map(|line| line.strip_prefix(UFRAG))?;
    let pwd = lines(text).find_map(|line| line.strip_prefix(PWD))?;
    (is_ice(ufrag, &UFRAG_LENGTHS) && is_ice(pwd, &PWD_LENGTHS)).then_some((ufrag, pwd))
}

/// Whether a line says where an end is reached, and nothing a line may
/// not hold.
fn is_a_place(line: &str) -> bool {
    line.starts_with(CANDIDATE) && line.len() <= 512 && !line.chars().any(char::is_control)
}

/// The part of `sdp` that is about its checks, as a fragment: what the
/// checks are made with, the line of its first stream and where that is
/// reached. `None` for a description that says nothing of checks or
/// has no stream.
#[must_use]
pub fn of(sdp: &str) -> Option<String> {
    let (ufrag, pwd) = credentials(sdp)?;
    let mut fragment = String::new();
    let mut push = |line: &str| {
        let _ = write!(fragment, "{line}\r\n");
    };
    // What is said of the whole description, before its first stream.
    for line in lines(sdp).take_while(|line| !line.starts_with(MEDIA)) {
        let of_ice = line == "a=ice-lite" || line.starts_with("a=ice-options:");
        if of_ice || line.starts_with("a=group:BUNDLE") {
            push(line);
        }
    }
    let mut streams = lines(sdp).skip_while(|line| !line.starts_with(MEDIA));
    push(streams.next()?);
    let first: Vec<&str> = streams
        .take_while(|line| !line.starts_with(MEDIA))
        .collect();
    if let Some(mid) = first.iter().find(|line| line.starts_with(MID)) {
        push(mid);
    }
    push(&format!("{UFRAG}{ufrag}"));
    push(&format!("{PWD}{pwd}"));
    // The streams of a call share one way, which is the first's.
    for place in first.iter().filter(|line| is_a_place(line)).take(PLACES) {
        push(place);
    }
    if lines(sdp).any(|line| line == NO_MORE) {
        push(NO_MORE);
    }
    Some(fragment)
}

/// `sdp` with what `fragment` says of checks in place of what it said
/// itself: the description that the fragment is a part of. `None` where
/// the fragment names no checks, or the description has none to
/// replace.
///
/// Nothing else of the fragment is taken: a line of it that is not
/// about checks is passed over.
#[must_use]
pub fn into(sdp: &str, fragment: &str) -> Option<String> {
    let (ufrag, pwd) = credentials(fragment)?;
    credentials(sdp)?;
    let places: Vec<&str> = lines(fragment)
        .filter(|line| is_a_place(line))
        .take(PLACES)
        .collect();
    let no_more = lines(fragment).any(|line| line == NO_MORE);
    // Where the fragment says its places are of: the stream it names,
    // or the first.
    let named = lines(fragment).find(|line| line.starts_with(MID));
    let streams = lines(sdp).filter(|line| line.starts_with(MEDIA)).count();
    if streams == 0 {
        return None;
    }
    let of_stream = |wanted: &str| {
        let mut stream = 0;
        for line in lines(sdp) {
            if line.starts_with(MEDIA) {
                stream += 1;
            } else if line == wanted && stream > 0 {
                return Some(stream);
            }
        }
        None
    };
    let target = named.and_then(of_stream).unwrap_or(1);

    let mut made = String::with_capacity(sdp.len() + fragment.len());
    let mut stream = 0;
    let put = |made: &mut String, stream: usize| {
        if stream == target {
            for place in &places {
                let _ = write!(made, "{place}\r\n");
            }
            if no_more {
                let _ = write!(made, "{NO_MORE}\r\n");
            }
        }
    };
    for line in lines(sdp) {
        if line.starts_with(MEDIA) {
            // What the stream before is reached at goes at its end.
            put(&mut made, stream);
            stream += 1;
        }
        if line.starts_with(CANDIDATE) || line == NO_MORE {
            continue;
        }
        if line.starts_with(UFRAG) {
            let _ = write!(made, "{UFRAG}{ufrag}\r\n");
        } else if line.starts_with(PWD) {
            let _ = write!(made, "{PWD}{pwd}\r\n");
        } else {
            let _ = write!(made, "{line}\r\n");
        }
    }
    put(&mut made, stream);
    Some(made)
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;

    use super::{credentials, into, of};

    /// An offer as a browser makes one, cut down to what matters here.
    /// Every name and address in it is made up.
    const OFFER: &str = "v=0\r\n\
        o=- 1 2 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=group:BUNDLE 0 1\r\n\
        a=ice-options:trickle ice2\r\n\
        m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=ice-ufrag:EsAw\r\n\
        a=ice-pwd:bP+XJMM09aR8AiX1jdukzR6Y\r\n\
        a=candidate:1 1 udp 2122260223 192.0.2.1 61764 typ host\r\n\
        a=candidate:2 1 tcp 1518280447 192.0.2.1 9 typ host tcptype active\r\n\
        a=end-of-candidates\r\n\
        a=mid:0\r\n\
        a=rtpmap:111 opus/48000/2\r\n\
        m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=ice-ufrag:EsAw\r\n\
        a=ice-pwd:bP+XJMM09aR8AiX1jdukzR6Y\r\n\
        a=mid:1\r\n\
        a=sctp-port:5000\r\n";

    #[test]
    fn the_part_of_a_description_that_is_about_checks_is_taken_out_of_it() {
        assert_eq!(
            credentials(OFFER),
            Some(("EsAw", "bP+XJMM09aR8AiX1jdukzR6Y"))
        );
        let fragment = of(OFFER).unwrap();
        // What RFC 9725 shows a fragment to be: what is said of the
        // whole, the line of the first stream and its name, what the
        // checks are made with, and where the end is reached.
        assert_eq!(
            fragment,
            "a=group:BUNDLE 0 1\r\n\
             a=ice-options:trickle ice2\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             a=mid:0\r\n\
             a=ice-ufrag:EsAw\r\n\
             a=ice-pwd:bP+XJMM09aR8AiX1jdukzR6Y\r\n\
             a=candidate:1 1 udp 2122260223 192.0.2.1 61764 typ host\r\n\
             a=candidate:2 1 tcp 1518280447 192.0.2.1 9 typ host tcptype active\r\n\
             a=end-of-candidates\r\n"
        );
        // Whichever way the lines of a description end.
        assert_eq!(of(&OFFER.replace("\r\n", "\n")).unwrap(), fragment);
        // An answer of a server that only answers checks says so, and
        // its fragment does.
        let lite = OFFER.replace("a=group", "a=ice-lite\r\na=group");
        assert!(
            of(&lite)
                .unwrap()
                .starts_with("a=ice-lite\r\na=group:BUNDLE")
        );
        // A description with no checks, or with no stream, has no such
        // part.
        assert_eq!(of("v=0\r\ns=-\r\n"), None);
        assert_eq!(
            of("v=0\r\na=ice-ufrag:EsAw\r\na=ice-pwd:bP+XJMM09aR8AiX1jdukzR6Y\r\n"),
            None
        );
    }

    #[test]
    fn a_fragment_is_put_into_the_description_it_is_a_part_of() {
        // The far end is somewhere else, and its checks have another
        // name and password. It has said one place it is reached at.
        let fragment = "a=ice-options:trickle ice2\r\n\
            a=group:BUNDLE 0 1\r\n\
            m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
            a=mid:0\r\n\
            a=ice-ufrag:ysXw\r\n\
            a=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n\
            a=candidate:9 1 udp 2122260223 198.51.100.2 61765 typ host\r\n";
        let made = into(OFFER, fragment).unwrap();
        let expected = OFFER
            .replace("EsAw", "ysXw")
            .replace("bP+XJMM09aR8AiX1jdukzR6Y", "vw5LmwG4y/e6dPP/zAP9Gp5k")
            .replace(
                "a=candidate:1 1 udp 2122260223 192.0.2.1 61764 typ host\r\n\
                 a=candidate:2 1 tcp 1518280447 192.0.2.1 9 typ host tcptype active\r\n\
                 a=end-of-candidates\r\n",
                "",
            )
            .replace(
                "a=rtpmap:111 opus/48000/2\r\n",
                "a=rtpmap:111 opus/48000/2\r\n\
                 a=candidate:9 1 udp 2122260223 198.51.100.2 61765 typ host\r\n",
            );
        assert_eq!(made, expected);
        // What is made holds what the fragment said, and nothing of
        // what it replaced.
        assert_eq!(of(&made).unwrap(), {
            let mut said = String::from("a=group:BUNDLE 0 1\r\na=ice-options:trickle ice2\r\n");
            said.push_str("m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=mid:0\r\n");
            said.push_str("a=ice-ufrag:ysXw\r\na=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n");
            said.push_str("a=candidate:9 1 udp 2122260223 198.51.100.2 61765 typ host\r\n");
            said
        });

        // A fragment that says where it is reached for the second
        // stream, and that there is nowhere else.
        let second = "a=mid:1\r\n\
            a=ice-ufrag:ysXw\r\n\
            a=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n\
            a=candidate:9 1 udp 2122260223 198.51.100.2 61765 typ host\r\n\
            a=end-of-candidates\r\n";
        let made = into(OFFER, second).unwrap();
        assert!(made.ends_with(
            "a=sctp-port:5000\r\n\
             a=candidate:9 1 udp 2122260223 198.51.100.2 61765 typ host\r\n\
             a=end-of-candidates\r\n"
        ));
        assert_eq!(made.matches("a=candidate:").count(), 1);
        // One that names a stream there is none of is of the first.
        let nowhere = second.replace("a=mid:1", "a=mid:7");
        let made = into(OFFER, &nowhere).unwrap();
        assert!(made.contains("opus/48000/2\r\na=candidate:9 "));
        // One that says no place at all leaves none.
        let bare = "a=ice-ufrag:ysXw\r\na=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n";
        let made = into(OFFER, bare).unwrap();
        assert_eq!(made.matches("a=candidate:").count(), 0);
        assert!(!made.contains("a=end-of-candidates"));
        assert_eq!(made.matches("a=ice-ufrag:ysXw\r\n").count(), 2);
    }

    #[test]
    fn nothing_but_what_is_about_checks_is_taken_from_a_fragment() {
        // A line of another kind is not put into the description,
        // whatever it says.
        let fragment = "a=ice-ufrag:ysXw\r\n\
            a=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n\
            a=fingerprint:sha-256 00:11\r\n\
            a=setup:active\r\n\
            m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
            a=candidate:9 1 udp 1 198.51.100.2 61765 typ host\r\n";
        let made = into(OFFER, fragment).unwrap();
        assert!(!made.contains("fingerprint"));
        assert!(!made.contains("a=setup"));
        assert!(!made.contains("m=video"));
        assert_eq!(made.matches("m=").count(), 2);

        // A fragment with no name or no password for its checks, or
        // with one that can be none, is no fragment to put anywhere.
        for fragment in [
            "",
            "a=ice-ufrag:ysXw\r\n",
            "a=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n",
            "a=ice-ufrag:ys\r\na=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n",
            "a=ice-ufrag:ysXw\r\na=ice-pwd:short\r\n",
            "a=ice-ufrag:ys Xw\r\na=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n",
            "a=ice-ufrag:ysXw\r\na=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k!\r\n",
        ] {
            assert_eq!(into(OFFER, fragment), None, "{fragment:?}");
            assert_eq!(credentials(fragment), None, "{fragment:?}");
        }
        // And neither is a description that has none to replace, or no
        // stream.
        let good = "a=ice-ufrag:ysXw\r\na=ice-pwd:vw5LmwG4y/e6dPP/zAP9Gp5k\r\n";
        assert_eq!(into("v=0\r\nm=audio 9 RTP/AVP 0\r\n", good), None);
        assert_eq!(into(good, good), None);

        // No more places are taken than an end has.
        let mut many = String::from(good);
        for n in 0..100 {
            let _ = write!(
                many,
                "a=candidate:{n} 1 udp 1 198.51.100.2 61765 typ host\r\n"
            );
        }
        let made = into(OFFER, &many).unwrap();
        assert_eq!(made.matches("a=candidate:").count(), 64);
        assert_eq!(of(&made).unwrap().matches("a=candidate:").count(), 64);
        let crowded = OFFER.replace("a=end-of-candidates\r\n", &many.replace(good, ""));
        assert_eq!(crowded.matches("a=candidate:").count(), 102);
        assert_eq!(of(&crowded).unwrap().matches("a=candidate:").count(), 64);
        // And none that is longer than a place is, or holds what a line
        // does not: neither out of a fragment nor into one.
        let long = format!("a=candidate:{}\r\n", "9".repeat(600));
        let odd = "a=candidate:3 1 udp 1 198.51.100.2\u{7}61765 typ host\r\n";
        let place = "a=candidate:4 1 udp 1 198.51.100.2 61765 typ host\r\n";
        let made = into(OFFER, &format!("{good}{long}{odd}{place}")).unwrap();
        assert_eq!(made.matches("a=candidate:").count(), 1, "{made}");
        assert!(made.contains(place), "{made}");
        let said = OFFER.replace(
            "a=end-of-candidates\r\n",
            &format!("{long}{odd}a=end-of-candidates\r\n"),
        );
        assert_eq!(of(&said).unwrap().matches("a=candidate:").count(), 2);
    }
}

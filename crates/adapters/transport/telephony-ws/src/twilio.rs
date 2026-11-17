//! Twilio's Media Streams, as its documentation has them.
//!
//! Read on 17 November 2026: "Media Streams overview", "Media Streams
//! WebSocket messages", the pages of the instructions `<Stream>` and
//! `<Connect>`, and "Security" for how a request is signed. Nothing
//! here was held against a call yet.
//!
//! Everything in this module is a function of what it is given: what a
//! message says, what is written, what an answer document holds, and
//! whether a request was signed with the account's token.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use ring::{digest, hmac};
use serde::{Deserialize, Serialize};
use waveport_adapter_kit::secret::Secret;
use waveport_domain::event::Key;

use crate::{Asked, Heard, Malformed, Started, Unanswerable, Unverified};

/// The header a request of Twilio's is signed in.
const SIGNATURE: &str = "x-twilio-signature";

/// The header that says a request was signed in the newer way, with a
/// key of its own and another hash, which an account turns on for
/// itself.
const SIGNED_OTHERWISE: &str = "x-twilio-webhooks-signature-settings";

/// How long the name and the value of a `<Parameter>` may be together.
const PARAMETER_AT_MOST: usize = 500;

/// What Twilio says on a stream.
#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Said {
    Connected {},
    Start {
        start: Start,
    },
    Media {
        media: Media,
    },
    Dtmf {
        dtmf: Dtmf,
    },
    Mark {
        mark: Mark,
    },
    Stop {},
    /// An event of a kind that is not known here.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Start {
    stream_sid: String,
    call_sid: String,
    #[serde(default)]
    account_sid: Option<String>,
    media_format: MediaFormat,
    #[serde(default)]
    custom_parameters: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MediaFormat {
    encoding: String,
    sample_rate: u32,
    channels: u32,
}

#[derive(Deserialize)]
struct Media {
    #[serde(default)]
    track: Option<String>,
    payload: String,
}

#[derive(Deserialize)]
struct Dtmf {
    digit: String,
}

#[derive(Deserialize, Serialize)]
struct Mark {
    name: String,
}

/// What Twilio says in one message.
pub(crate) fn read(text: &str) -> Result<Heard, Malformed> {
    let said: Said = serde_json::from_str(text).map_err(|_| Malformed::Message)?;
    Ok(match said {
        Said::Connected {} => Heard::Hello,
        Said::Start { start } => {
            // It is "always" this, the documentation says. A stream of
            // anything else would be played as noise.
            let format = start.media_format;
            let ours = format.encoding == "audio/x-mulaw"
                && format.sample_rate == 8_000
                && format.channels == 1;
            if !ours {
                return Err(Malformed::Format);
            }
            Heard::Start(Started {
                stream: start.stream_sid,
                call: start.call_sid,
                account: start.account_sid,
                parameters: start.custom_parameters,
            })
        }
        Said::Media { media } => {
            // A stream that a call is connected to carries what the
            // caller says and nothing else.
            if media
                .track
                .as_deref()
                .is_some_and(|track| track != "inbound")
            {
                return Ok(Heard::Other);
            }
            let audio = STANDARD
                .decode(media.payload)
                .map_err(|_| Malformed::Audio)?;
            Heard::Audio(Bytes::from(audio))
        }
        Said::Dtmf { dtmf } => {
            let mut digits = dtmf.digit.chars();
            match (digits.next().and_then(Key::from_char), digits.next()) {
                (Some(key), None) => Heard::Key(key),
                _ => Heard::Other,
            }
        }
        Said::Mark { mark } => Heard::Mark(mark.name),
        Said::Stop {} => Heard::Stop,
        Said::Other => Heard::Other,
    })
}

/// What is sent to Twilio. It warns of a field it does not know, so a
/// message holds what its documentation shows and nothing more.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
enum Sent<'a> {
    Media {
        #[serde(rename = "streamSid")]
        stream: &'a str,
        media: Payload,
    },
    Mark {
        #[serde(rename = "streamSid")]
        stream: &'a str,
        mark: Mark,
    },
    Clear {
        #[serde(rename = "streamSid")]
        stream: &'a str,
    },
}

#[derive(Serialize)]
struct Payload {
    payload: String,
}

fn written(message: &Sent<'_>) -> String {
    // A value of these shapes is always written.
    serde_json::to_string(message).unwrap_or_default()
}

/// Audio for the caller: µ-law at 8 kHz, with no header before it.
pub(crate) fn media(stream: &str, audio: &[u8]) -> String {
    let media = Payload {
        payload: STANDARD.encode(audio),
    };
    written(&Sent::Media { stream, media })
}

/// A place in the audio that was sent, which Twilio says back when the
/// caller has been played everything before it.
pub(crate) fn mark(stream: &str, name: &str) -> String {
    let mark = Mark {
        name: name.to_owned(),
    };
    written(&Sent::Mark { stream, mark })
}

/// What Twilio holds and has not played is to be discarded.
pub(crate) fn clear(stream: &str) -> String {
    written(&Sent::Clear { stream })
}

/// A value, written between the quotes of an attribute.
fn attribute(value: &str, out: &mut String) -> Result<(), Unanswerable> {
    for written in value.chars() {
        match written {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Nothing a document may hold as it is.
            other if other.is_control() => return Err(Unanswerable::Unwritable),
            other => out.push(other),
        }
    }
    Ok(())
}

/// The document a call is answered with: connect it to a stream at
/// `stream`, and give the stream `parameters` to say as it begins.
pub(crate) fn answer(stream: &str, parameters: &[(&str, &str)]) -> Result<String, Unanswerable> {
    if !stream.starts_with("wss://") {
        return Err(Unanswerable::NotSecure);
    }
    // "The url does not support query string parameters": what a
    // stream is to be told goes into parameters.
    if stream.contains(['?', '#']) {
        return Err(Unanswerable::Query);
    }
    let mut document =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Connect><Stream url=\"");
    attribute(stream, &mut document)?;
    document.push_str("\">");
    for (name, value) in parameters {
        let long = name.chars().count() + value.chars().count() >= PARAMETER_AT_MOST;
        if name.is_empty() || long {
            return Err(Unanswerable::Parameter);
        }
        document.push_str("<Parameter name=\"");
        attribute(name, &mut document)?;
        document.push_str("\" value=\"");
        attribute(value, &mut document)?;
        document.push_str("\"/>");
    }
    document.push_str("</Stream></Connect></Response>");
    Ok(document)
}

/// What is signed of a request: its address, and after it the fields
/// of its form in the order of their names, each as its name and its
/// value with nothing between.
///
/// What becomes of a field that is given twice is not in the
/// documentation. Here the two follow each other in the order of their
/// values.
fn signed_text(url: &str, form: &[(String, String)]) -> String {
    let mut fields: Vec<&(String, String)> = form.iter().collect();
    // "Unix-style case-sensitive sorting order": by bytes.
    fields.sort();
    let mut text = String::from(url);
    for (name, value) in fields {
        text.push_str(name);
        text.push_str(value);
    }
    text
}

/// The signature Twilio puts on a request to `url` with `form`, for an
/// account whose token is `token`.
pub(crate) fn signature(token: &Secret, url: &str, form: &[(String, String)]) -> String {
    let key = hmac::Key::new(
        hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        token.expose().as_bytes(),
    );
    let tag = hmac::sign(&key, signed_text(url, form).as_bytes());
    STANDARD.encode(tag.as_ref())
}

/// The address without what Twilio leaves out of it: a name and a
/// password before the host, and whatever follows a `#`.
fn bare(url: &str) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let ends = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(ends);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    format!("{scheme}://{host}{path}")
}

/// The address without its port, if it names one.
fn without_port(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let ends = rest.find(['/', '?']).unwrap_or(rest.len());
    let (host, path) = rest.split_at(ends);
    // The last colon is a port's unless it is inside the brackets of
    // an address.
    let colon = host.rfind(':')?;
    if host.get(colon..)?.contains(']') {
        return None;
    }
    let named = host.get(..colon)?;
    Some(format!("{scheme}://{named}{path}"))
}

/// The addresses Twilio may have signed for a request to `url`.
///
/// The documentation says of a request over TLS that the port is left
/// out, and of the request for a stream only that a stroke at the end
/// of the address may have to be added. Whether that one is signed as
/// `wss` or as `https` it does not say. So each of those is tried: a
/// signature that fits one of them was made with the token.
fn as_signed(url: &str) -> Vec<String> {
    let url = bare(url);
    let mut schemes = vec![url.clone()];
    for (stream, page) in [("wss://", "https://"), ("ws://", "http://")] {
        if let Some(rest) = url.strip_prefix(stream) {
            schemes.push(format!("{page}{rest}"));
        }
    }
    let mut all = Vec::new();
    for url in schemes {
        let portless = without_port(&url);
        for url in core::iter::once(url).chain(portless) {
            let stroke = (!url.contains('?') && !url.ends_with('/')).then(|| format!("{url}/"));
            for url in core::iter::once(url).chain(stroke) {
                if !all.contains(&url) {
                    all.push(url);
                }
            }
        }
    }
    all
}

/// What the address says the body hashes to, if it says: Twilio signs
/// a body that is no form by putting its hash into the address.
fn body_hash(url: &str) -> Option<&str> {
    let (_, query) = url.split_once('?')?;
    let query = query.split('#').next().unwrap_or(query);
    query
        .split('&')
        .find_map(|field| field.strip_prefix("bodySHA256="))
}

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Whether a request was signed with `token`.
pub(crate) fn verify(token: &Secret, asked: &Asked<'_>) -> Result<(), Unverified> {
    if asked.header(SIGNED_OTHERWISE).is_some() {
        return Err(Unverified::SignedOtherwise);
    }
    let said = asked.header(SIGNATURE).ok_or(Unverified::Unsigned)?;
    // What is no signature at all fits nothing.
    let said = STANDARD
        .decode(said.trim())
        .map_err(|_| Unverified::Mismatch)?;
    // A body that is hashed into the address is signed by the address
    // alone, and has to be the body that was hashed.
    let form: &[(String, String)] = match body_hash(asked.url) {
        Some(hash) => {
            let is = hex(digest::digest(&digest::SHA256, asked.body).as_ref());
            if !hash.eq_ignore_ascii_case(&is) {
                return Err(Unverified::Body);
            }
            &[]
        }
        None => asked.form,
    };
    let key = hmac::Key::new(
        hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        token.expose().as_bytes(),
    );
    // Each is compared in a time that does not tell how much of it fits.
    let fits = as_signed(asked.url)
        .iter()
        .any(|url| hmac::verify(&key, signed_text(url, form).as_bytes(), &said).is_ok());
    if fits {
        Ok(())
    } else {
        Err(Unverified::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use waveport_adapter_kit::secret::Secret;
    use waveport_domain::event::Key;

    use super::{answer, as_signed, clear, mark, media, read, signature, verify};
    use crate::{Asked, Heard, Malformed, Unanswerable, Unverified};

    const FROM_CARRIER: &str = include_str!("../tests/fixtures/twilio/from_carrier.jsonl");
    const TO_CARRIER: &str = include_str!("../tests/fixtures/twilio/to_carrier.jsonl");

    /// The names the documentation writes for a stream, a call and an
    /// account.
    const STREAM: &str = "MZXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";

    /// The audio of the documentation's example, which is nine bytes.
    const AUDIO: [u8; 9] = [0x9e, 0x8f, 0x89, 0x86, 0x86, 0x89, 0x8e, 0x9c, 0xd2];

    #[test]
    fn what_the_documentation_shows_twilio_saying_is_read() {
        let heard: Vec<Heard> = FROM_CARRIER
            .lines()
            .map(|line| read(line).unwrap())
            .collect();
        let [hello, start, audio, key, hash, marked, stop] = heard.as_slice() else {
            panic!("seven messages are shown: {heard:?}");
        };
        assert_eq!(*hello, Heard::Hello);
        let Heard::Start(started) = start else {
            panic!("the second begins the call: {start:?}");
        };
        assert_eq!(started.stream, STREAM);
        assert_eq!(started.call, "CAXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX");
        assert_eq!(
            started.account.as_deref(),
            Some("ACXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX")
        );
        let parameters: Vec<(&str, &str)> = started
            .parameters
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            parameters,
            [
                ("FirstName", "Jane"),
                ("LastName", "Doe"),
                ("RemoteParty", "Bob")
            ]
        );
        assert_eq!(*audio, Heard::Audio(AUDIO.to_vec().into()));
        // The track of a key was named in two ways over the years.
        assert_eq!(*key, Heard::Key(Key::One));
        assert_eq!(*hash, Heard::Key(Key::Hash));
        assert_eq!(*marked, Heard::Mark("my label".to_owned()));
        assert_eq!(*stop, Heard::Stop);
    }

    #[test]
    fn what_twilio_may_come_to_say_is_passed_over_and_what_is_no_message_is_not_taken() {
        for other in [
            r#"{"event":"transcript","sequenceNumber":"9","text":"hello"}"#,
            // What the agent was played, on a stream that carries both.
            r#"{"event":"media","media":{"track":"outbound","payload":"no+J"}}"#,
            // A key that is none, and more than one.
            r#"{"event":"dtmf","dtmf":{"track":"inbound","digit":"x"}}"#,
            r#"{"event":"dtmf","dtmf":{"track":"inbound","digit":"12"}}"#,
        ] {
            assert_eq!(read(other), Ok(Heard::Other), "{other}");
        }
        // A message of today with a field of tomorrow is still read.
        let more = r#"{"event":"mark","mark":{"name":"r1.160","kind":"new"},"region":"ie1"}"#;
        assert_eq!(read(more), Ok(Heard::Mark("r1.160".to_owned())));
        for (none, why) in [
            ("", Malformed::Message),
            ("no+JhoaJjpzS", Malformed::Message),
            (r#"{"sequenceNumber":"4"}"#, Malformed::Message),
            (r#"{"event":"media","media":{}}"#, Malformed::Message),
            (
                r#"{"event":"media","media":{"track":"inbound","payload":"not base64!"}}"#,
                Malformed::Audio,
            ),
        ] {
            assert_eq!(read(none), Err(why), "{none}");
        }
    }

    #[test]
    fn a_stream_of_other_audio_than_a_telephone_s_is_not_taken() {
        let start = |format: &str| {
            format!(
                r#"{{"event":"start","start":{{"streamSid":"MZ1","callSid":"CA1","mediaFormat":{format}}}}}"#
            )
        };
        let ours = r#"{"encoding":"audio/x-mulaw","sampleRate":8000,"channels":1}"#;
        let Ok(Heard::Start(started)) = read(&start(ours)) else {
            panic!("a call of a telephone's audio begins");
        };
        // The account and the parameters are not always there.
        assert_eq!((started.account, started.parameters.len()), (None, 0));
        for other in [
            r#"{"encoding":"audio/x-alaw","sampleRate":8000,"channels":1}"#,
            r#"{"encoding":"audio/x-mulaw","sampleRate":16000,"channels":1}"#,
            r#"{"encoding":"audio/x-mulaw","sampleRate":8000,"channels":2}"#,
        ] {
            assert_eq!(read(&start(other)), Err(Malformed::Format), "{other}");
        }
    }

    #[test]
    fn what_is_sent_is_what_the_documentation_shows_and_nothing_more() {
        let said = [
            media(STREAM, &AUDIO),
            mark(STREAM, "my label"),
            clear(STREAM),
        ];
        let shown: Vec<&str> = TO_CARRIER.lines().collect();
        assert_eq!(said.len(), shown.len());
        for (said, shown) in said.iter().zip(shown) {
            // The same fields with the same values, in whatever order.
            let said: serde_json::Value = serde_json::from_str(said).unwrap();
            let shown: serde_json::Value = serde_json::from_str(shown).unwrap();
            assert_eq!(said, shown);
        }
        // A name is written so that it is read as it was.
        let odd = mark(STREAM, "a \"name\" with\\ strokes");
        let read: serde_json::Value = serde_json::from_str(&odd).unwrap();
        assert_eq!(read["mark"]["name"], "a \"name\" with\\ strokes");
    }

    #[test]
    fn a_call_is_answered_with_a_stream_and_what_the_stream_is_to_say() {
        let document = answer(
            "wss://voice.example/v1/telephony/twilio/stream",
            &[("once", "a-made-up-ticket"), ("note", "a<b & \"c\" 'd'")],
        );
        assert_eq!(
            document.as_deref(),
            Ok(concat!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Connect>",
                "<Stream url=\"wss://voice.example/v1/telephony/twilio/stream\">",
                "<Parameter name=\"once\" value=\"a-made-up-ticket\"/>",
                "<Parameter name=\"note\" value=\"a&lt;b &amp; &quot;c&quot; &apos;d&apos;\"/>",
                "</Stream></Connect></Response>"
            ))
        );
        let none = answer("wss://voice.example/stream", &[]);
        assert!(
            none.unwrap()
                .contains("<Stream url=\"wss://voice.example/stream\"></Stream>")
        );

        // A stream is reached over TLS, and its address says nothing
        // after a `?`: the carrier would not ask for it.
        for (stream, why) in [
            ("ws://voice.example/stream", Unanswerable::NotSecure),
            ("https://voice.example/stream", Unanswerable::NotSecure),
            ("wss://voice.example/stream?token=1", Unanswerable::Query),
            ("wss://voice.example/stream#top", Unanswerable::Query),
            ("wss://voice.example/a\nb", Unanswerable::Unwritable),
        ] {
            assert_eq!(answer(stream, &[]), Err(why), "{stream}");
        }
        let long = "v".repeat(496);
        let fits = [("name", &long[..495])];
        assert!(answer("wss://voice.example/stream", &fits).is_ok());
        for parameters in [[("name", long.as_str())], [("", "value")]] {
            let refused = answer("wss://voice.example/stream", &parameters);
            assert_eq!(refused, Err(Unanswerable::Parameter));
        }
    }

    /// The example of the documentation: a token, a request and what it
    /// is signed with. The numbers in it are the documentation's.
    fn documented() -> (Secret, &'static str, Vec<(String, String)>, &'static str) {
        let form = [
            ("To", "+18005551212"),
            ("Digits", "1234"),
            ("From", "+14158675310"),
            ("Caller", "+14158675310"),
            ("CallSid", "CA1234567890ABCDE"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .to_vec();
        (
            Secret::new("12345"),
            "https://example.com/myapp.php?foo=1&bar=2",
            form,
            "L/OH5YylLD5NRKLltdqwSvS0BnU=",
        )
    }

    fn asked<'a>(
        url: &'a str,
        headers: &'a [(&'a str, &'a str)],
        form: &'a [(String, String)],
    ) -> Asked<'a> {
        Asked {
            url,
            headers,
            form,
            body: &[],
        }
    }

    #[test]
    fn the_example_of_the_documentation_is_signed_as_the_documentation_says() {
        let (token, url, form, signed) = documented();
        // The fields are signed in the order of their names, whatever
        // order they arrive in.
        assert_eq!(signature(&token, url, &form), signed);
        let headers = [("x-twilio-signature", signed)];
        assert_eq!(verify(&token, &asked(url, &headers, &form)), Ok(()));
    }

    #[test]
    fn a_request_that_was_not_signed_with_the_token_is_not_the_carrier_s() {
        let (token, url, form, signed) = documented();
        let headers = [("x-twilio-signature", signed)];
        let mismatch = Err(Unverified::Mismatch);

        // Another token, another address, a field that changed, one
        // more, one less.
        let other = Secret::new("12346");
        assert_eq!(verify(&other, &asked(url, &headers, &form)), mismatch);
        let elsewhere = "https://example.com/myapp.php?foo=1&bar=3";
        assert_eq!(verify(&token, &asked(elsewhere, &headers, &form)), mismatch);
        let mut changed = form.clone();
        changed[1].1 = "1235".to_owned();
        assert_eq!(verify(&token, &asked(url, &headers, &changed)), mismatch);
        let mut more = form.clone();
        more.push(("Extra".to_owned(), "1".to_owned()));
        assert_eq!(verify(&token, &asked(url, &headers, &more)), mismatch);
        assert_eq!(verify(&token, &asked(url, &headers, &form[1..])), mismatch);

        // No signature, one that is none, and one of another length.
        assert_eq!(
            verify(&token, &asked(url, &[], &form)),
            Err(Unverified::Unsigned)
        );
        for none in ["", "not base64!", "L/OH5YylLD5NRKLltdqwSvS0", "AAAA"] {
            let headers = [("x-twilio-signature", none)];
            assert_eq!(
                verify(&token, &asked(url, &headers, &form)),
                mismatch,
                "{none}"
            );
        }

        // One that was signed in the way an account can turn on, with a
        // key that is not the token, is said to be that.
        let newer = [
            ("x-twilio-signature", signed),
            (
                "x-twilio-webhooks-signature-settings",
                "key=1,algorithm=HMAC_SHA256",
            ),
        ];
        assert_eq!(
            verify(&token, &asked(url, &newer, &form)),
            Err(Unverified::SignedOtherwise)
        );
    }

    #[test]
    fn an_address_is_tried_in_the_ways_twilio_may_have_signed_it() {
        assert_eq!(
            as_signed("wss://user:word@voice.example:8443/stream#top"),
            [
                "wss://voice.example:8443/stream",
                "wss://voice.example:8443/stream/",
                "wss://voice.example/stream",
                "wss://voice.example/stream/",
                "https://voice.example:8443/stream",
                "https://voice.example:8443/stream/",
                "https://voice.example/stream",
                "https://voice.example/stream/",
            ]
        );
        // An address with something after a `?` gets no stroke, and one
        // that is written in brackets keeps what is inside them.
        assert_eq!(
            as_signed("https://voice.example/answer?agent=desk"),
            ["https://voice.example/answer?agent=desk"]
        );
        assert_eq!(
            as_signed("http://[::1]:7070/answer/"),
            ["http://[::1]:7070/answer/", "http://[::1]/answer/"]
        );
        assert_eq!(as_signed("http://[::1]/"), ["http://[::1]/"]);

        // A request that was signed without the port the daemon is
        // reached at, as Twilio signs one over TLS, is the carrier's.
        let token = Secret::new("a-made-up-token");
        let form = [("CallSid".to_owned(), "CA1".to_owned())];
        let signed = signature(&token, "https://voice.example/answer", &form);
        let headers = [("x-twilio-signature", signed.as_str())];
        let reached = "https://voice.example:8443/answer";
        assert_eq!(verify(&token, &asked(reached, &headers, &form)), Ok(()));
        // And so is the request for a stream that was signed with a
        // stroke at its end, and as a page.
        let signed = signature(&token, "https://voice.example/stream/", &[]);
        let headers = [("x-twilio-signature", signed.as_str())];
        let stream = "wss://voice.example/stream";
        assert_eq!(verify(&token, &asked(stream, &headers, &[])), Ok(()));
    }

    #[test]
    fn a_body_that_is_no_form_is_signed_by_its_hash_in_the_address() {
        let token = Secret::new("a-made-up-token");
        // The hash is written as everybody writes it: this is the one
        // of "abc" that the standard gives.
        let abc = super::hex(ring::digest::digest(&ring::digest::SHA256, b"abc").as_ref());
        assert_eq!(
            abc,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let body = br#"{"CallSid":"CA1"}"#;
        let is = super::hex(ring::digest::digest(&ring::digest::SHA256, body).as_ref());
        let url = format!("https://voice.example/events?bodySHA256={is}");
        let signed = signature(&token, &url, &[]);
        let headers = [("x-twilio-signature", signed.as_str())];
        let whole = Asked {
            url: &url,
            headers: &headers,
            // Fields that somebody took the body for are not signed.
            form: &[("CallSid".to_owned(), "CA1".to_owned())],
            body,
        };
        assert_eq!(verify(&token, &whole), Ok(()));
        // Another body under the same address is not what was signed.
        let other = Asked {
            body: br#"{"CallSid":"CA2"}"#,
            ..whole
        };
        assert_eq!(verify(&token, &other), Err(Unverified::Body));
    }
}

//! The caller's side of a call that goes over WebRTC, to an agent that
//! a daemon runs.
//!
//! The daemon is asked for a session of the agent, the call is offered
//! where the daemon takes offers (`POST /v1/webrtc`), and it is made
//! from a socket of this machine. What the caller says is sent as Opus,
//! a packet every 20 ms, and what the agent says is decoded as it
//! arrives, so the rest of the command sees the samples it sees on any
//! other line.
//!
//! A caller whose call dropped offers another, with the token it was
//! given to come back with. One who hangs up says so on the call, and
//! where the call was offered.

use core::time::Duration;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use bytes::Bytes;
use waveport_adapter_kit::http::{Auth, Body, HttpClient, HttpRequest, HttpSettings};
use waveport_adapter_kit::secret::Secret;
use waveport_media::codec::opus::{MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
use waveport_ports::audio::Rate;
use waveport_ports::error::{PortError, RetryClass};
use waveport_protocol::client::v1::{Resume, features};
use waveport_transport_webrtc::{self as webrtc, Dialer, Like};
use waveport_transport_websocket::{Heard, Hello, Welcome};

use crate::client::Unreached;
use crate::{Context, Failure};

/// How many samples a packet holds: 20 ms at 48 kHz.
const PACKET: usize = 960;

/// The most samples a packet can hold: 120 ms.
const LONGEST: usize = 5_760;

/// The most that is read of what a daemon answers.
const ANSWER: usize = 64 * 1024;

/// Where a daemon is.
#[derive(Debug, PartialEq, Eq)]
struct Place {
    /// What its addresses begin with: `http://127.0.0.1:7070`.
    base: String,
    /// Its name, or its address.
    host: String,
    port: u16,
}

/// Where the daemon at `url` is: `http://` or `https://`, its name, and
/// its port if it says one. A daemon is asked at addresses of its own,
/// so an address that goes on after the name is not one of a daemon.
fn place(url: &str) -> Option<Place> {
    let (scheme, rest) = url.split_once("://")?;
    let usual = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => {
            (host, port.parse().ok().filter(|port| *port > 0)?)
        }
        _ => (authority, usual),
    };
    let bracketed = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'));
    let host = match bracketed {
        Some(address) => address.parse::<Ipv6Addr>().ok()?.to_string(),
        None if host.is_empty() || host.contains([':', '[', ']']) => return None,
        None => host.to_owned(),
    };
    Some(Place {
        base: format!("{scheme}://{authority}"),
        host,
        port,
    })
}

/// The address of this machine that `to` is reached from. A socket is
/// told where its datagrams would go, and sends none.
fn seen_from(to: SocketAddr) -> std::io::Result<IpAddr> {
    let any: IpAddr = if to.is_ipv4() {
        Ipv4Addr::UNSPECIFIED.into()
    } else {
        Ipv6Addr::UNSPECIFIED.into()
    };
    let socket = UdpSocket::bind(SocketAddr::new(any, 0))?;
    socket.connect(to)?;
    Ok(socket.local_addr()?.ip())
}

/// What a daemon said of a request it did not take, as a caller is
/// told it: the reason it gave, where it gave one.
fn said(error: &PortError) -> String {
    let message = error.message.as_ref();
    let problem = message
        .find('{')
        .and_then(|begins| message.get(begins..))
        .and_then(|problem| serde_json::from_str::<serde_json::Value>(problem).ok());
    let detail = problem
        .as_ref()
        .and_then(|problem| problem.get("detail"))
        .and_then(serde_json::Value::as_str);
    detail.map_or_else(|| message.to_owned(), str::to_owned)
}

/// Why a request to the daemon came to nothing, for a caller who would
/// come back: what a daemon turned away is not asked again.
fn unreached(error: PortError) -> Unreached {
    match error.class {
        RetryClass::Auth | RetryClass::Permanent => Unreached::Refused(said(&error)),
        _ => Unreached::Failed(error),
    }
}

/// Where a call is, for what is said of it afterwards: the place an
/// answer named, on the daemon at `base` that gave the answer. A place
/// anywhere else is none. A caller says its token where it says
/// something of its call, and an answer that points elsewhere would
/// have it say the token there.
fn held_at(base: &str, location: Option<&str>) -> Option<String> {
    let at = location?;
    let here = at.starts_with('/') && !at.starts_with("//");
    here.then(|| format!("{base}{at}"))
}

/// What one thing a call says comes to for a caller at a terminal.
#[derive(Debug)]
enum Told {
    /// Something the caller is told, with its audio as samples.
    Heard(Heard),
    /// Nothing a caller at a terminal takes.
    Nothing,
    /// Nothing arrives from the agent's side any more.
    Silent,
}

/// What `heard` comes to. Audio is decoded with `decoder`.
fn told(decoder: &mut OpusDecoder, heard: webrtc::Heard) -> Told {
    Told::Heard(match heard {
        webrtc::Heard::Begins { run, generation } => Heard::Begins { run, generation },
        webrtc::Heard::Audio(packet) => {
            let mut samples = [0_i16; LONGEST];
            // What is no Opus is not played, and so is not said to
            // have been.
            let Ok(length) = decoder.decode(&packet, &mut samples) else {
                return Told::Nothing;
            };
            let bytes: Vec<u8> = samples
                .iter()
                .take(length)
                .flat_map(|sample| sample.to_le_bytes())
                .collect();
            Heard::Audio(Bytes::from(bytes))
        }
        webrtc::Heard::Ends(run) => Heard::Ends(run),
        webrtc::Heard::Clear(run) => Heard::Clear(run),
        webrtc::Heard::Fact { seq, fact } => Heard::Fact { seq, fact },
        webrtc::Heard::Told(news) => Heard::Told(news),
        webrtc::Heard::Gap { resumes } => Heard::Gap { resumes },
        webrtc::Heard::State(state) => Heard::State(state),
        webrtc::Heard::Error { code, message } => Heard::Error { code, message },
        webrtc::Heard::Bye(reason) => Heard::Bye(reason),
        webrtc::Heard::Down => return Told::Silent,
        // The agent pressed a key, which nothing at a terminal takes.
        // And a caller that is told its line is back had left it by
        // then.
        webrtc::Heard::Key(_) | webrtc::Heard::Up => return Told::Nothing,
    })
}

/// A daemon, and the session it made for this caller.
pub(crate) struct Daemon {
    http: HttpClient,
    base: String,
    /// The address of this machine that the daemon is reached from.
    from: IpAddr,
    /// Whether calls are made over a connection.
    tcp: bool,
    /// What the caller was given with its session: it lets its holder
    /// connect, once.
    token: Secret,
}

impl Daemon {
    /// Reaches the daemon at `url` and has it make a session of `agent`.
    /// `key_env` names the environment variable that holds the daemon's
    /// control key, for a daemon that has one.
    ///
    /// # Errors
    ///
    /// The address is none of a daemon, nothing is there, or the daemon
    /// made no session.
    pub(crate) async fn session(
        url: &str,
        agent: &str,
        key_env: Option<&str>,
        tcp: bool,
    ) -> Result<Self, Failure> {
        let Some(place) = place(url) else {
            return Err(Failure(format!(
                "{url} is not where a daemon is: that is `http://` or `https://`, its name, \
                 and its port if it has one"
            )));
        };
        let found = tokio::net::lookup_host((place.host.as_str(), place.port)).await;
        let Some(to) = found.ok().and_then(|mut found| found.next()) else {
            return Err(Failure(format!(
                "no machine is known by the name {}",
                place.host
            )));
        };
        let from = seen_from(to)
            .with(|| format!("this machine has no address that {to} is reached from"))?;
        let settings = HttpSettings {
            direct: to.ip().is_loopback(),
            ..HttpSettings::default()
        };
        let http = HttpClient::new(settings).with(|| "the daemon cannot be asked".to_owned())?;
        let key = match key_env {
            None => None,
            Some(name) => Some(Secret::from_env(name).map_err(|_| {
                Failure(format!(
                    "the environment variable {name} holds no control key, and --key-env names it"
                ))
            })?),
        };
        let body = serde_json::json!({ "agent": agent }).to_string();
        let asked = HttpRequest {
            url: &format!("{}/v1/sessions", place.base),
            headers: &[],
            auth: key.as_ref().map_or(Auth::None, Auth::Bearer),
            body: Body::Json(body.as_bytes()),
        };
        let no_session = || format!("the daemon at {url} made no session of the agent {agent}");
        let made = match http.post(asked).await {
            Ok(made) => made.body(ANSWER).await.with(no_session)?,
            Err(error) => return Err(Failure(format!("{}: {}", no_session(), said(&error)))),
        };
        let token = serde_json::from_slice::<serde_json::Value>(&made)
            .ok()
            .and_then(|made| Some(Secret::new(made.get("client_token")?.as_str()?)))
            .ok_or_else(|| Failure(format!("{}: it gave no token", no_session())))?;
        Ok(Self {
            http,
            base: place.base,
            from,
            tcp,
            token,
        })
    }

    /// Offers a call and makes it. `resume` is where a caller that
    /// comes back was. The agent's side has `within` to welcome the
    /// caller once the offer is answered.
    ///
    /// # Errors
    ///
    /// The daemon did not answer the offer, or the call the answer
    /// describes could not be made.
    pub(crate) async fn call(
        &self,
        resume: Option<Resume>,
        within: Duration,
    ) -> Result<OnCall, Unreached> {
        let local = SocketAddr::new(self.from, 0);
        let (dialing, offer) = Dialer::offer(Like::Chrome, local, self.tcp)
            .await
            .map_err(Unreached::Failed)?;
        // A caller that comes back says the token it was given to come
        // back with, where it offers and in its hello.
        let bearer = resume
            .as_ref()
            .map_or_else(|| self.token.clone(), |resume| Secret::new(&*resume.token));
        let hello = first(&self.token, resume);
        let offered = HttpRequest {
            url: &format!("{}/v1/webrtc", self.base),
            headers: &[],
            auth: Auth::Bearer(&bearer),
            body: Body::Of {
                kind: "application/sdp",
                bytes: offer.as_bytes(),
            },
        };
        let answered = self.http.post(offered).await.map_err(unreached)?;
        // Where the call is, on the daemon that answered. The token is
        // said to nobody else, wherever an answer points.
        let at = held_at(&self.base, answered.header("location"));
        let held = Offered {
            http: self.http.clone(),
            at,
            bearer,
        };
        let answer = match answered.body(ANSWER).await.map(String::from_utf8) {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                held.let_go().await;
                let error = PortError::permanent("the daemon's answer to the offer is no text");
                return Err(Unreached::Failed(error));
            }
            Err(error) => {
                held.let_go().await;
                return Err(Unreached::Failed(error));
            }
        };
        let dialer = match dialing.answered(&answer, &hello, within).await {
            Ok(dialer) => dialer,
            Err(unconnected) => {
                // The daemon waits for the call no longer.
                held.let_go().await;
                return Err(match unconnected {
                    webrtc::Unconnected::Refused { message, .. } => Unreached::Refused(message),
                    webrtc::Unconnected::Failed(error) => Unreached::Failed(error),
                });
            }
        };
        let codec = |_| Unreached::Failed(PortError::permanent("Opus cannot be used"));
        Ok(OnCall {
            dialer,
            encoder: OpusEncoder::new(Rate::Hz48000, OpusSettings::default()).map_err(codec)?,
            decoder: OpusDecoder::new(Rate::Hz48000).map_err(codec)?,
            spare: Vec::with_capacity(2 * PACKET),
            held,
        })
    }
}

/// What a caller says first on a call: the token of its session, or,
/// when it comes back, where it was and the token it was given to come
/// back with. A token is good once: the one a caller came with first
/// has been used, and is said to nobody again.
fn first(token: &Secret, resume: Option<Resume>) -> Hello {
    Hello {
        token: resume.is_none().then(|| token.expose().to_owned()),
        // A caller at a terminal is told what the tools did, as far as
        // the agent's side lets it.
        features: vec![features::TOOLS.to_owned()],
        resume,
        ..Hello::new(None)
    }
}

/// A call as the daemon holds it: where it is, and what lets its caller
/// say something of it.
struct Offered {
    http: HttpClient,
    /// Where it is. A daemon that did not say cannot be told.
    at: Option<String>,
    bearer: Secret,
}

impl Offered {
    /// Tells the daemon that the call is over. A daemon that does not
    /// hear it lets go of the call by itself.
    async fn let_go(&self) {
        let Some(at) = &self.at else {
            return;
        };
        let gone = HttpRequest {
            url: at,
            headers: &[],
            auth: Auth::Bearer(&self.bearer),
            body: Body::None,
        };
        // A call the daemon has let go of already is no call there.
        let _ = self.http.delete(gone).await;
    }
}

/// A caller on a call.
pub(crate) struct OnCall {
    dialer: Dialer,
    encoder: OpusEncoder,
    decoder: OpusDecoder,
    /// What the caller said that does not fill a packet yet.
    spare: Vec<i16>,
    held: Offered,
}

impl OnCall {
    /// What the agent's side answered the hello with.
    pub(crate) const fn welcome(&self) -> &Welcome {
        self.dialer.welcome()
    }

    /// The next thing the agent's side says, with its audio as samples.
    /// `None` means the call is over.
    ///
    /// # Errors
    ///
    /// The call failed.
    pub(crate) async fn next(&mut self) -> Result<Option<Heard>, PortError> {
        loop {
            let Some(heard) = self.dialer.next().await? else {
                return Ok(None);
            };
            match told(&mut self.decoder, heard) {
                Told::Heard(heard) => return Ok(Some(heard)),
                Told::Nothing => {}
                // A caller does not wait on a call for its agent's side
                // to be reached again: its line dropped, and it comes
                // back on another call.
                Told::Silent => {
                    let silent = "nothing arrives from the agent's side any more";
                    return Err(PortError::transient(silent));
                }
            }
        }
    }

    /// Sends what the caller said: samples of 16 bits at 48 kHz, as
    /// many as there are. They leave in packets of 20 ms, and what does
    /// not fill one waits for the rest.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub(crate) fn audio(&mut self, samples: &[u8]) -> Result<(), PortError> {
        let (said, _) = samples.as_chunks::<2>();
        self.spare
            .extend(said.iter().map(|sample| i16::from_le_bytes(*sample)));
        let mut packet = [0_u8; MAX_PACKET];
        let mut sent = 0;
        let mut failed = None;
        for frame in self.spare.as_chunks::<PACKET>().0 {
            sent += PACKET;
            // A frame that cannot be encoded is one the agent does not
            // hear: the call goes on.
            let Ok(length) = self.encoder.encode(frame, &mut packet) else {
                continue;
            };
            if let Err(error) = self.dialer.audio(packet.get(..length).unwrap_or_default()) {
                failed = Some(error);
                break;
            }
        }
        self.spare.drain(..sent);
        failed.map_or(Ok(()), Err)
    }

    /// Says how much of the run `run` has been played.
    ///
    /// # Errors
    ///
    /// The call is over.
    pub(crate) fn played(&mut self, run: u32, samples: u32) -> Result<(), PortError> {
        self.dialer.played(run, samples)
    }

    /// Hangs up: says goodbye on the call, and tells the daemon that
    /// the call is over where it was offered.
    pub(crate) async fn hang_up(self) {
        self.dialer.bye().await;
        self.held.let_go().await;
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::path::{Path, PathBuf};

    use waveport_adapter_kit::http::{Auth, Body, HttpRequest};
    use waveport_adapter_kit::secret::Secret;
    use waveport_ports::error::{PortError, RetryClass};
    use waveport_protocol::client::v1::Resume;
    use waveport_testkit::servers::{HttpServer, WsServer, WsStep};
    use waveport_testkit::stand_in::{HEARD, spoken};
    use waveportd::{Config, Threads};

    use bytes::Bytes;
    use waveport_media::codec::opus::{MAX_PACKET, OpusDecoder, OpusEncoder, OpusSettings};
    use waveport_ports::audio::Rate;
    use waveport_protocol::client::v1::ByeReason;
    use waveport_transport_webrtc as webrtc;
    use waveport_transport_websocket::Heard;

    use super::{
        Daemon, OnCall, PACKET, Place, Told, first, held_at, place, said, seen_from, told,
        unreached,
    };
    use crate::client::Unreached;

    #[test]
    fn a_daemon_is_at_a_name_and_perhaps_a_port_and_nowhere_further() {
        let at = |base: &str, host: &str, port: u16| {
            Some(Place {
                base: base.to_owned(),
                host: host.to_owned(),
                port,
            })
        };
        assert_eq!(
            place("http://127.0.0.1:7070"),
            at("http://127.0.0.1:7070", "127.0.0.1", 7070)
        );
        // Written with a stroke at its end or without, it is one
        // address, and the port that is not said is the usual one.
        assert_eq!(
            place("https://voice.example/"),
            at("https://voice.example", "voice.example", 443)
        );
        assert_eq!(
            place("http://voice.example"),
            at("http://voice.example", "voice.example", 80)
        );
        assert_eq!(
            place("http://[::1]:7070/"),
            at("http://[::1]:7070", "::1", 7070)
        );
        assert_eq!(
            place("https://[2001:db8::7]"),
            at("https://[2001:db8::7]", "2001:db8::7", 443)
        );
        for none in [
            "",
            "127.0.0.1:7070",
            "ws://127.0.0.1:9283",
            "http://",
            "http:///",
            "http://:7070",
            "http://voice.example:0",
            "http://voice.example:port",
            "http://voice.example/v1/webrtc",
            "http://voice.example?agent=desk",
            "http://voice.example#desk",
            // A key is said in a header, and never in an address.
            "http://someone:wpc1_madeup@voice.example",
            "http://someone@voice.example",
            "http://[voice.example]",
            "http://::1:7070",
            "http://voice example",
        ] {
            assert_eq!(place(none), None, "{none}");
        }
    }

    #[test]
    fn this_machine_reaches_itself_from_itself() {
        let from = seen_from("127.0.0.1:7070".parse().unwrap()).unwrap();
        assert_eq!(from.to_string(), "127.0.0.1");
    }

    #[test]
    fn a_caller_is_told_why_a_daemon_did_not_take_what_it_asked() {
        let refused = |status: u16, class: RetryClass, detail: &str| PortError {
            class,
            retry_after: None,
            message: format!(
                "the provider answered {status}: {{\"type\":\"about:blank\",\"title\":\"No\",\
                 \"status\":{status},\"code\":\"no_agent\",\"detail\":\"{detail}\"}}"
            )
            .into(),
        };
        // The reason the daemon gave, and not all it sent.
        let no_agent = refused(404, RetryClass::Permanent, "there is no agent of that name");
        assert_eq!(said(&no_agent), "there is no agent of that name");
        // What gives no reason is told as it is.
        let plain = PortError::transient("the provider answered 502");
        assert_eq!(said(&plain), "the provider answered 502");
        let cut = PortError::transient("the provider answered 500: {\"detail\":\"it bro");
        assert_eq!(
            said(&cut),
            "the provider answered 500: {\"detail\":\"it bro"
        );

        // What a daemon turned away is not asked again, and what failed
        // otherwise may be.
        for (error, turned_away) in [
            (no_agent, true),
            (
                refused(401, RetryClass::Auth, "this is no client token"),
                true,
            ),
            (
                refused(503, RetryClass::Transient, "as many calls are on their way"),
                false,
            ),
            (PortError::timeout("nothing answered"), false),
            (plain, false),
        ] {
            let message = error.message.clone();
            match unreached(error) {
                Unreached::Refused(why) => {
                    assert!(turned_away, "{message}");
                    assert!(!why.contains("about:blank"), "{why}");
                }
                Unreached::Failed(error) => {
                    assert!(!turned_away, "{message}");
                    assert_eq!(error.message, message);
                }
            }
        }
    }

    #[test]
    fn a_call_is_spoken_of_on_the_daemon_that_answered_and_nowhere_else() {
        let base = "http://127.0.0.1:7070";
        assert_eq!(
            held_at(base, Some("/v1/webrtc/0123abcd")).as_deref(),
            Some("http://127.0.0.1:7070/v1/webrtc/0123abcd")
        );
        // A daemon that did not say where the call is cannot be told of
        // it, and one that points elsewhere is not followed there: the
        // token would be said to whoever is there.
        for elsewhere in [
            None,
            Some(""),
            Some("https://elsewhere.example/v1/webrtc/0123abcd"),
            Some("//elsewhere.example/v1/webrtc/0123abcd"),
            Some("v1/webrtc/0123abcd"),
            Some("@elsewhere.example/v1/webrtc/0123abcd"),
        ] {
            assert_eq!(held_at(base, elsewhere), None, "{elsewhere:?}");
        }
    }

    #[test]
    fn a_caller_that_comes_back_says_what_it_was_given_and_not_what_it_came_with() {
        let came_with = Secret::new("wpc1_madeup");
        // A first call: the token of the session, and nowhere to have
        // been.
        let hello = first(&came_with, None);
        assert_eq!(hello.token.as_deref(), Some("wpc1_madeup"));
        assert_eq!(hello.resume, None);
        assert_eq!(hello.features, ["tools"]);
        // One that comes back: where it was, with the token it was
        // given for that. The token it came with first was used up by
        // its first hello, and is not said again.
        let was = Resume {
            token: "wpr1_madeup".to_owned(),
            life: 2,
            seen: 41,
        };
        let hello = first(&came_with, Some(was.clone()));
        assert_eq!(hello.token, None);
        assert_eq!(hello.resume, Some(was));
        assert_eq!(hello.features, ["tools"]);
    }

    #[test]
    fn what_a_call_says_is_what_a_caller_at_a_terminal_is_told() {
        let mut encoder = OpusEncoder::new(Rate::Hz48000, OpusSettings::default()).unwrap();
        let mut decoder = OpusDecoder::new(Rate::Hz48000).unwrap();
        // Audio arrives as Opus and is told as samples: a packet of
        // 20 ms is 960 of them, two bytes each.
        let mut packet = [0_u8; MAX_PACKET];
        let length = encoder.encode(&[0_i16; PACKET], &mut packet).unwrap();
        let audio = webrtc::Heard::Audio(Bytes::copy_from_slice(&packet[..length]));
        let Told::Heard(Heard::Audio(samples)) = told(&mut decoder, audio) else {
            panic!("audio is told as audio");
        };
        assert_eq!(samples.len(), 2 * PACKET);
        // What is no Opus is nothing to play.
        let noise = webrtc::Heard::Audio(Bytes::new());
        assert!(matches!(told(&mut decoder, noise), Told::Nothing));

        // What is said of the call is told as it is.
        let begins = webrtc::Heard::Begins {
            run: 3,
            generation: 7,
        };
        assert!(matches!(
            told(&mut decoder, begins),
            Told::Heard(Heard::Begins {
                run: 3,
                generation: 7
            })
        ));
        for (said, run) in [(webrtc::Heard::Ends(3), 3), (webrtc::Heard::Clear(4), 4)] {
            let ends = matches!(said, webrtc::Heard::Ends(_));
            match told(&mut decoder, said) {
                Told::Heard(Heard::Ends(of)) => assert!(ends && of == run),
                Told::Heard(Heard::Clear(of)) => assert!(!ends && of == run),
                other => panic!("{other:?}"),
            }
        }
        let bye = webrtc::Heard::Bye(ByeReason::GoingAway);
        assert!(matches!(
            told(&mut decoder, bye),
            Told::Heard(Heard::Bye(ByeReason::GoingAway))
        ));
        let gap = webrtc::Heard::Gap { resumes: 12 };
        assert!(matches!(
            told(&mut decoder, gap),
            Told::Heard(Heard::Gap { resumes: 12 })
        ));

        // A far end that fell silent is a line that dropped. One that
        // is heard again, by a caller that did not leave, is nothing to
        // tell.
        assert!(matches!(
            told(&mut decoder, webrtc::Heard::Down),
            Told::Silent
        ));
        assert!(matches!(
            told(&mut decoder, webrtc::Heard::Up),
            Told::Nothing
        ));
    }

    /// An environment variable nobody sets. A daemon that runs in a
    /// test has the environment of whoever runs the tests: told
    /// nothing, it would read the keys that are there and send them to
    /// the stand-ins.
    const NO_KEY: &str = "WAVEPORT_STAND_IN_KEY";

    /// How long anything on one machine may take.
    const SOON: Duration = Duration::from_secs(10);

    /// Where the models that listen are.
    fn assets() -> PathBuf {
        std::env::var_os("WAVEPORT_ASSETS").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/assets"),
            PathBuf::from,
        )
    }

    #[tokio::test]
    async fn a_caller_whose_call_dropped_offers_another_with_what_it_was_given() {
        assert!(
            std::env::var_os(NO_KEY).is_none(),
            "{NO_KEY} is set, and is to hold nothing"
        );
        let stt = WsServer::start(vec![WsStep::script(HEARD)]).await;
        let llm = HttpServer::start(Vec::new()).await;
        let tts = WsServer::start(vec![spoken()]).await;
        let text = format!(
            "listen = \"127.0.0.1:0\"\n\
             assets = '{}'\n\
             [webrtc]\n\
             udp = \"127.0.0.1:0\"\n\
             [agents.desk.llm]\n\
             url = \"{}\"\n\
             model = \"a-model\"\n\
             key_env = \"{NO_KEY}\"\n\
             [agents.desk.speech.stt]\n\
             provider = \"openai\"\n\
             url = \"{}\"\n\
             key_env = \"{NO_KEY}\"\n\
             [agents.desk.speech.tts]\n\
             provider = \"cartesia\"\n\
             url = \"{}\"\n\
             key_env = \"{NO_KEY}\"\n",
            assets().display(),
            llm.url("/v1"),
            stt.url("/v1/realtime"),
            tts.url("/tts/websocket"),
        );
        assert_eq!(
            text.matches("key_env").count(),
            text.matches("url = ").count()
        );
        let config = Config::from_toml(&text).unwrap();
        let threads = Threads::place(&config).unwrap();
        let running = waveportd::Daemon::start(&config, &threads)
            .await
            .unwrap_or_else(|failure| panic!("the daemon did not start: {failure}"));
        let url = format!("http://{}", running.address());

        let daemon = Daemon::session(&url, "desk", None, false)
            .await
            .unwrap_or_else(|failure| panic!("{failure}"));
        let call = daemon
            .call(None, SOON)
            .await
            .unwrap_or_else(|unreached| panic!("{unreached}"));
        let welcome = call.welcome().clone();
        assert!(!welcome.resumed);
        // What it was given with its session let it connect once. An
        // offer that is made with it again is turned away, which a
        // caller does not ask again.
        let Err(Unreached::Refused(why)) = daemon.call(None, SOON).await else {
            panic!("a token was good twice");
        };
        assert!(why.contains("this is no client token"), "{why}");

        // Its network is gone, and it with it. It offers another call,
        // with what it was given to come back with, and is on the
        // session it was on.
        let OnCall { dialer, .. } = call;
        dialer.drop_line();
        let back = Resume {
            token: welcome.resume_token.clone().unwrap(),
            life: welcome.life,
            seen: 0,
        };
        let call = daemon
            .call(Some(back.clone()), SOON)
            .await
            .unwrap_or_else(|unreached| panic!("{unreached}"));
        assert!(call.welcome().resumed);
        assert_eq!(call.welcome().session, welcome.session);
        assert_eq!(call.welcome().life, welcome.life);
        // What nobody gave it lets it come back to nothing.
        let made_up = Resume {
            token: "wpr1_madeup".to_owned(),
            ..back
        };
        let Err(Unreached::Refused(why)) = daemon.call(Some(made_up), SOON).await else {
            panic!("a token that is none was good");
        };
        assert!(!why.contains("madeup"), "{why}");

        // It hangs up, and the daemon does not wait for a caller who
        // said that they are gone: the session is let go of.
        call.hang_up().await;
        let session = format!("{url}/v1/sessions/{}", welcome.session);
        let began = std::time::Instant::now();
        loop {
            let asked = HttpRequest {
                url: &session,
                headers: &[],
                auth: Auth::None,
                body: Body::None,
            };
            match daemon.http.get(asked).await {
                Err(error) if error.class == RetryClass::Permanent => break,
                still => assert!(began.elapsed() < SOON, "{still:?}"),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let left = running.drain().await;
        assert_eq!(left, [] as [String; 0]);
    }
}

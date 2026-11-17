//! The calls a carrier brings.
//!
//! A carrier that takes a call asks `POST /v1/telephony/{carrier}/voice`
//! what to do with it, in a form it signs. A request that is the
//! carrier's makes a session of the agent that answers calls, and is
//! answered with the document that connects the call to a stream at
//! `GET /v1/telephony/{carrier}/stream`, and with a ticket for the
//! stream to say as it begins: the client token of the session, which is
//! good once and for a while.
//!
//! The carrier then asks for the stream, which becomes a WebSocket. The
//! address of a stream says nothing, so which call it is of is what it
//! says first: the ticket, and the carrier's name for the call, which
//! has to be the call the ticket was made for. The session is started
//! on that stream, and hears and speaks over the telephone.
//!
//! Nothing in a carrier's signature says when it was made, so a request
//! that was signed once is good again for whoever has seen it. A call
//! is therefore answered once: its name is remembered, and whoever asks
//! about it again while nothing has come with its ticket is given the
//! same answer, as a carrier that asks twice is, and after that is
//! refused. A daemon remembers the last [`REMEMBERED`] calls it
//! answered, and has forgotten them when it is started again.
//!
//! The session that is made for a call waits for the call: until its
//! stream has come, nobody starts it, whoever follows it or writes to
//! it. Once it runs it is a session like any other, which whoever holds
//! a control key may follow and write to.
//!
//! Nobody comes back to a telephone call. A caller who hangs up ends
//! the session, and nothing is kept of it. A call whose daemon stops or
//! dies is over for its caller too: what it had come to is kept, as it
//! is of any session, and is listed as one that broke off until whoever
//! holds a control key lets go of it.

use core::sync::atomic::{AtomicBool, Ordering};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use waveport_adapter_kit::secret::Secret;
use waveport_adapter_kit::ws::WsConn;
use waveport_assembly::Failure;
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::ErrorCode;
use waveport_protocol::control::v1::Problem;
use waveport_transport_telephony_ws::{Answer, Asked, Call, Carrier, STARTS_WITHIN, Started};

use crate::config;
use crate::daemon::{Shared, raised};
use crate::http::{Refused, problem, said};
use crate::line;

/// What the answer to a call gives its stream to say, by name: the
/// client token of the session that was made for the call.
const TICKET: &str = "ticket";

/// How many calls are remembered as answered, at most. The oldest is
/// forgotten for the next.
pub(crate) const REMEMBERED: usize = 16_384;

/// What answers the calls of one carrier.
pub(crate) struct Answering {
    /// The agent that answers, by its name.
    agent: String,
    /// What the carrier signs with.
    secret: Secret,
    /// The account whose calls are answered, if only one's are.
    account: Option<String>,
    /// Whether a stream whose request is not signed is turned away.
    signed_streams: bool,
}

/// A call that was answered.
struct Answered {
    session: SessionId,
    /// The answer, which is given again while the ticket in it is good
    /// and unused.
    answer: Answer,
}

/// The calls that were answered, by the carrier and its name for each.
#[derive(Default)]
struct Calls {
    known: HashMap<(Carrier, String), Answered>,
    /// The same calls, the oldest first.
    order: VecDeque<(Carrier, String)>,
}

impl Calls {
    /// Remembers a call, and forgets the oldest for it where there are
    /// as many as are remembered.
    fn remember(&mut self, call: (Carrier, String), answered: Answered) {
        while self.order.len() >= REMEMBERED {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.known.remove(&oldest);
        }
        self.order.push_back(call.clone());
        self.known.insert(call, answered);
    }
}

/// What a daemon that answers calls is told, and what it remembers of
/// them.
pub(crate) struct Telephony {
    /// What follows `https://` in the address carriers reach the daemon
    /// at.
    authority: String,
    twilio: Option<Answering>,
    calls: Mutex<Calls>,
    /// Whether it has been said that a stream came unsigned.
    said_unsigned: AtomicBool,
}

impl Telephony {
    /// What answers calls as `config` says, if anything does.
    ///
    /// # Errors
    ///
    /// A variable that is to hold what a carrier signs with holds
    /// nothing.
    pub(crate) fn of(config: &config::Telephony) -> Result<Option<Self>, Failure> {
        Self::held_by(config, |name| std::env::var(name).ok())
    }

    /// The same, with the variables as `read` reads them.
    fn held_by(
        config: &config::Telephony,
        read: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, Failure> {
        let (Some(authority), true) = (config.authority(), config.answers()) else {
            return Ok(None);
        };
        let twilio = match &config.twilio {
            None => None,
            Some(twilio) => {
                let name = &twilio.token_env;
                let token = read(name).unwrap_or_default();
                let token = token.trim();
                if token.is_empty() {
                    return Err(Failure(format!(
                        "the environment variable {name} holds no auth token, and \
                         `telephony.twilio.token_env` names it"
                    )));
                }
                Some(Answering {
                    agent: twilio.agent.clone(),
                    secret: Secret::new(token),
                    account: twilio.account.clone(),
                    signed_streams: twilio.signed_streams,
                })
            }
        };
        Ok(Some(Self {
            authority: authority.to_owned(),
            twilio,
            calls: Mutex::default(),
            said_unsigned: AtomicBool::new(false),
        }))
    }

    /// The name carriers reach the daemon by, as a request says it in
    /// `Host`.
    pub(crate) fn host(&self) -> &str {
        &self.authority
    }

    /// What whoever starts the daemon is told of the calls it answers:
    /// where each carrier is told to ask.
    pub(crate) fn notes(&self) -> impl Iterator<Item = String> {
        self.carriers().map(|carrier| {
            format!(
                "calls of {carrier} are answered: it is told to ask \
                 https://{}/v1/telephony/{carrier}/voice",
                self.authority
            )
        })
    }

    /// The carriers whose calls are answered.
    fn carriers(&self) -> impl Iterator<Item = Carrier> {
        Carrier::ALL
            .into_iter()
            .filter(|carrier| self.answering(*carrier).is_some())
    }

    fn answering(&self, carrier: Carrier) -> Option<&Answering> {
        match carrier {
            Carrier::Twilio => self.twilio.as_ref(),
            // A carrier this daemon has not learnt to be told of.
            _ => None,
        }
    }

    fn calls(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.calls.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where a carrier asked that asked for `path` and what followed
    /// its `?`, as the carrier wrote the address. `scheme` is `https`
    /// for a request and `wss` for a stream.
    fn asked_at(&self, scheme: &str, path: &str, query: Option<&str>) -> String {
        let mut url = format!("{scheme}://{}{path}", self.authority);
        if let Some(query) = query {
            url.push('?');
            url.push_str(query);
        }
        url
    }

    /// The address of the stream a call of `carrier` is connected to.
    fn stream_of(&self, carrier: Carrier) -> String {
        let path = format!("/v1/telephony/{}/stream", carrier.name());
        self.asked_at("wss", &path, None)
    }

    /// Says once, where the daemon runs, that a stream came whose
    /// request was not signed as its carrier signs.
    fn say_unsigned(&self, carrier: Carrier, why: &str) {
        if !self.said_unsigned.swap(true, Ordering::Relaxed) {
            eprintln!(
                "waveportd: a request for a stream of {carrier} was not signed as {carrier} \
                 signs: {why}. Such a stream is let in by its ticket alone, and \
                 `telephony.{carrier}.signed_streams` turns it away"
            );
        }
    }
}

/// What a form holds: its fields, as they read once they are decoded.
/// `None` for what is no form.
fn form(body: &[u8]) -> Option<Vec<(String, String)>> {
    let body = core::str::from_utf8(body).ok()?;
    let mut fields = Vec::new();
    for field in body.split('&').filter(|field| !field.is_empty()) {
        let (name, value) = field.split_once('=').unwrap_or((field, ""));
        fields.push((decoded(name)?, decoded(value)?));
    }
    Some(fields)
}

/// A name or a value of a form, as it reads: a `+` is a space, and `%`
/// with two digits is the byte they name.
fn decoded(written: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(written.len());
    let mut rest = written.bytes();
    while let Some(byte) = rest.next() {
        bytes.push(match byte {
            b'+' => b' ',
            b'%' => {
                let digit = |byte: u8| char::from(byte).to_digit(16);
                let (high, low) = (digit(rest.next()?)?, digit(rest.next()?)?);
                u8::try_from(high * 16 + low).ok()?
            }
            other => other,
        });
    }
    String::from_utf8(bytes).ok()
}

/// The headers of a request that are text, for whoever checks that it
/// is a carrier's.
fn headers_of(headers: &HeaderMap) -> Vec<(&str, &str)> {
    headers
        .iter()
        .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
        .collect()
}

fn no_carrier() -> Problem {
    problem(
        StatusCode::NOT_FOUND,
        ErrorCode::UnknownMessage,
        "the daemon answers no calls of a carrier of that name",
    )
}

fn not_the_carriers(detail: impl Into<String>) -> Problem {
    Problem::of(ErrorCode::Forbidden, detail)
}

impl Shared {
    /// What answers the calls of the carrier a path names.
    fn answers(&self, named: &str) -> Result<(Carrier, &Telephony, &Answering), Problem> {
        let telephony = self.telephony.as_ref().ok_or_else(no_carrier)?;
        let carrier = Carrier::named(named).ok_or_else(no_carrier)?;
        let answering = telephony.answering(carrier).ok_or_else(no_carrier)?;
        Ok((carrier, telephony, answering))
    }

    /// The answer to `carrier`, which asks about the call it names
    /// `call`: a session is made for it, unless one was.
    fn answer(
        &self,
        carrier: Carrier,
        telephony: &Telephony,
        answering: &Answering,
        call: &str,
    ) -> Result<Answer, Problem> {
        let named = (carrier, call.to_owned());
        // Held until the call is remembered, so that a call that is
        // asked about twice at once is answered once.
        let mut calls = telephony.calls();
        if let Some(known) = calls.known.get(&named) {
            // The carrier asks again, and nothing has come with the
            // ticket it was given.
            if self.tickets.waits_for(known.session) {
                return Ok(known.answer.clone());
            }
            return Err(Problem::of(
                ErrorCode::OutOfPlace,
                "the call was answered: a request that was signed once is not taken twice",
            ));
        }
        let made = self.create_for_a_call(&answering.agent)?;
        let stream = telephony.stream_of(carrier);
        let answer = carrier
            .answer(&stream, &[(TICKET, made.client_token.as_str())])
            .map_err(|unanswerable| {
                eprintln!("waveportd: a call could not be answered: {unanswerable}");
                Problem::of(ErrorCode::Internal, "the call could not be answered")
            })?;
        // The name of a session is one this daemon made.
        let session = u128::from_str_radix(&made.session, 16).map(SessionId);
        let Ok(session) = session else {
            return Err(Problem::of(
                ErrorCode::Internal,
                "the call could not be answered",
            ));
        };
        let answered = Answered {
            session,
            answer: answer.clone(),
        };
        calls.remember(named, answered);
        Ok(answer)
    }

    /// The session of the call whose stream began with `started`, if the
    /// stream is let in: it says a ticket that is good, and is of the
    /// call the ticket was made for.
    fn admitted(&self, carrier: Carrier, started: &Started) -> Option<SessionId> {
        let (_, telephony, answering) = self.answers(carrier.name()).ok()?;
        let ticket = started.parameters.get(TICKET)?;
        // Used up here, whatever comes of it.
        let session = self.tickets.redeem_for_its_session(ticket)?;
        if let Some(account) = &answering.account
            && started.account.as_ref() != Some(account)
        {
            return None;
        }
        // A ticket lets in the stream of the call it was made for, and
        // of no other, whatever the other is.
        let calls = telephony.calls();
        let known = calls.known.get(&(carrier, started.call.clone()))?;
        (known.session == session).then_some(session)
    }
}

/// Answers a carrier that asks what to do with a call.
pub(crate) async fn voice(
    State(daemon): State<Arc<Shared>>,
    Path(named): Path<String>,
    request: Request,
) -> Result<Response, Refused> {
    let (carrier, telephony, answering) = daemon.answers(&named)?;
    let (asked, body) = request.into_parts();
    let is_a_form = said(&asked.headers, CONTENT_TYPE)
        .and_then(|kind| kind.split(';').next())
        .is_some_and(|kind| {
            kind.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        });
    if !is_a_form {
        return Err(problem(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::Unsupported,
            "a carrier asks with a form, and says so: `Content-Type: \
             application/x-www-form-urlencoded`",
        )
        .into());
    }
    let Ok(body) = to_bytes(body, daemon.limits.body_bytes).await else {
        return Err(problem(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::Malformed,
            "the body is larger than a body may be, or did not arrive",
        )
        .into());
    };
    let Some(fields) = form(&body) else {
        return Err(Problem::of(ErrorCode::Malformed, "the body is no form").into());
    };
    let url = telephony.asked_at("https", asked.uri.path(), asked.uri.query());
    let headers = headers_of(&asked.headers);
    let signed = Asked {
        url: &url,
        headers: &headers,
        form: &fields,
        body: &body,
    };
    carrier
        .verify(&answering.secret, &signed)
        .map_err(|unverified| not_the_carriers(unverified.to_string()))?;
    let Some(about) = carrier.asks_about(&fields) else {
        return Err(Problem::of(ErrorCode::Malformed, "the request names no call").into());
    };
    if let Some(account) = &answering.account
        && about.account != Some(account.as_str())
    {
        return Err(not_the_carriers(
            "the call is of another account than the one whose calls are answered",
        )
        .into());
    }
    let answer = daemon.answer(carrier, telephony, answering, about.call)?;
    let mut response = (StatusCode::OK, answer.body).into_response();
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(answer.content_type));
    // It holds what lets a stream in.
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Answers a carrier that asks for the stream of a call: the connection
/// becomes a WebSocket, and the call is heard from there on.
pub(crate) async fn stream(
    State(daemon): State<Arc<Shared>>,
    Path(named): Path<String>,
    mut request: Request,
) -> Result<Response, Refused> {
    let (carrier, telephony, answering) = daemon.answers(&named)?;
    if daemon.is_draining() {
        return Err(Shared::busy().into());
    }
    if request.uri().query().is_some() {
        let detail = "the address of a stream says nothing: which call it is of is said as it \
                      begins";
        return Err(Problem::of(ErrorCode::Malformed, detail).into());
    }
    let url = telephony.stream_of(carrier);
    let headers = headers_of(request.headers());
    let signed = Asked {
        url: &url,
        headers: &headers,
        form: &[],
        body: &[],
    };
    if let Err(unverified) = carrier.verify(&answering.secret, &signed) {
        if answering.signed_streams {
            return Err(not_the_carriers(unverified.to_string()).into());
        }
        telephony.say_unsigned(carrier, &unverified.to_string());
    }
    let (upgraded, answer) = line::becomes_a_websocket(&mut request, None);
    if let Some(upgraded) = upgraded {
        tokio::spawn(serve(Arc::clone(&daemon), carrier, upgraded));
    }
    Ok(answer)
}

/// One stream, from the answer that made it a WebSocket to the session
/// that takes it.
async fn serve(daemon: Arc<Shared>, carrier: Carrier, upgraded: OnUpgrade) {
    // A carrier that left before it was answered was never there.
    let Ok(upgraded) = upgraded.await else {
        return;
    };
    let conn = WsConn::from_upgraded(TokioIo::new(upgraded)).await;
    let mut going = daemon.going();
    let began = tokio::select! {
        began = Call::of(carrier, conn, STARTS_WITHIN) => began,
        // Whoever has not said which call it brings is not waited for.
        () = raised(&mut going) => return,
    };
    // One that did not begin as a carrier's stream does is closed.
    let Ok(call) = began else {
        return;
    };
    match daemon.admitted(carrier, call.started()) {
        Some(session) => daemon.on_call(session, call).await,
        None => call.refuse().await,
    }
}

#[cfg(test)]
mod tests {

    use waveport_ports::journal::SessionId;
    use waveport_transport_telephony_ws::Carrier;

    use super::{Answered, Calls, REMEMBERED, Telephony, decoded, form};
    use crate::config::Config;

    #[test]
    fn a_form_is_read_as_it_was_written() {
        let fields =
            form(b"CallSid=CA1&From=%2B15550100&Note=a+b%26c%3Dd&Empty=&Bare&%C3%A9=%c3%A9");
        let fields = fields.unwrap();
        let read: Vec<(&str, &str)> = fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                ("CallSid", "CA1"),
                ("From", "+15550100"),
                ("Note", "a b&c=d"),
                ("Empty", ""),
                ("Bare", ""),
                ("é", "é"),
            ]
        );
        assert_eq!(form(b""), Some(Vec::new()));
        // What is cut off, what names no byte, and what is no text.
        for none in ["a=%", "a=%4", "a=%zz", "a=%ff", "%e9=1"] {
            assert_eq!(form(none.as_bytes()), None, "{none}");
        }
        assert_eq!(form(&[b'a', b'=', 0xff]), None);
        assert_eq!(decoded("%2b+%2B").as_deref(), Some("+ +"));
    }

    fn answered(session: u128) -> Answered {
        let answer = Carrier::Twilio.answer("wss://voice.example/stream", &[]);
        Answered {
            session: SessionId(session),
            answer: answer.unwrap(),
        }
    }

    #[test]
    fn the_last_calls_are_remembered_and_the_oldest_is_forgotten_for_the_next() {
        let call = |n: usize| (Carrier::Twilio, format!("CA{n}"));
        let mut calls = Calls::default();
        for n in 0..REMEMBERED {
            calls.remember(call(n), answered(1));
        }
        assert_eq!(
            (calls.known.len(), calls.order.len()),
            (REMEMBERED, REMEMBERED)
        );
        assert!(calls.known.contains_key(&call(0)));
        // One more, and the oldest is forgotten for it: that one and no
        // other.
        calls.remember(call(REMEMBERED), answered(2));
        assert_eq!(
            (calls.known.len(), calls.order.len()),
            (REMEMBERED, REMEMBERED)
        );
        assert!(!calls.known.contains_key(&call(0)));
        assert!(calls.known.contains_key(&call(1)));
        assert_eq!(calls.known[&call(REMEMBERED)].session, SessionId(2));
        calls.remember(call(REMEMBERED + 1), answered(3));
        assert!(!calls.known.contains_key(&call(1)));
        assert!(calls.known.contains_key(&call(2)));
        assert_eq!(calls.order.front(), Some(&call(2)));
    }

    const CALLS: &str = "[telephony]\npublic = \"https://voice.example/\"\n\
                         [telephony.twilio]\nagent = \"desk\"\ntoken_env = \"A_TOKEN\"\n\
                         [agents.desk.speech.stt]\nprovider = \"openai\"\n\
                         [agents.desk.speech.tts]\nprovider = \"cartesia\"\n";

    #[test]
    fn a_daemon_that_answers_calls_holds_what_its_carrier_signs_with() {
        let config = Config::from_toml(CALLS).unwrap();
        let held = |token: &'static str| {
            Telephony::held_by(&config.telephony, |name| {
                (name == "A_TOKEN").then(|| token.to_owned())
            })
        };
        let telephony = held(" a-made-up-token\n").unwrap().unwrap();
        assert_eq!(telephony.host(), "voice.example");
        assert_eq!(telephony.carriers().collect::<Vec<_>>(), [Carrier::Twilio]);
        // The addresses are as the carrier writes them.
        assert_eq!(
            telephony.asked_at("https", "/v1/telephony/twilio/voice", Some("a=1")),
            "https://voice.example/v1/telephony/twilio/voice?a=1"
        );
        assert_eq!(
            telephony.stream_of(Carrier::Twilio),
            "wss://voice.example/v1/telephony/twilio/stream"
        );
        // A variable that holds nothing is no token, and what one holds
        // is not said.
        for none in ["", "  "] {
            let Err(failure) = held(none) else {
                panic!("a daemon was given no token and took it");
            };
            assert!(failure.0.contains("A_TOKEN"), "{failure}");
        }
        // A daemon that is told of no carrier answers no calls, though
        // it is told where carriers would reach it.
        let reached = "[telephony]\npublic = \"https://voice.example\"\n[agents.desk]\n";
        let reached = Config::from_toml(reached).unwrap();
        let none = Telephony::held_by(&reached.telephony, |_| Some("a-made-up-token".to_owned()));
        assert!(none.unwrap().is_none());
        let quiet = Config::from_toml("[agents.desk]\n").unwrap();
        assert!(
            Telephony::held_by(&quiet.telephony, |_| None)
                .unwrap()
                .is_none()
        );
    }
}

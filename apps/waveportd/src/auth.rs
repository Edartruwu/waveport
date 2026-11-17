//! Who may ask: the control keys, the client tokens, and where a
//! request says it is going and where it comes from.

use core::time::Duration;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use sha2::{Digest as _, Sha256};
use waveport_assembly::Failure;
use waveport_ports::journal::SessionId;
use waveport_protocol::client::v1::CLIENT_TOKEN;

/// What is hashed before a secret, so that the hash of one is the hash
/// of nothing else.
const DOMAIN: &[u8] = b"waveportd secret v1\0";

/// What is kept of a secret: from this the secret cannot be had.
fn digest(secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

/// Whether two hashes are the same, found out in the same time whether
/// they are or not.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0, |differ, (a, b)| differ | (a ^ b)) == 0
}

/// The fewest characters a control key has. A key is made by a machine,
/// and one that is shorter was typed by somebody.
const SHORTEST_KEY: usize = 24;

/// The control keys, as their hashes.
pub(crate) struct Keys(Vec<[u8; 32]>);

impl Keys {
    /// The keys the environment variables `names` hold.
    pub(crate) fn from_env(names: &[String]) -> Result<Self, Failure> {
        Self::held_by(names, |name| std::env::var(name).ok())
    }

    /// The keys the variables `names` hold, as `read` reads a variable.
    fn held_by(names: &[String], read: impl Fn(&str) -> Option<String>) -> Result<Self, Failure> {
        let mut keys = Vec::new();
        for name in names {
            let held = read(name).unwrap_or_default();
            let key = held.trim();
            if key.is_empty() {
                return Err(Failure(format!(
                    "the environment variable {name} holds no control key, and `control.key_env` \
                     names it"
                )));
            }
            if key.len() < SHORTEST_KEY {
                return Err(Failure(format!(
                    "the control key in {name} is shorter than {SHORTEST_KEY} characters: it is \
                     to be one nobody can guess, such as `openssl rand -hex 32` makes"
                )));
            }
            keys.push(digest(key));
        }
        Ok(Self(keys))
    }

    /// Whether there is no key, and so nobody to ask for one.
    pub(crate) const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `key` is one of the keys. Every key is looked at, however
    /// the ones before it compared.
    pub(crate) fn admit(&self, key: &str) -> bool {
        let given = digest(key);
        self.0
            .iter()
            .fold(false, |admitted, held| admitted | same(held, &given))
    }
}

/// A client token that was made and has not been used.
struct Ticket {
    session: SessionId,
    until: Instant,
}

/// The client tokens: each lets its holder connect to one session, once,
/// for a while.
#[derive(Default)]
pub(crate) struct Tickets {
    held: Mutex<HashMap<[u8; 32], Ticket>>,
}

impl Tickets {
    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Ticket>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new token for `session`, good for `good_for`.
    pub(crate) fn mint(&self, session: SessionId, good_for: Duration) -> Result<String, Failure> {
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret)
            .map_err(|error| Failure(format!("the operating system gave no entropy: {error}")))?;
        let mut token = String::with_capacity(CLIENT_TOKEN.len() + 64);
        token.push_str(CLIENT_TOKEN);
        for byte in secret {
            token.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            token.push(char::from_digit(u32::from(byte & 15), 16).unwrap_or('0'));
        }
        let now = Instant::now();
        let mut held = self.held();
        // The ones nobody came for are let go of as new ones are made.
        held.retain(|_, ticket| ticket.until > now);
        let until = now + good_for;
        held.insert(digest(&token), Ticket { session, until });
        Ok(token)
    }

    /// Uses `token` for `session`: whether it was made for that session,
    /// is still good and was not used before. It is used up either way,
    /// unless it is a token for another session.
    pub(crate) fn redeem(&self, token: &str, session: SessionId) -> bool {
        let hash = digest(token);
        let mut held = self.held();
        match held.get(&hash) {
            Some(ticket) if ticket.session == session => {
                let good = ticket.until > Instant::now();
                held.remove(&hash);
                good
            }
            _ => false,
        }
    }

    /// Uses `token` for the session it was made for, whichever that is:
    /// the session, if the token is still good and was not used before.
    /// It is used up either way.
    pub(crate) fn redeem_for_its_session(&self, token: &str) -> Option<SessionId> {
        let ticket = self.held().remove(&digest(token))?;
        (ticket.until > Instant::now()).then_some(ticket.session)
    }

    /// Whether a token that was made for `session` is still good and
    /// was not used.
    pub(crate) fn waits_for(&self, session: SessionId) -> bool {
        let now = Instant::now();
        let held = self.held();
        held.values()
            .any(|ticket| ticket.session == session && ticket.until > now)
    }

    /// Lets go of every token that was made for `session`.
    pub(crate) fn revoke(&self, session: SessionId) {
        self.held().retain(|_, ticket| ticket.session != session);
    }
}

/// Where a request may say it is going, and where it may come from.
pub(crate) struct Gate {
    /// What `Host` may say. `None` where anything may be said: the
    /// daemon listens on every address of its machine and was told no
    /// name.
    hosts: Option<Vec<String>>,
    /// Where a page may come from, beside the daemon itself.
    origins: Vec<String>,
}

/// What a request's `Origin` comes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum From {
    /// It names none: it is no page in a browser, or one of the
    /// daemon's own that asks as pages of the same origin do.
    Nowhere,
    /// A page of the daemon itself.
    Here,
    /// A page from somewhere the daemon was told of.
    Allowed,
    /// A page from anywhere else.
    Elsewhere,
}

impl Gate {
    /// The gate of a daemon that listens on `listen` and was told the
    /// names `hosts` and the origins `origins`.
    pub(crate) fn new(listen: SocketAddr, hosts: &[String], origins: &[String]) -> Self {
        let port = listen.port();
        let mut names: Vec<String> = Vec::new();
        if listen.ip().is_loopback() {
            names.extend(["localhost", "127.0.0.1", "[::1]"].map(str::to_owned));
        } else if !listen.ip().is_unspecified() {
            names.push(match listen {
                SocketAddr::V4(at) => at.ip().to_string(),
                SocketAddr::V6(at) => format!("[{}]", at.ip()),
            });
        }
        names.extend(hosts.iter().map(|host| host.to_ascii_lowercase()));
        let mut known = Vec::with_capacity(names.len() * 2);
        for name in names {
            // A name that says its port is that and no other. One that
            // says none is said alone, or with the port listened on.
            let says_its_port = name
                .rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok());
            if !says_its_port {
                known.push(format!("{name}:{port}"));
            }
            known.push(name);
        }
        let open = listen.ip().is_unspecified() && hosts.is_empty();
        Self {
            hosts: (!open).then_some(known),
            origins: origins
                .iter()
                .map(|origin| origin.trim_end_matches('/').to_ascii_lowercase())
                .collect(),
        }
    }

    /// Whether a request that says it is going to `host` is one for this
    /// daemon. A name that somebody pointed at this machine is not its
    /// name.
    pub(crate) fn is_here(&self, host: Option<&str>) -> bool {
        let Some(known) = &self.hosts else {
            return true;
        };
        host.is_some_and(|host| known.contains(&host.to_ascii_lowercase()))
    }

    /// Where a request comes from that says `origin`, on its way to
    /// `host`.
    pub(crate) fn from(&self, origin: Option<&str>, host: Option<&str>) -> From {
        let Some(origin) = origin else {
            return From::Nowhere;
        };
        let origin = origin.trim_end_matches('/').to_ascii_lowercase();
        if self.origins.contains(&origin) {
            return From::Allowed;
        }
        let here = host.is_some_and(|host| {
            let host = host.to_ascii_lowercase();
            origin == format!("http://{host}") || origin == format!("https://{host}")
        });
        if here { From::Here } else { From::Elsewhere }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::net::SocketAddr;

    use waveport_ports::journal::SessionId;

    use super::{From, Gate, Keys, Tickets};

    #[test]
    fn a_client_token_is_good_once_for_its_session_and_for_a_while() {
        let tickets = Tickets::default();
        let (a, b) = (SessionId(1), SessionId(2));
        let token = tickets.mint(a, Duration::from_secs(60)).unwrap();
        assert!(
            token.starts_with("wpc1_") && token.len() == 69,
            "{}",
            token.len()
        );
        assert_ne!(token, tickets.mint(a, Duration::from_secs(60)).unwrap());
        // For another session it is no token, and is not used up.
        assert!(!tickets.redeem(&token, b));
        assert!(tickets.redeem(&token, a));
        assert!(!tickets.redeem(&token, a), "it was used");
        assert!(!tickets.redeem("wpc1_madeup", a));

        // One that nobody came for in time is none.
        let late = tickets.mint(a, Duration::ZERO).unwrap();
        assert!(!tickets.redeem(&late, a));
        // One that was taken back is none.
        let revoked = tickets.mint(b, Duration::from_secs(60)).unwrap();
        let kept = tickets.mint(a, Duration::from_secs(60)).unwrap();
        tickets.revoke(b);
        assert!(!tickets.redeem(&revoked, b));
        assert!(tickets.redeem(&kept, a));
    }

    #[test]
    fn a_client_token_says_which_session_it_is_for_once() {
        let tickets = Tickets::default();
        let (a, b) = (SessionId(1), SessionId(2));
        let for_a = tickets.mint(a, Duration::from_secs(60)).unwrap();
        let for_b = tickets.mint(b, Duration::from_secs(60)).unwrap();
        assert_eq!(tickets.redeem_for_its_session(&for_b), Some(b));
        assert_eq!(tickets.redeem_for_its_session(&for_a), Some(a));
        assert_eq!(tickets.redeem_for_its_session(&for_a), None, "it was used");
        assert_eq!(tickets.redeem_for_its_session("wpc1_madeup"), None);

        // One that nobody came for in time is none, and one that was
        // taken back is none.
        let late = tickets.mint(a, Duration::ZERO).unwrap();
        assert_eq!(tickets.redeem_for_its_session(&late), None);
        let revoked = tickets.mint(b, Duration::from_secs(60)).unwrap();
        tickets.revoke(b);
        assert_eq!(tickets.redeem_for_its_session(&revoked), None);
    }

    #[test]
    fn a_session_is_waited_for_while_a_token_of_its_own_is_good_and_unused() {
        let tickets = Tickets::default();
        let (a, b) = (SessionId(1), SessionId(2));
        assert!(!tickets.waits_for(a));
        let for_a = tickets.mint(a, Duration::from_secs(60)).unwrap();
        assert!(tickets.waits_for(a));
        assert!(!tickets.waits_for(b), "the token is another session's");
        // Used, it is waited for no more.
        assert_eq!(tickets.redeem_for_its_session(&for_a), Some(a));
        assert!(!tickets.waits_for(a));
        // One that nobody came for in time is none, and neither is one
        // that was taken back.
        let _late = tickets.mint(a, Duration::ZERO).unwrap();
        assert!(!tickets.waits_for(a));
        let _revoked = tickets.mint(b, Duration::from_secs(60)).unwrap();
        tickets.revoke(b);
        assert!(!tickets.waits_for(b));
    }

    #[test]
    fn a_control_key_is_one_of_two_and_nothing_shorter_than_a_key() {
        let names = ["KEY_A".to_owned(), "KEY_B".to_owned()];
        let Err(none) = Keys::held_by(&names, |_| None).map(|_| ()) else {
            panic!("there is no key");
        };
        assert!(none.0.contains("KEY_A holds no control key"), "{none}");
        assert!(Keys::from_env(&[]).unwrap().is_empty());
        let held = |second: &'static str| {
            move |name: &str| {
                let key = if name == "KEY_A" {
                    " made-up-control-key-of-a-test\n"
                } else {
                    second
                };
                Some(key.to_owned())
            }
        };
        let Err(short) = Keys::held_by(&names, held("typed-by-hand")).map(|_| ()) else {
            panic!("the second is too short");
        };
        assert!(short.0.contains("KEY_B is shorter"), "{short}");
        // What a variable holds is not repeated.
        assert!(!short.0.contains("typed-by-hand"), "{short}");
        let keys = Keys::held_by(&names, held("another-made-up-key-of-a-test")).unwrap();
        assert!(keys.admit("made-up-control-key-of-a-test"));
        assert!(keys.admit("another-made-up-key-of-a-test"));
        assert!(!keys.admit("made-up-control-key-of-a-tes"));
        assert!(!keys.admit(""));
    }

    #[test]
    fn a_request_is_for_the_daemon_when_it_says_so_by_a_name_of_the_daemon() {
        let local: SocketAddr = "127.0.0.1:7070".parse().unwrap();
        let gate = Gate::new(local, &[], &["https://app.example/".to_owned()]);
        for host in [
            "127.0.0.1:7070",
            "localhost:7070",
            "LOCALHOST:7070",
            "[::1]:7070",
        ] {
            assert!(gate.is_here(Some(host)), "{host}");
        }
        // A name that somebody pointed at this machine, another port,
        // and no name at all.
        for host in ["evil.example:7070", "evil.example", "localhost:7071"] {
            assert!(!gate.is_here(Some(host)), "{host}");
        }
        assert!(!gate.is_here(None));

        assert_eq!(gate.from(None, Some("localhost:7070")), From::Nowhere);
        assert_eq!(
            gate.from(Some("http://localhost:7070"), Some("localhost:7070")),
            From::Here
        );
        assert_eq!(
            gate.from(Some("https://app.example"), Some("localhost:7070")),
            From::Allowed
        );
        for origin in ["https://evil.example", "http://localhost:3000", "null"] {
            assert_eq!(
                gate.from(Some(origin), Some("localhost:7070")),
                From::Elsewhere,
                "{origin}"
            );
        }

        // A daemon that was told its name is reached by it, with its
        // port or with the one the name says.
        let public: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let named = ["voice.example".to_owned(), "edge.example:443".to_owned()];
        let gate = Gate::new(public, &named, &[]);
        for host in ["voice.example", "voice.example:8443", "edge.example:443"] {
            assert!(gate.is_here(Some(host)), "{host}");
        }
        for host in ["edge.example:8443", "localhost:8443", "10.0.0.7:8443"] {
            assert!(!gate.is_here(Some(host)), "{host}");
        }
        // One that listens everywhere and was told no name takes any.
        let gate = Gate::new(public, &[], &[]);
        assert!(gate.is_here(Some("anything.example")) && gate.is_here(None));
        // One on an address of its own is reached by that address.
        let lan: SocketAddr = "10.0.0.7:7070".parse().unwrap();
        let gate = Gate::new(lan, &[], &[]);
        assert!(gate.is_here(Some("10.0.0.7:7070")));
        assert!(!gate.is_here(Some("localhost:7070")));
    }
}

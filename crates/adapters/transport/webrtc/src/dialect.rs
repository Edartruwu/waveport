//! Another protocol than client protocol v1 on the data channel of a
//! call.
//!
//! A far end that speaks one opens the channel itself and is not told
//! of marks or of what to discard, and it does not say what it has
//! played. What it says is put into client protocol v1 by a
//! [`Dialect`], and what it is told in v1 is put into its own protocol
//! by the same. The call knows neither protocol: whoever answers the
//! offer gives it the dialect.

use waveport_protocol::client::v1::{FromClient, FromServer};

/// What a far end says and is told in, where that is not client
/// protocol v1.
///
/// A dialect is of one call. It is asked on the thread that runs the
/// call, which is the thread of a session once the far end is attached
/// to one: it does nothing that takes time.
pub trait Dialect: Send + 'static {
    /// The name of the data channel the far end opens to speak on. A
    /// channel of another name is not read.
    fn channel(&self) -> &'static str;

    /// Takes what the far end said, as the text it sent.
    fn heard(&mut self, said: &str) -> Meant;

    /// What the far end is told in place of `message`, each as the
    /// text that is sent: nothing, one thing or several.
    fn told(&mut self, message: &FromServer) -> Vec<String>;
}

/// What something a far end said in its own protocol comes to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meant {
    /// What it means in client protocol v1, if it means anything there.
    /// It is dealt with as if the far end had said that.
    pub means: Option<FromClient>,
    /// What the far end is answered at once, in its own protocol.
    pub answers: Vec<String>,
}

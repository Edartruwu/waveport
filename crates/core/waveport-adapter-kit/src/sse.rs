//! Server-sent events: the format language models stream their output in.
//!
//! The parser does no I/O. It is given the bytes of a response body as
//! they arrive, cut anywhere, and calls back with each event that is
//! complete. It follows the format as browsers read it: lines end with a
//! line feed, a carriage return or both, an empty line ends an event,
//! several `data` lines are joined with line feeds, and a line that
//! starts with a colon is a comment.

use core::fmt;

/// An event of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SseEvent<'a> {
    /// Its type: what the `event` line said, or `message` when there was
    /// none.
    pub event: &'a str,
    /// Its data.
    pub data: &'a str,
}

/// Why a stream cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SseError {
    /// An event is longer than the parser was told to hold.
    TooLong,
    /// A line is not UTF-8.
    NotText,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLong => "an event of the stream is too long",
            Self::NotText => "a line of the stream is not UTF-8",
        })
    }
}

impl core::error::Error for SseError {}

/// Reads a stream of server-sent events.
#[derive(Clone, Debug)]
pub struct SseParser {
    /// The line being read.
    line: Vec<u8>,
    /// The data of the event being read, each line followed by a line
    /// feed.
    data: String,
    /// The type of the event being read.
    event: String,
    /// Whether the last byte was a carriage return, so that a line feed
    /// that follows belongs to the same line end.
    after_cr: bool,
    /// Whether the stream has begun: a byte order mark is passed over at
    /// the very start only.
    begun: bool,
    /// The most bytes an event may have.
    limit: usize,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    /// The most bytes an event may have unless told otherwise.
    pub const DEFAULT_LIMIT: usize = 1 << 20;

    /// A parser at the start of a stream.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_limit(Self::DEFAULT_LIMIT)
    }

    /// A parser that refuses an event of more than `limit` bytes.
    #[must_use]
    pub const fn with_limit(limit: usize) -> Self {
        Self {
            line: Vec::new(),
            data: String::new(),
            event: String::new(),
            after_cr: false,
            begun: false,
            limit,
        }
    }

    /// Takes the next bytes of the stream and calls `sink` with each
    /// event they complete.
    ///
    /// # Errors
    ///
    /// The stream cannot be read. Nothing more can be made of it.
    pub fn push(
        &mut self,
        bytes: &[u8],
        mut sink: impl FnMut(SseEvent<'_>),
    ) -> Result<(), SseError> {
        for &byte in bytes {
            let after_cr = core::mem::replace(&mut self.after_cr, byte == b'\r');
            match byte {
                b'\n' if after_cr => {}
                b'\n' | b'\r' => self.end_line(&mut sink)?,
                _ => {
                    if self.line.len() + self.data.len() >= self.limit {
                        return Err(SseError::TooLong);
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(())
    }

    fn end_line(&mut self, sink: &mut impl FnMut(SseEvent<'_>)) -> Result<(), SseError> {
        let line = core::str::from_utf8(&self.line).map_err(|_| SseError::NotText)?;
        let line = if self.begun {
            line
        } else {
            line.strip_prefix('\u{FEFF}').unwrap_or(line)
        };
        self.begun = true;

        if line.is_empty() {
            // The event is complete. One without data is not an event.
            if !self.data.is_empty() {
                let data = self.data.strip_suffix('\n').unwrap_or(&self.data);
                let event = if self.event.is_empty() {
                    "message"
                } else {
                    &self.event
                };
                sink(SseEvent { event, data });
            }
            self.data.clear();
            self.event.clear();
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                "event" => {
                    self.event.clear();
                    self.event.push_str(value);
                }
                // `id` and `retry` are for reconnecting, which a response
                // to one request does not do.
                _ => {}
            }
        }
        self.line.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{SseError, SseParser};

    /// The events of `stream`, given to a parser `cut` bytes at a time.
    fn read(stream: &[u8], cut: usize) -> Result<Vec<(String, String)>, SseError> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for piece in stream.chunks(cut) {
            parser.push(piece, |event| {
                events.push((event.event.to_owned(), event.data.to_owned()));
            })?;
        }
        Ok(events)
    }

    fn message(data: &str) -> (String, String) {
        ("message".to_owned(), data.to_owned())
    }

    #[test]
    fn events_end_at_an_empty_line() {
        let stream = b"data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n";
        assert_eq!(
            read(stream, stream.len()).unwrap(),
            [
                message("{\"a\":1}"),
                message("{\"a\":2}"),
                message("[DONE]")
            ]
        );
    }

    #[test]
    fn where_the_stream_is_cut_does_not_matter() {
        let stream = "event: delta\r\ndata: h\u{e9}llo\r\n\r\n: a comment\r\ndata: one\rdata: two\r\rdata:three\n\n"
            .as_bytes();
        let whole = read(stream, stream.len()).unwrap();
        assert_eq!(
            whole,
            [
                ("delta".to_owned(), "h\u{e9}llo".to_owned()),
                message("one\ntwo"),
                message("three"),
            ]
        );
        for cut in 1..stream.len() {
            assert_eq!(read(stream, cut).unwrap(), whole, "cut every {cut} bytes");
        }
    }

    #[test]
    fn the_format_is_read_as_a_browser_reads_it() {
        // One space after the colon is not part of the value; a second is.
        assert_eq!(
            read(b"data:  two spaces\n\n", 4).unwrap(),
            [message(" two spaces")]
        );
        // A line with no colon is a field with an empty value.
        assert_eq!(read(b"data\ndata\n\n", 4).unwrap(), [message("\n")]);
        // An event without data is not one, and its type does not carry over.
        assert_eq!(
            read(b"event: ping\n\ndata: x\n\n", 4).unwrap(),
            [message("x")]
        );
        // Fields that are not known, and comments, are passed over.
        assert_eq!(
            read(b"id: 7\nretry: 100\nfoo: bar\n:keep-alive\ndata: x\n\n", 4).unwrap(),
            [message("x")]
        );
        // A byte order mark at the very start is not part of the stream.
        assert_eq!(read(b"\xEF\xBB\xBFdata: x\n\n", 1).unwrap(), [message("x")]);
        // An event that is not ended is not given.
        assert_eq!(read(b"data: x\n", 4).unwrap(), []);
    }

    #[test]
    fn what_cannot_be_read_is_refused() {
        assert_eq!(read(b"data: \xFF\xFE\n\n", 3), Err(SseError::NotText));

        let mut parser = SseParser::with_limit(16);
        let mut count = 0;
        // Events within the limit pass, however many.
        for _ in 0..100 {
            parser.push(b"data: 12345678\n\n", |_| count += 1).unwrap();
        }
        assert_eq!(count, 100);
        assert_eq!(
            parser.push(b"data: 1234567890123456789\n\n", |_| {}),
            Err(SseError::TooLong)
        );
        // Many short lines add up too.
        let mut parser = SseParser::with_limit(16);
        assert_eq!(
            parser.push(b"data: 123\ndata: 456\ndata: 789\ndata: 012\n\n", |_| {}),
            Err(SseError::TooLong)
        );
    }
}

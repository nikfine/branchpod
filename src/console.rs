//! Writing bytes of unknown encoding to stdout.
//!
//! Into a file or a pipe they go unchanged. A terminal gets text: the Windows console accepts only
//! UTF-8 from std, so anything else is shown as replacement characters.

use std::io::{self, IsTerminal, Write};

pub(crate) struct Output {
    out: Box<dyn Write>,
    /// Set for a terminal.
    decoder: Option<Decoder>,
}

impl Output {
    pub(crate) fn stdout() -> Output {
        let out = io::stdout();
        Output {
            decoder: out.is_terminal().then(Decoder::default),
            out: Box::new(out),
        }
    }

    /// Writes `bytes` and passes them on at once, for whoever watches a followed log.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        match &mut self.decoder {
            Some(decoder) => self.out.write_all(decoder.push(bytes).as_bytes())?,
            None => self.out.write_all(bytes)?,
        }
        self.out.flush()
    }

    /// After the last byte.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if let Some(decoder) = &mut self.decoder {
            self.out.write_all(decoder.finish().as_bytes())?;
        }
        self.out.flush()
    }
}

/// Lossy UTF-8 that arrives in pieces: a character split between two of them is kept whole.
#[derive(Default)]
struct Decoder {
    /// The beginning of a character.
    pending: Vec<u8>,
}

impl Decoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = &self.pending[..];
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (valid, bad) = rest.split_at(e.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).expect("checked"));
                    match e.error_len() {
                        Some(len) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            rest = &bad[len..];
                        }
                        None => {
                            rest = bad;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        text
    }

    fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&rest).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_character_split_between_pieces_is_whole() {
        let bytes = "a\u{e9}\u{20ac}\u{1f600}z".as_bytes();
        let mut decoder = Decoder::default();
        let mut text: String = bytes.iter().map(|&b| decoder.push(&[b])).collect();
        text += &decoder.finish();
        assert_eq!(text, "a\u{e9}\u{20ac}\u{1f600}z");
    }

    #[test]
    fn invalid_bytes_are_replaced() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(b"a\xffb\xe2\x82"), "a\u{fffd}b");
        assert_eq!(decoder.push(b"x"), "\u{fffd}x");
        assert_eq!(decoder.push(b"\xe2"), "");
        assert_eq!(decoder.finish(), "\u{fffd}");
    }
}

//! Server-sent events, as far as a streamed Responses reply needs them
//! (#620): the `data:` of each event, in order, whatever the chunks it
//! arrives in.

/// Bytes of a stream so far, cut into events as they complete.
#[derive(Debug, Default)]
pub(crate) struct EventStream {
    buffer: Vec<u8>,
}

impl EventStream {
    /// Adds the next chunk of the body.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    /// The data of the next complete event, or `None` when the buffer holds
    /// no complete event. Events without data (comments, keep-alives, an
    /// `event:` line alone) are skipped. Several `data:` lines of one event
    /// are joined with a newline.
    pub(crate) fn next_data(&mut self) -> Option<String> {
        loop {
            let (text_end, consumed) = event_end(&self.buffer)?;
            let raw: Vec<u8> = self.buffer.drain(..consumed).collect();
            let text = String::from_utf8_lossy(raw.get(..text_end).unwrap_or_default());
            if let Some(data) = data_of(&text) {
                return Some(data);
            }
        }
    }

    /// What is buffered and not yet a complete event.
    pub(crate) fn rest(&self) -> &[u8] {
        &self.buffer
    }
}

/// Where the first event in `buffer` ends: the length of its text and the
/// length to take off including the blank line that closes it. A line ends
/// with `\n`, `\r\n` or a lone `\r`; a `\r` at the very end waits for the
/// byte after it.
fn event_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut at = 0;
    let mut line_start = 0;
    while let Some(&byte) = buffer.get(at) {
        let terminator = match byte {
            b'\n' => 1,
            b'\r' => match buffer.get(at + 1) {
                Some(b'\n') => 2,
                Some(_) => 1,
                None => return None,
            },
            _ => {
                at += 1;
                continue;
            }
        };
        if at == line_start {
            return Some((line_start, at + terminator));
        }
        at += terminator;
        line_start = at;
    }
    None
}

/// The joined `data:` lines of one event's text, if it has any.
fn data_of(text: &str) -> Option<String> {
    let normal = text.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normal
        .lines()
        .filter_map(|line| {
            let value = line.strip_prefix("data:")?;
            Some(value.strip_prefix(' ').unwrap_or(value))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<String> {
        let mut stream = EventStream::default();
        let mut out = Vec::new();
        for chunk in chunks {
            stream.push(chunk);
            while let Some(data) = stream.next_data() {
                out.push(data);
            }
        }
        out
    }

    #[test]
    fn events_come_out_in_order_whatever_the_chunks() {
        let text = b"event: a\ndata: {\"n\":1}\n\nevent: b\ndata: {\"n\":2}\n\n";
        let whole = all(&[text]);
        assert_eq!(whole, ["{\"n\":1}", "{\"n\":2}"]);
        let bytes: Vec<&[u8]> = text.chunks(1).collect();
        assert_eq!(all(&bytes), whole);
        let thirds: Vec<&[u8]> = text.chunks(7).collect();
        assert_eq!(all(&thirds), whole);
    }

    #[test]
    fn crlf_and_lone_cr_end_lines_too() {
        assert_eq!(
            all(&[b"data: a\r\n\r\ndata: b\r\rdata: c\n\n"]),
            ["a", "b", "c"]
        );
        // A `\r` at the end of a chunk waits for the byte that follows.
        assert_eq!(all(&[b"data: a\r", b"\n\r", b"\ndata: b\n\n"]), ["a", "b"]);
    }

    #[test]
    fn data_lines_join_and_comments_and_empty_events_are_skipped() {
        assert_eq!(
            all(&[b": keep-alive\n\nevent: x\n\ndata: one\ndata: two\n\n\n\ndata:three\n\n"]),
            ["one\ntwo", "three"]
        );
    }

    #[test]
    fn an_event_without_its_blank_line_is_not_yet_an_event() {
        let mut stream = EventStream::default();
        stream.push(b"data: partial\n");
        assert_eq!(stream.next_data(), None);
        assert_eq!(stream.rest(), b"data: partial\n");
        stream.push(b"\n");
        assert_eq!(stream.next_data().unwrap(), "partial");
        assert!(stream.rest().is_empty());
    }

    #[test]
    fn a_multibyte_character_cut_between_chunks_survives() {
        let text = "data: h\u{e9}llo\n\n".as_bytes();
        let cut = text.iter().position(|&b| b == 0xC3).unwrap() + 1;
        let (first, second) = text.split_at(cut);
        assert_eq!(all(&[first, second]), ["h\u{e9}llo"]);
    }
}

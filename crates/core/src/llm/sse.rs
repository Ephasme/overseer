/// Incremental parser for the `text/event-stream` format, fed raw body chunks.
///
/// Only `data` lines are kept; comments (`:` lines, such as `OpenRouter`'s
/// `: OPENROUTER PROCESSING` keep-alives) and other fields are dropped. Bytes are
/// buffered until a full line is available, so a chunk boundary inside a line,
/// inside a `\r\n`, or inside a multi-byte UTF-8 character is harmless.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: String,
    has_data: bool,
}

impl SseParser {
    /// Feeds the next body chunk; returns the `data` payload of every event it completed.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(&String::from_utf8_lossy(&line), &mut events);
        }
        events
    }

    /// Flushes an event the stream ended without terminating with a blank line.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let mut events = Vec::new();
            self.line(&String::from_utf8_lossy(&rest), &mut events);
            debug_assert_eq!(events, Vec::<String>::new());
        }
        self.has_data.then(|| {
            self.has_data = false;
            std::mem::take(&mut self.data)
        })
    }

    fn line(&mut self, line: &str, events: &mut Vec<String>) {
        if line.is_empty() {
            if self.has_data {
                self.has_data = false;
                events.push(std::mem::take(&mut self.data));
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        if field == "data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(value);
            self.has_data = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SseParser;

    #[test]
    fn yields_data_of_each_event() {
        let mut parser = SseParser::default();
        let events = parser.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(events, ["one", "two"]);
    }

    #[test]
    fn joins_multi_line_data_with_newline() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: a\ndata: b\n\n"), ["a\nb"]);
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        let mut parser = SseParser::default();
        let events = parser.push(b": OPENROUTER PROCESSING\n\nevent: x\nid: 3\ndata: kept\n\n");
        assert_eq!(events, ["kept"]);
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: x\r\n\r\n"), ["x"]);
    }

    #[test]
    fn survives_a_chunk_split_inside_a_line() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: {\"a\""), Vec::<String>::new());
        assert_eq!(parser.push(b":1}\n\n"), ["{\"a\":1}"]);
    }

    #[test]
    fn survives_a_chunk_split_inside_a_utf8_character() {
        let mut parser = SseParser::default();
        let bytes = "data: é\n\n".as_bytes();
        assert_eq!(parser.push(&bytes[..7]), Vec::<String>::new());
        assert_eq!(parser.push(&bytes[7..]), ["é"]);
    }

    #[test]
    fn data_without_a_space_after_the_colon() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data:x\n\n"), ["x"]);
    }

    #[test]
    fn finish_flushes_an_unterminated_event() {
        let mut parser = SseParser::default();
        assert_eq!(parser.push(b"data: [DONE]"), Vec::<String>::new());
        assert_eq!(parser.finish().as_deref(), Some("[DONE]"));
        assert_eq!(parser.finish(), None);
    }
}

/// Longest tool output handed back to the model, in bytes.
pub const OUTPUT_LIMIT: usize = 30 * 1024;

const HALF: usize = OUTPUT_LIMIT / 2;

/// Cuts `text` to [`OUTPUT_LIMIT`] bytes, keeping the first and last half around a marker that
/// gives the number of bytes dropped. Cutting happens on `char` boundaries.
#[must_use]
pub fn cap_output(text: &str) -> String {
    let mut buffer = OutputBuffer::default();
    buffer.push(text);
    buffer.finish()
}

/// Accumulates output while keeping memory bounded: the first [`HALF`] bytes and the last [`HALF`]
/// bytes, with the count of what fell between them. A command that never stops printing costs no
/// more memory than this.
#[derive(Debug, Default)]
pub(crate) struct OutputBuffer {
    head: String,
    tail: String,
    dropped: usize,
}

impl OutputBuffer {
    pub(crate) fn push(&mut self, chunk: &str) {
        let mut rest = chunk;
        if self.head.len() < HALF {
            let cut = floor_boundary(rest, HALF - self.head.len());
            self.head.push_str(&rest[..cut]);
            rest = &rest[cut..];
        }
        if rest.is_empty() {
            return;
        }
        self.tail.push_str(rest);
        if self.tail.len() > HALF {
            let cut = ceil_boundary(&self.tail, self.tail.len() - HALF);
            self.dropped += cut;
            self.tail.drain(..cut);
        }
    }

    pub(crate) fn finish(self) -> String {
        if self.dropped == 0 {
            self.head + &self.tail
        } else {
            format!(
                "{}\n… [{} bytes omitted] …\n{}",
                self.head, self.dropped, self.tail
            )
        }
    }
}

fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{OUTPUT_LIMIT, OutputBuffer, cap_output};

    #[test]
    fn short_output_is_untouched() {
        assert_eq!(cap_output("hello\n"), "hello\n");
    }

    #[test]
    fn output_at_the_limit_is_untouched() {
        let text = "a".repeat(OUTPUT_LIMIT);
        assert_eq!(cap_output(&text), text);
    }

    #[test]
    fn long_output_keeps_head_and_tail_around_a_marker() {
        let text = format!(
            "{}{}{}",
            "H".repeat(OUTPUT_LIMIT),
            "x".repeat(5000),
            "T".repeat(OUTPUT_LIMIT)
        );
        let capped = cap_output(&text);
        assert!(capped.starts_with(&"H".repeat(100)), "head kept");
        assert!(capped.ends_with(&"T".repeat(100)), "tail kept");
        let dropped: usize = capped
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .expect("marker carries a byte count");
        assert_eq!(dropped, text.len() - OUTPUT_LIMIT, "count is exact");
        assert!(capped.len() <= OUTPUT_LIMIT + 64, "{}", capped.len());
    }

    #[test]
    fn cutting_never_splits_a_character() {
        let text = "é".repeat(OUTPUT_LIMIT);
        let capped = cap_output(&text);
        assert!(capped.starts_with('é') && capped.ends_with('é'));
        assert!(!capped.contains('\u{FFFD}'));
        assert!(capped.len() <= OUTPUT_LIMIT + 64, "{}", capped.len());
    }

    #[test]
    fn buffer_grows_by_chunks_and_stays_bounded() {
        let mut buffer = OutputBuffer::default();
        for _ in 0..1000 {
            buffer.push(&"y".repeat(1000));
        }
        let output = buffer.finish();
        assert!(output.len() <= OUTPUT_LIMIT + 64, "{}", output.len());
        assert!(output.contains("bytes omitted"), "{}", &output[..80]);
    }
}

//! Minimal incremental server-sent-events parser.

#[derive(Debug, Default, Clone)]
pub struct Event {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
    cur: Event,
    has_data: bool,
}

impl Parser {
    /// Feed bytes; complete events are appended to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Event>) {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(rel) = memchr(b'\n', &self.buf[start..]) {
            let end = start + rel;
            let mut line = &self.buf[start..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if line.is_empty() {
                if self.has_data || self.cur.event.is_some() {
                    out.push(std::mem::take(&mut self.cur));
                    self.has_data = false;
                }
            } else if line[0] != b':' {
                let (field, value) = match line.iter().position(|&b| b == b':') {
                    Some(i) => {
                        let v = &line[i + 1..];
                        (&line[..i], v.strip_prefix(b" ").unwrap_or(v))
                    }
                    None => (line, &b""[..]),
                };
                match field {
                    b"data" => {
                        if self.has_data {
                            self.cur.data.push('\n');
                        }
                        self.cur.data.push_str(&String::from_utf8_lossy(value));
                        self.has_data = true;
                    }
                    b"event" => self.cur.event = Some(String::from_utf8_lossy(value).into_owned()),
                    _ => {}
                }
            }
            start = end + 1;
        }
        self.buf.drain(..start);
    }

    /// Flush a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self, out: &mut Vec<Event>) {
        if !self.buf.is_empty() {
            self.push(b"\n", out);
        }
        if self.has_data {
            out.push(std::mem::take(&mut self.cur));
            self.has_data = false;
        }
    }
}

fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

pub fn frame(event: Option<&str>, data: &str) -> String {
    match event {
        Some(e) => format!("event: {e}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_across_chunks() {
        let mut p = Parser::default();
        let mut out = Vec::new();
        p.push(b"event: a\ndata: {\"x\"", &mut out);
        assert!(out.is_empty());
        p.push(b":1}\r\n\r\ndata: two\ndata: lines\n\n: comment\n\n", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].event.as_deref(), Some("a"));
        assert_eq!(out[0].data, "{\"x\":1}");
        assert_eq!(out[1].data, "two\nlines");
    }
}

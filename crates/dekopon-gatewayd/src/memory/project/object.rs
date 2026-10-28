use std::io::{self, BufRead};

// serde_json's default nesting limit; deeper arguments are cut rather than walked.
const MAX_DEPTH: u32 = 128;

enum Stop {
    Invalid,
    Io(io::Error),
}

impl From<io::Error> for Stop {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(super) fn is_json_object(reader: impl BufRead) -> io::Result<bool> {
    let mut scanner = Scanner { reader };
    match scanner.document() {
        Ok(()) => Ok(true),
        Err(Stop::Invalid) => Ok(false),
        Err(Stop::Io(error)) => Err(error),
    }
}

struct Scanner<R> {
    reader: R,
}

impl<R: BufRead> Scanner<R> {
    fn document(&mut self) -> Result<(), Stop> {
        self.whitespace()?;
        self.expect(b'{')?;
        self.object(1)?;
        self.whitespace()?;
        match self.peek()? {
            None => Ok(()),
            Some(_) => Err(Stop::Invalid),
        }
    }

    fn peek(&mut self) -> Result<Option<u8>, Stop> {
        Ok(self.reader.fill_buf()?.first().copied())
    }

    fn byte(&mut self) -> Result<u8, Stop> {
        let byte = self.peek()?.ok_or(Stop::Invalid)?;
        self.reader.consume(1);
        Ok(byte)
    }

    fn expect(&mut self, wanted: u8) -> Result<(), Stop> {
        if self.byte()? == wanted {
            Ok(())
        } else {
            Err(Stop::Invalid)
        }
    }

    fn whitespace(&mut self) -> Result<(), Stop> {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek()? {
            self.reader.consume(1);
        }
        Ok(())
    }

    fn value(&mut self, depth: u32) -> Result<(), Stop> {
        self.whitespace()?;
        match self.byte()? {
            b'{' => self.object(depth + 1),
            b'[' => self.array(depth + 1),
            b'"' => self.string(),
            b't' => self.literal(b"rue"),
            b'f' => self.literal(b"alse"),
            b'n' => self.literal(b"ull"),
            b'-' => {
                let first = self.byte()?;
                self.number(first)
            }
            first @ b'0'..=b'9' => self.number(first),
            _ => Err(Stop::Invalid),
        }
    }

    fn object(&mut self, depth: u32) -> Result<(), Stop> {
        if depth > MAX_DEPTH {
            return Err(Stop::Invalid);
        }
        self.whitespace()?;
        if self.peek()? == Some(b'}') {
            self.reader.consume(1);
            return Ok(());
        }
        loop {
            self.whitespace()?;
            self.expect(b'"')?;
            self.string()?;
            self.whitespace()?;
            self.expect(b':')?;
            self.value(depth)?;
            self.whitespace()?;
            match self.byte()? {
                b',' => {}
                b'}' => return Ok(()),
                _ => return Err(Stop::Invalid),
            }
        }
    }

    fn array(&mut self, depth: u32) -> Result<(), Stop> {
        if depth > MAX_DEPTH {
            return Err(Stop::Invalid);
        }
        self.whitespace()?;
        if self.peek()? == Some(b']') {
            self.reader.consume(1);
            return Ok(());
        }
        loop {
            self.value(depth)?;
            self.whitespace()?;
            match self.byte()? {
                b',' => {}
                b']' => return Ok(()),
                _ => return Err(Stop::Invalid),
            }
        }
    }

    fn string(&mut self) -> Result<(), Stop> {
        loop {
            match self.byte()? {
                b'"' => return Ok(()),
                b'\\' => match self.byte()? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                    b'u' => {
                        for _ in 0..4 {
                            if !self.byte()?.is_ascii_hexdigit() {
                                return Err(Stop::Invalid);
                            }
                        }
                    }
                    _ => return Err(Stop::Invalid),
                },
                0x00..=0x1f => return Err(Stop::Invalid),
                _ => {}
            }
        }
    }

    fn literal(&mut self, rest: &[u8]) -> Result<(), Stop> {
        for wanted in rest {
            self.expect(*wanted)?;
        }
        Ok(())
    }

    fn digits(&mut self) -> Result<usize, Stop> {
        let mut count = 0;
        while let Some(b'0'..=b'9') = self.peek()? {
            self.reader.consume(1);
            count += 1;
        }
        Ok(count)
    }

    fn number(&mut self, first: u8) -> Result<(), Stop> {
        match first {
            b'0' => {}
            b'1'..=b'9' => {
                self.digits()?;
            }
            _ => return Err(Stop::Invalid),
        }
        if self.peek()? == Some(b'.') {
            self.reader.consume(1);
            if self.digits()? == 0 {
                return Err(Stop::Invalid);
            }
        }
        if let Some(b'e' | b'E') = self.peek()? {
            self.reader.consume(1);
            if let Some(b'+' | b'-') = self.peek()? {
                self.reader.consume(1);
            }
            if self.digits()? == 0 {
                return Err(Stop::Invalid);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEPTH, is_json_object};

    fn object(text: &[u8]) -> bool {
        is_json_object(text).unwrap()
    }

    #[test]
    fn only_a_complete_json_object_is_an_object() {
        for valid in [
            &br#"{}"#[..],
            br#" { "a" : [1, -0.5e+3, true, false, null, "x\"\u00e9\n"], "b": {} } "#,
            br#"{"nested":[[{"k":[]}]]}"#,
        ] {
            assert!(object(valid), "{}", String::from_utf8_lossy(valid));
        }
        for invalid in [
            &b""[..],
            b"[]",
            b"\"text\"",
            b"{",
            b"{\"a\":1,}",
            b"{\"a\" 1}",
            b"{\"a\":01}",
            b"{\"a\":1.}",
            b"{\"a\":tru}",
            b"{\"a\":\"\\x\"}",
            b"{\"a\":\"\x01\"}",
            b"{} {}",
            b"{a:1}",
        ] {
            assert!(!object(invalid), "{}", String::from_utf8_lossy(invalid));
        }
    }

    #[test]
    fn long_keys_and_values_are_scanned_without_being_kept() {
        let long = "k".repeat(4 * 1024 * 1024);
        let document = format!("{{\"{long}\":\"{long}\",\"{long}x\":[\"{long}\"]}}");
        assert!(object(document.as_bytes()));
    }

    #[test]
    fn nesting_past_the_limit_is_refused() {
        let nested = |depth: u32| {
            let depth = depth as usize;
            format!(
                "{}{}",
                "{\"a\":".repeat(depth - 1),
                "{}".to_owned() + &"}".repeat(depth - 1)
            )
        };
        assert!(object(nested(MAX_DEPTH).as_bytes()));
        assert!(!object(nested(MAX_DEPTH + 1).as_bytes()));
        let deep_array = format!("{{\"a\":{}}}", "[".repeat(1_000_000));
        assert!(!object(deep_array.as_bytes()));
    }
}

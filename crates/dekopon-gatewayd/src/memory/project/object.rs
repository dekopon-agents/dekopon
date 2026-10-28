use std::io::{self, BufRead};

// serde_json refuses its 128th nested container, so the scan accepts at most 127.
const MAX_DEPTH: u32 = 127;

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
        if let Some(b'-' | b'0'..=b'9') = self.peek()? {
            return self.number();
        }
        match self.byte()? {
            b'{' => self.object(depth + 1),
            b'[' => self.array(depth + 1),
            b'"' => self.string(),
            b't' => self.literal(b"rue"),
            b'f' => self.literal(b"alse"),
            b'n' => self.literal(b"ull"),
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
                    b'u' => match self.hex()? {
                        0xdc00..=0xdfff => return Err(Stop::Invalid),
                        0xd800..=0xdbff => {
                            self.expect(b'\\')?;
                            self.expect(b'u')?;
                            if !(0xdc00..=0xdfff).contains(&self.hex()?) {
                                return Err(Stop::Invalid);
                            }
                        }
                        _ => {}
                    },
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

    fn hex(&mut self) -> Result<u16, Stop> {
        let mut code = 0;
        for _ in 0..4 {
            let digit = char::from(self.byte()?).to_digit(16).ok_or(Stop::Invalid)?;
            code = code * 16 + digit as u16;
        }
        Ok(code)
    }

    // serde_json decides which numbers parse, including the overflow it refuses; without
    // `float_roundtrip` its number path keeps no digit buffer.
    fn number(&mut self) -> Result<(), Stop> {
        let mut token = serde_json::Deserializer::from_reader(NumberToken(&mut self.reader));
        match serde::Deserialize::deserialize(&mut token)
            .and_then(|_: serde_json::Number| token.end())
        {
            Ok(()) => Ok(()),
            Err(error) => match error.io_error_kind() {
                Some(kind) => Err(Stop::Io(io::Error::from(kind))),
                None => Err(Stop::Invalid),
            },
        }
    }
}

struct NumberToken<'a, R>(&'a mut R);

impl<R: BufRead> io::Read for NumberToken<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.0.fill_buf()?;
        let count = available
            .iter()
            .take(buf.len())
            .take_while(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
            .count();
        buf[..count].copy_from_slice(&available[..count]);
        self.0.consume(count);
        Ok(count)
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

    #[test]
    fn the_scan_accepts_exactly_what_serde_json_parses_as_an_object() {
        let nest = |depth: usize| {
            format!(
                "{}{}",
                "{\"a\":".repeat(depth - 1),
                "{}".to_owned() + &"}".repeat(depth - 1)
            )
        };
        let arrays = |depth: usize| {
            format!(
                "{{\"a\":{}{}}}",
                "[".repeat(depth - 1),
                "]".repeat(depth - 1)
            )
        };
        let mut cases: Vec<String> = [
            r#"{}"#,
            r#"{"a":1}"#,
            r#" {"a" : [1, 2, {"b": null}]} "#,
            r#"{"a":1,"a":2}"#,
            r#"{"\ud800":0}"#,
            r#"{"\udc00":0}"#,
            r#"{"a":"😀"}"#,
            r#"{"a":"\ud83d"}"#,
            r#"{"a":"\ud83dx"}"#,
            r#"{"a":"\ud83d\n"}"#,
            r#"{"a":"\ud83dA"}"#,
            r#"{"😀":1}"#,
            r#"{"a":"􏿿"}"#,
            r#"{"a":"\u00zz"}"#,
            r#"{"a":"\q"}"#,
            r#"{"a":1e308}"#,
            r#"{"a":1e309}"#,
            r#"{"a":-1e400}"#,
            r#"{"a":1e-400}"#,
            r#"{"a":0e999999}"#,
            r#"{"a":1.7976931348623157e308}"#,
            r#"{"a":1.7976931348623159e308}"#,
            r#"{"a":18446744073709551615}"#,
            r#"{"a":18446744073709551616}"#,
            r#"{"a":-9223372036854775809}"#,
            r#"{"a":01}"#,
            r#"{"a":-}"#,
            r#"{"a":1.}"#,
            r#"{"a":.5}"#,
            r#"{"a":1e}"#,
            r#"{"a":1e+}"#,
            r#"{"a":+1}"#,
            r#"{"a":-0}"#,
            r#"{"a":1E5}"#,
            r#"{"a":1-2}"#,
            r#"{"a":1.5.5}"#,
            r#"{"a":tru}"#,
            r#"{"a":true}"#,
            r#"{"a":nul}"#,
            "{\"a\":\"\x01\"}",
            "{\"a\":\"\x7f\"}",
            r#"{"a":1,}"#,
            r#"[]"#,
            r#""x""#,
            r#"1"#,
            r#"{} {}"#,
            r#"{"a" 1}"#,
            "",
            " ",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        cases.push(format!("{{\"a\":1{}}}", "0".repeat(400)));
        cases.push(format!("{{\"a\":0.{}1}}", "0".repeat(5000)));
        cases.push(format!("{{\"a\":1{}e-400}}", "0".repeat(300)));
        for depth in [126, 127, 128, 129] {
            cases.push(nest(depth));
            cases.push(arrays(depth));
        }
        for case in cases {
            let serde = serde_json::from_str::<serde_json::Value>(&case)
                .is_ok_and(|value| value.is_object());
            assert_eq!(
                object(case.as_bytes()),
                serde,
                "{}",
                &case[..case.len().min(80)]
            );
        }
    }
}
